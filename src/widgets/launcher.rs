use std::{
    cell::{Cell, RefCell},
    collections::{HashSet, VecDeque},
    env, fs,
    path::{Component, Path, PathBuf},
    rc::{Rc, Weak},
    time::Duration,
};

use gtk::{gdk, gio, glib, pango, prelude::*};
use tracing::warn;

use super::tooltip::BarTooltipExt;
use super::{
    BAR_POPUP_WIDTH, CssTransition, Generation, PopupReveal, SmoothScrollConfig,
    attach_popup_escape_handler, attach_popup_lifecycle, build_bar_popup, clear_box,
    detach_application_window, install_smooth_scroll, reset_hidden_popup_state,
    run_when_popup_visible,
};

const LIST_HEIGHT: i32 = 300;
const SMOOTH_SCROLL: SmoothScrollConfig = SmoothScrollConfig::new(72.0, 150.0, 90.0);
const LAUNCHER_POPUP_NAMESPACE: &str = "obsidian-bar-launcher";
const FAVORITES_STATE_FILE: &str = "favorite-launcher-apps.json";
const HIDDEN_STATE_FILE: &str = "hidden-launcher-apps.json";
const VIEW_TRANSITION_DURATION: Duration = Duration::from_millis(190);
const FAVORITES_SLOT_WIDTH: i32 = 42;
const COMPACT_POPUP_WIDTH: i32 = BAR_POPUP_WIDTH - FAVORITES_SLOT_WIDTH;
const FAVORITES_COLUMNS: i32 = 5;
const FAVORITES_LAYOUT_TRANSITION_DURATION: Duration = Duration::from_millis(190);
const FAVORITE_REMOVE_TRANSITION_DURATION: Duration = Duration::from_millis(160);

const ICON_LAUNCHER: &str = "\u{f003b}";
const ICON_FALLBACK: &str = "\u{f003b}";
const ICON_HIDE: &str = "\u{f06d1}";
const ICON_RESTORE: &str = "\u{f05e1}";
const ICON_BACK: &str = "\u{f004d}";
const ICON_FAVORITE: &str = "\u{f04ce}";
const ICON_FAVORITE_OUTLINE: &str = "\u{f04d2}";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LauncherViewTransition {
    HiddenForward,
    HiddenBack,
    CategoryForward,
    CategoryBack,
}

impl LauncherViewTransition {
    const CLASSES: &[&str] = &[
        "launcher-view-hidden-forward",
        "launcher-view-hidden-back",
        "launcher-view-category-forward",
        "launcher-view-category-back",
    ];

    fn css_class(self) -> &'static str {
        match self {
            Self::HiddenForward => "launcher-view-hidden-forward",
            Self::HiddenBack => "launcher-view-hidden-back",
            Self::CategoryForward => "launcher-view-category-forward",
            Self::CategoryBack => "launcher-view-category-back",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LauncherCategory {
    Favorites,
    All,
    Internet,
    Media,
    Office,
    Games,
    System,
}

impl LauncherCategory {
    const BUTTONS: [Self; 7] = [
        Self::Favorites,
        Self::All,
        Self::Internet,
        Self::Media,
        Self::Office,
        Self::Games,
        Self::System,
    ];

    const PRIORITY: [Self; 5] = [
        Self::Games,
        Self::Internet,
        Self::Media,
        Self::Office,
        Self::System,
    ];

    fn index(self) -> usize {
        match self {
            Self::Favorites => 0,
            Self::All => 1,
            Self::Internet => 2,
            Self::Media => 3,
            Self::Office => 4,
            Self::Games => 5,
            Self::System => 6,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Favorites => "Favorites",
            Self::All => "All",
            Self::Internet => "Web",
            Self::Media => "Media",
            Self::Office => "Office",
            Self::Games => "Games",
            Self::System => "System",
        }
    }

    fn matches(self) -> &'static [&'static str] {
        match self {
            Self::Favorites | Self::All => &[],
            Self::Internet => &[
                "network",
                "webbrowser",
                "email",
                "chat",
                "instantmessaging",
                "filetransfer",
                "p2p",
            ],
            Self::Media => &[
                "audiovideo",
                "audio",
                "video",
                "player",
                "recorder",
                "music",
                "photography",
                "graphics",
            ],
            Self::Office => &[
                "office",
                "wordprocessor",
                "spreadsheet",
                "presentation",
                "calendar",
                "contactmanagement",
                "education",
                "science",
            ],
            Self::Games => &["game"],
            Self::System => &[
                "system",
                "settings",
                "utility",
                "security",
                "monitor",
                "terminalemulator",
                "filemanager",
            ],
        }
    }
}

struct LaunchableApp {
    key: String,
    name: String,
    meta: String,
    primary_category: Option<LauncherCategory>,
    search_blob: String,
    info: gio::AppInfo,
}

#[derive(Default)]
struct LauncherCatalogData {
    apps: Vec<Rc<LaunchableApp>>,
    valid_keys: HashSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct DesktopWatch {
    directory: PathBuf,
    exact_path: Option<PathBuf>,
}

struct LauncherCatalogBuild {
    pending: VecDeque<gio::AppInfo>,
    data: LauncherCatalogData,
}

impl LaunchableApp {
    fn matches_category(&self, category: LauncherCategory) -> bool {
        matches!(
            category,
            LauncherCategory::Favorites | LauncherCategory::All
        ) || self.primary_category == Some(category)
    }
}

#[derive(Clone, Default)]
struct LauncherUserState {
    hidden: HashSet<String>,
    favorites: HashSet<String>,
}

impl LauncherUserState {
    fn read() -> Self {
        Self {
            hidden: read_state_keys(HIDDEN_STATE_FILE),
            favorites: read_state_keys(FAVORITES_STATE_FILE),
        }
    }

    fn default_category(&self) -> LauncherCategory {
        if self.favorites.is_empty() {
            LauncherCategory::All
        } else {
            LauncherCategory::Favorites
        }
    }
}

#[derive(Clone, Copy)]
enum LauncherStateKind {
    Hidden,
    Favorite,
}

impl LauncherStateKind {
    fn file_name(self) -> &'static str {
        match self {
            Self::Hidden => HIDDEN_STATE_FILE,
            Self::Favorite => FAVORITES_STATE_FILE,
        }
    }

    fn keys_mut(self, state: &mut LauncherUserState) -> &mut HashSet<String> {
        match self {
            Self::Hidden => &mut state.hidden,
            Self::Favorite => &mut state.favorites,
        }
    }
}

struct LauncherAppRow {
    app: Rc<LaunchableApp>,
    root: gtk::Box,
    hidden_icon: gtk::Label,
    favorite_icon: gtk::Label,
    hidden_state: Cell<Option<bool>>,
    favorite_state: Cell<Option<bool>>,
    visible_state: Cell<Option<bool>>,
}

impl LauncherAppRow {
    fn sync_state(&self, state: &LauncherUserState) {
        let hidden = state.hidden.contains(&self.app.key);
        if self.hidden_state.get() != Some(hidden) {
            self.hidden_state.set(Some(hidden));
            self.hidden_icon
                .set_label(if hidden { ICON_RESTORE } else { ICON_HIDE });
        }

        let favorite = state.favorites.contains(&self.app.key);
        if self.favorite_state.get() != Some(favorite) {
            self.favorite_state.set(Some(favorite));
            self.favorite_icon.set_label(if favorite {
                ICON_FAVORITE
            } else {
                ICON_FAVORITE_OUTLINE
            });
            if favorite {
                self.favorite_icon.add_css_class("launcher-favorite-active");
            } else {
                self.favorite_icon
                    .remove_css_class("launcher-favorite-active");
            }
        }
    }

