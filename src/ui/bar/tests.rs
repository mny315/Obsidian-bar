use super::*;

fn close(left: f64, right: f64) -> bool {
    (left - right).abs() < 0.001
}

#[test]
fn full_slide_uses_configured_duration() {
    assert!(close(
        bar_slide_duration_ms(
            true,
            BAR_VISIBLE_TOP_MARGIN as f64,
            BAR_HIDDEN_TOP_MARGIN as f64,
        ),
        BAR_HIDE_DURATION_MS,
    ));
    assert!(close(
        bar_slide_duration_ms(
            false,
            BAR_HIDDEN_TOP_MARGIN as f64,
            BAR_VISIBLE_TOP_MARGIN as f64,
        ),
        BAR_SHOW_DURATION_MS,
    ));
}

#[test]
fn short_reverse_slide_does_not_take_the_full_duration() {
    let duration = bar_slide_duration_ms(true, -40.0, BAR_HIDDEN_TOP_MARGIN as f64);
    assert!(close(duration, BAR_MIN_ANIMATION_DURATION_MS));
}

#[test]
fn slide_endpoints_match_the_reserved_work_area() {
    assert_eq!(BAR_HIDDEN_TOP_MARGIN + BAR_FALLBACK_HEIGHT, 0);
    assert_eq!(BAR_VISIBLE_TOP_MARGIN + BAR_FALLBACK_HEIGHT, 46);
}

#[test]
fn fullscreen_size_match_has_a_small_absolute_tolerance() {
    assert!(size_matches_output(1920.0, 1920.0));
    assert!(size_matches_output(1919.0, 1920.0));
    assert!(!size_matches_output(1918.0, 1920.0));
}

#[test]
#[ignore = "requires GTK Wayland; checks a synthetic layer surface without changing workspaces or the reserved work area"]
fn slide_replacements_reverse_from_the_current_margin_and_honor_animation_settings() {
    gtk::init().unwrap();
    let application = gtk::Application::builder()
        .application_id("dev.obsidian.BarSlideTest")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    application.register(gio::Cancellable::NONE).unwrap();
    let display = gdk::Display::default().unwrap();
    let monitor = display
        .monitors()
        .item(0)
        .and_downcast::<gdk::Monitor>()
        .unwrap();
    let css = gtk::CssProvider::new();
    css.load_from_data(include_str!("../../../assets/window.css"));
    gtk::style_context_add_provider_for_display(
        &display,
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
    frame.add_css_class("widget-popup-frame");
    frame.append(&gtk::Label::new(Some("Panel slide check")));
    let window = gtk::ApplicationWindow::builder()
        .application(&application)
        .default_width(260)
        // Keep part of the fixture on screen even at the hidden margin, so
        // compositor occlusion does not suppress the frames being checked.
        .default_height(BAR_FALLBACK_HEIGHT * 3)
        .child(&frame)
        .build();
    window.add_css_class("widget-popup-window");
    window.init_layer_shell();
    window.set_namespace(Some("obsidian-bar-slide-test"));
    window.set_layer(Layer::Overlay);
    window.set_keyboard_mode(KeyboardMode::None);
    window.set_anchor(Edge::Top, true);
    window.set_anchor(Edge::Right, true);
    window.set_margin(Edge::Right, 80);
    window.set_margin(Edge::Top, BAR_HIDDEN_TOP_MARGIN);
    window.set_exclusive_zone(-1);
    let tooltip = BarTooltip::new(&application, &monitor);
    let slide = Rc::new(BarSlideState::default());
    let settings = window.settings();
    settings.set_gtk_enable_animations(true);
    let wait = |condition: &dyn Fn() -> bool| {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !condition() {
            assert!(
                std::time::Instant::now() < deadline,
                "slide did not advance"
            );
            glib::MainContext::default().iteration(false);
            std::thread::sleep(Duration::from_millis(1));
        }
    };
    for index in 0..1000 {
        let hidden = index % 2 == 0;
        animate_bar_slide(window.clone(), Rc::clone(&slide), tooltip.clone(), hidden);
        assert_eq!(slide.frame.is_pending(), !hidden);
    }
    assert!(!window.is_mapped());
    assert_eq!(Rc::strong_count(&slide), 1);
    slide.frame.cancel();
    apply_bar_margin(&window, &slide, f64::from(BAR_VISIBLE_TOP_MARGIN));
    let painted = Rc::new(Cell::new(false));
    let first_frame = FrameCallback::default();
    let ready = Rc::clone(&painted);
    first_frame.start(&window, move |_, _| {
        ready.set(true);
        glib::ControlFlow::Break
    });
    window.present();
    wait(&|| painted.get());
    animate_bar_slide(window.clone(), Rc::clone(&slide), tooltip.clone(), true);
    wait(&|| slide.current_margin.get() < f64::from(BAR_VISIBLE_TOP_MARGIN));
    let halfway = slide.current_margin.get();
    assert!(halfway > f64::from(BAR_HIDDEN_TOP_MARGIN));
    animate_bar_slide(window.clone(), Rc::clone(&slide), tooltip.clone(), false);
    assert_eq!(
        slide.current_margin.get(),
        halfway,
        "reversal jumped before its first frame"
    );
    wait(&|| !slide.frame.is_pending());
    assert_eq!(
        slide.current_margin.get(),
        f64::from(BAR_VISIBLE_TOP_MARGIN)
    );
    assert_eq!(window.margin(Edge::Top), BAR_VISIBLE_TOP_MARGIN);
    assert!(window.can_target());
    animate_bar_slide(window.clone(), Rc::clone(&slide), tooltip.clone(), true);
    wait(&|| !slide.frame.is_pending());
    assert_eq!(window.margin(Edge::Top), BAR_HIDDEN_TOP_MARGIN);
    assert!(!window.can_target());
    settings.set_gtk_enable_animations(false);
    animate_bar_slide(window.clone(), Rc::clone(&slide), tooltip.clone(), false);
    assert_eq!(window.margin(Edge::Top), BAR_VISIBLE_TOP_MARGIN);
    assert!(!slide.frame.is_pending());
    animate_bar_slide(window.clone(), Rc::clone(&slide), tooltip.clone(), true);
    assert_eq!(window.margin(Edge::Top), BAR_HIDDEN_TOP_MARGIN);
    assert!(!slide.frame.is_pending());
    settings.set_gtk_enable_animations(true);
    animate_bar_slide(window.clone(), Rc::clone(&slide), tooltip.clone(), false);
    assert!(slide.frame.is_pending());
    let weak = Rc::downgrade(&slide);
    drop(slide);
    assert!(weak.upgrade().is_none());
    tooltip.close();
    window.destroy();
}
