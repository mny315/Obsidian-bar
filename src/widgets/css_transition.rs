use std::{rc::Rc, time::Duration};

use gtk::{glib, prelude::*};

use super::{FrameCallback, Generation, Timer};

pub(super) fn attach_popover_reveal(
    popover: &gtk::Popover,
    class: &'static str,
    duration: Duration,
) {
    let timer = Rc::new(Timer::default());
    let opening = Rc::clone(&timer);
    popover.connect_map(move |popover| {
        opening.cancel();
        if !popover.settings().is_gtk_enable_animations() {
            return;
        }
        // Set before the first paint so a native popup cannot flash at full
        // opacity before its entrance animation starts.
        popover.add_css_class(class);
        let weak = popover.downgrade();
        // Compositors can stop frames for covered popups. Clear the entrance
        // state even then so the popup cannot remain faint indefinitely.
        opening.schedule(duration, move || {
            if let Some(popover) = weak.upgrade() {
                popover.remove_css_class(class);
            }
        });
    });
    popover.connect_unmap(move |popover| {
        timer.cancel();
        popover.remove_css_class(class);
    });
}

struct CssTransitionState {
    generation: Generation,
    frame: FrameCallback,
    timer: Timer,
    widget: gtk::Widget,
    classes: &'static [&'static str],
    duration: Duration,
}

pub(super) struct CssTransition(Rc<CssTransitionState>);

impl CssTransition {
    pub(super) fn new(
        widget: gtk::Widget,
        classes: &'static [&'static str],
        duration: Duration,
    ) -> Self {
        Self(Rc::new(CssTransitionState {
            generation: Generation::default(),
            frame: FrameCallback::default(),
            timer: Timer::default(),
            widget,
            classes,
            duration,
        }))
    }

    pub(super) fn clear(&self) {
        self.0.generation.bump();
        self.0.frame.cancel();
        self.0.timer.cancel();
        for &class in self.0.classes {
            self.0.widget.remove_css_class(class);
        }
    }

    pub(super) fn replay(&self, class: &'static str) {
        debug_assert!(self.0.classes.contains(&class));
        let repeating = self.0.widget.has_css_class(class);
        self.clear();
        if !self.0.widget.settings().is_gtk_enable_animations() {
            return;
        }
        if !repeating {
            // Commit the animation style with the content update, before GTK paints.
            self.0.start_animation(class);
            return;
        }
        let generation = self.0.generation.current();

        let weak_state = Rc::downgrade(&self.0);
        glib::idle_add_local_once(move || {
            let Some(state) = weak_state.upgrade() else {
                return;
            };
            if !state.generation.is_current(generation) {
                return;
            }

            state.start_animation(class);
        });
    }
}

impl CssTransitionState {
    fn start_animation(self: &Rc<Self>, class: &'static str) {
        self.widget.add_css_class(class);
        if self.widget.is_mapped() {
            let weak = Rc::downgrade(self);
            self.frame.start(&self.widget, move |_, _| {
                if let Some(state) = weak.upgrade() {
                    state.schedule_cleanup(class);
                }
                glib::ControlFlow::Break
            });
        } else {
            self.schedule_cleanup(class);
        }
    }

    fn schedule_cleanup(self: &Rc<Self>, class: &'static str) {
        let weak = Rc::downgrade(self);
        self.timer.schedule(self.duration, move || {
            if let Some(state) = weak.upgrade() {
                state.widget.remove_css_class(class);
            }
        });
    }
}

#[cfg(test)]
mod tests;
