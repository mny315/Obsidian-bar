use super::files::{EntryKind, ScanRequests, list_entries};
use super::thumbnails::{request_thumbnail, thumbnail_cache_path};
use super::{
    CARD_HEIGHT, CARD_WIDTH, ICON_WALLPAPER, MAX_RANDOM_INTERVAL_MINUTES,
    MIN_RANDOM_INTERVAL_MINUTES, WallpaperController, WallpaperSnapshot, is_video_wallpaper,
    wallpaper_key,
};
use crate::widgets::audio_spectrum::AudioSpectrumController;
use crate::widgets::bar_features::{BarFeatureController, BarFeatureState};
use crate::widgets::reorder::{ReorderGrid, sort_by_order};
use crate::widgets::system_monitor::SystemMonitorController;
use crate::widgets::tooltip::BarTooltipExt;
use crate::widgets::{
    BAR_POPUP_TOP_MARGIN, Generation, PopupReveal, SmoothScrollConfig, Timer,
    attach_bar_click_dismiss, attach_popup_focus_dismiss, clear_box, detach_application_window,
    install_smooth_scroll, run_background_async, set_optional_label, set_spinner_active,
};
use gtk::{gdk, glib, prelude::*};
use gtk4_layer_shell::LayerShell;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};
use tracing::warn;

mod directory;
use directory::{DirectoryBrowser, build_directory_page};

const GRID_COLUMNS: i32 = 3;
const GRID_GAP: i32 = 8;
const GALLERY_WIDTH: i32 = CARD_WIDTH * GRID_COLUMNS + GRID_GAP * (GRID_COLUMNS - 1);
const GALLERY_HEIGHT: i32 = CARD_HEIGHT * 6 + GRID_GAP * 5;
const WALLPAPER_NOTICE_MAX_CHARS: i32 = 58;
const GALLERY_APPEAR_CASCADE_ROWS: u32 = 6;
const GALLERY_ROW_APPEAR_STAGGER_MS: u64 = 24;
const SMOOTH_SCROLL: SmoothScrollConfig = SmoothScrollConfig::new(96.0, 130.0, 72.0);
const REFRESH_ANIMATION_MIN_DURATION: Duration = Duration::from_secs(2);
const DELETE_CONFIRM_DURATION: Duration = Duration::from_secs(3);
const DELETE_FADE_DURATION: Duration = Duration::from_millis(160);
const PICKER_NAMESPACE: &str = "obsidian-bar-wallpaper";
const ICON_FOLDER: &str = "\u{f024b}";
const ICON_REFRESH: &str = "\u{f0450}";
const ICON_VIDEO: &str = "\u{f040a}";
const ICON_BACK: &str = "\u{f004d}";
const ICON_UP: &str = "\u{f005d}";
const ICON_HOME: &str = "\u{f02dc}";
const ICON_SHUFFLE: &str = "\u{f049d}";
const ICON_PLAYER_ENABLED: &str = "󰎇";
const ICON_PLAYER_DISABLED: &str = "󰎈";
const ICON_WORKSPACE: &str = "󰕰";
const ICON_EQUALIZER: &str = "󰺢";
const ICON_SYSTEM_MONITOR: &str = "\u{f0379}";

struct RenderedGallery {
    directory: PathBuf,
    current: Option<PathBuf>,
    loaded: bool,
    order: Vec<String>,
    revision: u64,
}

type LiveWallpaperCards = Rc<RefCell<HashMap<PathBuf, glib::WeakRef<gtk::Overlay>>>>;

#[derive(Clone, Default)]
struct GalleryReuse {
    cards: Rc<RefCell<HashMap<PathBuf, gtk::Box>>>,
    reordering: Rc<Cell<bool>>,
    settle_timer: Rc<Timer>,
}

impl GalleryReuse {
    fn begin_reordering(&self) {
        self.reordering.set(true);
        let reordering = Rc::downgrade(&self.reordering);
        // ListView can bind changed rows on the next layout. Suppress their
        // entrance animation briefly, without disabling it for all later scrolls.
        self.settle_timer
            .schedule(Duration::from_millis(250), move || {
                if let Some(reordering) = reordering.upgrade() {
                    reordering.set(false);
                }
            });
    }
}

#[derive(Clone)]
struct GalleryView {
    model: gio::ListStore,
    reorder: ReorderGrid,
    active_wallpaper: Rc<RefCell<Option<PathBuf>>>,
    live_wallpaper_cards: LiveWallpaperCards,
    reuse: GalleryReuse,
    scroller: glib::WeakRef<gtk::ScrolledWindow>,
    empty: glib::WeakRef<gtk::Box>,
    count: glib::WeakRef<gtk::Label>,
    notice: glib::WeakRef<gtk::Label>,
    rendered: Rc<RefCell<RenderedGallery>>,
    refresh_generation: Rc<Generation>,
    scan_requests: ScanRequests,
    refresh_animation_timer: Rc<Timer>,
    refresh_button: glib::WeakRef<gtk::Button>,
    refresh_icon: glib::WeakRef<gtk::Label>,
    refresh_spinner: glib::WeakRef<gtk::Spinner>,
}

struct GalleryRenderWidgets<'a> {
    scroller: &'a gtk::ScrolledWindow,
    empty: &'a gtk::Box,
    count: &'a gtk::Label,
    notice: &'a gtk::Label,
}

pub struct WallpaperIndicator {
    button: gtk::Button,
    picker: gtk::ApplicationWindow,
    picker_reveal: PopupReveal,
    focus_armed: Rc<Cell<bool>>,
}

impl Drop for WallpaperIndicator {
    fn drop(&mut self) {
        detach_application_window(&self.picker);
    }
}

