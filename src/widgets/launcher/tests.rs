use super::*;

#[test]
#[ignore = "requires GTK layer-shell, SVG loader and XDG_DATA_HOME under /tmp; run alone"]
fn svg_desktop_icons_render_in_app_rows_and_favorites() {
    use crate::widgets::test_support::{pump, snapshot_pixels, widgets_with_class};
    use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

    let directory = glib::user_data_dir();
    assert!(directory.starts_with("/tmp"));
    let applications = directory.join("applications");
    let icons = directory.join("icons");
    let theme = icons.join("obsidian-svg-test");
    let scalable = theme.join("scalable/apps");
    fs::create_dir_all(&applications).unwrap();
    fs::create_dir_all(&scalable).unwrap();
    fs::write(
        theme.join("index.theme"),
        "[Icon Theme]\nName=Obsidian SVG Test\nDirectories=scalable/apps\n\n\
         [scalable/apps]\nSize=48\nMinSize=1\nMaxSize=256\nType=Scalable\nContext=Applications\n",
    )
    .unwrap();
    let svg = scalable.join("obsidian-svg-fixture.svg");
    fs::write(
        &svg,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="48" height="48" viewBox="0 0 48 48"><rect width="48" height="48" fill="#13e74b"/></svg>"##,
    )
    .unwrap();
    for (name, icon) in [
        ("file", svg.to_str().unwrap()),
        ("themed", "obsidian-svg-fixture"),
    ] {
        fs::write(
            applications.join(format!("obsidian-svg-{name}.desktop")),
            format!("[Desktop Entry]\nType=Application\nName=SVG {name}\nExec=true\nIcon={icon}\n"),
        )
        .unwrap();
    }
    // Fail explicitly if the runtime is missing librsvg, rather than accepting
    // GTK's image-missing fallback as a successfully loaded application icon.
    let pixbuf = gdk_pixbuf::Pixbuf::from_file_at_scale(&svg, 40, 40, true)
        .expect("the runtime must provide an SVG image loader");
    assert_eq!((pixbuf.width(), pixbuf.height()), (40, 40));
    gtk::init().unwrap();
    let display = gdk::Display::default().unwrap();
    gtk::IconTheme::for_display(&display).add_search_path(&icons);
    gtk::Settings::for_display(&display).set_gtk_icon_theme_name(Some("obsidian-svg-test"));
    let application = gtk::Application::builder()
        .application_id("dev.obsidian.LauncherSvgTest")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    application.register(gio::Cancellable::NONE).unwrap();
    let bar = gtk::ApplicationWindow::builder()
        .application(&application)
        .build();
    let monitor = display
        .monitors()
        .item(0)
        .and_downcast::<gdk::Monitor>()
        .unwrap();
    let indicator = LauncherIndicator::new(&application, &bar, &monitor, &LauncherCatalog::new());
    let controller = &indicator._controller;
    let content = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let mut images = Vec::new();
    let mut keys = HashSet::new();
    let mut rows = Vec::new();
    let mut favorites = Vec::new();
    for name in ["file", "themed"] {
        let id = format!("obsidian-svg-{name}.desktop");
        let info = gio::AppInfo::all()
            .into_iter()
            .find(|info| info.id().as_deref() == Some(&id))
            .expect("temporary desktop entry must be discovered");
        let app = Rc::new(catalog_app(info, &mut keys).unwrap());
        let row = controller.build_app_row(Rc::clone(&app));
        let icon_wrap =
            widgets_with_class(row.root.upcast_ref(), "launcher-app-icon-wrap")[0].clone();
        images.push(
            icon_wrap
                .first_child()
                .and_downcast::<gtk::Image>()
                .unwrap(),
        );
        content.append(&row.root);
        let favorite = controller.build_favorite_tile(app);
        images.push(
            widgets_with_class(favorite.root.upcast_ref(), "launcher-favorite-tile-icon")[0]
                .clone()
                .downcast::<gtk::Image>()
                .unwrap(),
        );
        content.append(&favorite.root);
        rows.push(row);
        favorites.push(favorite);
    }
    let window = gtk::ApplicationWindow::builder()
        .application(&application)
        .child(&content)
        .build();
    window.init_layer_shell();
    window.set_namespace(Some("obsidian-bar-launcher-svg-test"));
    window.set_layer(Layer::Overlay);
    window.set_keyboard_mode(KeyboardMode::None);
    window.set_anchor(Edge::Right, true);
    window.set_anchor(Edge::Bottom, true);
    window.present();
    pump(300);
    for image in &images {
        let pixels = snapshot_pixels(image, &window);
        assert!(
            pixels
                .chunks_exact(4)
                .filter(|p| p[0] < 40 && p[1] > 200 && p[2] < 110 && p[3] > 200)
                .count()
                > 100,
            "both launcher views must render the SVG's green pixels, not a missing-image placeholder"
        );
    }
    window.destroy();
    detach_application_window(&bar);
}