    fn set_visible(&self, visible: bool) {
        if self.visible_state.get() != Some(visible) {
            self.visible_state.set(Some(visible));
            self.root.set_visible(visible);
        }
    }
}

pub struct LauncherCatalog {
    data: RefCell<LauncherCatalogData>,
    subscribers: RefCell<Vec<Weak<LauncherController>>>,
    refresh_scheduled: Cell<bool>,
    refresh_again: Cell<bool>,
    initialized: Cell<bool>,
    app_monitor: gio::AppInfoMonitor,
    app_monitor_handler: RefCell<Option<glib::SignalHandlerId>>,
    desktop_monitors: RefCell<Vec<gio::FileMonitor>>,
    monitor_rebuild_scheduled: Cell<bool>,
}

impl LauncherCatalog {
    pub fn new() -> Rc<Self> {
        let app_monitor = gio::AppInfoMonitor::get();
        let catalog = Rc::new(Self {
            data: RefCell::new(LauncherCatalogData::default()),
            subscribers: RefCell::new(Vec::new()),
            refresh_scheduled: Cell::new(false),
            refresh_again: Cell::new(false),
            initialized: Cell::new(false),
            app_monitor,
            app_monitor_handler: RefCell::new(None),
            desktop_monitors: RefCell::new(Vec::new()),
            monitor_rebuild_scheduled: Cell::new(false),
        });

        let weak = Rc::downgrade(&catalog);
        let handler = catalog.app_monitor.connect_changed(move |_| {
            if let Some(catalog) = weak.upgrade() {
                catalog.schedule_refresh();
                catalog.schedule_monitor_rebuild();
            }
        });
        catalog.app_monitor_handler.replace(Some(handler));
        catalog.rebuild_desktop_monitors();
        catalog.schedule_refresh();
        catalog
    }

    fn subscribe(&self, controller: &Rc<LauncherController>) {
        self.subscribers
            .borrow_mut()
            .push(Rc::downgrade(controller));
    }

    fn unsubscribe(&self, controller: &Rc<LauncherController>) {
        self.subscribers.borrow_mut().retain(|subscriber| {
            subscriber
                .upgrade()
                .is_some_and(|candidate| !Rc::ptr_eq(&candidate, controller))
        });
    }

    fn refresh_if_stale(self: &Rc<Self>) {
        if self.refresh_scheduled.get() {
            return;
        }

        let stale = !self.initialized.get() || {
            let current_keys = current_visible_app_keys();
            let catalog = self.data.borrow();
            !current_keys.eq(&catalog.valid_keys)
        };
        if stale {
            self.schedule_refresh();
        }
    }

    fn rebuild_desktop_monitors(self: &Rc<Self>) {
        let mut monitors = Vec::new();

        for watch in desktop_watches() {
            let file = gio::File::for_path(&watch.directory);
            let Ok(monitor) =
                file.monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
            else {
                continue;
            };

            let weak = Rc::downgrade(self);
            let exact_path = watch.exact_path.clone();
            let rebuild_after_change = exact_path.is_some();
            monitor.connect_changed(move |_, file, other_file, _| {
                if !desktop_watch_matches(exact_path.as_deref(), file, other_file) {
                    return;
                }

                let Some(catalog) = weak.upgrade() else {
                    return;
                };
                catalog.schedule_refresh();
                if rebuild_after_change {
                    catalog.schedule_monitor_rebuild();
                }
            });
            monitors.push(monitor);
        }

        self.desktop_monitors.replace(monitors);
    }

    fn schedule_monitor_rebuild(self: &Rc<Self>) {
        if self.monitor_rebuild_scheduled.replace(true) {
            return;
        }

        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            let Some(catalog) = weak.upgrade() else {
                return;
            };
            catalog.monitor_rebuild_scheduled.set(false);
            catalog.rebuild_desktop_monitors();
        });
    }

    fn schedule_refresh(self: &Rc<Self>) {
        if self.refresh_scheduled.replace(true) {
            self.refresh_again.set(true);
            return;
        }

        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            let Some(catalog) = weak.upgrade() else {
                return;
            };
            catalog.begin_refresh();
        });
    }

    fn begin_refresh(self: &Rc<Self>) {
        const APPS_PER_IDLE: usize = 12;

        let build = Rc::new(RefCell::new(LauncherCatalogBuild {
            pending: gio::AppInfo::all().into_iter().collect(),
            data: LauncherCatalogData::default(),
        }));
        let weak = Rc::downgrade(self);
        glib::idle_add_local(move || {
            let Some(catalog) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };

            let finished = {
                let mut build = build.borrow_mut();
                for _ in 0..APPS_PER_IDLE {
                    let Some(info) = build.pending.pop_front() else {
                        break;
                    };
                    let LauncherCatalogData { apps, valid_keys } = &mut build.data;
                    if let Some(app) = catalog_app(info, valid_keys) {
                        apps.push(Rc::new(app));
                    }
                }
                build.pending.is_empty()
            };

            if !finished {
                return glib::ControlFlow::Continue;
            }

            let mut data = std::mem::take(&mut build.borrow_mut().data);
            data.apps.sort_by_cached_key(|app| app.name.to_lowercase());
            catalog.apply_refresh(data);
            glib::ControlFlow::Break
        });
    }

    fn apply_refresh(self: &Rc<Self>, data: LauncherCatalogData) {
        self.data.replace(data);
        self.initialized.set(true);
        self.refresh_scheduled.set(false);

        for controller in self.live_controllers() {
            controller.catalog_changed();
        }

        if self.refresh_again.replace(false) {
            self.schedule_refresh();
        }
    }

    fn notify_user_state_changed(&self, state: &LauncherUserState) {
        for controller in self.live_controllers() {
            controller.user_state_changed(state);
        }
    }

    fn live_controllers(&self) -> Vec<Rc<LauncherController>> {
        let mut subscribers = self.subscribers.borrow_mut();
        let mut controllers = Vec::with_capacity(subscribers.len());
        subscribers.retain(|subscriber| {
            if let Some(controller) = subscriber.upgrade() {
                controllers.push(controller);
                true
            } else {
                false
            }
        });
        controllers
    }
}

impl Drop for LauncherCatalog {
    fn drop(&mut self) {
        if let Some(handler) = self.app_monitor_handler.get_mut().take() {
            self.app_monitor.disconnect(handler);
        }
    }
}

struct LauncherController {
    trigger: gtk::Button,
    popup: gtk::ApplicationWindow,
    popup_root: gtk::Box,
    popup_reveal: PopupReveal,
    frame: gtk::Box,
    favorites_slot: gtk::Overlay,
    favorites_spacer: gtk::Box,
    favorites_layout_generation: Generation,
    favorites_layout_expanded: Cell<bool>,
    popup_width: Cell<i32>,
    favorites_slot_width: Cell<i32>,
    search: gtk::SearchEntry,
    title: gtk::Label,
    hidden_toggle: gtk::Button,
    hidden_toggle_label: gtk::Label,
    list: gtk::Box,
    favorites_grid: gtk::Box,
    favorites_grid_keys: RefCell<Vec<String>>,
    empty_label: gtk::Label,
    scroller: gtk::ScrolledWindow,
    category_buttons: Vec<(LauncherCategory, gtk::Button)>,
    catalog: Rc<LauncherCatalog>,
    rows: RefCell<Vec<LauncherAppRow>>,
    rows_dirty: Cell<bool>,
    row_state_dirty: Cell<bool>,
    user_state: RefCell<LauncherUserState>,
    selected_category: Cell<LauncherCategory>,
    show_hidden: Cell<bool>,
    search_updates_suspended: Cell<bool>,
    focus_armed: Rc<Cell<bool>>,
    view_transition: CssTransition,
}

pub struct LauncherIndicator {
    root: gtk::Box,
    _controller: Rc<LauncherController>,
}