impl WallpaperIndicator {
    pub fn new(
        application: &gtk::Application,
        bar_window: &gtk::ApplicationWindow,
        monitor: &gdk::Monitor,
        controller: &Rc<WallpaperController>,
        bar_features: &Rc<BarFeatureController>,
        audio_spectrum: &Rc<AudioSpectrumController>,
        system_monitor: &Rc<SystemMonitorController>,
    ) -> Self {
        let picker = gtk::ApplicationWindow::builder()
            .application(application)
            .decorated(false)
            .resizable(false)
            .build();
        picker.add_css_class("wallpaper-picker-window");
        picker.init_layer_shell();
        picker.set_namespace(Some(PICKER_NAMESPACE));
        picker.set_layer(Layer::Top);
        picker.set_keyboard_mode(KeyboardMode::OnDemand);
        picker.set_monitor(Some(monitor));
        picker.set_anchor(Edge::Top, true);
        picker.set_anchor(Edge::Left, true);
        picker.set_anchor(Edge::Right, false);
        picker.set_anchor(Edge::Bottom, false);
        picker.set_exclusive_zone(-1);
        picker.set_margin(Edge::Top, 0);
        picker.set_margin(Edge::Left, 0);
        picker.set_hide_on_close(true);

        let picker_root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        picker_root.add_css_class("widget-popup-root");

        let surface = gtk::Box::new(gtk::Orientation::Vertical, 0);
        surface.add_css_class("wallpaper-picker-surface");

        let popup_content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        surface.append(&popup_content);
        let picker_reveal = PopupReveal::masked(surface.upcast::<gtk::Widget>());
        picker_root.append(picker_reveal.widget());
        picker.set_child(Some(&picker_root));

        popup_content.append(&build_picker_content(
            controller,
            bar_features,
            audio_spectrum,
            system_monitor,
        ));

        let button = gtk::Button::new();
        button.add_css_class("wallpaper-widget-trigger");
        button.set_bar_tooltip_text(Some("Wallpapers"));
        button.set_valign(gtk::Align::Center);

        let trigger_icon = gtk::Label::new(Some(ICON_WALLPAPER));
        trigger_icon.add_css_class("wallpaper-trigger-icon");
        button.set_child(Some(&trigger_icon));

        let focus_armed = Rc::new(Cell::new(false));
        {
            let weak_picker = picker.downgrade();
            let close_focus = Rc::clone(&focus_armed);
            let close_reveal = picker_reveal.clone();
            attach_popup_focus_dismiss(&picker, bar_window, &focus_armed, move || {
                if let Some(picker) = weak_picker.upgrade() {
                    close_focus.set(false);
                    close_reveal.hide(&picker);
                }
            });
        }

        {
            let focus_armed = Rc::clone(&focus_armed);
            let picker_reveal = picker_reveal.clone();
            picker.connect_visible_notify(move |picker| {
                if picker.is_visible() {
                    return;
                }
                focus_armed.set(false);
                picker_reveal.reset_hidden();
            });
        }

        let key = gtk::EventControllerKey::new();
        {
            let weak_picker = picker.downgrade();
            let focus_armed = Rc::clone(&focus_armed);
            let picker_reveal = picker_reveal.clone();
            key.connect_key_pressed(move |_, key, _, _| {
                if key == gdk::Key::Escape {
                    if let Some(picker) = weak_picker.upgrade() {
                        focus_armed.set(false);
                        picker_reveal.hide(&picker);
                    }
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
        }
        picker.add_controller(key);

        {
            let weak_picker = picker.downgrade();
            let close_focus = Rc::clone(&focus_armed);
            let close_reveal = picker_reveal.clone();
            attach_bar_click_dismiss(bar_window, &button, &picker, move || {
                if let Some(picker) = weak_picker.upgrade() {
                    close_focus.set(false);
                    close_reveal.hide(&picker);
                }
            });
        }

        {
            let weak_picker = picker.downgrade();
            let weak_bar_window = bar_window.downgrade();
            let monitor = monitor.clone();
            let focus_armed = Rc::clone(&focus_armed);
            let picker_reveal = picker_reveal.clone();
            button.connect_clicked(move |button| {
                let Some(picker) = weak_picker.upgrade() else {
                    return;
                };
                if picker_reveal.is_revealed() {
                    focus_armed.set(false);
                    picker_reveal.hide(&picker);
                } else {
                    let Some(bar_window) = weak_bar_window.upgrade() else {
                        return;
                    };
                    position_picker_at_trigger(&picker, button, &bar_window, &monitor);
                    focus_armed.set(false);
                    picker_reveal.sync_top_anchor(&picker);
                    picker_reveal.show(&picker);
                }
            });
        }

        Self {
            button,
            picker,
            picker_reveal,
            focus_armed,
        }
    }

    pub fn widget(&self) -> &gtk::Button {
        &self.button
    }

    pub fn dismiss(&self) {
        self.focus_armed.set(false);
        self.picker_reveal.hide(&self.picker);
    }
}

fn position_picker_at_trigger(
    picker: &gtk::ApplicationWindow,
    button: &gtk::Button,
    bar_window: &gtk::ApplicationWindow,
    monitor: &gdk::Monitor,
) {
    const BAR_MARGIN_LEFT: f32 = 9.0;
    const PICKER_WIDTH: f32 = (GALLERY_WIDTH + 48) as f32;

    let Some(bounds) = button.compute_bounds(bar_window) else {
        return;
    };

    let monitor_width = monitor.geometry().width() as f32;
    let anchor_center_x = BAR_MARGIN_LEFT + bounds.x() + bounds.width() / 2.0;
    let min_left = BAR_MARGIN_LEFT;
    let max_left = (monitor_width - PICKER_WIDTH - BAR_MARGIN_LEFT).max(min_left);
    let left = (anchor_center_x - PICKER_WIDTH / 2.0).clamp(min_left, max_left);
    picker.set_margin(Edge::Left, left.round() as i32);
    picker.set_margin(Edge::Top, BAR_POPUP_TOP_MARGIN);
}

struct GalleryPage {
    root: gtk::Box,
    model: gio::ListStore,
    reorder: ReorderGrid,
    active_wallpaper: Rc<RefCell<Option<PathBuf>>>,
    live_wallpaper_cards: LiveWallpaperCards,
    reuse: GalleryReuse,
    scroller: gtk::ScrolledWindow,
    empty: gtk::Box,
    count: gtk::Label,
    notice: gtk::Label,
    folder_button: gtk::Button,
    refresh_button: gtk::Button,
    refresh_icon: gtk::Label,
    refresh_spinner: gtk::Spinner,
}

fn wallpaper_notice_label() -> gtk::Label {
    let notice = gtk::Label::new(None);
    notice.add_css_class("wallpaper-notice");
    notice.set_halign(gtk::Align::Start);
    notice.set_xalign(0.0);
    notice.set_wrap(true);
    notice.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    notice.set_natural_wrap_mode(gtk::NaturalWrapMode::Word);
    notice.set_max_width_chars(WALLPAPER_NOTICE_MAX_CHARS);
    notice.set_lines(2);
    notice.set_ellipsize(gtk::pango::EllipsizeMode::End);
    notice.set_visible(false);
    notice
}

fn build_gallery_page(
    controller: &Rc<WallpaperController>,
    bar_features: &Rc<BarFeatureController>,
    audio_spectrum: &Rc<AudioSpectrumController>,
    system_monitor: &Rc<SystemMonitorController>,
) -> GalleryPage {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 10);
    root.add_css_class("wallpaper-selector-page");

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    header.add_css_class("wallpaper-header");
    header.set_valign(gtk::Align::Start);

    let title_column = gtk::Box::new(gtk::Orientation::Vertical, 2);
    title_column.set_hexpand(true);

    let title_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    title_row.set_valign(gtk::Align::Center);

    let header_icon = gtk::Label::new(Some(ICON_WALLPAPER));
    header_icon.add_css_class("wallpaper-header-icon");

    let title = gtk::Label::new(Some("Wallpapers"));
    title.add_css_class("wallpaper-title");
    title.set_xalign(0.0);

    let count = gtk::Label::new(None);
    count.add_css_class("wallpaper-count");

    title_row.append(&header_icon);
    title_row.append(&title);
    title_row.append(&count);

    let path_actions = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    path_actions.add_css_class("wallpaper-path-actions");
    path_actions.set_halign(gtk::Align::Start);
    path_actions.set_valign(gtk::Align::Center);

    let folder_button = header_button(ICON_FOLDER);
    let (refresh_button, refresh_icon, refresh_spinner) = wallpaper_refresh_button(ICON_REFRESH);
    let random_button = random_menu_button(controller);
    path_actions.append(&folder_button);
    path_actions.append(&refresh_button);
    path_actions.append(&random_button);

    title_column.append(&title_row);
    title_column.append(&path_actions);

    let feature_actions = bar_feature_actions(bar_features, audio_spectrum, system_monitor);
    header.append(&title_column);
    header.append(&feature_actions);

    let gallery_frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
    gallery_frame.add_css_class("wallpaper-gallery-frame");

    let notice = wallpaper_notice_label();

    let scroller = gtk::ScrolledWindow::new();
    scroller.add_css_class("wallpaper-list-wrap");
    scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::External);
    scroller.set_min_content_width(GALLERY_WIDTH);
    scroller.set_min_content_height(GALLERY_HEIGHT);
    scroller.set_max_content_height(GALLERY_HEIGHT);
    scroller.set_propagate_natural_height(false);
    scroller.set_propagate_natural_width(false);
    scroller.set_halign(gtk::Align::Start);
    scroller.set_valign(gtk::Align::Start);

    let model = gio::ListStore::new::<glib::BoxedAnyObject>();
    let (reorder, drag_overlay) =
        ReorderGrid::new(&scroller, GRID_COLUMNS as usize, GRID_GAP, GRID_GAP);
    {
        let controller = Rc::downgrade(controller);
        let notice = notice.downgrade();
        reorder.on_commit(move |order| {
            let Some(controller) = controller.upgrade() else {
                return false;
            };
            match controller.set_gallery_order(order) {
                Ok(()) => true,
                Err(error) => {
                    if let Some(notice) = notice.upgrade() {
                        set_optional_label(&notice, Some(&error.to_string()));
                    }
                    false
                }
            }
        });
    }
    let selection = gtk::NoSelection::new(Some(model.clone()));
    let factory = gtk::SignalListItemFactory::new();
    let active_wallpaper = Rc::new(RefCell::new(None::<PathBuf>));
    let live_wallpaper_cards: LiveWallpaperCards = Rc::new(RefCell::new(HashMap::new()));
    let reuse = GalleryReuse::default();

    factory.connect_setup(|_, object| {
        let Some(list_item) = object.downcast_ref::<gtk::ListItem>() else {
            return;
        };

        list_item.set_selectable(false);
        list_item.set_activatable(false);

        let row = gtk::Box::new(gtk::Orientation::Horizontal, GRID_GAP);
        row.add_css_class("wallpaper-grid-row");
        row.set_size_request(GALLERY_WIDTH, CARD_HEIGHT);
        row.set_halign(gtk::Align::Start);
        row.set_hexpand(false);
        list_item.set_child(Some(&row));
    });

    {
        let controller = Rc::clone(controller);
        let weak_notice = notice.downgrade();
        let active_wallpaper = Rc::clone(&active_wallpaper);
        let live_wallpaper_cards = Rc::clone(&live_wallpaper_cards);
        let model = model.clone();
        let reorder = reorder.clone();
        let reuse = reuse.clone();
        factory.connect_bind(move |_, object| {
            let Some(list_item) = object.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let Some(row_data) = list_item.item().and_downcast::<glib::BoxedAnyObject>() else {
                return;
            };
            let Some(row) = list_item.child().and_downcast::<gtk::Box>() else {
                return;
            };

            row.remove_css_class("wallpaper-grid-row-enter");
            row.add_css_class("wallpaper-grid-row-pending");
            clear_box(&row);
            {
                let current = active_wallpaper.borrow();
                let paths = row_data.borrow::<Vec<PathBuf>>();
                for path in paths.iter().cloned() {
                    let active = current.as_deref() == Some(path.as_path());
                    let cached = reuse.cards.borrow_mut().remove(&path).or_else(|| {
                        reorder
                            .retained_slot(&wallpaper_key(&path))
                            .and_downcast::<gtk::Box>()
                    });
                    let card = cached.unwrap_or_else(|| {
                        wallpaper_card(
                            &controller,
                            &weak_notice,
                            &live_wallpaper_cards,
                            &reorder,
                            path,
                            active,
                        )
                    });
                    row.append(&card);
                }
            }

            let has_next_row = list_item.position().saturating_add(1) < model.n_items();
            row.set_margin_bottom(if has_next_row { GRID_GAP } else { 0 });
            if reuse.reordering.get() || !reorder.activation_allowed() {
                row.remove_css_class("wallpaper-grid-row-pending");
                return;
            }

            let expected_item = row_data.upcast::<glib::Object>();
            let weak_list_item = list_item.downgrade();
            let weak_row = row.downgrade();
            let cascade_index = list_item.position() % GALLERY_APPEAR_CASCADE_ROWS;
            let delay =
                Duration::from_millis(u64::from(cascade_index) * GALLERY_ROW_APPEAR_STAGGER_MS);
            let reordering = Rc::clone(&reuse.reordering);
            glib::timeout_add_local_once(delay, move || {
                let (Some(list_item), Some(row)) = (weak_list_item.upgrade(), weak_row.upgrade())
                else {
                    return;
                };
                if list_item.item().as_ref() == Some(&expected_item) {
                    row.remove_css_class("wallpaper-grid-row-pending");
                    if !reordering.get() {
                        row.add_css_class("wallpaper-grid-row-enter");
                    }
                }
            });
        });
    }

    factory.connect_unbind(|_, object| {
        let Some(list_item) = object.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(row) = list_item.child().and_downcast::<gtk::Box>() else {
            return;
        };
        row.remove_css_class("wallpaper-grid-row-pending");
        row.remove_css_class("wallpaper-grid-row-enter");
        clear_box(&row);
    });

    let gallery = gtk::ListView::new(Some(selection), Some(factory));
    gallery.add_css_class("wallpaper-grid-rows");
    gallery.set_show_separators(false);
    gallery.set_size_request(GALLERY_WIDTH, -1);
    gallery.set_halign(gtk::Align::Fill);
    gallery.set_valign(gtk::Align::Fill);
    gallery.set_hexpand(true);
    scroller.set_child(Some(&gallery));
    install_smooth_scroll(&scroller, SMOOTH_SCROLL);

    let empty = gtk::Box::new(gtk::Orientation::Vertical, 4);
    empty.add_css_class("wallpaper-empty");
    empty.set_size_request(GALLERY_WIDTH, CARD_HEIGHT * 2 + GRID_GAP);
    empty.set_halign(gtk::Align::Center);
    empty.set_valign(gtk::Align::Center);

    let empty_icon = gtk::Label::new(Some(ICON_WALLPAPER));
    empty_icon.add_css_class("wallpaper-empty-icon");
    let empty_title = gtk::Label::new(Some("No wallpapers found"));
    let empty_meta = gtk::Label::new(Some("Choose a folder with images or videos"));
    empty_meta.add_css_class("wallpaper-empty-meta");
    empty.append(&empty_icon);
    empty.append(&empty_title);
    empty.append(&empty_meta);

    gallery_frame.append(&drag_overlay);
    gallery_frame.append(&empty);

    root.append(&header);
    root.append(&gallery_frame);
    root.append(&notice);

    GalleryPage {
        root,
        model,
        reorder,
        active_wallpaper,
        live_wallpaper_cards,
        reuse,
        scroller,
        empty,
        count,
        notice,
        folder_button,
        refresh_button,
        refresh_icon,
        refresh_spinner,
    }
}

fn build_picker_content(
    controller: &Rc<WallpaperController>,
    bar_features: &Rc<BarFeatureController>,
    audio_spectrum: &Rc<AudioSpectrumController>,
    system_monitor: &Rc<SystemMonitorController>,
) -> gtk::Box {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.add_css_class("wallpaper-picker");
    root.set_size_request(GALLERY_WIDTH + 24, -1);

    let stack = gtk::Stack::new();
    stack.add_css_class("wallpaper-stack");
    stack.set_hhomogeneous(true);
    stack.set_vhomogeneous(true);
    stack.set_transition_type(gtk::StackTransitionType::SlideLeftRight);
    stack.set_transition_duration(240);

    let GalleryPage {
        root: selector_page,
        model,
        reorder,
        active_wallpaper,
        live_wallpaper_cards,
        reuse,
        scroller,
        empty,
        count,
        notice,
        folder_button,
        refresh_button,
        refresh_icon,
        refresh_spinner,
    } = build_gallery_page(controller, bar_features, audio_spectrum, system_monitor);
    let directory_page = build_directory_page();

    stack.add_named(&selector_page, Some("wallpapers"));
    stack.add_named(&directory_page.root, Some("directories"));
    stack.set_visible_child_name("wallpapers");
    root.append(&stack);

    let initial_snapshot = controller.snapshot();
    let browser = DirectoryBrowser::new(initial_snapshot.directory.clone(), &directory_page);
    let rendered = Rc::new(RefCell::new(RenderedGallery {
        revision: initial_snapshot.gallery_revision,
        directory: initial_snapshot.directory.clone(),
        current: initial_snapshot.current.clone(),
        loaded: false,
        order: initial_snapshot.gallery_order.clone(),
    }));
    let scan_requests = ScanRequests::default();
    let scans_on_destroy = scan_requests.clone();
    let refresh_animation_timer = Rc::new(Timer::default());
    let animation_on_destroy = Rc::clone(&refresh_animation_timer);
    root.connect_destroy(move |_| {
        scans_on_destroy.cancel();
        animation_on_destroy.cancel();
    });
    let gallery_view = GalleryView {
        model,
        reorder,
        active_wallpaper,
        live_wallpaper_cards,
        reuse,
        scroller: scroller.downgrade(),
        empty: empty.downgrade(),
        count: count.downgrade(),
        notice: notice.downgrade(),
        rendered,
        refresh_generation: Rc::new(Generation::default()),
        scan_requests,
        refresh_animation_timer,
        refresh_button: refresh_button.downgrade(),
        refresh_icon: refresh_icon.downgrade(),
        refresh_spinner: refresh_spinner.downgrade(),
    };
    gallery_view.refresh(initial_snapshot, false);

    let receiver = controller.subscribe();
    let receiver_on_destroy = receiver.clone();
    root.connect_destroy(move |_| {
        receiver_on_destroy.close();
    });

    let gallery_view_for_updates = gallery_view.clone();
    glib::MainContext::default().spawn_local(async move {
        while let Ok(snapshot) = receiver.recv().await {
            if gallery_view_for_updates.scroller.upgrade().is_none() {
                receiver.close();
                break;
            }
            gallery_view_for_updates.apply_snapshot(snapshot);
        }
    });

    {
        let controller = Rc::clone(controller);
        let browser = browser.clone();
        let weak_stack = stack.downgrade();
        folder_button.connect_clicked(move |_| {
            let Some(stack) = weak_stack.upgrade() else {
                return;
            };

            browser.set_path(controller.snapshot().directory);
            browser.render();

            stack.set_visible_child_name("directories");
        });
    }

    {
        let weak_stack = stack.downgrade();
        directory_page.back_button.connect_clicked(move |_| {
            if let Some(stack) = weak_stack.upgrade() {
                stack.set_visible_child_name("wallpapers");
            }
        });
    }

    {
        let browser = browser.clone();
        directory_page.home_button.connect_clicked(move |_| {
            browser.navigate_to(glib::home_dir());
        });
    }

    {
        let browser = browser.clone();
        directory_page.up_button.connect_clicked(move |_| {
            browser.navigate_up();
        });
    }

    {
        let controller = Rc::clone(controller);
        let weak_stack = stack.downgrade();
        let weak_notice = directory_page.notice.downgrade();
        directory_page.select_button.connect_clicked(move |_| {
            let (Some(stack), Some(notice)) = (weak_stack.upgrade(), weak_notice.upgrade()) else {
                return;
            };

            match controller.set_directory(browser.path()) {
                Ok(()) => {
                    set_optional_label(&notice, None);
                    stack.set_visible_child_name("wallpapers");
                }
                Err(error) => set_optional_label(&notice, Some(&error.to_string())),
            }
        });
    }

    {
        let controller = Rc::clone(controller);
        refresh_button.connect_clicked(move |_| {
            gallery_view.refresh(controller.snapshot(), true);
        });
    }

    root
}

fn render_gallery_items(
    model: &gio::ListStore,
    active_wallpaper: &Rc<RefCell<Option<PathBuf>>>,
    widgets: GalleryRenderWidgets<'_>,
    current: Option<PathBuf>,
    items: Vec<PathBuf>,
) {
    let adjustment = widgets.scroller.vadjustment();
    let previous_scroll = adjustment.value();

    set_optional_label(widgets.notice, None);
    active_wallpaper.replace(current);
    widgets.count.set_label(&items.len().to_string());
    widgets.scroller.set_visible(!items.is_empty());
    widgets.empty.set_visible(items.is_empty());

    replace_gallery_rows(model, &items);

    glib::idle_add_local_once(move || {
        let lower = adjustment.lower();
        let upper = (adjustment.upper() - adjustment.page_size()).max(lower);
        adjustment.set_value(previous_scroll.clamp(lower, upper));
    });
}

fn gallery_paths(model: &gio::ListStore) -> Vec<PathBuf> {
    (0..model.n_items())
        .filter_map(|index| model.item(index).and_downcast::<glib::BoxedAnyObject>())
        .flat_map(|row| row.borrow::<Vec<PathBuf>>().clone())
        .collect()
}

fn replace_gallery_rows(model: &gio::ListStore, items: &[PathBuf]) {
    let rows = items
        .chunks(GRID_COLUMNS as usize)
        .map(|paths| glib::BoxedAnyObject::new(paths.to_vec()))
        .collect::<Vec<_>>();
    model.splice(0, model.n_items(), &rows);
}

impl GalleryView {
    fn refresh(&self, snapshot: WallpaperSnapshot, animate: bool) {
        self.reuse.reordering.set(false);
        self.reuse.cards.borrow_mut().clear();
        let generation = self.refresh_generation.bump();
        let request = self.scan_requests.begin();
        let animation_started = animate.then(Instant::now);
        if animate {
            self.set_refresh_animating(true);
        } else {
            self.set_refresh_animating(false);
        }

        let reset_model = {
            let rendered = self.rendered.borrow();
            rendered.directory != snapshot.directory
                || !rendered.loaded && self.model.n_items() == 0
        };
        let preserve_cards =
            !reset_model && self.rendered.borrow().revision != snapshot.gallery_revision;
        {
            let mut rendered = self.rendered.borrow_mut();
            rendered.directory.clone_from(&snapshot.directory);
            rendered.current.clone_from(&snapshot.current);
            rendered.loaded = false;
            rendered.order.clone_from(&snapshot.gallery_order);
            rendered.revision = snapshot.gallery_revision;
        }
        self.active_wallpaper.replace(snapshot.current);

        let (Some(scroller), Some(empty), Some(count), Some(notice)) = (
            self.scroller.upgrade(),
            self.empty.upgrade(),
            self.count.upgrade(),
            self.notice.upgrade(),
        ) else {
            self.set_refresh_animating(false);
            return;
        };

        if reset_model {
            self.reorder.set_order(Vec::new());
            self.model.remove_all();
            self.live_wallpaper_cards.borrow_mut().clear();
            count.set_label("0");
            scroller.set_visible(false);
            empty.set_visible(false);
        }
        if !preserve_cards {
            set_optional_label(&notice, Some("Loading wallpapers…"));
        }

        let directory = snapshot.directory;
        let view = self.clone();
        glib::MainContext::default().spawn_local(async move {
            let result = run_background_async({
                let directory = directory.clone();
                let request = request.clone();
                move || {
                    list_entries(&directory, EntryKind::Wallpaper, Some(&request))
                        .map_err(|error| error.to_string())
                }
            })
            .await
            .unwrap_or_else(|| Err("wallpaper scan worker stopped".to_owned()));

            if request.is_cancelled() || !view.refresh_generation.is_current(generation) {
                return;
            }
            if preserve_cards && let Ok(items) = &result {
                let mut remaining = items.clone();
                sort_by_order(&mut remaining, &view.rendered.borrow().order, |path| {
                    wallpaper_key(path)
                });
                let remaining = remaining
                    .iter()
                    .map(|path| wallpaper_key(path))
                    .collect::<Vec<_>>();
                let duration = view.reorder.animate_removal(&remaining);
                glib::timeout_future(duration).await;
            }
            view.finish_refresh(
                generation,
                directory,
                result,
                animation_started,
                preserve_cards,
            );
        });
    }

