//! Shared bar popup construction, focus handling and reveal animations.
use super::Generation;
use gtk::{gdk, glib, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::{cell::Cell, rc::Rc};

pub(super) fn reset_hidden_popup_state(
    reveal: &PopupReveal,
    focus_armed: &Cell<bool>,
    trigger: &gtk::Button,
    open_css_class: &str,
) {
    reveal.reset_hidden();
    focus_armed.set(false);
    trigger.remove_css_class(open_css_class);
}

pub(super) fn attach_bar_click_dismiss(
    bar_window: &gtk::ApplicationWindow,
    trigger: &gtk::Button,
    popup: &gtk::ApplicationWindow,
    close: impl Fn() + 'static,
) {
    let click = gtk::GestureClick::new();
    // Audio opens its device popup with RMB, so dismissal must observe every button.
    click.set_button(0);
    click.set_propagation_phase(gtk::PropagationPhase::Capture);

    let weak_popup = popup.downgrade();
    let weak_bar = bar_window.downgrade();
    let weak_trigger = trigger.downgrade();
    click.connect_pressed(move |_, _, x, y| {
        if !weak_popup.upgrade().is_some_and(|popup| popup.is_visible()) {
            return;
        }

        let on_trigger = weak_trigger
            .upgrade()
            .zip(weak_bar.upgrade())
            .and_then(|(trigger, bar)| trigger.compute_bounds(&bar))
            .is_some_and(|bounds| {
                let x = x as f32;
                let y = y as f32;
                x >= bounds.x()
                    && x < bounds.x() + bounds.width()
                    && y >= bounds.y()
                    && y < bounds.y() + bounds.height()
            });

        if !on_trigger {
            close();
        }
    });
    bar_window.add_controller(click);
}

pub(super) fn attach_popup_focus_dismiss(
    popup: &gtk::ApplicationWindow,
    bar_window: &gtk::ApplicationWindow,
    focus_armed: &Rc<Cell<bool>>,
    close: impl Fn() + 'static,
) {
    let weak_bar = bar_window.downgrade();
    let focus_armed = Rc::clone(focus_armed);
    let close = Rc::new(close);

    popup.connect_is_active_notify(move |popup| {
        if popup.is_active() {
            if popup.is_visible() {
                focus_armed.set(true);
            }
            return;
        }

        if !popup.is_visible() || !focus_armed.get() {
            return;
        }

        let weak_popup = popup.downgrade();
        let weak_bar = weak_bar.clone();
        let focus_armed = Rc::clone(&focus_armed);
        let close = Rc::clone(&close);
        glib::idle_add_local_once(move || {
            let Some(popup) = weak_popup.upgrade() else {
                return;
            };
            if !popup.is_visible() || popup.is_active() || !focus_armed.get() {
                return;
            }
            if weak_bar.upgrade().is_some_and(|bar| bar.is_active()) {
                return;
            }
            (close.as_ref())();
        });
    });
}

pub(super) fn attach_popup_escape_handler<T: 'static>(
    popup: &gtk::ApplicationWindow,
    owner: std::rc::Weak<T>,
    handler: impl Fn(&Rc<T>) -> bool + 'static,
) {
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed(move |_, key, _, _| {
        let handled =
            key == gdk::Key::Escape && owner.upgrade().is_some_and(|owner| handler(&owner));
        if handled {
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    popup.add_controller(keys);
}

pub(super) fn run_when_popup_visible<T: 'static>(
    popup: &gtk::ApplicationWindow,
    reveal: &PopupReveal,
    generation: u64,
    owner: std::rc::Weak<T>,
    action: impl Fn(&Rc<T>) + 'static,
) {
    let weak_popup = popup.downgrade();
    let reveal = reveal.clone();
    glib::idle_add_local_once(move || {
        let Some(owner) = owner.upgrade() else {
            return;
        };
        if !reveal.is_current(generation)
            || !weak_popup.upgrade().is_some_and(|popup| popup.is_visible())
        {
            return;
        }
        action(&owner);
    });
}

pub(super) fn attach_popup_lifecycle<T: 'static>(
    bar_window: &gtk::ApplicationWindow,
    trigger: &gtk::Button,
    popup: &gtk::ApplicationWindow,
    focus_armed: &Rc<Cell<bool>>,
    owner: std::rc::Weak<T>,
    close: impl Fn(&Rc<T>) + 'static,
    hidden: impl Fn(&Rc<T>) + 'static,
) {
    let close = Rc::new(close);

    let focus_owner = owner.clone();
    let focus_close = Rc::clone(&close);
    attach_popup_focus_dismiss(popup, bar_window, focus_armed, move || {
        if let Some(owner) = focus_owner.upgrade() {
            focus_close(&owner);
        }
    });

    let hidden_owner = owner.clone();
    popup.connect_visible_notify(move |popup| {
        if !popup.is_visible()
            && let Some(owner) = hidden_owner.upgrade()
        {
            hidden(&owner);
        }
    });

    attach_bar_click_dismiss(bar_window, trigger, popup, move || {
        if let Some(owner) = owner.upgrade() {
            close(&owner);
        }
    });
}

pub(super) const BAR_POPUP_TOP_MARGIN: i32 = 53;
const BAR_POPUP_HORIZONTAL_MARGIN: i32 = 15;
pub(super) const BAR_POPUP_WIDTH: i32 = 392;

pub(super) fn build_bar_popup(
    application: &gtk::Application,
    monitor: &gdk::Monitor,
    namespace: &str,
    css_class: &str,
) -> gtk::ApplicationWindow {
    let popup = gtk::ApplicationWindow::builder()
        .application(application)
        .decorated(false)
        .resizable(false)
        .build();
    popup.add_css_class(css_class);
    popup.init_layer_shell();
    popup.set_namespace(Some(namespace));
    popup.set_layer(Layer::Top);
    popup.set_keyboard_mode(KeyboardMode::OnDemand);
    popup.set_monitor(Some(monitor));
    popup.set_anchor(Edge::Top, true);
    popup.set_anchor(Edge::Right, true);
    popup.set_anchor(Edge::Left, false);
    popup.set_anchor(Edge::Bottom, false);
    popup.set_exclusive_zone(-1);
    popup.set_margin(Edge::Top, BAR_POPUP_TOP_MARGIN);
    popup.set_margin(Edge::Right, BAR_POPUP_HORIZONTAL_MARGIN);
    popup.set_hide_on_close(true);
    popup
}

pub(super) fn build_bar_popup_left(
    application: &gtk::Application,
    monitor: &gdk::Monitor,
    namespace: &str,
    css_class: &str,
) -> gtk::ApplicationWindow {
    let popup = build_bar_popup(application, monitor, namespace, css_class);
    popup.set_anchor(Edge::Right, false);
    popup.set_anchor(Edge::Left, true);
    popup.set_margin(Edge::Right, 0);
    popup.set_margin(Edge::Left, BAR_POPUP_HORIZONTAL_MARGIN);
    popup
}

const POPUP_FADE_OPEN_MS: f64 = 260.0;
const POPUP_FADE_CLOSE_MS: f64 = 180.0;
const POPUP_OPEN_OFFSET: i32 = 9;
const POPUP_CLOSE_OFFSET: i32 = 7;
// Keep a clearly visible surface until the layer window is unmapped. If the
// child reaches full transparency first, Niri can render one frame containing
// only the blur region after the popup itself has seemingly disappeared.
const POPUP_CLOSE_OPACITY_FLOOR: f64 = 0.58;

struct PopupRevealState {
    generation: Generation,
    revealed: Cell<bool>,
    rest_top_margin: Cell<Option<i32>>,
    root: gtk::Box,
    child: gtk::Widget,
    animation: super::FrameCallback,
}

#[derive(Clone)]
pub(super) struct PopupReveal(Rc<PopupRevealState>);

impl PopupReveal {
    pub(super) fn masked(child: gtk::Widget) -> Self {
        child.set_opacity(0.0);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("widget-popup-revealer");
        root.append(&child);

        Self(Rc::new(PopupRevealState {
            generation: Generation::default(),
            revealed: Cell::new(false),
            rest_top_margin: Cell::new(None),
            root,
            child,
            animation: super::FrameCallback::default(),
        }))
    }

    pub(super) fn widget(&self) -> &gtk::Box {
        &self.0.root
    }

    pub(super) fn show(&self, window: &gtk::ApplicationWindow) -> u64 {
        let generation = self.0.generation.bump();
        self.0.revealed.set(true);
        let rest_top_margin = self
            .0
            .rest_top_margin
            .get()
            .unwrap_or_else(|| window.margin(Edge::Top));
        self.0.rest_top_margin.set(Some(rest_top_margin));

        if !window.is_visible() {
            // Move the layer surface itself so Niri's blur, the popup frame and
            // its contents all begin at exactly the same position.
            window.set_margin(Edge::Top, rest_top_margin - POPUP_OPEN_OFFSET);
            self.0.child.set_opacity(0.0);
            window.present();
        }

        self.animate(
            window,
            generation,
            1.0,
            rest_top_margin,
            POPUP_FADE_OPEN_MS,
            false,
        );
        generation
    }

    pub(super) fn sync_top_anchor(&self, window: &gtk::ApplicationWindow) {
        self.0.rest_top_margin.set(Some(window.margin(Edge::Top)));
    }

    pub(super) fn hide(&self, window: &gtk::ApplicationWindow) {
        if !window.is_visible() {
            self.reset_hidden();
            return;
        }
        if !self.0.revealed.get() {
            return;
        }

        let generation = self.0.generation.bump();
        self.0.revealed.set(false);
        if self.0.child.opacity() <= POPUP_CLOSE_OPACITY_FLOOR {
            self.0.animation.cancel();
            // An opening interrupted before reaching the safe floor must not
            // brighten again or leave a mostly transparent blur-only surface.
            window.set_visible(false);
            return;
        }
        let rest_top_margin = self
            .0
            .rest_top_margin
            .get()
            .unwrap_or_else(|| window.margin(Edge::Top));
        self.animate(
            window,
            generation,
            POPUP_CLOSE_OPACITY_FLOOR,
            rest_top_margin - POPUP_CLOSE_OFFSET,
            POPUP_FADE_CLOSE_MS,
            true,
        );
    }

    fn animate(
        &self,
        window: &gtk::ApplicationWindow,
        generation: u64,
        target_opacity: f64,
        target_top_margin: i32,
        duration_ms: f64,
        hide_on_finish: bool,
    ) {
        if !window.settings().is_gtk_enable_animations() {
            self.0.animation.cancel();
            self.0.child.set_opacity(target_opacity);
            window.set_margin(Edge::Top, target_top_margin);
            if hide_on_finish {
                window.set_visible(false);
            }
            return;
        }
        let start_opacity = self.0.child.opacity();
        let start_top_margin = window.margin(Edge::Top);
        let weak_child = self.0.child.downgrade();
        let weak_window = window.downgrade();
        let weak_state = Rc::downgrade(&self.0);
        let start_time_us = Cell::new(None::<i64>);

        self.0.animation.start(&self.0.root, move |_, frame_clock| {
            let Some(state) = weak_state.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if !state.generation.is_current(generation) {
                return glib::ControlFlow::Break;
            }

            let now_us = frame_clock.frame_time();
            let start_us = match start_time_us.get() {
                Some(value) => value,
                None => {
                    start_time_us.set(Some(now_us));
                    now_us
                }
            };

            let elapsed_ms = (now_us - start_us).max(0) as f64 / 1_000.0;
            let t = (elapsed_ms / duration_ms).clamp(0.0, 1.0);
            let eased = popup_animation_progress(t, hide_on_finish);
            let opacity = start_opacity + (target_opacity - start_opacity) * eased;
            let top_margin =
                start_top_margin as f64 + (target_top_margin - start_top_margin) as f64 * eased;

            let (Some(child), Some(window)) = (weak_child.upgrade(), weak_window.upgrade()) else {
                return glib::ControlFlow::Break;
            };

            child.set_opacity(opacity.clamp(0.0, 1.0));
            window.set_margin(Edge::Top, top_margin.round() as i32);

            if t < 1.0 {
                glib::ControlFlow::Continue
            } else {
                child.set_opacity(target_opacity);
                window.set_margin(Edge::Top, target_top_margin);
                if hide_on_finish && !state.revealed.get() {
                    // Do not clear opacity or reset the margin here. Keeping the last
                    // rendered popup frame intact until unmap prevents Niri's
                    // blur region from visually outliving the window contents.
                    window.set_visible(false);
                }
                glib::ControlFlow::Break
            }
        });
    }

    pub(super) fn reset_hidden(&self) {
        self.0.animation.cancel();
        self.0.generation.bump();
        self.0.revealed.set(false);
        // Visual state is intentionally preserved while GTK/Niri unmaps the
        // surface. show() initializes the next opening while it is still hidden.
    }

    pub(super) fn is_current(&self, generation: u64) -> bool {
        self.0.generation.is_current(generation)
    }

    pub(super) fn is_revealed(&self) -> bool {
        self.0.revealed.get()
    }
}

pub(super) fn popup_animation_progress(t: f64, closing: bool) -> f64 {
    let t = t.clamp(0.0, 1.0);
    if closing {
        t * t * t
    } else {
        1.0 - (1.0 - t).powi(3)
    }
}