impl Drop for LauncherIndicator {
    fn drop(&mut self) {
        self._controller.catalog.unsubscribe(&self._controller);
        detach_application_window(&self._controller.popup);
    }
}

impl LauncherIndicator {
    pub fn new(
        application: &gtk::Application,
        bar_window: &gtk::ApplicationWindow,
        monitor: &gdk::Monitor,
        catalog: &Rc<LauncherCatalog>,
    ) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        root.add_css_class("launcher-panel");
        root.set_valign(gtk::Align::Center);

        let trigger_icon = gtk::Label::new(Some(ICON_LAUNCHER));
        trigger_icon.add_css_class("launcher-trigger-icon");
        trigger_icon.add_css_class("launcher-material-icon");
        trigger_icon.add_css_class("module-icon");
        trigger_icon.add_css_class("control-trigger-icon");
        trigger_icon.set_halign(gtk::Align::Center);
        trigger_icon.set_valign(gtk::Align::Center);
        trigger_icon.set_xalign(0.5);
        trigger_icon.set_yalign(0.5);

        let trigger = gtk::Button::new();
        trigger.add_css_class("app-launcher-trigger");
        trigger.set_valign(gtk::Align::Center);
        trigger.set_bar_tooltip_text(Some("Applications"));
        trigger.set_child(Some(&trigger_icon));
        root.append(&trigger);

        let popup = build_bar_popup(
            application,
            monitor,
            LAUNCHER_POPUP_NAMESPACE,
            "launcher-popup-window",
        );

        let popup_root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        popup_root.set_focusable(true);

        let user_state = LauncherUserState::read();
        let initial_category = user_state.default_category();
        let has_favorites = !user_state.favorites.is_empty();
        let initial_popup_width = if has_favorites {
            BAR_POPUP_WIDTH
        } else {
            COMPACT_POPUP_WIDTH
        };
        let initial_favorites_slot_width = if has_favorites {
            FAVORITES_SLOT_WIDTH
        } else {
            0
        };

        let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
        frame.add_css_class("launcher-popover-window");
        frame.set_overflow(gtk::Overflow::Hidden);
        frame.set_size_request(initial_popup_width, -1);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        content.add_css_class("launcher-popover");

        let search_container = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        search_container.add_css_class("launcher-search-container");
        search_container.set_valign(gtk::Align::Center);

        let search = gtk::SearchEntry::new();
        search.add_css_class("launcher-search");
        search.set_hexpand(true);
        search.set_placeholder_text(Some("Search applications"));
        search_container.append(&search);
        content.append(&search_container);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        header.add_css_class("launcher-header");
        header.set_valign(gtk::Align::Center);

        let title = gtk::Label::new(Some("Applications 0"));
        title.add_css_class("launcher-title");
        title.set_xalign(0.0);
        title.set_hexpand(true);

        let hidden_toggle_label = gtk::Label::new(None);
        hidden_toggle_label.add_css_class("launcher-hidden-toggle-label");
        hidden_toggle_label.add_css_class("launcher-material-icon");
        hidden_toggle_label.set_xalign(0.5);
        hidden_toggle_label.set_yalign(0.5);
        hidden_toggle_label.set_halign(gtk::Align::Center);
        hidden_toggle_label.set_valign(gtk::Align::Center);
        hidden_toggle_label.set_size_request(20, 20);

        let hidden_toggle_icon_slot = fixed_icon_slot(&hidden_toggle_label, 20, 20);

        let hidden_toggle = gtk::Button::new();
        hidden_toggle.add_css_class("launcher-hidden-toggle");
        hidden_toggle.set_halign(gtk::Align::Center);
        hidden_toggle.set_valign(gtk::Align::Center);
        hidden_toggle.set_child(Some(&hidden_toggle_icon_slot));
        hidden_toggle.set_visible(false);

        header.append(&title);
        header.append(&hidden_toggle);

        let view = gtk::Box::new(gtk::Orientation::Vertical, 8);
        view.add_css_class("launcher-view");
        view.set_overflow(gtk::Overflow::Hidden);
        view.append(&header);

        let scroller = gtk::ScrolledWindow::new();
        scroller.add_css_class("launcher-list-wrap");
        scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroller.set_kinetic_scrolling(true);
        scroller.set_propagate_natural_height(false);
        scroller.set_propagate_natural_width(false);
        scroller.set_height_request(LIST_HEIGHT);
        scroller.set_min_content_height(LIST_HEIGHT);
        scroller.set_max_content_height(LIST_HEIGHT);

        let list = gtk::Box::new(gtk::Orientation::Vertical, 4);
        list.add_css_class("launcher-list-content");

        let empty_label = gtk::Label::new(None);
        empty_label.add_css_class("launcher-empty-title");
        empty_label.set_halign(gtk::Align::Center);
        empty_label.set_margin_top(118);
        empty_label.set_visible(false);
        list.append(&empty_label);

        let favorites_grid = gtk::Box::new(gtk::Orientation::Vertical, 4);
        favorites_grid.add_css_class("launcher-favorites-grid");
        favorites_grid.set_hexpand(false);
        favorites_grid.set_halign(gtk::Align::Start);
        favorites_grid.set_valign(gtk::Align::Start);
        favorites_grid.set_visible(false);

        let content_body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content_body.add_css_class("launcher-content-body");
        content_body.append(&list);
        content_body.append(&favorites_grid);

        scroller.set_child(Some(&content_body));
        install_smooth_scroll(&scroller, SMOOTH_SCROLL);
        view.append(&scroller);
        content.append(&view);

        let category_bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        category_bar.add_css_class("launcher-category-bar");
        category_bar.set_homogeneous(false);
        category_bar.set_hexpand(true);
        category_bar.set_valign(gtk::Align::Center);

        let category_text_bar = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        category_text_bar.set_homogeneous(true);
        category_text_bar.set_hexpand(true);
        category_text_bar.set_valign(gtk::Align::Center);

        // Keep Favorites in an overlay whose measured child is a controllable
        // spacer. The star itself remains unmeasured, so its glyph/focus styling
        // cannot affect geometry. When favorites appear, the spacer and popup grow
        // together; with no favorites both collapse back to the compact width.
        let favorites_slot = gtk::Overlay::new();
        favorites_slot.add_css_class("launcher-category-favorite-slot");
        favorites_slot.set_overflow(gtk::Overflow::Hidden);

        let favorites_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        favorites_spacer.set_size_request(initial_favorites_slot_width, 28);
        favorites_slot.set_child(Some(&favorites_spacer));
        favorites_slot.set_visible(has_favorites);
        category_bar.append(&favorites_slot);

        let category_buttons = LauncherCategory::BUTTONS
            .into_iter()
            .map(|category| {
                let label_text = if category == LauncherCategory::Favorites {
                    ICON_FAVORITE
                } else {
                    category.label()
                };
                let label = gtk::Label::new(Some(label_text));
                label.add_css_class("launcher-category-label");
                if category == LauncherCategory::Favorites {
                    label.add_css_class("launcher-material-icon");
                    label.add_css_class("launcher-category-favorite-icon");
                }

                let button = gtk::Button::new();
                button.add_css_class("launcher-category-button");
                button.set_bar_tooltip_text(Some(category.label()));
                button.set_child(Some(&label));
                if category == LauncherCategory::Favorites {
                    button.add_css_class("launcher-category-favorite-button");
                    button.set_hexpand(true);
                    button.set_halign(gtk::Align::Fill);
                    button.set_valign(gtk::Align::Fill);
                    button.set_sensitive(false);
                    button.set_focusable(false);
                    button.set_visible(false);
                    favorites_slot.add_overlay(&button);
                    favorites_slot.set_measure_overlay(&button, false);
                } else {
                    button.set_hexpand(true);
                    category_text_bar.append(&button);
                }
                (category, button)
            })
            .collect::<Vec<_>>();
        category_bar.append(&category_text_bar);
        content.append(&category_bar);