    fn apply_snapshot(&self, snapshot: WallpaperSnapshot) {
        let same_directory = self.rendered.borrow().directory == snapshot.directory;
        if !same_directory || self.rendered.borrow().revision != snapshot.gallery_revision {
            self.refresh(snapshot, false);
            return;
        }

        let mut rendered = self.rendered.borrow_mut();
        if rendered.order != snapshot.gallery_order {
            rendered.order.clone_from(&snapshot.gallery_order);
            if rendered.loaded {
                let mut items = gallery_paths(&self.model);
                sort_by_order(&mut items, &rendered.order, |path| wallpaper_key(path));
                let scroll = self.scroller.upgrade().map(|scroller| {
                    let adjustment = scroller.vadjustment();
                    let value = adjustment.value();
                    (adjustment, value)
                });
                self.reuse.begin_reordering();
                self.reorder_rows(&items);
                if let Some((adjustment, value)) = scroll {
                    glib::idle_add_local_once(move || {
                        let lower = adjustment.lower();
                        let upper = (adjustment.upper() - adjustment.page_size()).max(lower);
                        adjustment.set_value(value.clamp(lower, upper));
                    });
                }
            }
        }
        let previous = rendered.current.clone();
        rendered.current.clone_from(&snapshot.current);
        if rendered.loaded {
            update_gallery_active(
                &self.active_wallpaper,
                &self.live_wallpaper_cards,
                previous.as_deref(),
                snapshot.current.as_deref(),
            );
        } else {
            self.active_wallpaper.replace(snapshot.current);
        }
    }

