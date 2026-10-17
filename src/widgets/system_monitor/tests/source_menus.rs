use super::*;
use crate::widgets::test_support::pump;

fn menu_texture(popover: &gtk::Popover, name: &str) -> gdk::Texture {
    let snapshot = gtk::Snapshot::new();
    gtk::WidgetPaintable::new(Some(popover)).snapshot(
        &snapshot,
        f64::from(popover.width()),
        f64::from(popover.height()),
    );
    let node = snapshot.to_node().expect("visible source menu must render");
    let texture = popover.renderer().unwrap().render_texture(&node, None);
    if let Some(directory) = std::env::var_os("OBSIDIAN_BAR_TEST_SNAPSHOTS") {
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        texture
            .save_to_png(directory.join(format!("{name}.png")))
            .unwrap();
    }
    texture
}

fn menu_pixels(texture: &gdk::Texture) -> Vec<u8> {
    let stride = texture.width() as usize * 4;
    let mut pixels = vec![0; stride * texture.height() as usize];
    texture.download(&mut pixels, stride);
    pixels
}

#[test]
#[ignore = "requires Wayland layer-shell and XDG_STATE_HOME=/tmp/obsidian-monitor-items-test-state"]
fn source_menus_keep_glass_surfaces_and_replay_their_animation() {
    assert!(settings_path().starts_with("/tmp/obsidian-monitor-items-test-state"));
    gtk::init().unwrap();
    let application = gtk::Application::builder()
        .application_id("dev.obsidian.MonitorSourceMenusTest")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    application.register(None::<&gio::Cancellable>).unwrap();
    let display = gdk::Display::default().unwrap();
    let gtk_settings = gtk::Settings::for_display(&display);
    gtk_settings.set_gtk_enable_animations(true);
    let errors = Rc::new(RefCell::new(Vec::new()));
    let css = gtk::CssProvider::new();
    let css_errors = Rc::clone(&errors);
    css.connect_parsing_error(move |_, _, error| css_errors.borrow_mut().push(error.to_string()));
    css.load_from_data(include_str!("../../../../assets/window.css"));
    assert!(errors.borrow().is_empty(), "{:?}", errors.borrow());
    gtk::style_context_add_provider_for_display(
        &display,
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let controller = SystemMonitorController::new();
    let mut sample = metric_selection_snapshot();
    sample.network_choices = vec!["enp1s0".into(), "wlan0".into()];
    controller.latest.replace(Arc::new(sample));
    let panel = MonitorSettingsPanel::new(&controller);
    panel.sync_sources(&controller.settings(), &controller.latest());
    panel.root.add_css_class("system-monitor-settings-surface");
    let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
    frame.add_css_class("system-monitor-card");
    frame.append(&panel.root);
    let window = gtk::ApplicationWindow::builder()
        .application(&application)
        .title("Monitor source menu check")
        .default_width(400)
        .default_height(430)
        .child(&frame)
        .build();
    window.add_css_class("system-monitor-window");
    window.init_layer_shell();
    window.set_namespace(Some(super::super::view::CONTENT_NAMESPACE));
    window.set_layer(Layer::Overlay);
    window.set_keyboard_mode(gtk4_layer_shell::KeyboardMode::None);
    window.set_anchor(Edge::Top, true);
    window.set_anchor(Edge::Right, true);
    window.present();
    pump(300);

    for (name, dropdown) in [("display", &panel.output), ("network", &panel.network)] {
        let popover = settings_popover(dropdown).unwrap();
        assert!(popover.has_css_class("system-monitor-source-menu"));
        popover.set_autohide(false);
        popover.popup();
        pump(400);
        let settled = menu_pixels(&menu_texture(&popover, &format!("{name}-menu")));
        let glass = settled
            .chunks_exact(4)
            .filter(|pixel| (100..=185).contains(&pixel[3]))
            .count();
        assert!(
            glass * 2 > settled.len() / 4,
            "{name} menu must retain the shared translucent glass surface"
        );
        popover.popdown();
        pump(250);
        assert!(!popover.is_mapped());

        popover.popup();
        pump(35);
        let opening = menu_pixels(&menu_texture(&popover, &format!("{name}-menu-opening")));
        pump(280);
        let opened = menu_pixels(&menu_texture(&popover, &format!("{name}-menu-opened")));
        let alpha = |pixels: &[u8]| {
            pixels
                .chunks_exact(4)
                .map(|pixel| u64::from(pixel[3]))
                .sum::<u64>()
        };
        assert!(
            alpha(&opening) < alpha(&opened),
            "{name} entrance animation did not replay"
        );
        dropdown.set_selected(1);
        if name == "display" {
            assert!(controller.settings().output.is_some());
        } else {
            assert_eq!(
                controller.settings().network_interface.as_deref(),
                Some("enp1s0")
            );
        }
        popover.popdown();
        pump(250);

        gtk_settings.set_gtk_enable_animations(false);
        popover.popup();
        pump(60);
        assert!(popover.is_mapped());
        assert!(!popover.has_css_class("system-monitor-source-menu-opening"));
        menu_texture(&popover, &format!("{name}-menu-without-animation"));
        popover.popdown();
        pump(60);
        gtk_settings.set_gtk_enable_animations(true);
    }

    let weak_panel = Rc::downgrade(&panel);
    let weak_popover = panel.popovers[0].downgrade();
    let weak_dropdown = panel.output.downgrade();
    let weak_root = panel.root.downgrade();
    window.set_child(None::<&gtk::Widget>);
    window.destroy();
    drop(window);
    drop(frame);
    drop(panel);
    pump(300);
    assert!(weak_panel.upgrade().is_none());
    assert!(weak_root.upgrade().is_none());
    assert!(weak_dropdown.upgrade().is_none());
    assert!(weak_popover.upgrade().is_none());
}