        frame.append(&content);
        let popup_reveal = PopupReveal::masked(frame.clone().upcast::<gtk::Widget>());
        popup_root.append(popup_reveal.widget());
        popup.set_child(Some(&popup_root));

        let view_transition = CssTransition::new(
            view.clone().upcast::<gtk::Widget>(),
            LauncherViewTransition::CLASSES,
            VIEW_TRANSITION_DURATION,
        );

        let controller = Rc::new(LauncherController {
            trigger,
            popup,
            popup_root,
            popup_reveal,
            frame,
            favorites_slot,
            favorites_spacer,
            favorites_layout_generation: Generation::default(),
            favorites_layout_expanded: Cell::new(has_favorites),
            popup_width: Cell::new(initial_popup_width),
            favorites_slot_width: Cell::new(initial_favorites_slot_width),
            search,
            title,
            hidden_toggle,
            hidden_toggle_label,
            list,
            favorites_grid,
            favorites_grid_keys: RefCell::new(Vec::new()),
            empty_label,
            scroller,
            category_buttons,
            catalog: Rc::clone(catalog),
            rows: RefCell::new(Vec::new()),
            rows_dirty: Cell::new(true),
            row_state_dirty: Cell::new(true),
            user_state: RefCell::new(user_state),
            selected_category: Cell::new(initial_category),
            show_hidden: Cell::new(false),
            search_updates_suspended: Cell::new(false),
            focus_armed: Rc::new(Cell::new(false)),
            view_transition,
        });

        catalog.subscribe(&controller);
        LauncherController::connect(&controller, bar_window);
        controller.prune_state_keys();
        controller.ensure_valid_category();
        controller.sync_category_buttons();

        Self {
            root,
            _controller: controller,
        }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    pub fn show_launcher(&self) {
        self._controller.open_popup();
    }

    pub fn dismiss(&self) {
        self._controller.close_popup();
    }
}

