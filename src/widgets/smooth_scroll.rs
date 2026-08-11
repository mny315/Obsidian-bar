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

pub(super) fn install_smooth_scroll(scroller: &gtk::ScrolledWindow, config: SmoothScrollConfig) {
    #[derive(Default)]
    struct ScrollState {
        active: Cell<bool>,
        start: Cell<f64>,
        target: Cell<f64>,
        start_time_us: Cell<i64>,
        duration_ms: Cell<f64>,
    }

    let state = Rc::new(ScrollState::default());
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
            let state = Rc::clone(&state);
            scroller.add_tick_callback(move |scroller, frame_clock| {
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
                adjustment.set_value(start + (target - start) * eased);
                scroller.queue_draw();

                if progress >= 1.0 {
                    adjustment.set_value(target);
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
