use super::*;
use crate::widgets::test_support::{
    capture_if_requested, drag_pointer, drag_pointer_with_wheel, pump, widgets_with_class,
};
use std::fs;

#[test]
#[ignore = "requires GTK layer-shell and XDG_STATE_HOME/XDG_CACHE_HOME under /tmp; run alone"]
fn gallery_loads_thumbnails_after_scrolling_reuses_cards_and_releases_widgets() {
    assert!(glib::user_state_dir().starts_with("/tmp"));
    assert!(glib::user_cache_dir().starts_with("/tmp"));
    gtk::init().unwrap();
    let app = gtk::Application::builder()
        .application_id("dev.obsidian.GalleryThumbnailTest")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.register(gio::Cancellable::NONE).unwrap();
    let css = gtk::CssProvider::new();
    css.load_from_data(include_str!("../../../../assets/window.css"));
    gtk::style_context_add_provider_for_display(
        &gdk::Display::default().unwrap(),
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    let directory = std::env::temp_dir().join(format!(
        "obsidian-gallery-thumbnails-{}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).unwrap();
    let pixbuf = gdk_pixbuf::Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, false, 8, 288, 168).unwrap();
    let paths = (0..90)
        .map(|index| {
            let path = directory.join(format!("{index:03}.png"));
            pixbuf.fill(0x234578ff_u32.wrapping_add(index * 0x02010000));
            pixbuf.savev(&path, "png", &[]).unwrap();
            path
        })
        .collect::<Vec<_>>();
    let controller = WallpaperController::new();
    controller.settings.borrow_mut().directory = directory.clone();
    controller.settings.borrow_mut().current = Some(paths[0].clone());
    let content = build_picker_content(
        &controller,
        &BarFeatureController::new(),
        &AudioSpectrumController::new(),
        &SystemMonitorController::new(),
    );
    content.add_css_class("wallpaper-picker-surface");
    let window = gtk::ApplicationWindow::builder()
        .application(&app)
        .child(&content)
        .build();
    window.init_layer_shell();
    window.add_css_class("wallpaper-picker-window");
    window.set_namespace(Some("obsidian-bar-wallpaper-gallery-test"));
    window.set_layer(Layer::Overlay);
    window.set_exclusive_zone(-1);
    window.set_anchor(Edge::Left, true);
    window.set_anchor(Edge::Top, true);
    window.present();
    let wait_for_thumbnails = || {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            pump(20);
            let cards = widgets_with_class(content.upcast_ref(), "wallpaper-card");
            if !cards.is_empty()
                && widgets_with_class(content.upcast_ref(), "wallpaper-thumb").len() == cards.len()
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "bound cards did not receive thumbnails"
            );
        }
        pump(200);
    };
    wait_for_thumbnails();
    assert_eq!(
        widgets_with_class(content.upcast_ref(), "wallpaper-thumb-wrap-active").len(),
        1
    );
    let scroller = widgets_with_class(content.upcast_ref(), "wallpaper-list-wrap")[0]
        .clone()
        .downcast::<gtk::ScrolledWindow>()
        .unwrap();
    for _ in 0..3 {
        let adjustment = scroller.vadjustment();
        adjustment.set_value(adjustment.upper() - adjustment.page_size());
        pump(25);
        adjustment.set_value(0.0);
        pump(25);
    }
    let adjustment = scroller.vadjustment();
    adjustment.set_value(adjustment.upper() - adjustment.page_size());
    wait_for_thumbnails();
    let original = widgets_with_class(content.upcast_ref(), "wallpaper-card")
        .last()
        .unwrap()
        .clone();
    let weak_card = original.downgrade();
    controller.settings.borrow_mut().current = paths.last().cloned();
    controller.broadcast();
    pump(80);
    assert!(
        original
            .parent()
            .unwrap()
            .has_css_class("wallpaper-thumb-wrap-active")
    );
    let mut order = paths
        .iter()
        .map(|path| wallpaper_key(path))
        .collect::<Vec<_>>();
    let last = order.len() - 1;
    order.swap(last, last - 1);
    controller.set_gallery_order(order).unwrap();
    pump(300);
    assert!(widgets_with_class(content.upcast_ref(), "wallpaper-card").contains(&original));
    assert!(
        original
            .parent()
            .unwrap()
            .has_css_class("wallpaper-thumb-wrap-active")
    );
    capture_if_requested(&window, "gallery-loaded-thumbnails");
    let count = widgets_with_class(content.upcast_ref(), "wallpaper-count")[0]
        .clone()
        .downcast::<gtk::Label>()
        .unwrap();
    let thumbnail = original.first_child();
    let scroll = adjustment.value();
    controller.delete_wallpaper(&paths[85]).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while count.text() != "89" {
        assert!(
            Instant::now() < deadline,
            "gallery deletion refresh stalled"
        );
        pump(20);
    }
    pump(250);
    assert!(!paths[85].exists());
    assert!(widgets_with_class(content.upcast_ref(), "wallpaper-card").contains(&original));
    assert_eq!(
        original.first_child(),
        thumbnail,
        "deletion must retain loaded thumbnails"
    );
    assert!(
        original
            .parent()
            .unwrap()
            .has_css_class("wallpaper-thumb-wrap-active")
    );
    assert!(
        (adjustment.value() - scroll.min(adjustment.upper() - adjustment.page_size())).abs() < 2.0,
        "deletion must preserve the scroll position within its new limits"
    );
    drop(thumbnail);
    drop(count);
    // Close during the two-second refresh indication. The pending callback
    // must not keep the gallery model or its loaded thumbnail cards alive.
    let spinner = widgets_with_class(content.upcast_ref(), "wallpaper-refresh-spinner")[0].clone();
    let refresh = spinner
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .downcast::<gtk::Button>()
        .unwrap();
    refresh.emit_clicked();
    pump(250);
    assert!(!refresh.is_sensitive());
    let gallery = widgets_with_class(content.upcast_ref(), "wallpaper-grid-rows")[0]
        .clone()
        .downcast::<gtk::ListView>()
        .unwrap();
    let model = gallery
        .model()
        .unwrap()
        .downcast::<gtk::NoSelection>()
        .unwrap()
        .model()
        .unwrap();
    let weak_model = model.downgrade();
    drop(model);
    drop(gallery);
    drop(refresh);
    drop(spinner);
    drop(original);
    drop(scroller);
    drop(adjustment);
    window.set_child(None::<&gtk::Widget>);
    window.destroy();
    drop(content);
    drop(window);
    pump(150);
    assert!(
        weak_card.upgrade().is_none(),
        "closing the picker retained a thumbnail card"
    );
    assert!(
        weak_model.upgrade().is_none(),
        "refresh indication retained the gallery model"
    );
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn wallpaper_delete_ui_helper() {
    let Some(root) = std::env::var_os("OBSIDIAN_WALLPAPER_DELETE_UI_DIR").map(PathBuf::from) else {
        return;
    };
    gtk::init().unwrap();
    let app = gtk::Application::builder()
        .application_id("dev.obsidian.WallpaperDeleteTest")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.register(None::<&gio::Cancellable>).unwrap();
    let css = gtk::CssProvider::new();
    css.connect_parsing_error(|_, _, error| panic!("CSS error: {error}"));
    css.load_from_data(include_str!("../../../../assets/window.css"));
    gtk::style_context_add_provider_for_display(
        &gdk::Display::default().unwrap(),
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    let directory = root.join("wallpapers");
    fs::create_dir_all(&directory).unwrap();
    let paths = (0..8)
        .map(|i| directory.join(format!("{i}.png")))
        .collect::<Vec<_>>();
    let pixbuf = gdk_pixbuf::Pixbuf::new(
        gdk_pixbuf::Colorspace::Rgb,
        false,
        8,
        CARD_WIDTH,
        CARD_HEIGHT,
    )
    .unwrap();
    for (i, path) in paths.iter().enumerate() {
        pixbuf.fill(0x234578ff + (i as u32 * 0x12050000));
        pixbuf.savev(path, "png", &[]).unwrap();
    }
    let controller = WallpaperController::new();
    controller.settings.borrow_mut().directory = directory;
    controller.settings.borrow_mut().current = Some(paths[0].clone());
    let content = build_picker_content(
        &controller,
        &BarFeatureController::new(),
        &AudioSpectrumController::new(),
        &SystemMonitorController::new(),
    );
    let window = gtk::ApplicationWindow::builder()
        .application(&app)
        .child(&content)
        .build();
    window.init_layer_shell();
    window.set_namespace(Some("obsidian-wallpaper-delete-test"));
    window.set_layer(Layer::Overlay);
    window.set_exclusive_zone(-1);
    window.set_margin(Edge::Top, 100);
    window.set_margin(Edge::Left, 100);
    window.set_anchor(Edge::Top, true);
    window.set_anchor(Edge::Left, true);
    window.set_keyboard_mode(KeyboardMode::None);
    window.present();
    pump(700);

    fn descendants(widget: &gtk::Widget, class: &str) -> Vec<gtk::Widget> {
        let mut found = Vec::new();
        if widget.has_css_class(class) {
            found.push(widget.clone());
        }
        let mut child = widget.first_child();
        while let Some(current) = child {
            found.extend(descendants(&current, class));
            child = current.next_sibling();
        }
        found
    }
    let cards = || descendants(content.upcast_ref(), "wallpaper-card");
    let card = cards()[0].clone().downcast::<gtk::Button>().unwrap();
    let retained = cards()[1].clone();
    let overlay = card.parent().unwrap().downcast::<gtk::Overlay>().unwrap();
    let corner = overlay
        .last_child()
        .unwrap()
        .downcast::<gtk::Button>()
        .unwrap();
    let confirmation = corner
        .prev_sibling()
        .unwrap()
        .downcast::<gtk::Revealer>()
        .unwrap();
    let bounds = corner.compute_bounds(&window).unwrap();
    let position = (bounds.x() + 107.0, bounds.y() + bounds.height() + 93.0);
    let activations = Rc::new(Cell::new(0));
    let clicked = Rc::clone(&activations);
    card.connect_clicked(move |_| clicked.set(clicked.get() + 1));
    let corner_clicks = Rc::new(Cell::new(0));
    let corner_clicked = Rc::clone(&corner_clicks);
    corner.connect_clicked(move |_| {
        corner_clicked.set(corner_clicked.get() + 1);
    });
    capture_if_requested(&window, "wallpaper-delete-corner");
    drag_pointer(position, position);
    capture_if_requested(&window, "wallpaper-delete-after-click");
    assert!(
        confirmation.reveals_child(),
        "native corner click must show confirmation: bounds={bounds:?}, pointer={position:?}, flags={:?}, card clicks={}, corner clicks={}, file exists={}",
        corner.state_flags(),
        activations.get(),
        corner_clicks.get(),
        paths[0].exists()
    );
    assert!(paths[0].exists());
    assert_eq!(
        corner_clicks.get(),
        1,
        "one press must request confirmation only"
    );
    assert_eq!(
        activations.get(),
        0,
        "corner click must not activate the card"
    );
    assert_eq!(
        controller.selection_generation.current(),
        0,
        "corner click must not apply wallpaper"
    );
    capture_if_requested(&window, "wallpaper-delete-confirmation");
    pump(3300);
    assert!(!confirmation.reveals_child());
    assert!(!confirmation.is_child_revealed());
    assert!(paths[0].exists(), "expiration must never delete a file");

    corner.emit_clicked();
    pump(220);
    assert!(
        confirmation.reveals_child(),
        "click after expiry must ask again"
    );
    card.emit_clicked();
    assert_eq!(controller.selection_generation.current(), 0);
    corner.emit_clicked();
    assert!(
        !paths[0].exists(),
        "second corner click must unlink the file"
    );
    assert!(overlay.has_css_class("wallpaper-removing"));
    assert!(!card.is_sensitive());
    pump(650);
    assert_eq!(cards().len(), 7);
    assert!(
        cards().contains(&retained),
        "surviving cards must be reused"
    );
    assert_eq!(controller.settings.borrow().current, None);
    assert_eq!(controller.selection_generation.current(), 1);
    capture_if_requested(&window, "wallpaper-deleted");

    let corner = descendants(&retained.parent().unwrap(), "wallpaper-delete-corner")[0]
        .clone()
        .downcast::<gtk::Button>()
        .unwrap();
    let confirmation = corner
        .prev_sibling()
        .unwrap()
        .downcast::<gtk::Revealer>()
        .unwrap();
    corner.emit_clicked();
    pump(180);
    window.set_visible(false);
    pump(180);
    assert!(!confirmation.reveals_child());
    window.present();
    pump(250);
    corner.emit_clicked();
    assert!(
        paths[1].exists(),
        "closing the picker must disarm confirmation"
    );
    assert!(confirmation.reveals_child());
    corner.emit_clicked();
    pump(350);
    assert!(!paths[1].exists());
    assert_eq!(cards().len(), 6);

    // An I/O error leaves the card available and exposes the failure.
    fs::remove_file(&paths[2]).unwrap();
    fs::create_dir(&paths[2]).unwrap();
    let corner = descendants(&cards()[0].parent().unwrap(), "wallpaper-delete-corner")[0]
        .clone()
        .downcast::<gtk::Button>()
        .unwrap();
    corner.emit_clicked();
    corner.emit_clicked();
    assert!(paths[2].is_dir());
    assert_eq!(cards().len(), 6);
    let notices = descendants(content.upcast_ref(), "wallpaper-notice");
    assert!(notices.iter().any(|notice| {
        notice.is_visible()
            && notice
                .clone()
                .downcast::<gtk::Label>()
                .unwrap()
                .text()
                .contains("filesystem error")
    }));
    fs::remove_dir(&paths[2]).unwrap();
    pixbuf.savev(&paths[2], "png", &[]).unwrap();
    for remaining in (1..=6).rev() {
        let card = cards()[0].clone();
        let corner = descendants(&card.parent().unwrap(), "wallpaper-delete-corner")[0]
            .clone()
            .downcast::<gtk::Button>()
            .unwrap();
        corner.emit_clicked();
        corner.emit_clicked();
        pump(350);
        assert_eq!(cards().len(), remaining - 1);
    }
    assert!(paths.iter().all(|path| !path.exists()));
    assert!(descendants(content.upcast_ref(), "wallpaper-empty")[0].is_visible());
    assert_eq!(
        descendants(content.upcast_ref(), "wallpaper-count")[0]
            .clone()
            .downcast::<gtk::Label>()
            .unwrap()
            .text(),
        "0"
    );
    window.close();
    pump(100);
}

#[test]
#[ignore = "moves the pointer; requires a single Niri output and /dev/uinput access; run alone"]
fn wallpaper_delete_corner_confirms_expires_and_preserves_gallery() {
    let root = std::env::temp_dir().join(format!(
        "obsidian-wallpaper-delete-ui-{}",
        std::process::id()
    ));
    fs::create_dir_all(&root).unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "widgets::wallpaper::picker::reorder_tests::wallpaper_delete_ui_helper",
            "--nocapture",
        ])
        .env("OBSIDIAN_WALLPAPER_DELETE_UI_DIR", &root)
        .env("XDG_STATE_HOME", &root)
        .env("XDG_CACHE_HOME", root.join("cache"))
        .output()
        .unwrap();
    fs::remove_dir_all(root).unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
#[ignore = "moves the pointer; requires a single Niri output and /dev/uinput access; run alone"]
fn virtualized_gallery_reorders_and_preserves_scroll_and_selection() {
    gtk::init().unwrap();
    let app = gtk::Application::builder()
        .application_id("dev.obsidian.GalleryReorderTest")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.register(None::<&gio::Cancellable>).unwrap();
    let css = gtk::CssProvider::new();
    css.load_from_data(include_str!("../../../../assets/window.css"));
    gtk::style_context_add_provider_for_display(
        &gdk::Display::default().unwrap(),
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    let controller = WallpaperController::new();
    let directory = PathBuf::from("/tmp/obsidian-gallery-reorder-fixture");
    let paths = (0..90)
        .map(|index| directory.join(format!("{index}.png")))
        .collect::<Vec<_>>();
    controller.settings.borrow_mut().directory = directory.clone();
    controller.settings.borrow_mut().current = Some(paths[0].clone());
    let page = build_gallery_page(
        &controller,
        &BarFeatureController::new(),
        &AudioSpectrumController::new(),
        &SystemMonitorController::new(),
    );
    let view = Rc::new(GalleryView {
        model: page.model.clone(),
        reorder: page.reorder.clone(),
        active_wallpaper: Rc::clone(&page.active_wallpaper),
        live_wallpaper_cards: Rc::clone(&page.live_wallpaper_cards),
        reuse: page.reuse.clone(),
        scroller: page.scroller.downgrade(),
        empty: page.empty.downgrade(),
        count: page.count.downgrade(),
        notice: page.notice.downgrade(),
        rendered: Rc::new(RefCell::new(RenderedGallery {
            revision: 0,
            directory: directory.clone(),
            current: Some(paths[0].clone()),
            loaded: true,
            order: Vec::new(),
        })),
        refresh_generation: Rc::new(Generation::default()),
        scan_requests: ScanRequests::default(),
        refresh_animation_timer: Rc::new(Timer::default()),
        refresh_button: page.refresh_button.downgrade(),
        refresh_icon: page.refresh_icon.downgrade(),
        refresh_spinner: page.refresh_spinner.downgrade(),
    });
    let saved = Rc::new(RefCell::new(Vec::new()));
    let saved_order = Rc::clone(&saved);
    let weak_view = Rc::downgrade(&view);
    let current = paths[0].clone();
    page.reorder.on_commit(move |order| {
        saved_order.replace(order.clone());
        weak_view
            .upgrade()
            .unwrap()
            .apply_snapshot(WallpaperSnapshot {
                gallery_revision: 0,
                directory: directory.clone(),
                current: Some(current.clone()),
                gallery_order: order,
            });
        true
    });
    page.reorder
        .set_order(paths.iter().map(|path| wallpaper_key(path)).collect());
    render_gallery_items(
        &page.model,
        &page.active_wallpaper,
        GalleryRenderWidgets {
            scroller: &page.scroller,
            empty: &page.empty,
            count: &page.count,
            notice: &page.notice,
        },
        Some(paths[0].clone()),
        paths.clone(),
    );
    let window = gtk::ApplicationWindow::builder()
        .application(&app)
        .child(&page.root)
        .build();
    window.init_layer_shell();
    window.set_namespace(Some("obsidian-gallery-reorder-test"));
    window.set_layer(Layer::Overlay);
    window.set_exclusive_zone(-1);
    window.set_anchor(Edge::Left, true);
    window.set_anchor(Edge::Top, true);
    window.set_margin(Edge::Left, 100);
    window.set_margin(Edge::Top, 100);
    window.present();
    pump(350);
    page.scroller.vadjustment().set_value(900.0);
    pump(300);
    let scroll = page.scroller.vadjustment().value();
    let bounds = |index: usize| {
        page.live_wallpaper_cards.borrow()[&paths[index]]
            .upgrade()
            .unwrap()
            .parent()
            .unwrap()
            .compute_bounds(&window)
            .unwrap()
    };
    let source = bounds(40);
    let target = bounds(33);
    let original_cards = paths[33..42]
        .iter()
        .map(|path| {
            let card = page.live_wallpaper_cards.borrow()[path].upgrade().unwrap();
            (path.clone(), card.clone(), card.child())
        })
        .collect::<Vec<_>>();
    let untouched_row = page.model.item(0).unwrap();
    let changed_rows = Rc::new(RefCell::new(Vec::new()));
    let changes = Rc::clone(&changed_rows);
    page.model
        .connect_items_changed(move |_, position, removed, added| {
            changes.borrow_mut().push((position, removed, added));
        });
    drag_pointer(
        (
            100.0 + source.x() + source.width() / 2.0,
            100.0 + source.y() + source.height() / 2.0,
        ),
        (
            100.0 + target.x() + target.width() / 2.0,
            100.0 + target.y() + target.height() / 2.0,
        ),
    );
    let expected = crate::widgets::reorder::move_item(&paths, &paths[40], 33).unwrap();
    assert_eq!(gallery_paths(&page.model), expected);
    assert_eq!(
        page.model.item(0).unwrap(),
        untouched_row,
        "unaffected rows must stay intact"
    );
    assert_eq!(
        *changed_rows.borrow(),
        [(11, 3, 3)],
        "only the moved range must be rebound"
    );
    for (path, original, picture) in original_cards {
        let current = page.live_wallpaper_cards.borrow()[&path].upgrade().unwrap();
        assert_eq!(current, original, "reordering must retain card widgets");
        assert_eq!(
            current.child(),
            picture,
            "reordering must retain loaded thumbnails"
        );
        assert!(
            !current
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .has_css_class("wallpaper-grid-row-enter")
        );
    }
    assert_eq!(
        *saved.borrow(),
        expected
            .iter()
            .map(|path| wallpaper_key(path))
            .collect::<Vec<_>>()
    );
    assert!(
        (page.scroller.vadjustment().value() - scroll).abs() < 2.0,
        "reordering must preserve scroll"
    );
    assert_eq!(*page.active_wallpaper.borrow(), Some(paths[0].clone()));
    assert!(
        !controller.applying.get(),
        "drop must not apply a wallpaper"
    );
    capture_if_requested(&window, "gallery-reordered-scrolled");
    let source = bounds(40);
    let viewport = page.scroller.compute_bounds(&window).unwrap();
    let weak_window = window.downgrade();
    glib::timeout_add_local_once(Duration::from_millis(2000), move || {
        if let Some(window) = weak_window.upgrade() {
            capture_if_requested(&window, "gallery-drag-wheel");
        }
    });
    drag_pointer_with_wheel(
        (
            100.0 + source.x() + source.width() / 2.0,
            100.0 + source.y() + source.height() / 2.0,
        ),
        (
            100.0 + viewport.x() + viewport.width() / 2.0,
            100.0 + viewport.y() + viewport.height() / 2.0,
        ),
        -12,
    );
    assert!(
        page.scroller.vadjustment().value() > scroll + 700.0,
        "wheel must scroll far enough to recycle the source row: {} from {scroll}",
        page.scroller.vadjustment().value()
    );
    let moved_index = gallery_paths(&page.model)
        .iter()
        .position(|path| path == &paths[40])
        .unwrap();
    assert!(
        moved_index > 60,
        "the drop must follow the newly scrolled destination, got {moved_index}"
    );
    assert!(!controller.applying.get());
    // Move the last card all the way to the first row. Keep only weak
    // references: a strong test reference would conceal premature disposal.
    let adjustment = page.scroller.vadjustment();
    adjustment.set_value(adjustment.upper() - adjustment.page_size());
    pump(300);
    let last_path = paths.last().unwrap();
    let (original_card, original_picture) = {
        let visual = page.live_wallpaper_cards.borrow()[last_path]
            .upgrade()
            .unwrap();
        let texture = gdk::MemoryTexture::new(
            1,
            1,
            gdk::MemoryFormat::R8g8b8,
            &glib::Bytes::from_owned([231u8, 101, 77]),
            3,
        );
        let picture = gtk::Picture::for_paintable(&texture);
        picture.set_size_request(CARD_WIDTH, CARD_HEIGHT);
        visual
            .child()
            .and_downcast::<gtk::Button>()
            .unwrap()
            .child()
            .and_downcast::<gtk::Overlay>()
            .unwrap()
            .set_child(Some(&picture));
        (visual.downgrade(), picture.downgrade())
    };
    pump(50);
    let source = bounds(89);
    let floating_layer = page
        .scroller
        .parent()
        .unwrap()
        .downcast::<gtk::Overlay>()
        .unwrap()
        .last_child()
        .unwrap();
    let missing_frames = Rc::new(Cell::new(0));
    let gaps = Rc::clone(&missing_frames);
    let original = original_card.clone();
    let watch = page.scroller.add_tick_callback(move |scroller, _| {
        let floating = floating_layer
            .first_child()
            .is_some_and(|child| child.is_mapped());
        let in_grid = original.upgrade().is_some_and(|visual| {
            visual.is_mapped()
                && visual.compute_bounds(scroller).is_some_and(|bounds| {
                    bounds.y() + bounds.height() / 2.0 >= 0.0
                        && bounds.y() + bounds.height() / 2.0 < scroller.height() as f32
                })
        });
        if !floating && !in_grid {
            gaps.set(gaps.get() + 1);
        }
        glib::ControlFlow::Continue
    });
    drag_pointer_with_wheel(
        (
            100.0 + source.x() + source.width() / 2.0,
            100.0 + source.y() + source.height() / 2.0,
        ),
        (
            100.0 + viewport.x() + CARD_WIDTH as f32 / 2.0,
            100.0 + viewport.y() + CARD_HEIGHT as f32 / 2.0,
        ),
        32,
    );
    watch.remove();
    assert_eq!(
        gallery_paths(&page.model).first(),
        Some(last_path),
        "last card must reach the first slot; scroll={}",
        adjustment.value()
    );
    assert_eq!(
        missing_frames.get(),
        0,
        "the dragged preview must remain visible through the drop"
    );
    let retained = original_card
        .upgrade()
        .expect("a long drag must retain its original card");
    assert_eq!(
        page.live_wallpaper_cards.borrow()[last_path].upgrade(),
        Some(retained.clone())
    );
    assert_eq!(
        retained
            .child()
            .and_downcast::<gtk::Button>()
            .unwrap()
            .child()
            .and_downcast::<gtk::Overlay>()
            .unwrap()
            .child(),
        original_picture.upgrade().map(|picture| picture.upcast())
    );
    window.close();
    pump(50);
}