    fn reorder_rows(&self, items: &[PathBuf]) {
        let previous = gallery_paths(&self.model);
        let old_rows = previous.chunks(GRID_COLUMNS as usize).collect::<Vec<_>>();
        let new_rows = items.chunks(GRID_COLUMNS as usize).collect::<Vec<_>>();
        let prefix = old_rows
            .iter()
            .zip(&new_rows)
            .take_while(|(a, b)| a == b)
            .count();
        let suffix = old_rows[prefix..]
            .iter()
            .rev()
            .zip(new_rows[prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        let end = old_rows.len() - suffix;
        self.reuse.cards.borrow_mut().clear();
        for path in old_rows[prefix..end].iter().flat_map(|row| row.iter()) {
            let card = self
                .live_wallpaper_cards
                .borrow()
                .get(path)
                .and_then(glib::WeakRef::upgrade)
                .and_then(|overlay| overlay.parent().and_downcast::<gtk::Box>());
            if let Some(card) = card {
                if let Some(row) = card.parent().and_downcast::<gtk::Box>() {
                    row.remove(&card);
                }
                self.reuse.cards.borrow_mut().insert(path.clone(), card);
            }
        }
        let rows = new_rows[prefix..new_rows.len() - suffix]
            .iter()
            .map(|paths| glib::BoxedAnyObject::new(paths.to_vec()))
            .collect::<Vec<_>>();
        // Move the retained source into the reuse pool before ending its drag.
        self.reorder
            .set_order(items.iter().map(|path| wallpaper_key(path)).collect());
        self.model
            .splice(prefix as u32, (end - prefix) as u32, &rows);
        // An unchanged prefix is not rebound when the final row is removed or
        // appended. Its previous last row still needs the correct trailing gap.
        if prefix > 0
            && let Some(row) = self
                .live_wallpaper_cards
                .borrow()
                .get(&old_rows[prefix - 1][0])
                .and_then(glib::WeakRef::upgrade)
                .and_then(|visual| visual.parent())
                .and_then(|slot| slot.parent())
        {
            row.set_margin_bottom(if prefix < new_rows.len() { GRID_GAP } else { 0 });
        }
    }

    fn finish_refresh(
        &self,
        generation: u64,
        directory: PathBuf,
        result: Result<Vec<PathBuf>, String>,
        animation_started: Option<Instant>,
        preserve_cards: bool,
    ) {
        if !self.refresh_generation.is_current(generation) {
            return;
        }

        let (Some(scroller), Some(empty), Some(count), Some(notice)) = (
            self.scroller.upgrade(),
            self.empty.upgrade(),
            self.count.upgrade(),
            self.notice.upgrade(),
        ) else {
            self.finish_refresh_animation(generation, animation_started);
            return;
        };

        let mut rendered = self.rendered.borrow_mut();
        if rendered.directory != directory {
            drop(rendered);
            self.finish_refresh_animation(generation, animation_started);
            return;
        }

        match result {
            Ok(mut items) => {
                sort_by_order(&mut items, &rendered.order, |path| wallpaper_key(path));
                if preserve_cards {
                    set_optional_label(&notice, None);
                    let adjustment = scroller.vadjustment();
                    let scroll = adjustment.value();
                    self.reuse.begin_reordering();
                    self.reorder_rows(&items);
                    glib::idle_add_local_once(move || {
                        let lower = adjustment.lower();
                        let upper = (adjustment.upper() - adjustment.page_size()).max(lower);
                        adjustment.set_value(scroll.clamp(lower, upper));
                    });
                    count.set_label(&items.len().to_string());
                    scroller.set_visible(!items.is_empty());
                    empty.set_visible(items.is_empty());
                    let remaining = items.iter().collect::<HashSet<_>>();
                    self.live_wallpaper_cards.borrow_mut().retain(|path, weak| {
                        if !remaining.contains(path) {
                            return false;
                        }
                        let Some(card) = weak.upgrade() else {
                            return false;
                        };
                        if rendered.current.as_ref() == Some(path) {
                            card.add_css_class("wallpaper-thumb-wrap-active");
                        } else {
                            card.remove_css_class("wallpaper-thumb-wrap-active");
                        }
                        true
                    });
                    self.reuse
                        .cards
                        .borrow_mut()
                        .retain(|path, _| remaining.contains(path));
                    rendered.loaded = true;
                    drop(rendered);
                    self.finish_refresh_animation(generation, animation_started);
                    return;
                }
                self.reorder
                    .set_order(items.iter().map(|path| wallpaper_key(path)).collect());
                render_gallery_items(
                    &self.model,
                    &self.active_wallpaper,
                    GalleryRenderWidgets {
                        scroller: &scroller,
                        empty: &empty,
                        count: &count,
                        notice: &notice,
                    },
                    rendered.current.clone(),
                    items,
                );
            }
            Err(error) => {
                set_optional_label(&notice, Some(&error));
                if self.model.n_items() == 0 {
                    scroller.set_visible(false);
                    empty.set_visible(true);
                }
            }
        }
        rendered.loaded = true;
        drop(rendered);
        self.finish_refresh_animation(generation, animation_started);
    }

    fn finish_refresh_animation(&self, generation: u64, animation_started: Option<Instant>) {
        let Some(started) = animation_started else {
            self.set_refresh_animating(false);
            return;
        };

        let remaining = REFRESH_ANIMATION_MIN_DURATION.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            self.set_refresh_animating(false);
            return;
        }

        let refresh_generation = Rc::clone(&self.refresh_generation);
        let button = self.refresh_button.clone();
        let icon = self.refresh_icon.clone();
        let spinner = self.refresh_spinner.clone();
        self.refresh_animation_timer.schedule(remaining, move || {
            if refresh_generation.is_current(generation)
                && let (Some(button), Some(icon), Some(spinner)) =
                    (button.upgrade(), icon.upgrade(), spinner.upgrade())
            {
                button.set_sensitive(true);
                set_spinner_active(&icon, &spinner, false);
            }
        });
    }

    fn set_refresh_animating(&self, active: bool) {
        self.refresh_animation_timer.cancel();
        if let (Some(button), Some(icon), Some(spinner)) = (
            self.refresh_button.upgrade(),
            self.refresh_icon.upgrade(),
            self.refresh_spinner.upgrade(),
        ) {
            button.set_sensitive(!active);
            set_spinner_active(&icon, &spinner, active);
        }
    }
}

fn update_gallery_active(
    active_wallpaper: &Rc<RefCell<Option<PathBuf>>>,
    live_wallpaper_cards: &LiveWallpaperCards,
    previous: Option<&Path>,
    current: Option<&Path>,
) {
    if previous == current {
        return;
    }

    active_wallpaper.replace(current.map(Path::to_path_buf));

    if let Some(path) = previous {
        set_live_wallpaper_card_active(live_wallpaper_cards, path, false);
    }
    if let Some(path) = current {
        set_live_wallpaper_card_active(live_wallpaper_cards, path, true);
    }
}

fn set_live_wallpaper_card_active(
    live_wallpaper_cards: &LiveWallpaperCards,
    path: &Path,
    active: bool,
) {
    let overlay = live_wallpaper_cards
        .borrow()
        .get(path)
        .and_then(glib::WeakRef::upgrade);

    let Some(overlay) = overlay else {
        return;
    };

    if active {
        overlay.add_css_class("wallpaper-thumb-wrap-active");
    } else {
        overlay.remove_css_class("wallpaper-thumb-wrap-active");
    }
}

fn wallpaper_card(
    controller: &Rc<WallpaperController>,
    weak_notice: &glib::WeakRef<gtk::Label>,
    live_wallpaper_cards: &LiveWallpaperCards,
    reorder: &ReorderGrid,
    path: PathBuf,
    active: bool,
) -> gtk::Box {
    let button = gtk::Button::new();
    button.add_css_class("wallpaper-card");
    button.set_size_request(CARD_WIDTH, CARD_HEIGHT);
    button.set_halign(gtk::Align::Start);
    button.set_valign(gtk::Align::Start);
    button.set_hexpand(false);
    button.set_vexpand(false);

    let visual = gtk::Overlay::new();
    visual.add_css_class("wallpaper-thumb-wrap");
    if active {
        visual.add_css_class("wallpaper-thumb-wrap-active");
    }
    visual.set_size_request(CARD_WIDTH, CARD_HEIGHT);
    visual.set_overflow(gtk::Overflow::Hidden);
    let overlay = gtk::Overlay::new();

    let is_video = is_video_wallpaper(&path);
    match thumbnail_cache_path(&path) {
        Ok(thumbnail_path) if thumbnail_path.is_file() => {
            overlay.set_child(Some(&thumbnail_picture(&thumbnail_path)));
        }
        Ok(_) if is_video => {
            overlay.set_child(Some(&video_preview_placeholder(&path)));
            queue_thumbnail(&overlay, path.clone());
        }
        Ok(_) => {
            overlay.set_child(Some(&image_preview_placeholder()));
            queue_thumbnail(&overlay, path.clone());
        }
        Err(error) => {
            let kind = if is_video { "video" } else { "image" };
            warn!(
                path = %path.display(),
                thumbnail_kind = kind,
                %error,
                "failed to resolve wallpaper thumbnail cache path"
            );
            if is_video {
                overlay.set_child(Some(&video_preview_placeholder(&path)));
            } else {
                overlay.set_child(Some(&image_preview_placeholder()));
            }
        }
    }

    if is_video {
        let play_icon = gtk::Label::new(Some(ICON_VIDEO));
        play_icon.add_css_class("wallpaper-video-play-icon");
        play_icon.set_halign(gtk::Align::Start);
        play_icon.set_valign(gtk::Align::Start);
        play_icon.set_margin_start(8);
        play_icon.set_margin_top(6);
        play_icon.set_can_target(false);
        overlay.add_overlay(&play_icon);
    }

    button.set_child(Some(&overlay));
    visual.set_child(Some(&button));
    let slot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    slot.set_halign(gtk::Align::Start);
    slot.set_valign(gtk::Align::Start);
    slot.append(&visual);
    reorder.attach(wallpaper_key(&path), &slot, &visual, &button);
    let confirmation = attach_wallpaper_delete(&visual, controller, weak_notice, reorder, &path);

    {
        let mut live_cards = live_wallpaper_cards.borrow_mut();
        live_cards.retain(|_, weak| weak.upgrade().is_some());
        live_cards.insert(path.clone(), visual.downgrade());
    }

    let controller = Rc::clone(controller);
    let weak_notice = weak_notice.clone();
    let reorder = reorder.clone();
    button.connect_clicked(move |_| {
        if !reorder.activation_allowed() || confirmation.reveals_child() {
            return;
        }
        let Some(notice) = weak_notice.upgrade() else {
            return;
        };

        set_optional_label(&notice, None);
        let weak_notice = notice.downgrade();
        controller.request_apply(path.clone(), move |error| {
            if let Some(notice) = weak_notice.upgrade() {
                set_optional_label(&notice, Some(&error.to_string()));
            }
        });
    });

    slot
}

fn attach_wallpaper_delete(
    overlay: &gtk::Overlay,
    controller: &Rc<WallpaperController>,
    notice: &glib::WeakRef<gtk::Label>,
    reorder: &ReorderGrid,
    path: &Path,
) -> gtk::Revealer {
    let message = gtk::Box::new(gtk::Orientation::Vertical, 3);
    message.add_css_class("wallpaper-delete-confirmation");
    message.set_valign(gtk::Align::Fill);
    let title = gtk::Label::new(Some("Delete file?"));
    title.add_css_class("wallpaper-delete-title");
    title.set_vexpand(true);
    title.set_valign(gtk::Align::End);
    let hint = gtk::Label::new(Some("Click red corner again"));
    hint.add_css_class("wallpaper-delete-hint");
    hint.set_vexpand(true);
    hint.set_valign(gtk::Align::Start);
    message.append(&title);
    message.append(&hint);
    let confirmation = gtk::Revealer::new();
    confirmation.set_transition_type(gtk::RevealerTransitionType::Crossfade);
    confirmation.set_transition_duration(DELETE_FADE_DURATION.as_millis() as u32);
    confirmation.set_can_target(false);
    confirmation.set_child(Some(&message));
    overlay.add_overlay(&confirmation);

    let corner = gtk::Button::new();
    corner.add_css_class("wallpaper-delete-corner");
    corner.set_halign(gtk::Align::Start);
    corner.set_valign(gtk::Align::End);
    corner.set_bar_tooltip_text(Some("Delete wallpaper file"));
    corner.update_property(&[gtk::accessible::Property::Label("Delete wallpaper file")]);
    overlay.add_overlay(&corner);

    let deadline = Rc::new(Cell::new(None::<Instant>));
    let timeout = Rc::new(Timer::default());
    {
        let deadline = Rc::clone(&deadline);
        let timeout = Rc::clone(&timeout);
        let confirmation = confirmation.downgrade();
        corner.connect_unmap(move |_| {
            deadline.set(None);
            timeout.cancel();
            if let Some(confirmation) = confirmation.upgrade() {
                confirmation.set_reveal_child(false);
            }
        });
    }
    let weak_confirmation = confirmation.downgrade();
    let weak_overlay = overlay.downgrade();
    let controller = Rc::clone(controller);
    let notice = notice.clone();
    let reorder = reorder.clone();
    let path = path.to_path_buf();
    corner.connect_clicked(move |corner| {
        if !reorder.activation_allowed() {
            return;
        }
        let Some(confirmation) = weak_confirmation.upgrade() else {
            return;
        };
        timeout.cancel();
        if deadline.take().is_some_and(|at| Instant::now() < at) {
            confirmation.set_reveal_child(false);
            match controller.delete_wallpaper(&path) {
                Ok(()) => {
                    corner.set_sensitive(false);
                    if let Some(overlay) = weak_overlay.upgrade() {
                        overlay.add_css_class("wallpaper-removing");
                        if let Some(card) = overlay.parent() {
                            card.set_sensitive(false);
                        }
                    }
                    if let Some(notice) = notice.upgrade() {
                        set_optional_label(&notice, None);
                    }
                }
                Err(error) => {
                    if let Some(notice) = notice.upgrade() {
                        set_optional_label(&notice, Some(&error.to_string()));
                    }
                }
            }
            return;
        }
        deadline.set(Some(Instant::now() + DELETE_CONFIRM_DURATION));
        confirmation.set_reveal_child(true);
        let weak = confirmation.downgrade();
        let deadline = Rc::clone(&deadline);
        timeout.schedule(DELETE_CONFIRM_DURATION, move || {
            deadline.set(None);
            if let Some(confirmation) = weak.upgrade() {
                confirmation.set_reveal_child(false);
            }
        });
    });
    confirmation
}

fn thumbnail_picture(path: &Path) -> gtk::Picture {
    let picture = gtk::Picture::for_filename(path);
    picture.add_css_class("wallpaper-thumb");
    picture.set_content_fit(gtk::ContentFit::Cover);
    picture.set_can_shrink(true);
    picture.set_size_request(CARD_WIDTH, CARD_HEIGHT);
    picture.set_halign(gtk::Align::Start);
    picture.set_valign(gtk::Align::Start);
    picture.set_hexpand(false);
    picture.set_vexpand(false);
    picture
}

fn set_thumbnail_with_appear(overlay: &gtk::Overlay, path: &Path) {
    let stage = gtk::Box::new(gtk::Orientation::Vertical, 0);
    stage.add_css_class("wallpaper-thumbnail-stage");
    stage.add_css_class("wallpaper-thumbnail-pending");
    stage.set_size_request(CARD_WIDTH, CARD_HEIGHT);
    stage.set_halign(gtk::Align::Start);
    stage.set_valign(gtk::Align::Start);
    stage.append(&thumbnail_picture(path));
    overlay.set_child(Some(&stage));

    let weak_stage = stage.downgrade();
    glib::idle_add_local_once(move || {
        if let Some(stage) = weak_stage.upgrade() {
            stage.remove_css_class("wallpaper-thumbnail-pending");
            stage.add_css_class("wallpaper-thumbnail-enter");
        }
    });
}

fn video_preview_placeholder(path: &Path) -> gtk::Box {
    let placeholder = gtk::Box::new(gtk::Orientation::Vertical, 4);
    placeholder.add_css_class("wallpaper-video-placeholder");
    placeholder.set_size_request(CARD_WIDTH, CARD_HEIGHT);

    let filename = path
        .file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or("Video wallpaper");
    let label = gtk::Label::new(Some(filename));
    label.add_css_class("wallpaper-video-title");
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_max_width_chars(18);
    label.set_halign(gtk::Align::Center);
    label.set_valign(gtk::Align::Center);
    label.set_vexpand(true);
    placeholder.append(&label);
    placeholder
}

#[derive(Clone, Copy)]
enum BarFeature {
    Player,
    Workspace,
}

impl BarFeature {
    fn enabled(self, state: BarFeatureState) -> bool {
        match self {
            Self::Player => state.player_visible,
            Self::Workspace => state.workspace_visible,
        }
    }