impl LauncherController {
    fn connect(this: &Rc<Self>, bar_window: &gtk::ApplicationWindow) {
        let weak = Rc::downgrade(this);
        this.trigger.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.toggle_popup();
            }
        });

        let weak = Rc::downgrade(this);
        this.search.connect_search_changed(move |_| {
            if let Some(this) = weak.upgrade() {
                if this.search_updates_suspended.get() {
                    return;
                }
                this.refresh_list();
                this.scroll_to_top();
            }
        });

        let weak = Rc::downgrade(this);
        this.search.connect_activate(move |_| {
            if let Some(this) = weak.upgrade() {
                this.launch_first_match();
            }
        });

        let weak = Rc::downgrade(this);
        this.hidden_toggle.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                let entering_hidden = !this.show_hidden.get();
                this.show_hidden.set(entering_hidden);
                this.refresh_list();
                this.scroll_to_top();
                this.replay_view_transition(if entering_hidden {
                    LauncherViewTransition::HiddenForward
                } else {
                    LauncherViewTransition::HiddenBack
                });
            }
        });

        for (category, button) in this.category_buttons.iter().cloned() {
            let weak = Rc::downgrade(this);
            button.connect_clicked(move |_| {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                let previous = this.selected_category.get();
                let searching = this.has_search_query();
                let leaving_hidden = this.show_hidden.get();
                if previous == category && !searching && !leaving_hidden {
                    return;
                }

                if leaving_hidden {
                    this.show_hidden.set(false);
                }
                this.selected_category.set(category);
                this.sync_category_buttons();
                if searching {
                    this.search.set_text("");
                } else {
                    this.refresh_list();
                    this.scroll_to_top();
                }

                if leaving_hidden {
                    this.replay_view_transition(LauncherViewTransition::HiddenBack);
                } else if previous != category {
                    this.replay_view_transition(if category.index() > previous.index() {
                        LauncherViewTransition::CategoryForward
                    } else {
                        LauncherViewTransition::CategoryBack
                    });
                }
            });
        }

        attach_popup_escape_handler(&this.popup, Rc::downgrade(this), |this| {
            if !this.popup.is_visible() {
                return false;
            }
            this.close_popup();
            true
        });

        attach_popup_lifecycle(
            bar_window,
            &this.trigger,
            &this.popup,
            &this.focus_armed,
            Rc::downgrade(this),
            |this| this.close_popup(),
            |this| {
                reset_hidden_popup_state(
                    &this.popup_reveal,
                    &this.focus_armed,
                    &this.trigger,
                    "launcher-popup-open",
                );
                this.clear_view_transition();
            },
        );
    }

    fn toggle_popup(self: &Rc<Self>) {
        if self.popup_reveal.is_revealed() {
            self.close_popup();
        } else {
            self.open_popup();
        }
    }

    fn open_popup(self: &Rc<Self>) {
        self.focus_armed.set(false);
        self.catalog.refresh_if_stale();
        self.reload_user_state();
        self.prune_state_keys();
        self.reset_view();
        let generation = self.popup_reveal.show(&self.popup);
        self.trigger.add_css_class("launcher-popup-open");

        run_when_popup_visible(
            &self.popup,
            &self.popup_reveal,
            generation,
            Rc::downgrade(self),
            |this| {
                this.popup_root.grab_focus();
                this.search.grab_focus();
            },
        );
    }

    fn close_popup(self: &Rc<Self>) {
        if !self.popup.is_visible() {
            return;
        }
        self.focus_armed.set(false);
        self.trigger.remove_css_class("launcher-popup-open");
        self.popup_reveal.hide(&self.popup);
    }

    fn reset_view(self: &Rc<Self>) {
        self.clear_view_transition();
        self.show_hidden.set(false);
        self.selected_category.set(self.default_category());

        self.search_updates_suspended.set(true);
        self.search.set_text("");
        self.search_updates_suspended.set(false);

        self.sync_category_buttons();
        self.refresh_list();
        self.scroll_to_top();
    }

    fn has_search_query(&self) -> bool {
        self.search
            .text()
            .as_str()
            .split_whitespace()
            .next()
            .is_some()
    }

    fn reload_user_state(&self) {
        self.user_state.replace(LauncherUserState::read());
        self.row_state_dirty.set(true);
    }

    fn catalog_changed(self: &Rc<Self>) {
        self.rows_dirty.set(true);
        self.favorites_grid_keys.borrow_mut().clear();
        self.reload_user_state();
        self.prune_state_keys();
        self.ensure_valid_category();
        if self.popup.is_visible() {
            self.sync_category_buttons();
            self.refresh_list();
        }
    }

    fn user_state_changed(self: &Rc<Self>, state: &LauncherUserState) {
        self.user_state.replace(state.clone());
        self.row_state_dirty.set(true);
        self.ensure_valid_category();
        if self.popup.is_visible() {
            self.sync_category_buttons();
            self.refresh_list();
        }
    }

    fn prune_state_keys(&self) {
        // Controllers are created before the asynchronous catalog build finishes.
        // Pruning against that temporary empty catalog would overwrite persisted
        // favorites/hidden entries with an empty array during startup.
        if !self.catalog.initialized.get() {
            return;
        }

        let catalog = self.catalog.data.borrow();
        // An empty result can also be transient while desktop entries are
        // being refreshed. Never destroy persisted state on that basis.
        if catalog.valid_keys.is_empty() {
            return;
        }

        let mut state = self.user_state.borrow_mut();

        if retain_known_keys(&mut state.hidden, &catalog.valid_keys) {
            save_state_keys(HIDDEN_STATE_FILE, &state.hidden);
        }
        if retain_known_keys(&mut state.favorites, &catalog.valid_keys) {
            save_state_keys(FAVORITES_STATE_FILE, &state.favorites);
        }
    }

    fn toggle_state_key(&self, kind: LauncherStateKind, key: &str) {
        // Read both files once so a toggle preserves external changes to either
        // state set, then broadcast the same snapshot to every controller.
        let mut state = LauncherUserState::read();
        let keys = kind.keys_mut(&mut state);
        if !keys.remove(key) {
            keys.insert(key.to_owned());
        }
        if save_state_keys(kind.file_name(), keys) {
            self.catalog.notify_user_state_changed(&state);
        }
    }

    fn default_category(&self) -> LauncherCategory {
        self.user_state.borrow().default_category()
    }

    fn ensure_valid_category(&self) {
        if self.selected_category.get() == LauncherCategory::Favorites
            && self.user_state.borrow().favorites.is_empty()
        {
            self.selected_category.set(LauncherCategory::All);
        }
    }

    fn sync_category_buttons(self: &Rc<Self>) {
        let selected = self.selected_category.get();
        let has_favorites = !self.user_state.borrow().favorites.is_empty();

        for (category, button) in &self.category_buttons {
            if *category == LauncherCategory::Favorites {
                button.set_visible(has_favorites);
                button.set_sensitive(has_favorites);
                button.set_focusable(has_favorites);
            }

            // Category buttons are ordinary GtkButtons, not ToggleButtons. This
            // keeps GTK theme :checked metrics out of layout and also avoids the
            // "click selected category -> visually unchecked" ToggleButton edge
            // case. Selection is paint-only and entirely controlled by this class.
            if *category == selected {
                button.add_css_class("launcher-category-selected");
            } else {
                button.remove_css_class("launcher-category-selected");
            }
        }

        self.sync_favorites_layout(has_favorites);
    }

    fn sync_favorites_layout(self: &Rc<Self>, has_favorites: bool) {
        if self.favorites_layout_expanded.get() == has_favorites {
            return;
        }
        self.favorites_layout_expanded.set(has_favorites);

        let target_popup_width = if has_favorites {
            BAR_POPUP_WIDTH
        } else {
            COMPACT_POPUP_WIDTH
        };
        let target_slot_width = if has_favorites {
            FAVORITES_SLOT_WIDTH
        } else {
            0
        };

        if !self.popup.is_visible() {
            self.favorites_layout_generation.bump();
            self.frame.set_size_request(target_popup_width, -1);
            self.favorites_spacer
                .set_size_request(target_slot_width, 28);
            self.favorites_slot.set_visible(has_favorites);
            self.popup_width.set(target_popup_width);
            self.favorites_slot_width.set(target_slot_width);
            return;
        }

        let generation = self.favorites_layout_generation.bump();
        let start_popup_width = self.popup_width.get();
        let start_slot_width = self.favorites_slot_width.get();
        let duration_ms = FAVORITES_LAYOUT_TRANSITION_DURATION.as_secs_f64() * 1_000.0;

        // Keep the slot in layout while it grows/shrinks. Its measured width and
        // the popup width change by exactly the same amount, so the six ordinary
        // category buttons do not wobble during the transition.
        self.favorites_slot.set_visible(true);

        let weak = Rc::downgrade(self);
        let start_time_us = Cell::new(None::<i64>);
        self.frame.add_tick_callback(move |_, frame_clock| {
            let Some(this) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if !this.favorites_layout_generation.is_current(generation) {
                return glib::ControlFlow::Break;
            }

            let now_us = frame_clock.frame_time();
            let start_us = match start_time_us.get() {
                Some(value) => value,
                None => {
                    start_time_us.set(Some(now_us));
                    now_us
                }
            };

            let elapsed_ms = (now_us - start_us).max(0) as f64 / 1_000.0;
            let t = (elapsed_ms / duration_ms).clamp(0.0, 1.0);
            let eased = 1.0 - (1.0 - t).powi(3);
            let popup_width = (start_popup_width as f64
                + (target_popup_width - start_popup_width) as f64 * eased)
                .round() as i32;
            let slot_width = (start_slot_width as f64
                + (target_slot_width - start_slot_width) as f64 * eased)
                .round() as i32;

            this.frame.set_size_request(popup_width, -1);
            this.favorites_spacer.set_size_request(slot_width, 28);
            this.popup_width.set(popup_width);
            this.favorites_slot_width.set(slot_width);

            if t < 1.0 {
                glib::ControlFlow::Continue
            } else {
                this.frame.set_size_request(target_popup_width, -1);
                this.favorites_spacer
                    .set_size_request(target_slot_width, 28);
                this.popup_width.set(target_popup_width);
                this.favorites_slot_width.set(target_slot_width);
                if !has_favorites {
                    this.favorites_slot.set_visible(false);
                }
                glib::ControlFlow::Break
            }
        });
    }

    fn app_matches(&self, app: &LaunchableApp, query: &str, state: &LauncherUserState) -> bool {
        let hidden = state.hidden.contains(&app.key);
        let searching = !query.is_empty();

        if self.show_hidden.get() {
            return hidden && (!searching || app.search_blob.contains(query));
        }

        if searching {
            // Global search intentionally includes hidden applications. Hidden
            // state only filters normal category views.
            return app.search_blob.contains(query);
        }

        let category = self.selected_category.get();
        !hidden
            && (category != LauncherCategory::Favorites || state.favorites.contains(&app.key))
            && app.matches_category(category)
    }

    fn refresh_list(self: &Rc<Self>) {
        self.ensure_rows();

        let query = normalize_text(self.search.text().as_str());
        let searching = !query.is_empty();
        let favorites_view = !self.show_hidden.get()
            && !searching
            && self.selected_category.get() == LauncherCategory::Favorites;
        let state = self.user_state.borrow();
        let rows = self.rows.borrow();
        let sync_row_state = self.row_state_dirty.replace(false);
        let mut visible_count = 0;
        let mut favorite_apps = Vec::new();

        for row in rows.iter() {
            if sync_row_state {
                row.sync_state(&state);
            }
            let visible = self.app_matches(&row.app, &query, &state);
            row.set_visible(visible);
            if visible {
                visible_count += 1;
                if favorites_view {
                    favorite_apps.push(Rc::clone(&row.app));
                }
            }
        }

        let hidden_count = state.hidden.len();
        drop(rows);
        drop(state);

        if favorites_view {
            self.sync_favorites_grid(favorite_apps);
        }

        let show_empty = visible_count == 0;
        self.list.set_visible(!favorites_view || show_empty);
        self.favorites_grid
            .set_visible(favorites_view && !show_empty);

        self.empty_label.set_label(if self.show_hidden.get() {
            "No hidden applications"
        } else {
            "Nothing found"
        });
        self.empty_label.set_visible(show_empty);
        self.sync_header(visible_count, searching, hidden_count);
    }

    fn sync_favorites_grid(self: &Rc<Self>, apps: Vec<Rc<LaunchableApp>>) {
        let unchanged = {
            let current_keys = self.favorites_grid_keys.borrow();
            current_keys.len() == apps.len()
                && current_keys
                    .iter()
                    .zip(&apps)
                    .all(|(key, app)| key == &app.key)
        };
        if unchanged {
            return;
        }

        clear_box(&self.favorites_grid);
        self.favorites_grid_keys
            .replace(apps.iter().map(|app| app.key.clone()).collect::<Vec<_>>());

        for apps_row in apps.chunks(FAVORITES_COLUMNS as usize) {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 14);
            row.set_hexpand(false);
            row.set_halign(gtk::Align::Start);

            for app in apps_row.iter().cloned() {
                let button = self.build_favorite_tile(app);
                button.set_hexpand(false);
                row.append(&button);
            }

            self.favorites_grid.append(&row);
        }
    }

    fn ensure_rows(self: &Rc<Self>) {
        if self.rows_dirty.get() {
            self.rebuild_rows();
        }
    }

    fn rebuild_rows(self: &Rc<Self>) {
        clear_box(&self.list);

        let apps = self.catalog.data.borrow().apps.clone();
        let rows = apps
            .into_iter()
            .map(|app| {
                let row = self.build_app_row(app);
                self.list.append(&row.root);
                row
            })
            .collect();

        self.list.append(&self.empty_label);
        self.rows.replace(rows);
        self.rows_dirty.set(false);
        self.row_state_dirty.set(true);
    }

    fn sync_header(&self, count: usize, searching: bool, hidden_count: usize) {
        let title = if self.show_hidden.get() {
            format!("Hidden applications {count}")
        } else if searching {
            format!("Search results {count}")
        } else {
            let category = self.selected_category.get();
            let name = match category {
                LauncherCategory::Favorites => "Favorites",
                LauncherCategory::All => "Applications",
                _ => category.label(),
            };
            format!("{name} {count}")
        };
        self.title.set_label(&title);

        let show_toggle = hidden_count > 0 || self.show_hidden.get();
        self.hidden_toggle.set_visible(show_toggle);
        if self.show_hidden.get() {
            self.hidden_toggle_label.set_label(ICON_BACK);
            self.hidden_toggle
                .set_bar_tooltip_text(Some("Back to applications"));
        } else {
            self.hidden_toggle_label.set_label(ICON_HIDE);
            self.hidden_toggle
                .set_bar_tooltip_text(Some("Show hidden applications"));
        }
    }

    fn build_favorite_tile(self: &Rc<Self>, app: Rc<LaunchableApp>) -> gtk::Overlay {
        let overlay = gtk::Overlay::new();
        overlay.add_css_class("launcher-favorite-tile-wrap");
        overlay.set_halign(gtk::Align::Center);
        overlay.set_valign(gtk::Align::Center);
        overlay.set_hexpand(false);
        overlay.set_size_request(54, 54);
        overlay.set_overflow(gtk::Overflow::Visible);

        let button = gtk::Button::new();
        button.add_css_class("launcher-favorite-tile");
        button.set_halign(gtk::Align::Center);
        button.set_valign(gtk::Align::Center);
        button.set_size_request(54, 54);
        button.set_bar_tooltip_text(Some(&app.name));

        if let Some(icon) = app.info.icon() {
            let image = gtk::Image::from_gicon(&icon);
            image.add_css_class("launcher-favorite-tile-icon");
            image.set_pixel_size(36);
            image.set_halign(gtk::Align::Center);
            image.set_valign(gtk::Align::Center);
            button.set_child(Some(&image));
        } else {
            let fallback = gtk::Label::new(Some(ICON_FALLBACK));
            fallback.add_css_class("launcher-favorite-tile-fallback");
            fallback.add_css_class("launcher-material-icon");
            fallback.set_halign(gtk::Align::Center);
            fallback.set_valign(gtk::Align::Center);
            button.set_child(Some(&fallback));
        }

        let app_for_launch = Rc::clone(&app);
        let weak = Rc::downgrade(self);
        button.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.launch_app(&app_for_launch);
            }
        });

        overlay.set_child(Some(&button));

        let favorite_toggle = gtk::Button::new();
        favorite_toggle.add_css_class("launcher-favorite-tile-toggle");
        favorite_toggle.set_halign(gtk::Align::End);
        favorite_toggle.set_valign(gtk::Align::End);
        favorite_toggle.set_margin_end(3);
        favorite_toggle.set_margin_bottom(3);
        favorite_toggle.set_bar_tooltip_text(Some("Remove from favorites"));

        let favorite_toggle_icon = gtk::Label::new(Some(ICON_FAVORITE));
        favorite_toggle_icon.add_css_class("launcher-material-icon");
        favorite_toggle_icon.add_css_class("launcher-favorite-tile-toggle-icon");
        favorite_toggle_icon.set_halign(gtk::Align::Center);
        favorite_toggle_icon.set_valign(gtk::Align::Center);
        favorite_toggle.set_child(Some(&favorite_toggle_icon));

        let app_for_toggle = Rc::clone(&app);
        let weak = Rc::downgrade(self);
        let overlay_for_toggle = overlay.clone();
        favorite_toggle.connect_clicked(move |toggle| {
            toggle.set_sensitive(false);
            overlay_for_toggle.add_css_class("launcher-favorite-tile-removing");

            let weak = weak.clone();
            let app_for_toggle = Rc::clone(&app_for_toggle);
            glib::timeout_add_local_once(FAVORITE_REMOVE_TRANSITION_DURATION, move || {
                if let Some(this) = weak.upgrade() {
                    this.toggle_state_key(LauncherStateKind::Favorite, &app_for_toggle.key);
                }
            });
        });

        overlay.add_overlay(&favorite_toggle);
        overlay.set_measure_overlay(&favorite_toggle, false);

        overlay
    }

    fn build_app_row(self: &Rc<Self>, app: Rc<LaunchableApp>) -> LauncherAppRow {
        let card = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        card.add_css_class("launcher-app-card");
        card.set_hexpand(true);
        card.set_valign(gtk::Align::Center);

        let main = gtk::Button::new();
        main.add_css_class("launcher-app-main");
        main.set_hexpand(true);

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        row.add_css_class("launcher-app-row");
        row.set_hexpand(true);
        row.set_valign(gtk::Align::Center);

        let icon_wrap = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        icon_wrap.add_css_class("launcher-app-icon-wrap");
        icon_wrap.set_halign(gtk::Align::Center);
        icon_wrap.set_valign(gtk::Align::Center);

        if let Some(icon) = app.info.icon() {
            let image = gtk::Image::from_gicon(&icon);
            image.set_pixel_size(40);
            image.set_halign(gtk::Align::Center);
            image.set_valign(gtk::Align::Center);
            icon_wrap.append(&image);
        } else {
            let fallback = gtk::Label::new(Some(ICON_FALLBACK));
            fallback.add_css_class("launcher-app-fallback");
            fallback.add_css_class("launcher-material-icon");
            fallback.set_halign(gtk::Align::Center);
            fallback.set_valign(gtk::Align::Center);
            icon_wrap.append(&fallback);
        }

        let app_content = gtk::Box::new(gtk::Orientation::Vertical, 2);
        app_content.add_css_class("launcher-app-content");
        app_content.set_hexpand(true);
        app_content.set_valign(gtk::Align::Center);

        let app_title = gtk::Label::new(Some(&app.name));
        app_title.add_css_class("launcher-app-title");
        app_title.set_xalign(0.0);
        app_title.set_ellipsize(pango::EllipsizeMode::End);
        app_title.set_max_width_chars(28);

        let app_meta = gtk::Label::new(Some(&app.meta));
        app_meta.add_css_class("launcher-app-meta");
        app_meta.set_xalign(0.0);
        app_meta.set_ellipsize(pango::EllipsizeMode::End);
        app_meta.set_max_width_chars(42);

        app_content.append(&app_title);
        app_content.append(&app_meta);
        row.append(&icon_wrap);
        row.append(&app_content);
        main.set_child(Some(&row));

        let app_for_launch = Rc::clone(&app);
        let weak = Rc::downgrade(self);
        main.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.launch_app(&app_for_launch);
            }
        });

        let hidden_icon = gtk::Label::new(Some(ICON_HIDE));
        hidden_icon.add_css_class("launcher-side-icon");
        hidden_icon.add_css_class("launcher-material-icon");
        hidden_icon.set_xalign(0.5);
        hidden_icon.set_yalign(0.5);
        hidden_icon.set_halign(gtk::Align::Center);
        hidden_icon.set_valign(gtk::Align::Center);
        hidden_icon.set_size_request(20, 20);

        let hidden_icon_slot = fixed_icon_slot(&hidden_icon, 20, 20);

        let hidden_button = gtk::Button::new();
        hidden_button.add_css_class("launcher-app-side-button");
        hidden_button.set_bar_tooltip_text(Some("Hide or restore application"));
        hidden_button.set_valign(gtk::Align::Center);
        hidden_button.set_child(Some(&hidden_icon_slot));

        let app_for_hidden = Rc::clone(&app);
        let weak = Rc::downgrade(self);
        hidden_button.connect_clicked(move |_| {
            let Some(this) = weak.upgrade() else {
                return;
            };
            this.toggle_state_key(LauncherStateKind::Hidden, &app_for_hidden.key);
        });

        let favorite_icon = gtk::Label::new(Some(ICON_FAVORITE_OUTLINE));
        favorite_icon.add_css_class("launcher-side-icon");
        favorite_icon.add_css_class("launcher-material-icon");
        favorite_icon.add_css_class("launcher-favorite-icon");
        favorite_icon.set_xalign(0.5);
        favorite_icon.set_yalign(0.5);
        favorite_icon.set_halign(gtk::Align::Center);
        favorite_icon.set_valign(gtk::Align::Center);
        favorite_icon.set_size_request(20, 20);

        let favorite_icon_slot = fixed_icon_slot(&favorite_icon, 20, 20);

        let favorite_button = gtk::Button::new();
        favorite_button.add_css_class("launcher-app-side-button");
        favorite_button.add_css_class("launcher-favorite-button");
        favorite_button.set_bar_tooltip_text(Some("Add or remove favorite"));
        favorite_button.set_valign(gtk::Align::Center);
        favorite_button.set_child(Some(&favorite_icon_slot));

        let app_for_favorite = Rc::clone(&app);
        let weak = Rc::downgrade(self);
        favorite_button.connect_clicked(move |_| {
            let Some(this) = weak.upgrade() else {
                return;
            };
            this.toggle_state_key(LauncherStateKind::Favorite, &app_for_favorite.key);
        });

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        actions.add_css_class("launcher-app-actions");
        actions.set_valign(gtk::Align::Center);
        actions.append(&favorite_button);
        actions.append(&hidden_button);

        card.append(&main);
        card.append(&actions);

        LauncherAppRow {
            app,
            root: card,
            hidden_icon,
            favorite_icon,
            hidden_state: Cell::new(None),
            favorite_state: Cell::new(None),
            visible_state: Cell::new(None),
        }
    }

    fn launch_first_match(self: &Rc<Self>) {
        self.ensure_rows();
        let app = self
            .rows
            .borrow()
            .iter()
            .find(|row| row.visible_state.get() == Some(true))
            .map(|row| Rc::clone(&row.app));

        if let Some(app) = app {
            self.launch_app(&app);
        }
    }

    fn launch_app(self: &Rc<Self>, app: &LaunchableApp) {
        self.close_popup();
        if let Err(error) = app.info.launch(&[], None::<&gio::AppLaunchContext>) {
            warn!(app = %app.name, %error, "failed to launch application");
        }
    }

    fn clear_view_transition(&self) {
        self.view_transition.clear();
    }

    fn replay_view_transition(&self, transition: LauncherViewTransition) {
        self.view_transition.replay(transition.css_class());
    }

    fn scroll_to_top(&self) {
        let adjustment = self.scroller.vadjustment();
        adjustment.set_value(adjustment.lower());
    }
}

