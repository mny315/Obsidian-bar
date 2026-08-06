use std::{rc::Rc, time::Duration};

use gtk::{glib, prelude::*};

use super::Generation;

struct CssTransitionState {
    generation: Generation,
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
            widget,
            classes,
            duration,
        }))
    }

    pub(super) fn clear(&self) {
        self.0.generation.bump();
        for &class in self.0.classes {
            self.0.widget.remove_css_class(class);
        }
    }

    pub(super) fn replay(&self, class: &'static str) {
        debug_assert!(self.0.classes.contains(&class));
        let generation = self.0.generation.bump();
        for &class in self.0.classes {
            self.0.widget.remove_css_class(class);
        }

        let weak_state = Rc::downgrade(&self.0);
        glib::idle_add_local_once(move || {
            let Some(state) = weak_state.upgrade() else {
                return;
            };
            if !state.generation.is_current(generation) {
                return;
            }

            state.widget.add_css_class(class);
            let weak_state = Rc::downgrade(&state);
            glib::timeout_add_local_once(state.duration, move || {
                let Some(state) = weak_state.upgrade() else {
                    return;
                };
                if state.generation.is_current(generation) {
                    state.widget.remove_css_class(class);
                }
            });
        });
    }
}