    fn set(self, controller: &BarFeatureController, enabled: bool) -> bool {
        match self {
            Self::Player => controller.set_player_visible(enabled),
            Self::Workspace => controller.set_workspace_visible(enabled),
        }
    }
}

fn bar_feature_actions(
    controller: &Rc<BarFeatureController>,
    audio_spectrum: &Rc<AudioSpectrumController>,
    system_monitor: &Rc<SystemMonitorController>,
) -> gtk::Box {
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    actions.add_css_class("wallpaper-header-actions");
    actions.set_halign(gtk::Align::End);
    actions.set_valign(gtk::Align::Start);

    actions.append(&system_monitor_toggle(system_monitor));
    actions.append(&bar_feature_toggle(
        controller,
        BarFeature::Player,
        ICON_PLAYER_ENABLED,
        ICON_PLAYER_DISABLED,
    ));
    actions.append(&bar_feature_toggle(
        controller,
        BarFeature::Workspace,
        ICON_WORKSPACE,
        ICON_WORKSPACE,
    ));

    actions.append(&audio_spectrum_toggle(audio_spectrum));

    actions
}

fn system_monitor_toggle(controller: &Rc<SystemMonitorController>) -> gtk::ToggleButton {
    let button = gtk::ToggleButton::new();
    button.add_css_class("wallpaper-refresh-button");
    button.add_css_class("wallpaper-feature-button");
    button.set_focus_on_click(false);
    wallpaper_feature_tooltip(&button, "Show system monitor", "Hide system monitor");

    let icon = gtk::Label::new(Some(ICON_SYSTEM_MONITOR));
    icon.add_css_class("wallpaper-refresh-icon");
    button.set_child(Some(&icon));

    let syncing = Rc::new(Cell::new(false));
    button.set_active(controller.enabled());
    {
        let weak_button = button.downgrade();
        let syncing = Rc::clone(&syncing);
        controller.subscribe_state(move |enabled| {
            let Some(button) = weak_button.upgrade() else {
                return false;
            };
            syncing.set(true);
            button.set_active(enabled);
            syncing.set(false);
            true
        });
    }

    {
        let controller = Rc::clone(controller);
        let syncing = Rc::clone(&syncing);
        button.connect_toggled(move |button| {
            if syncing.get() {
                return;
            }
            let requested = button.is_active();
            if controller.set_enabled(requested) {
                return;
            }
            syncing.set(true);
            button.set_active(controller.enabled());
            syncing.set(false);
        });
    }

    button
}

fn audio_spectrum_toggle(controller: &Rc<AudioSpectrumController>) -> gtk::ToggleButton {
    let button = gtk::ToggleButton::new();
    button.add_css_class("wallpaper-refresh-button");
    button.add_css_class("wallpaper-feature-button");
    button.set_focus_on_click(false);
    wallpaper_feature_tooltip(&button, "Show equalizer", "Hide equalizer");

    let icon = gtk::Label::new(Some(ICON_EQUALIZER));
    icon.add_css_class("wallpaper-refresh-icon");
    button.set_child(Some(&icon));

    let syncing = Rc::new(Cell::new(false));
    {
        let weak_button = button.downgrade();
        let syncing = Rc::clone(&syncing);
        controller.subscribe_state(move |enabled| {
            let Some(button) = weak_button.upgrade() else {
                return false;
            };
            syncing.set(true);
            button.set_active(enabled);
            syncing.set(false);
            true
        });
    }

    {
        let controller = Rc::clone(controller);
        let syncing = Rc::clone(&syncing);
        button.connect_toggled(move |button| {
            if syncing.get() {
                return;
            }
            let requested = button.is_active();
            if !controller.set_enabled(requested) {
                syncing.set(true);
                button.set_active(controller.enabled());
                syncing.set(false);
            }
        });
    }

    button
}

fn bar_feature_toggle(
    controller: &Rc<BarFeatureController>,
    feature: BarFeature,
    enabled_icon: &'static str,
    disabled_icon: &'static str,
) -> gtk::ToggleButton {
    let button = gtk::ToggleButton::new();
    button.add_css_class("wallpaper-refresh-button");
    button.add_css_class("wallpaper-feature-button");
    button.set_focus_on_click(false);
    let (show, hide) = match feature {
        BarFeature::Player => ("Show player", "Hide player"),
        BarFeature::Workspace => ("Show workspaces", "Hide workspaces"),
    };
    wallpaper_feature_tooltip(&button, show, hide);

    let icon = gtk::Label::new(None);
    icon.add_css_class("wallpaper-refresh-icon");
    button.set_child(Some(&icon));

    let syncing = Rc::new(Cell::new(false));
    let apply_state = {
        let weak_button = button.downgrade();
        let weak_icon = icon.downgrade();
        let syncing = Rc::clone(&syncing);
        move |state: BarFeatureState| {
            let (Some(button), Some(icon)) = (weak_button.upgrade(), weak_icon.upgrade()) else {
                return false;
            };
            let enabled = feature.enabled(state);
            syncing.set(true);
            button.set_active(enabled);
            icon.set_label(if enabled { enabled_icon } else { disabled_icon });
            syncing.set(false);
            true
        }
    };
    controller.subscribe(apply_state);

    let controller = Rc::clone(controller);
    button.connect_toggled(move |button| {
        if syncing.get() {
            return;
        }

        let requested = button.is_active();
        if feature.set(&controller, requested) {
            return;
        }

        let saved = feature.enabled(controller.state());
        syncing.set(true);
        button.set_active(saved);
        if let Some(icon) = button
            .child()
            .and_then(|child| child.downcast::<gtk::Label>().ok())
        {
            icon.set_label(if saved { enabled_icon } else { disabled_icon });
        }
        syncing.set(false);
    });

    button
}

fn wallpaper_feature_tooltip(button: &gtk::ToggleButton, show: &'static str, hide: &'static str) {
    let update = move |button: &gtk::ToggleButton| {
        button.set_bar_tooltip_text(Some(if button.is_active() { hide } else { show }));
    };
    update(button);
    button.connect_active_notify(update);
}

fn random_menu_button(controller: &Rc<WallpaperController>) -> gtk::MenuButton {
    let button = gtk::MenuButton::new();
    button.add_css_class("wallpaper-random-button");
    button.set_valign(gtk::Align::Center);

    let icon = gtk::Label::new(Some(ICON_SHUFFLE));
    icon.add_css_class("wallpaper-refresh-icon");
    button.set_child(Some(&icon));

    let popover = gtk::Popover::new();
    popover.add_css_class("wallpaper-random-popover");
    popover.set_has_arrow(false);
    popover.set_autohide(true);
    popover.set_position(gtk::PositionType::Bottom);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 4);
    content.add_css_class("wallpaper-random-panel");

