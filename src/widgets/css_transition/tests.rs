use super::*;
use crate::widgets::{detach_application_window, test_support::pump};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

#[test]
#[ignore = "requires GTK Wayland; checks native popup timer replacement without moving the pointer"]
fn native_popup_reopening_cancels_old_timers_and_releases_the_surface() {
    gtk::init().unwrap();
    let application = gtk::Application::builder()
        .application_id("dev.obsidian.NativePopupRevealTest")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    application.register(gio::Cancellable::NONE).unwrap();
    let display = gtk::gdk::Display::default().unwrap();
    let settings = gtk::Settings::for_display(&display);
    settings.set_gtk_enable_animations(true);
    let css = gtk::CssProvider::new();
    css.load_from_data(include_str!("../../../assets/window.css"));
    gtk::style_context_add_provider_for_display(
        &display,
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
    frame.add_css_class("widget-popup-frame");
    let button = gtk::Button::with_label("Menu animation check");
    frame.append(&button);
    let window = gtk::ApplicationWindow::builder()
        .application(&application)
        .child(&frame)
        .build();
    window.add_css_class("widget-popup-window");
    window.init_layer_shell();
    window.set_namespace(Some("obsidian-bar-native-popup-test"));
    window.set_layer(Layer::Overlay);
    window.set_keyboard_mode(KeyboardMode::None);
    window.set_anchor(Edge::Right, true);
    window.set_anchor(Edge::Bottom, true);
    window.set_margin(Edge::Bottom, 80);
    let popover = gtk::Popover::new();
    popover.add_css_class("tray-menu-popover-window");
    popover.set_has_arrow(false);
    popover.set_autohide(false);
    popover.set_child(Some(&gtk::Label::new(Some("Native menu"))));
    popover.set_parent(&button);
    // A longer cleanup delay makes the replacement interval unambiguous;
    // the real tray CSS animation still retains its 220 ms duration.
    attach_popover_reveal(&popover, "tray-menu-opening", Duration::from_secs(1));
    window.present();
    pump(100);
    popover.popup();
    pump(300);
    assert!(popover.is_mapped());
    assert!(popover.has_css_class("tray-menu-opening"));
    popover.set_visible(false);
    assert!(!popover.is_mapped());
    assert!(!popover.has_css_class("tray-menu-opening"));
    popover.popup();
    pump(750);
    assert!(
        popover.has_css_class("tray-menu-opening"),
        "the cancelled old timer must not clear a newer entrance"
    );
    popover.set_visible(false);
    settings.set_gtk_enable_animations(false);
    popover.popup();
    pump(50);
    assert!(popover.is_mapped());
    assert_eq!(popover.opacity(), 1.0);
    assert!(!popover.has_css_class("tray-menu-opening"));
    popover.set_visible(false);
    settings.set_gtk_enable_animations(true);
    popover.popup();
    pump(30);
    assert!(popover.has_css_class("tray-menu-opening"));
    let weak = popover.downgrade();
    popover.set_visible(false);
    popover.unparent();
    drop(popover);
    detach_application_window(&window);
    drop(window);
    pump(100);
    assert!(
        weak.upgrade().is_none(),
        "pending cleanup must not retain the popup"
    );
}
