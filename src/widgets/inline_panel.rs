//! Inline control revealers and shared slider/scroll input handling.
use super::Timer;
use gtk::{gdk, glib, prelude::*};
use std::{rc::Rc, time::Duration};

pub(super) fn build_inline_panel(
    reveal_duration_ms: u32,
    panel_spacing: i32,
    panel_css_class: &str,
) -> (gtk::Box, gtk::Revealer, gtk::Box) {
    let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    root.add_css_class("quick-control");
    root.add_css_class("inline-control");
    root.set_valign(gtk::Align::Center);

    let revealer = gtk::Revealer::new();
    revealer.add_css_class("inline-revealer");
    revealer.set_transition_type(gtk::RevealerTransitionType::SlideLeft);
    revealer.set_transition_duration(reveal_duration_ms);
    revealer.set_reveal_child(false);

    let panel = gtk::Box::new(gtk::Orientation::Horizontal, panel_spacing);
    panel.add_css_class("inline-panel");
    panel.add_css_class(panel_css_class);
    panel.set_valign(gtk::Align::Center);
    revealer.set_child(Some(&panel));

    (root, revealer, panel)
}

pub(super) fn attach_vertical_step_scroll<T: 'static>(
    trigger: &gtk::Button,
    owner: std::rc::Weak<T>,
    step: f64,
    adjust: impl Fn(&Rc<T>, f64) + 'static,
    after_adjust: impl Fn(&Rc<T>) + 'static,
) {
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    scroll.connect_scroll(move |_, _, dy| {
        let Some(owner) = owner.upgrade() else {
            return glib::Propagation::Proceed;
        };
        let delta = if dy < 0.0 {
            step
        } else if dy > 0.0 {
            -step
        } else {
            return glib::Propagation::Proceed;
        };

        adjust(&owner, delta);
        after_adjust(&owner);
        glib::Propagation::Stop
    });
    trigger.add_controller(scroll);
}

pub(super) fn attach_scale_value_changed<T: 'static>(
    scale: &gtk::Scale,
    owner: std::rc::Weak<T>,
    updating: impl Fn(&Rc<T>) -> bool + 'static,
    changed: impl Fn(&Rc<T>, f64) + 'static,
) {
    scale.connect_value_changed(move |scale| {
        let Some(owner) = owner.upgrade() else {
            return;
        };
        if !updating(&owner) {
            changed(&owner, scale.value());
        }
    });
}

pub(super) fn attach_inline_revealer_behavior(
    root: &gtk::Box,
    revealer: &gtk::Revealer,
    hide_delay: Duration,
) {
    let hide_timer = Rc::new(Timer::default());
    let motion = gtk::EventControllerMotion::new();

    let timer = Rc::clone(&hide_timer);
    motion.connect_enter(move |_, _, _| {
        timer.cancel();
    });

    let timer = Rc::clone(&hide_timer);
    revealer.connect_reveal_child_notify(move |revealer| {
        if !revealer.reveals_child() {
            timer.cancel();
        }
    });

    let weak_revealer = revealer.downgrade();
    motion.connect_leave(move |_| {
        if !weak_revealer
            .upgrade()
            .is_some_and(|revealer| revealer.reveals_child())
        {
            return;
        }

        let weak_revealer = weak_revealer.clone();
        hide_timer.schedule(hide_delay, move || {
            if let Some(revealer) = weak_revealer.upgrade() {
                revealer.set_reveal_child(false);
            }
        });
    });
    root.add_controller(motion);

    let keys = gtk::EventControllerKey::new();
    let weak_revealer = revealer.downgrade();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key != gdk::Key::Escape {
            return glib::Propagation::Proceed;
        }

        if let Some(revealer) = weak_revealer.upgrade()
            && revealer.reveals_child()
        {
            revealer.set_reveal_child(false);
            return glib::Propagation::Stop;
        }

        glib::Propagation::Proceed
    });
    root.add_controller(keys);
}