    let title = gtk::Label::new(Some("Random wallpapers"));
    title.add_css_class("wallpaper-random-title");
    title.set_xalign(0.0);

    let now_button = gtk::Button::new();
    now_button.add_css_class("wallpaper-random-now");
    let now_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let now_icon = gtk::Label::new(Some(ICON_SHUFFLE));
    now_icon.add_css_class("wallpaper-random-action-icon");
    let now_label = gtk::Label::new(Some("Change now"));
    now_label.set_xalign(0.0);
    now_row.append(&now_icon);
    now_row.append(&now_label);
    now_button.set_child(Some(&now_row));

    let divider = gtk::Separator::new(gtk::Orientation::Horizontal);
    divider.add_css_class("wallpaper-random-divider");

    let (enabled_value, interval_value) = controller.random_config();
    let enabled = gtk::ToggleButton::new();
    enabled.add_css_class("wallpaper-random-toggle");
    enabled.set_hexpand(true);

    let enabled_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let enabled_label = gtk::Label::new(Some("Change automatically"));
    enabled_label.add_css_class("wallpaper-random-toggle-label");
    enabled_label.set_xalign(0.0);
    enabled_label.set_hexpand(true);

    let enabled_status = gtk::Label::new(Some(if enabled_value { "On" } else { "Off" }));
    enabled_status.add_css_class("wallpaper-random-toggle-state");
    enabled_row.append(&enabled_label);
    enabled_row.append(&enabled_status);
    enabled.set_child(Some(&enabled_row));
    enabled.set_active(enabled_value);

