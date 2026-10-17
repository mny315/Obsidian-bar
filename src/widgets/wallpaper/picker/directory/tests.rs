use super::*;
use std::fs;

#[test]
fn directory_scan_follows_folder_links_and_ignores_files_and_broken_links() {
    use std::os::unix::fs::symlink;
    let root = std::env::temp_dir().join(format!("obsidian-directory-scan-{}", std::process::id()));
    fs::create_dir_all(root.join("Alpha")).unwrap();
    fs::create_dir(root.join("zeta")).unwrap();
    fs::write(root.join("file.png"), b"fixture").unwrap();
    symlink(root.join("Alpha"), root.join("linked folder")).unwrap();
    symlink(root.join("file.png"), root.join("linked file")).unwrap();
    symlink(root.join("missing"), root.join("broken")).unwrap();
    assert_eq!(
        list_entries(&root, EntryKind::Directory, None).unwrap(),
        [
            root.join("Alpha"),
            root.join("linked folder"),
            root.join("zeta")
        ]
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "requires a GTK Wayland display; scans synthetic folders and runs alone"]
fn large_directory_recycles_rows_navigates_current_bindings_and_releases_widgets() {
    use crate::widgets::test_support::{capture_if_requested, pump, widgets_with_class};
    use gtk::{gdk, prelude::*};
    use gtk4_layer_shell::{Edge, Layer, LayerShell};
    use std::os::unix::ffi::OsStringExt;
    use std::time::{Duration, Instant};

    gtk::init().unwrap();
    let application = gtk::Application::builder()
        .application_id("dev.obsidian.DirectoryListTest")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    application.register(None::<&gio::Cancellable>).unwrap();
    let css = gtk::CssProvider::new();
    css.connect_parsing_error(|_, _, error| panic!("CSS error: {error}"));
    css.load_from_data(include_str!("../../../../../assets/window.css"));
    gtk::style_context_add_provider_for_display(
        &gdk::Display::default().unwrap(),
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let root =
        std::env::temp_dir().join(format!("obsidian-virtual-folders-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    for index in 0..5000 {
        fs::create_dir(root.join(format!("folder-{index:04}"))).unwrap();
    }
    let raw_name = std::ffi::OsString::from_vec(b"folder-\xff".to_vec());
    fs::create_dir(root.join(&raw_name)).unwrap();
    let page = build_directory_page();
    page.root.add_css_class("wallpaper-picker-surface");
    let browser = DirectoryBrowser::new(root.clone(), &page);
    let window = gtk::ApplicationWindow::builder()
        .application(&application)
        .child(&page.root)
        .build();
    window.init_layer_shell();
    window.add_css_class("wallpaper-picker-window");
    window.set_namespace(Some("obsidian-bar-wallpaper-directory-test"));
    window.set_layer(Layer::Overlay);
    window.set_keyboard_mode(gtk4_layer_shell::KeyboardMode::None);
    window.set_anchor(Edge::Top, true);
    window.set_anchor(Edge::Right, true);
    window.set_margin(Edge::Top, 100);
    window.present();
    browser.render();
    let wait = |condition: &dyn Fn() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "directory UI did not settle");
            pump(10);
        }
        pump(100);
    };
    wait(&|| page.model.n_items() == 5001 && page.list.is_mapped());
    let rows = || widgets_with_class(page.list.upcast_ref(), "wallpaper-directory-row");
    let bound = rows().len();
    assert!(
        bound > 0 && bound < 512,
        "5000 entries constructed {bound} buttons"
    );
    assert!(!page.empty.is_visible());
    let unusual = rows()
        .into_iter()
        .find_map(|row| {
            let button = row.downcast::<gtk::Button>().unwrap();
            let label = button
                .child()?
                .last_child()?
                .downcast::<gtk::Label>()
                .ok()?;
            (label.label().as_str() == "folder-�").then_some(button)
        })
        .expect("non-UTF-8 folder must have a readable name");
    unusual.emit_clicked();
    assert_eq!(browser.path(), root.join(&raw_name));
    drop(unusual);
    wait(&|| page.empty.is_visible());
    browser.navigate_up();
    wait(&|| page.model.n_items() == 5001);
    let scroller = page
        .list
        .parent()
        .unwrap()
        .downcast::<gtk::ScrolledWindow>()
        .unwrap();
    let scroll = scroller.vadjustment();
    scroll.set_value(scroll.upper() - scroll.page_size());
    let target = root.join("folder-4999");
    let target_label = "folder-4999";
    let find_target = || {
        rows().into_iter().find_map(|row| {
            let button = row.downcast::<gtk::Button>().unwrap();
            let label = button
                .child()?
                .last_child()?
                .downcast::<gtk::Label>()
                .ok()?;
            (label.label().as_str() == target_label).then_some(button)
        })
    };
    wait(&|| find_target().is_some());
    assert!(rows().len() < 512);
    capture_if_requested(&window, "directory-last-rows");
    let button = find_target().unwrap();
    button.emit_clicked();
    assert_eq!(browser.path(), target);
    wait(&|| page.empty.is_visible());
    assert_eq!(page.model.n_items(), 0);
    browser.navigate_up();
    browser.navigate_to(root.join("folder-0001"));
    browser.navigate_up();
    wait(&|| page.model.n_items() == 5001);
    assert_eq!(browser.path(), root);
    assert!(!page.notice.is_visible());

    let weak_list = page.list.downgrade();
    let weak_model = page.model.downgrade();
    let weak_button = button.downgrade();
    drop(button);
    drop(scroll);
    drop(scroller);
    window.set_child(None::<&gtk::Widget>);
    window.destroy();
    drop(window);
    drop(page);
    drop(browser);
    pump(150);
    assert!(weak_list.upgrade().is_none());
    assert!(weak_model.upgrade().is_none());
    assert!(weak_button.upgrade().is_none());
    fs::remove_dir_all(root).unwrap();
}