fn fixed_icon_slot(label: &gtk::Label, width: i32, height: i32) -> gtk::Overlay {
    let overlay = gtk::Overlay::new();
    // The spacer is the only measured child. Dynamic Material Design glyphs can
    // therefore change without changing the surrounding button's natural size.
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_size_request(width, height);
    overlay.set_child(Some(&spacer));
    overlay.add_overlay(label);
    overlay.set_measure_overlay(label, false);
    overlay
}

fn launchable_app_identity(info: &gio::AppInfo) -> Option<(String, String, String, String)> {
    if !info.should_show() {
        return None;
    }

    let name = info.display_name().trim().to_owned();
    if name.is_empty() {
        return None;
    }

    let id = info.id().map(|value| value.to_string()).unwrap_or_default();
    let executable = info.executable().to_string_lossy().trim().to_owned();
    let key = if id.is_empty() {
        format!("fallback:{executable}::{name}")
    } else {
        format!("id:{id}")
    };

    Some((key, name, id, executable))
}

fn desktop_application_dirs() -> Vec<PathBuf> {
    let mut data_dirs = Vec::new();

    if let Some(data_home) = env::var_os("XDG_DATA_HOME") {
        data_dirs.push(PathBuf::from(data_home));
    } else if let Some(home) = env::var_os("HOME") {
        data_dirs.push(PathBuf::from(home).join(".local/share"));
    }

    if let Some(raw_dirs) = env::var_os("XDG_DATA_DIRS") {
        data_dirs.extend(env::split_paths(&raw_dirs));
    } else {
        data_dirs.extend([
            PathBuf::from("/usr/local/share"),
            PathBuf::from("/usr/share"),
        ]);
    }

    if let Some(home) = env::var_os("HOME") {
        let home = PathBuf::from(home);
        data_dirs.push(home.join(".nix-profile/share"));
        data_dirs.push(home.join(".local/share/flatpak/exports/share"));
    }
    if let Some(user) = env::var_os("USER") {
        data_dirs.push(
            PathBuf::from("/etc/profiles/per-user")
                .join(user)
                .join("share"),
        );
    }
    data_dirs.push(PathBuf::from("/run/current-system/sw/share"));
    data_dirs.push(PathBuf::from("/var/lib/flatpak/exports/share"));

    let mut seen = HashSet::new();
    data_dirs
        .into_iter()
        .map(|directory| normalize_path(&directory.join("applications")))
        .filter(|directory| seen.insert(directory.clone()))
        .collect()
}