    let interval_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    interval_row.add_css_class("wallpaper-random-interval-row");
    let every_label = gtk::Label::new(Some("Every"));
    every_label.set_xalign(0.0);
    every_label.set_hexpand(true);

    let interval = gtk::SpinButton::with_range(
        f64::from(MIN_RANDOM_INTERVAL_MINUTES),
        f64::from(MAX_RANDOM_INTERVAL_MINUTES),
        1.0,
    );
    interval.add_css_class("wallpaper-random-interval");
    interval.set_numeric(true);
    interval.set_width_chars(3);
    interval.set_max_width_chars(4);
    interval.set_value(f64::from(interval_value));
    interval.set_sensitive(enabled_value);
    let syncing = Rc::new(Cell::new(false));

    let minutes_label = gtk::Label::new(Some("min"));
    minutes_label.add_css_class("wallpaper-random-unit");
    interval_row.append(&every_label);
    interval_row.append(&interval);
    interval_row.append(&minutes_label);

    content.append(&title);
    content.append(&now_button);
    content.append(&divider);
    content.append(&enabled);
    content.append(&interval_row);
    popover.set_child(Some(&content));
    button.set_popover(Some(&popover));

    {
        let right_click = gtk::GestureClick::new();
        right_click.set_button(gdk::BUTTON_SECONDARY);
        right_click.set_propagation_phase(gtk::PropagationPhase::Capture);
        let controller = Rc::clone(controller);
        let weak_popover = popover.downgrade();
        right_click.connect_pressed(move |gesture, _, _, _| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            if let Some(popover) = weak_popover.upgrade() {
                popover.popdown();
            }
            if let Err(error) = controller.apply_random_wallpaper() {
                warn!(%error, "failed to apply random wallpaper");
            }
        });
        button.add_controller(right_click);
    }

