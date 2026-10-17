use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
};

use gtk::{gdk, gio, glib, pango, prelude::*};
use tracing::warn;

use super::reorder::{ReorderGrid, merge_visible_order, sort_by_order};
use super::tooltip::BarTooltipExt;
use super::{
    BAR_POPUP_WIDTH, CssTransition, FrameCallback, Generation, PopupReveal, SmoothScrollConfig,
    attach_popup_escape_handler, attach_popup_lifecycle, build_bar_popup, clear_box,
    detach_application_window, install_smooth_scroll, reset_hidden_popup_state,
    run_when_popup_visible,
};

mod app_row;
use app_row::LauncherAppRow;
mod favorite_tile;
use favorite_tile::LauncherFavoriteTile;
mod catalog;
pub use catalog::LauncherCatalog;
#[cfg(test)]
use catalog::{catalog_app, classify_category};
#[cfg(test)]
use std::env;

const LIST_HEIGHT: i32 = 300;
const SMOOTH_SCROLL: SmoothScrollConfig = SmoothScrollConfig::new(72.0, 150.0, 90.0);
const LAUNCHER_POPUP_NAMESPACE: &str = "obsidian-bar-launcher";
const FAVORITES_STATE_FILE: &str = "favorite-launcher-apps.json";
const FAVORITES_ORDER_FILE: &str = "favorite-launcher-order.json";
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
    favorite_order: Vec<String>,
}