fn desktop_watches() -> Vec<DesktopWatch> {
    let mut watches = HashSet::new();

    for applications in desktop_application_dirs() {
        if applications.is_dir() {
            watches.insert(DesktopWatch {
                directory: applications.clone(),
                exact_path: None,
            });
        }

        if let Some(watch) = nearest_missing_component_watch(&applications) {
            watches.insert(watch);
        } else if let Some(parent) = applications.parent().filter(|parent| parent.is_dir()) {
            watches.insert(DesktopWatch {
                directory: parent.to_owned(),
                exact_path: Some(applications.clone()),
            });
        }

        let (resolved, symlinks) = resolve_path_and_symlinks(&applications, 0);
        for symlink in symlinks {
            if let Some(parent) = symlink.parent().filter(|parent| parent.is_dir()) {
                watches.insert(DesktopWatch {
                    directory: parent.to_owned(),
                    exact_path: Some(symlink),
                });
            }
        }

        if resolved != applications {
            if resolved.is_dir() {
                watches.insert(DesktopWatch {
                    directory: resolved.clone(),
                    exact_path: None,
                });
            }
            if let Some(watch) = nearest_missing_component_watch(&resolved) {
                watches.insert(watch);
            } else if let Some(parent) = resolved.parent().filter(|parent| parent.is_dir()) {
                watches.insert(DesktopWatch {
                    directory: parent.to_owned(),
                    exact_path: Some(resolved),
                });
            }
        }
    }

    watches.into_iter().collect()
}

fn nearest_missing_component_watch(path: &Path) -> Option<DesktopWatch> {
    let mut missing = normalize_path(path);
    while !missing.exists() {
        let parent = missing.parent()?;
        if parent.is_dir() {
            return Some(DesktopWatch {
                directory: parent.to_owned(),
                exact_path: Some(missing),
            });
        }
        missing = parent.to_owned();
    }
    None
}