    {
        let controller = Rc::clone(controller);
        let weak_enabled = enabled.downgrade();
        let weak_interval = interval.downgrade();
        let syncing = Rc::clone(&syncing);
        popover.connect_visible_notify(move |popover| {
            if !popover.is_visible() {
                return;
            }
            let (Some(enabled), Some(interval)) = (weak_enabled.upgrade(), weak_interval.upgrade())
            else {
                return;
            };

            let (saved_enabled, saved_interval) = controller.random_config();
            syncing.set(true);
            enabled.set_active(saved_enabled);
            interval.set_value(f64::from(saved_interval));
            interval.set_sensitive(saved_enabled);
            syncing.set(false);
        });
    }

    {
        let controller = Rc::clone(controller);
        now_button.connect_clicked(move |_| {
            if let Err(error) = controller.apply_random_wallpaper() {
                warn!(%error, "failed to apply random wallpaper");
            }
        });
    }

    {
        let weak_status = enabled_status.downgrade();
        enabled.connect_toggled(move |toggle| {
            if let Some(status) = weak_status.upgrade() {
                status.set_text(if toggle.is_active() { "On" } else { "Off" });
            }
        });
    }

    {
        let controller = Rc::clone(controller);
        let weak_interval = interval.downgrade();
        let reverting = Rc::new(Cell::new(false));
        let syncing = Rc::clone(&syncing);
        enabled.connect_toggled(move |toggle| {
            if syncing.get() || reverting.replace(false) {
                return;
            }

            let requested = toggle.is_active();
            if let Some(interval) = weak_interval.upgrade() {
                interval.set_sensitive(requested);
            }
            if let Err(error) = controller.set_random_enabled(requested) {
                warn!(%error, "failed to save random wallpaper setting");
                let (saved, _) = controller.random_config();
                if let Some(interval) = weak_interval.upgrade() {
                    interval.set_sensitive(saved);
                }
                reverting.set(true);
                toggle.set_active(saved);
            }
        });
    }

    {
        let controller = Rc::clone(controller);
        let reverting = Rc::new(Cell::new(false));
        let syncing = Rc::clone(&syncing);
        interval.connect_value_changed(move |spin| {
            if syncing.get() || reverting.replace(false) {
                return;
            }

            let minutes = u32::try_from(spin.value_as_int())
                .unwrap_or(MIN_RANDOM_INTERVAL_MINUTES)
                .max(MIN_RANDOM_INTERVAL_MINUTES);
            if let Err(error) = controller.set_random_interval_minutes(minutes) {
                warn!(%error, "failed to save random wallpaper interval");
                let (_, saved) = controller.random_config();
                reverting.set(true);
                spin.set_value(f64::from(saved));
            }
        });
    }

    button
}

fn wallpaper_refresh_button(icon_text: &str) -> (gtk::Button, gtk::Label, gtk::Spinner) {
    let icon = gtk::Label::new(Some(icon_text));
    icon.add_css_class("wallpaper-refresh-icon");

    let spinner = gtk::Spinner::new();
    spinner.add_css_class("wallpaper-refresh-spinner");
    spinner.set_halign(gtk::Align::Center);
    spinner.set_valign(gtk::Align::Center);
    spinner.set_visible(false);

    let indicator = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    indicator.add_css_class("wallpaper-refresh-indicator");
    indicator.set_halign(gtk::Align::Center);
    indicator.set_valign(gtk::Align::Center);
    indicator.append(&icon);
    indicator.append(&spinner);

    let button = gtk::Button::new();
    button.add_css_class("wallpaper-refresh-button");
    button.set_focus_on_click(false);
    button.set_valign(gtk::Align::Center);
    button.set_child(Some(&indicator));

    (button, icon, spinner)
}

fn header_button(icon: &str) -> gtk::Button {
    let button = gtk::Button::new();
    button.add_css_class("wallpaper-refresh-button");
    button.set_focus_on_click(false);
    button.set_valign(gtk::Align::Center);

    let label = gtk::Label::new(Some(icon));
    label.add_css_class("wallpaper-refresh-icon");
    button.set_child(Some(&label));
    button
}

fn image_preview_placeholder() -> gtk::Box {
    let placeholder = gtk::Box::new(gtk::Orientation::Vertical, 0);
    placeholder.add_css_class("wallpaper-image-placeholder");
    placeholder.set_size_request(CARD_WIDTH, CARD_HEIGHT);
    placeholder.set_halign(gtk::Align::Start);
    placeholder.set_valign(gtk::Align::Start);
    placeholder.set_hexpand(false);
    placeholder.set_vexpand(false);

    let icon = gtk::Label::new(Some(ICON_WALLPAPER));
    icon.add_css_class("wallpaper-image-placeholder-icon");
    icon.set_halign(gtk::Align::Center);
    icon.set_valign(gtk::Align::Center);
    icon.set_hexpand(true);
    icon.set_vexpand(true);
    placeholder.append(&icon);
    placeholder
}

fn queue_thumbnail(overlay: &gtk::Overlay, source: PathBuf) {
    let receiver = request_thumbnail(source);
    let receiver_on_destroy = receiver.clone();
    overlay.connect_destroy(move |_| {
        receiver_on_destroy.close();
    });
    let weak_overlay = overlay.downgrade();
    glib::MainContext::default().spawn_local(async move {
        let Ok(result) = receiver.recv().await else {
            return;
        };
        let Some(overlay) = weak_overlay.upgrade() else {
            return;
        };
        match result {
            Ok(path) => set_thumbnail_with_appear(&overlay, &path),
            Err(error) => warn!(%error, "failed to build wallpaper thumbnail"),
        }
    });
}

#[cfg(test)]
#[path = "picker/tests.rs"]
mod reorder_tests;
