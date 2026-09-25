use std::rc::Rc;

use gtk::prelude::*;
use gtk4_layer_shell::{Edge, LayerShell};

use super::super::Generation;

pub(super) const TAIL_WIDTH: i32 = 14;
pub(super) const TAIL_HEIGHT: i32 = 64;
pub(super) const TAIL_TOP: i32 = 12;
pub(super) const RIGHT_MARGIN: i32 = 5;

pub(super) struct MonitorDrawer {
    window: gtk::ApplicationWindow,
    hotspot: gtk::ApplicationWindow,
    revealer: gtk::Revealer,
    generation: Generation,
}

impl MonitorDrawer {
    pub(super) fn new(
        window: &gtk::ApplicationWindow,
        tail: &gtk::ApplicationWindow,
        card: &gtk::Box,
    ) -> Rc<Self> {
        let revealer = gtk::Revealer::new();
        revealer.set_transition_type(gtk::RevealerTransitionType::SlideLeft);
        revealer.set_transition_duration(220);
        revealer.set_child(Some(card));
        window.set_child(Some(&revealer));
        window.set_margin(Edge::Right, RIGHT_MARGIN);
        tail.set_margin(Edge::Right, 0);
        let weak_window = window.downgrade();
        revealer.connect_child_revealed_notify(move |revealer| {
            if !revealer.reveals_child()
                && !revealer.is_child_revealed()
                && let Some(window) = weak_window.upgrade()
            {
                // Unmap the native surface as well, otherwise Niri can leave a
                // blur region after all of its pixels have become transparent.
                window.set_visible(false);
            }
        });
        Rc::new(Self {
            window: window.clone(),
            hotspot: tail.clone(),
            revealer,
            generation: Generation::default(),
        })
    }

    pub(super) fn is_open(&self) -> bool {
        self.revealer.reveals_child() && self.revealer.is_child_revealed()
    }

    pub(super) fn connect_settled(&self, callback: impl Fn() + 'static) {
        self.revealer
            .connect_child_revealed_notify(move |_| callback());
    }

    pub(super) fn set_revealed(self: &Rc<Self>, revealed: bool) {
        if self.revealer.reveals_child() == revealed {
            return;
        }
        let generation = self.generation.bump();
        if revealed {
            self.window.present();
            // Keep the hover/header input surface above the content within the
            // desktop layer; normal application windows still cover both.
            self.hotspot.set_visible(false);
            self.hotspot.present();
        }
        self.revealer.set_reveal_child(revealed);
        if !revealed && !self.revealer.is_child_revealed() {
            // An interrupted opening can still report child-revealed=false.
            // Keep it mapped until the animation reaches its hidden endpoint.
            let weak = Rc::downgrade(self);
            gtk::glib::timeout_add_local_once(std::time::Duration::from_millis(240), move || {
                if let Some(drawer) = weak.upgrade()
                    && drawer.generation.is_current(generation)
                    && !drawer.revealer.reveals_child()
                    && !drawer.revealer.is_child_revealed()
                {
                    drawer.window.set_visible(false);
                }
            });
        }
    }
}