fn resolve_path_and_symlinks(path: &Path, depth: u8) -> (PathBuf, Vec<PathBuf>) {
    if depth >= 32 {
        return (normalize_path(path), Vec::new());
    }

    let mut resolved = PathBuf::new();
    let mut components = path.components();

    while let Some(component) = components.next() {
        resolved.push(component.as_os_str());
        let resolved_normalized = normalize_path(&resolved);
        let is_symlink = fs::symlink_metadata(&resolved_normalized)
            .is_ok_and(|metadata| metadata.file_type().is_symlink());
        if !is_symlink {
            resolved = resolved_normalized;
            continue;
        }

        let Ok(target) = fs::read_link(&resolved_normalized) else {
            return (resolved_normalized.clone(), vec![resolved_normalized]);
        };
        let mut next = if target.is_absolute() {
            target
        } else {
            resolved_normalized
                .parent()
                .unwrap_or_else(|| Path::new("/"))
                .join(target)
        };
        for remaining in components {
            next.push(remaining.as_os_str());
        }

        let (final_path, mut nested_symlinks) =
            resolve_path_and_symlinks(&normalize_path(&next), depth + 1);
        nested_symlinks.insert(0, resolved_normalized);
        return (final_path, nested_symlinks);
    }

    (normalize_path(&resolved), Vec::new())
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

fn desktop_watch_matches(
    exact_path: Option<&Path>,
    file: &gio::File,
    other_file: Option<&gio::File>,
) -> bool {
    let Some(exact_path) = exact_path else {
        return true;
    };
    let exact_path = normalize_path(exact_path);
    let paths = [file.path(), other_file.and_then(|file| file.path())];

    let mut had_path = false;
    for path in paths.into_iter().flatten() {
        had_path = true;
        if normalize_path(&path) == exact_path {
            return true;
        }
    }

    !had_path
}

fn current_visible_app_keys() -> HashSet<String> {
    gio::AppInfo::all()
        .into_iter()
        .filter_map(|info| launchable_app_identity(&info).map(|(key, _, _, _)| key))
        .collect()
}

fn catalog_app(info: gio::AppInfo, valid_keys: &mut HashSet<String>) -> Option<LaunchableApp> {
    let (key, name, id, executable) = launchable_app_identity(&info)?;
    if !valid_keys.insert(key.clone()) {
        return None;
    }

    let description = info
        .description()
        .map(|value| value.trim().to_owned())
        .unwrap_or_default();
    let (categories, keywords) = desktop_metadata(&id);
    let primary_category = classify_category(&categories);
    let search_blob = normalize_fields(
        [
            name.as_str(),
            description.as_str(),
            executable.as_str(),
            keywords.as_str(),
            id.as_str(),
        ]
        .into_iter()
        .chain(categories.iter().map(String::as_str)),
    );
    let meta = if !description.is_empty() {
        description
    } else if !executable.is_empty() {
        executable
    } else if !id.is_empty() {
        id
    } else {
        "Desktop application".to_owned()
    };

    Some(LaunchableApp {
        key,
        name,
        meta,
        primary_category,
        search_blob,
        info,
    })
}

fn classify_category(categories: &[String]) -> Option<LauncherCategory> {
    LauncherCategory::PRIORITY.into_iter().find(|category| {
        category.matches().iter().any(|candidate| {
            categories
                .iter()
                .any(|actual| actual.eq_ignore_ascii_case(candidate))
        })
    })
}

fn desktop_metadata(id: &str) -> (Vec<String>, String) {
    if id.is_empty() {
        return (Vec::new(), String::new());
    }

    let key_file = glib::KeyFile::new();
    let desktop_file = PathBuf::from("applications").join(id);
    if key_file
        .load_from_data_dirs(&desktop_file, glib::KeyFileFlags::KEEP_TRANSLATIONS)
        .is_err()
    {
        return (Vec::new(), String::new());
    }

    let categories = key_file
        .string("Desktop Entry", "Categories")
        .ok()
        .map(|raw| desktop_values(&raw).map(ToOwned::to_owned).collect())
        .unwrap_or_default();

    let keywords = key_file
        .locale_string("Desktop Entry", "Keywords", None)
        .or_else(|_| key_file.string("Desktop Entry", "Keywords"))
        .map(|raw| desktop_values(&raw).collect::<Vec<_>>().join(" "))
        .unwrap_or_default();

    (categories, keywords)
}

fn desktop_values(raw: &str) -> impl Iterator<Item = &str> {
    raw.split(';')
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn normalize_text(value: &str) -> String {
    normalize_fields([value])
}

fn normalize_fields<'a>(fields: impl IntoIterator<Item = &'a str>) -> String {
    let mut normalized = String::new();
    for field in fields {
        for word in field.split_whitespace() {
            if !normalized.is_empty() {
                normalized.push(' ');
            }
            normalized.extend(word.chars().flat_map(|character| character.to_lowercase()));
        }
    }
    normalized
}

fn retain_known_keys(keys: &mut HashSet<String>, valid: &HashSet<String>) -> bool {
    let previous_len = keys.len();
    keys.retain(|key| valid.contains(key.as_str()));
    keys.len() != previous_len
}

fn launcher_state_path(file_name: &str) -> PathBuf {
    glib::user_state_dir().join("obsidian-bar").join(file_name)
}

fn read_state_keys(file_name: &str) -> HashSet<String> {
    let Ok(contents) = fs::read_to_string(launcher_state_path(file_name)) else {
        return HashSet::new();
    };

    serde_json::from_str::<Vec<String>>(&contents)
        .unwrap_or_default()
        .into_iter()
        .map(|key| key.trim().to_owned())
        .filter(|key| !key.is_empty())
        .collect()
}

fn save_state_keys(file_name: &str, keys: &HashSet<String>) -> bool {
    let path = launcher_state_path(file_name);
    let Some(parent) = path.parent() else {
        return false;
    };
    if let Err(error) = fs::create_dir_all(parent) {
        warn!(%error, path = %parent.display(), "failed to create launcher state directory");
        return false;
    }

    let mut keys = keys.iter().cloned().collect::<Vec<_>>();
    keys.sort();
    let Ok(contents) = serde_json::to_string(&keys) else {
        return false;
    };

    let temporary = path.with_extension("json.tmp");
    match fs::write(&temporary, contents).and_then(|_| fs::rename(&temporary, &path)) {
        Ok(()) => true,
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            warn!(%error, path = %path.display(), "failed to save launcher state");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_search_fields_without_intermediate_separators() {
        assert_eq!(
            normalize_fields(["  Foo\tBAR ", "", " Baz  qux "]),
            "foo bar baz qux"
        );
    }

    #[test]
    fn category_priority_is_stable() {
        let categories = vec!["Network".to_owned(), "Game".to_owned()];
        assert_eq!(
            classify_category(&categories),
            Some(LauncherCategory::Games)
        );
    }

    #[test]
    fn every_filterable_category_has_a_button_and_stable_index() {
        assert_eq!(
            LauncherCategory::BUTTONS,
            [
                LauncherCategory::Favorites,
                LauncherCategory::All,
                LauncherCategory::Internet,
                LauncherCategory::Media,
                LauncherCategory::Office,
                LauncherCategory::Games,
                LauncherCategory::System,
            ]
        );

        for (index, category) in LauncherCategory::BUTTONS.into_iter().enumerate() {
            assert_eq!(category.index(), index);
        }
    }

    #[test]
    fn pruning_reports_only_real_changes() {
        let mut keys = HashSet::from(["keep".to_owned(), "drop".to_owned()]);
        let valid = HashSet::from(["keep".to_owned()]);
        assert!(retain_known_keys(&mut keys, &valid));
        assert_eq!(keys, HashSet::from(["keep".to_owned()]));
        assert!(!retain_known_keys(&mut keys, &valid));
    }
}