impl LauncherUserState {
    fn read() -> Self {
        let favorites = read_state_keys(FAVORITES_STATE_FILE);
        let favorite_order = read_ordered_keys(FAVORITES_ORDER_FILE)
            .into_iter()
            .filter(|key| favorites.contains(key))
            .collect();
        Self {
            hidden: read_state_keys(HIDDEN_STATE_FILE),
            favorites,
            favorite_order,
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

struct LauncherController {
    trigger: gtk::Button,
    popup: gtk::ApplicationWindow,
    popup_root: gtk::Box,
    popup_reveal: PopupReveal,
    frame: gtk::Box,
    favorites_slot: gtk::Overlay,
    favorites_spacer: gtk::Box,
    favorites_layout_animation: FrameCallback,
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
    favorite_tiles: RefCell<Vec<LauncherFavoriteTile>>,
    favorites_metadata_dirty: Cell<bool>,
    favorite_reorder: ReorderGrid,
    scroll_generation: Generation,
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
        let (favorite_reorder, drag_overlay) =
            ReorderGrid::new(&scroller, FAVORITES_COLUMNS as usize, 14, 4);
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
        view.append(&drag_overlay);
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
            view.upcast::<gtk::Widget>(),
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
            favorites_layout_animation: FrameCallback::default(),
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
            favorite_tiles: RefCell::new(Vec::new()),
            favorites_metadata_dirty: Cell::new(true),
            favorite_reorder,
            scroll_generation: Generation::default(),
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
        {
            let weak = Rc::downgrade(&controller);
            controller.favorite_reorder.on_commit(move |order| {
                weak.upgrade()
                    .is_some_and(|controller| controller.reorder_favorites(&order))
            });
        }
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
        self.favorites_metadata_dirty.set(true);
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

    fn remove_favorite(&self, key: &str) {
        let mut state = LauncherUserState::read();
        // The animation can finish after another view or process removed the
        // entry. Removing it again must not toggle it back on.
        if !state.favorites.remove(key) || save_state_keys(FAVORITES_STATE_FILE, &state.favorites) {
            self.catalog.notify_user_state_changed(&state);
        }
    }

    fn reorder_favorites(&self, visible: &[String]) -> bool {
        let current = self.favorites_grid_keys.borrow();
        if visible.len() != current.len() || current.iter().any(|key| !visible.contains(key)) {
            return false;
        }
        drop(current);
        let mut state = LauncherUserState::read();
        let mut all = self
            .catalog
            .data
            .borrow()
            .apps
            .iter()
            .filter(|app| state.favorites.contains(&app.key))
            .map(|app| app.key.clone())
            .collect::<Vec<_>>();
        sort_by_order(&mut all, &state.favorite_order, Clone::clone);
        if visible.iter().any(|key| !all.contains(key)) {
            return false;
        }
        state.favorite_order = merge_visible_order(&all, visible);
        if !save_ordered_keys(FAVORITES_ORDER_FILE, &state.favorite_order) {
            return false;
        }
        self.catalog.notify_user_state_changed(&state);
        true
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

        if !self.popup.is_visible() || !self.popup.settings().is_gtk_enable_animations() {
            self.favorites_layout_animation.cancel();
            self.frame.set_size_request(target_popup_width, -1);
            self.favorites_spacer
                .set_size_request(target_slot_width, 28);
            self.favorites_slot.set_visible(has_favorites);
            self.popup_width.set(target_popup_width);
            self.favorites_slot_width.set(target_slot_width);
            return;
        }

        let start_popup_width = self.popup_width.get();
        let start_slot_width = self.favorites_slot_width.get();
        let duration_ms = FAVORITES_LAYOUT_TRANSITION_DURATION.as_secs_f64() * 1_000.0;

        // Keep the slot in layout while it grows/shrinks. Its measured width and
        // the popup width change by exactly the same amount, so the six ordinary
        // category buttons do not wobble during the transition.
        self.favorites_slot.set_visible(true);

        let weak = Rc::downgrade(self);
        let start_time_us = Cell::new(None::<i64>);
        self.favorites_layout_animation
            .start(&self.frame, move |_, frame_clock| {
                let Some(this) = weak.upgrade() else {
                    return glib::ControlFlow::Break;
                };

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
        let query = normalize_text(self.search.text().as_str());
        let searching = !query.is_empty();
        let favorites_view = !self.show_hidden.get()
            && !searching
            && self.selected_category.get() == LauncherCategory::Favorites;
        let (visible_count, hidden_count) = if favorites_view {
            // Favorites use their own tiles. Do not build the full application
            // list until a category or search actually needs those rows.
            for row in self.rows.borrow().iter() {
                row.set_visible(false);
            }
            let state = self.user_state.borrow();
            let mut apps = self
                .catalog
                .data
                .borrow()
                .apps
                .iter()
                .filter(|app| self.app_matches(app, &query, &state))
                .cloned()
                .collect::<Vec<_>>();
            sort_by_order(&mut apps, &state.favorite_order, |app| app.key.clone());
            let counts = (apps.len(), state.hidden.len());
            drop(state);
            self.sync_favorites_grid(apps);
            counts
        } else {
            self.ensure_rows();
            let state = self.user_state.borrow();
            let rows = self.rows.borrow();
            let sync_row_state = self.row_state_dirty.replace(false);
            let mut visible_count = 0;
            for row in rows.iter() {
                if sync_row_state {
                    row.sync_state(&state);
                }
                let visible = self.app_matches(&row.app, &query, &state);
                row.set_visible(visible);
                visible_count += usize::from(visible);
            }
            self.favorite_reorder.cancel();
            (visible_count, state.hidden.len())
        };

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
            if self.favorites_metadata_dirty.replace(false) {
                for (tile, app) in self.favorite_tiles.borrow_mut().iter_mut().zip(&apps) {
                    tile.update(app);
                }
            }
            return;
        }

        self.favorite_reorder
            .set_order(apps.iter().map(|app| app.key.clone()).collect());
        let adjustment = self.scroller.vadjustment();
        let previous_scroll = adjustment.value();
        let generation = self.scroll_generation.bump();
        clear_box(&self.favorites_grid);
        let mut tiles = self.favorite_tiles.borrow_mut();
        tiles.clear();
        self.favorites_grid_keys
            .replace(apps.iter().map(|app| app.key.clone()).collect::<Vec<_>>());

        for apps_row in apps.chunks(FAVORITES_COLUMNS as usize) {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 14);
            row.set_hexpand(false);
            row.set_halign(gtk::Align::Start);

            for app in apps_row.iter().cloned() {
                let tile = self.build_favorite_tile(app);
                tile.root.set_hexpand(false);
                row.append(&tile.root);
                tiles.push(tile);
            }

            self.favorites_grid.append(&row);
        }
        self.favorites_metadata_dirty.set(false);
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            let Some(this) = weak.upgrade() else {
                return;
            };
            if !this.scroll_generation.is_current(generation) {
                return;
            }
            let lower = adjustment.lower();
            let upper = (adjustment.upper() - adjustment.page_size()).max(lower);
            adjustment.set_value(previous_scroll.clamp(lower, upper));
        });
    }

    fn ensure_rows(self: &Rc<Self>) {
        if self.rows_dirty.get() {
            self.rebuild_rows();
        }
    }

    fn rebuild_rows(self: &Rc<Self>) {
        let apps = self.catalog.data.borrow().apps.clone();
        let keys = apps
            .iter()
            .map(|app| app.key.as_str())
            .collect::<HashSet<_>>();
        let mut existing = self
            .rows
            .borrow_mut()
            .drain(..)
            .filter_map(|row| {
                if keys.contains(row.app.key.as_str()) {
                    Some((row.app.key.clone(), row))
                } else {
                    self.list.remove(&row.root);
                    None
                }
            })
            .collect::<HashMap<_, _>>();
        let mut previous: Option<gtk::Widget> = None;
        let rows = apps
            .into_iter()
            .map(|app| {
                let row = if let Some(mut row) = existing.remove(&app.key) {
                    row.update_app(app);
                    row
                } else {
                    let row = self.build_app_row(app);
                    self.list.append(&row.root);
                    row
                };
                if row.root.prev_sibling() != previous {
                    self.list.reorder_child_after(&row.root, previous.as_ref());
                }
                previous = Some(row.root.clone().upcast());
                row
            })
            .collect();

        if self.empty_label.parent().is_none() {
            self.list.append(&self.empty_label);
        }
        if self.empty_label.prev_sibling() != previous {
            self.list
                .reorder_child_after(&self.empty_label, previous.as_ref());
        }
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

    fn launch_app_by_key(self: &Rc<Self>, key: &str) {
        let app = self
            .catalog
            .data
            .borrow()
            .apps
            .iter()
            .find(|app| app.key == key)
            .cloned();
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
        // A category/search reset takes precedence over a queued restoration
        // from rebuilding the favorites grid in the same event handler.
        self.scroll_generation.bump();
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
    read_ordered_keys(file_name).into_iter().collect()
}

fn read_ordered_keys(file_name: &str) -> Vec<String> {
    read_ordered_keys_at(&launcher_state_path(file_name))
}

fn read_ordered_keys_at(path: &Path) -> Vec<String> {
    let Ok(contents) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    serde_json::from_str::<Vec<String>>(&contents)
        .unwrap_or_default()
        .into_iter()
        .map(|key| key.trim().to_owned())
        .filter(|key| !key.is_empty())
        .filter(|key| seen.insert(key.clone()))
        .collect()
}

fn save_state_keys(file_name: &str, keys: &HashSet<String>) -> bool {
    let mut keys = keys.iter().cloned().collect::<Vec<_>>();
    keys.sort();
    save_ordered_keys(file_name, &keys)
}

fn save_ordered_keys(file_name: &str, keys: &[String]) -> bool {
    save_ordered_keys_at(&launcher_state_path(file_name), keys)
}

fn save_ordered_keys_at(path: &Path, keys: &[String]) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    if let Err(error) = fs::create_dir_all(parent) {
        warn!(%error, path = %parent.display(), "failed to create launcher state directory");
        return false;
    }

    let Ok(contents) = serde_json::to_string(&keys) else {
        return false;
    };

    let temporary = path.with_extension("json.tmp");
    match fs::write(&temporary, contents).and_then(|_| fs::rename(&temporary, path)) {
        Ok(()) => true,
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            warn!(%error, path = %path.display(), "failed to save launcher state");
            false
        }
    }
}

#[cfg(test)]
mod tests;
