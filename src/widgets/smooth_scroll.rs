use std::{cell::Cell, rc::Rc};

use gtk::{gdk, glib, prelude::*};

pub(super) struct SmoothScrollConfig {
    wheel_step: f64,
    wheel_duration_ms: f64,
    surface_duration_ms: f64,
    min_distance: f64,
}

impl SmoothScrollConfig {
    pub(super) const fn new(
        wheel_step: f64,
        wheel_duration_ms: f64,
        surface_duration_ms: f64,
    ) -> Self {
        Self {
            wheel_step,
            wheel_duration_ms,
            surface_duration_ms,
            min_distance: 0.01,
        }
    }
}

#[derive(Default)]
struct ScrollState {
    active: Cell<bool>,
    updating_value: Cell<bool>,
    tick: super::FrameCallback,
    start: Cell<f64>,
    target: Cell<f64>,
    start_time_us: Cell<i64>,
    duration_ms: Cell<f64>,
}

impl ScrollState {
    fn cancel(&self) {
        self.active.set(false);
        self.start_time_us.set(0);
        self.tick.cancel();
    }

    fn set_value(&self, adjustment: &gtk::Adjustment, value: f64) {
        self.updating_value.set(true);
        adjustment.set_value(value);
        self.updating_value.set(false);
    }
}

pub(super) fn install_smooth_scroll(scroller: &gtk::ScrolledWindow, config: SmoothScrollConfig) {
    let state = Rc::new(ScrollState::default());
    let weak_state = Rc::downgrade(&state);
    scroller.vadjustment().connect_value_changed(move |_| {
        if let Some(state) = weak_state.upgrade()
            && !state.updating_value.get()
        {
            // A category reset, scrollbar drag or layout correction takes
            // precedence over the old animation target.
            state.cancel();
        }
    });
    let weak_state = Rc::downgrade(&state);
    scroller.connect_unmap(move |_| {
        if let Some(state) = weak_state.upgrade() {
            state.cancel();
        }
    });
    let controller = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);

    let weak_scroller = scroller.downgrade();
    controller.connect_scroll(move |scroll, _dx, dy| {
        if !dy.is_finite() || dy.abs() < config.min_distance {
            return glib::Propagation::Proceed;
        }

        let Some(scroller) = weak_scroller.upgrade() else {
            return glib::Propagation::Proceed;
        };
        let adjustment = scroller.vadjustment();
        let lower = adjustment.lower();
        let upper = (adjustment.upper() - adjustment.page_size()).max(lower);
        if upper <= lower {
            return glib::Propagation::Proceed;
        }

        let (delta, duration_ms) = if scroll.unit() == gdk::ScrollUnit::Surface {
            (dy, config.surface_duration_ms)
        } else {
            (dy * config.wheel_step, config.wheel_duration_ms)
        };

        let current = adjustment.value();
        let base_target = if state.active.get() {
            state.target.get()
        } else {
            current
        };
        let target = (base_target + delta).clamp(lower, upper);
        if (target - current).abs() < config.min_distance {
            state.cancel();
            return glib::Propagation::Stop;
        }

        state.start.set(current);
        state.target.set(target);
        state.duration_ms.set(duration_ms);
        state.start_time_us.set(
            scroller
                .frame_clock()
                .map_or(0, |frame_clock| frame_clock.frame_time()),
        );

        if !state.active.replace(true) {
            let tick_state = Rc::clone(&state);
            state.tick.start(&scroller, move |scroller, frame_clock| {
                let state = &tick_state;
                if !state.active.get() {
                    return glib::ControlFlow::Break;
                }

                let adjustment = scroller.vadjustment();
                let lower = adjustment.lower();
                let upper = (adjustment.upper() - adjustment.page_size()).max(lower);
                let target = state.target.get().clamp(lower, upper);
                state.target.set(target);

                let start_time_us = state.start_time_us.get();
                let frame_time_us = frame_clock.frame_time();
                if start_time_us == 0 {
                    state.start_time_us.set(frame_time_us);
                    return glib::ControlFlow::Continue;
                }

                let duration_ms = state.duration_ms.get().max(1.0);
                let elapsed_ms = ((frame_time_us - start_time_us) as f64 / 1_000.0).max(0.0);
                let progress = (elapsed_ms / duration_ms).clamp(0.0, 1.0);
                let eased = 1.0 - (1.0 - progress) * (1.0 - progress);
                let start = state.start.get();
                state.set_value(&adjustment, start + (target - start) * eased);

                if progress >= 1.0 {
                    state.set_value(&adjustment, target);
                    state.active.set(false);
                    state.start_time_us.set(0);
                    glib::ControlFlow::Break
                } else {
                    glib::ControlFlow::Continue
                }
            });
        }

        glib::Propagation::Stop
    });

    scroller.add_controller(controller);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widgets::test_support::pump;
    use gtk4_layer_shell::{KeyboardMode, Layer, LayerShell};

    #[test]
    #[ignore = "requires Wayland layer-shell; synthesizes scroll signals without moving the pointer"]
    fn resets_and_unmapping_cancel_scroll_without_restoring_the_old_target() {
        gtk::init().unwrap();
        let application = gtk::Application::builder()
            .application_id("dev.obsidian.ScrollTest")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        application.register(gio::Cancellable::NONE).unwrap();
        let window = gtk::ApplicationWindow::builder()
            .application(&application)
            .default_width(180)
            .default_height(120)
            .build();
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_keyboard_mode(KeyboardMode::None);
        let scroller = gtk::ScrolledWindow::new();
        scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        for index in 0..100 {
            content.append(&gtk::Label::new(Some(&format!("Row {index}"))));
        }
        scroller.set_child(Some(&content));
        install_smooth_scroll(&scroller, SmoothScrollConfig::new(72.0, 150.0, 90.0));
        window.set_child(Some(&scroller));
        window.present();
        pump(100);
        let adjustment = scroller.vadjustment();
        assert!(adjustment.upper() > adjustment.page_size());
        let scroll = scroller
            .observe_controllers()
            .iter::<glib::Object>()
            .filter_map(Result::ok)
            .find_map(|controller| controller.downcast::<gtk::EventControllerScroll>().ok())
            .unwrap();
        assert!(scroll.emit_by_name::<bool>("scroll", &[&0.0_f64, &1.0_f64]));
        pump(45);
        assert!(adjustment.value() > 0.0);
        adjustment.set_value(0.0);
        pump(220);
        assert_eq!(
            adjustment.value(),
            0.0,
            "a category reset must win over the scroll animation"
        );

        scroll.emit_by_name::<bool>("scroll", &[&0.0_f64, &1.0_f64]);
        scroll.emit_by_name::<bool>("scroll", &[&0.0_f64, &-1.0_f64]);
        pump(220);
        assert_eq!(
            adjustment.value(),
            0.0,
            "opposite wheel events before a frame must cancel"
        );
        scroll.emit_by_name::<bool>("scroll", &[&0.0_f64, &1.0_f64]);
        pump(45);
        let value = adjustment.value();
        assert!(value > 0.0);
        window.set_visible(false);
        pump(100);
        window.present();
        pump(220);
        assert!(
            (adjustment.value() - value).abs() < 0.01,
            "reopening must not resume an old wheel event"
        );
        scroll.emit_by_name::<bool>("scroll", &[&0.0_f64, &1.0_f64]);
        pump(220);
        assert!(
            adjustment.value() > value,
            "scrolling must work again after cancellation"
        );
        crate::widgets::detach_application_window(&window);
    }
}
