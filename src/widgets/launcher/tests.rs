use super::*;

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