#[test]
#[ignore = "requires a GTK display; uses unmapped widgets and does not move the pointer"]
fn favorite_rebuild_respects_explicit_scroll_reset() {
    gtk::init().unwrap();
    let application = gtk::Application::builder()
        .application_id("dev.obsidian.LauncherScrollTest")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    application.register(None::<&gio::Cancellable>).unwrap();
    let bar = gtk::ApplicationWindow::builder()
        .application(&application)
        .build();
    let monitor = gdk::Display::default()
        .unwrap()
        .monitors()
        .item(0)
        .and_downcast::<gdk::Monitor>()
        .unwrap();
    let indicator = LauncherIndicator::new(&application, &bar, &monitor, &LauncherCatalog::new());
    let controller = &indicator._controller;
    let make_app = |key: &str| {
        Rc::new(LaunchableApp {
            key: key.into(),
            name: key.into(),
            meta: String::new(),
            primary_category: None,
            search_blob: key.into(),
            info: gio::AppInfo::create_from_commandline(
                "true",
                Some(key),
                gio::AppInfoCreateFlags::NONE,
            )
            .unwrap(),
        })
    };
    let adjustment = controller.scroller.vadjustment();
    adjustment.configure(300.0, 0.0, 1000.0, 10.0, 100.0, 100.0);
    controller.sync_favorites_grid(vec![make_app("first")]);
    controller.scroll_to_top();
    super::super::test_support::pump(20);
    assert_eq!(
        adjustment.value(),
        0.0,
        "a rebuild must not undo a category/search reset"
    );

    adjustment.configure(300.0, 0.0, 1000.0, 10.0, 100.0, 100.0);
    controller.sync_favorites_grid(vec![make_app("second")]);
    super::super::test_support::pump(20);
    assert_eq!(
        adjustment.value(),
        300.0,
        "a plain reorder must preserve scrolling"
    );

    controller.catalog.data.borrow_mut().apps = ["first", "second", "third"].map(make_app).into();
    controller.user_state.borrow_mut().favorites = HashSet::from(["first".into(), "second".into()]);
    controller
        .selected_category
        .set(LauncherCategory::Favorites);
    controller.refresh_list();
    assert!(
        controller.rows.borrow().is_empty(),
        "opening favorites must not construct the full list"
    );
    assert_eq!(
        *controller.favorites_grid_keys.borrow(),
        ["first", "second"]
    );
    controller.search.set_text("third");
    controller.refresh_list();
    assert_eq!(controller.rows.borrow().len(), 3);
    assert_eq!(
        controller
            .rows
            .borrow()
            .iter()
            .filter(|row| row.root.get_visible())
            .count(),
        1
    );
    controller.search.set_text("");
    controller.user_state.borrow_mut().hidden = HashSet::from(["first".into(), "second".into()]);
    controller.refresh_list();
    assert!(controller.empty_label.get_visible());
    assert!(controller.list.get_visible());
    assert!(
        controller
            .rows
            .borrow()
            .iter()
            .all(|row| !row.root.get_visible()),
        "an empty favorites view must not expose previous search rows"
    );
    drop(indicator);
    detach_application_window(&bar);
}

#[test]
fn favorite_order_round_trips_without_sorting_or_changing_membership() {
    let root = std::env::temp_dir().join(format!("obsidian-favorite-order-{}", std::process::id()));
    let path = root.join("order.json");
    let order = vec![
        "z.desktop".into(),
        "a.desktop".into(),
        "middle.desktop".into(),
    ];
    assert!(save_ordered_keys_at(&path, &order));
    assert_eq!(read_ordered_keys_at(&path), order);
    fs::write(&path, r#"["z.desktop", "", "a.desktop", "z.desktop"]"#).unwrap();
    assert_eq!(read_ordered_keys_at(&path), ["z.desktop", "a.desktop"]);
    fs::write(&path, "broken JSON").unwrap();
    assert!(read_ordered_keys_at(&path).is_empty());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn nested_desktop_entries_keep_categories_and_keywords() {
    const TEST_ROOT: &str = "OBSIDIAN_LAUNCHER_TEST_ROOT";
    if env::var_os(TEST_ROOT).is_some() {
        let info = gio::AppInfo::all()
            .into_iter()
            .find(|info| info.id().as_deref() == Some("vendor-audit.desktop"))
            .expect("GIO should discover the nested desktop entry");
        let app = catalog_app(info, &mut HashSet::new()).expect("visible app");
        assert_eq!(app.primary_category, Some(LauncherCategory::Games));
        assert!(app.search_blob.contains("nested-keyword"));
        let context = glib::MainContext::default();
        let _guard = context.acquire().unwrap();
        let catalog = LauncherCatalog::new();
        catalog.start();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !catalog.initialized.get() {
            context.iteration(false);
            assert!(
                std::time::Instant::now() < deadline,
                "background catalog build stalled"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let catalog = catalog.data.borrow();
        let app = catalog
            .apps
            .iter()
            .find(|app| app.key == "id:vendor-audit.desktop")
            .unwrap();
        assert_eq!(app.primary_category, Some(LauncherCategory::Games));
        assert!(app.search_blob.contains("nested-keyword"));
        return;
    }

    let root = env::temp_dir().join(format!("obsidian-launcher-nested-{}", std::process::id()));
    fs::create_dir_all(root.join("applications/vendor")).unwrap();
    fs::create_dir_all(root.join("empty-data-dir")).unwrap();
    fs::write(
        root.join("applications/vendor/audit.desktop"),
        "[Desktop Entry]\nType=Application\nName=Obsidian audit fixture\nExec=true\nCategories=Game;\nKeywords=nested-keyword;\n",
    ).unwrap();
    let output = std::process::Command::new(env::current_exe().unwrap())
        .args([
            "--exact",
            "widgets::launcher::tests::nested_desktop_entries_keep_categories_and_keywords",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(TEST_ROOT, &root)
        .env("XDG_DATA_HOME", &root)
        .env("XDG_DATA_DIRS", root.join("empty-data-dir"))
        .output()
        .unwrap();
    fs::remove_dir_all(&root).unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

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
