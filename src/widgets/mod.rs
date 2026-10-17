use std::cell::Cell;

use gtk::prelude::*;

#[derive(Default)]
struct Generation(Cell<u64>);

impl Generation {
    fn current(&self) -> u64 {
        self.0.get()
    }

    fn bump(&self) -> u64 {
        let generation = self.current().wrapping_add(1);
        self.0.set(generation);
        generation
    }

    fn is_current(&self, generation: u64) -> bool {
        self.current() == generation
    }
}

#[derive(Clone, Copy, Default)]
struct RefreshState {
    busy: bool,
    pending: bool,
}

#[derive(Default)]
struct RefreshGate(Cell<RefreshState>);

impl RefreshGate {
    fn begin(&self) -> bool {
        let mut state = self.0.get();
        if state.busy {
            state.pending = true;
            self.0.set(state);
            return false;
        }

        state.busy = true;
        state.pending = false;
        self.0.set(state);
        true
    }

    fn finish(&self) -> bool {
        self.0.replace(RefreshState::default()).pending
    }
}

fn set_optional_label(label: &gtk::Label, text: Option<&str>) {
    let text = text.filter(|text| !text.is_empty());
    label.set_label(text.unwrap_or_default());
    label.set_visible(text.is_some());
}

fn clear_box(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

fn empty_state_label(text: &str, xalign: Option<f32>) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("network-empty");
    if let Some(xalign) = xalign {
        label.set_xalign(xalign);
    }
    label.set_margin_top(18);
    label.set_margin_bottom(18);
    label
}

fn build_refresh_button(icon_text: &str) -> (gtk::Button, gtk::Label, gtk::Spinner) {
    let icon = gtk::Label::new(Some(icon_text));
    icon.add_css_class("network-action-icon");

    let spinner = gtk::Spinner::new();
    spinner.add_css_class("network-refresh-spinner");
    spinner.set_halign(gtk::Align::Center);
    spinner.set_valign(gtk::Align::Center);
    spinner.set_visible(false);

    let indicator = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    indicator.add_css_class("network-refresh-indicator");
    indicator.set_halign(gtk::Align::Center);
    indicator.set_valign(gtk::Align::Center);
    indicator.append(&icon);
    indicator.append(&spinner);

    let button = gtk::Button::new();
    button.add_css_class("network-icon-button");
    button.set_valign(gtk::Align::Center);
    button.set_child(Some(&indicator));

    (button, icon, spinner)
}

fn set_spinner_active(icon: &gtk::Label, spinner: &gtk::Spinner, active: bool) {
    icon.set_visible(!active);
    spinner.set_visible(active);
    if active {
        spinner.start();
    } else {
        spinner.stop();
    }
}

fn detach_application_window(window: &gtk::ApplicationWindow) {
    window.set_visible(false);
    // GTK 4.22's Wayland session cleanup assumes a realized surface when an
    // application window is removed. Create it without mapping unused popups.
    if !window.is_realized() && window.application().is_some() {
        WidgetExt::realize(window);
    }
    // GTK also owns a reference through its toplevel list. Removing only the
    // application reference leaves hidden windows alive after monitor changes.
    window.destroy();
}

fn make_window_click_through(window: &gtk::ApplicationWindow) {
    window.set_can_target(false);
    window.connect_realize(|window| {
        if let Some(surface) = window.surface() {
            // can-target only affects GTK picking. The compositor needs an
            // empty native input region to pass input to the window underneath.
            surface.set_input_region(Some(&gtk::cairo::Region::create()));
        }
    });
}

fn build_quick_toggle_button(
    icon: &str,
    trigger_css_class: &str,
    icon_css_classes: &[&str],
) -> (gtk::Button, gtk::Label) {
    let label = gtk::Label::new(Some(icon));
    label.add_css_class("module-icon");
    label.add_css_class("control-trigger-icon");
    for &css_class in icon_css_classes {
        label.add_css_class(css_class);
    }
    label.set_halign(gtk::Align::Center);
    label.set_xalign(0.5);
    label.set_yalign(0.5);
    label.set_valign(gtk::Align::Center);

    let button = gtk::Button::new();
    button.add_css_class("icon-button");
    button.add_css_class("quick-toggle");
    button.add_css_class(trigger_css_class);
    button.set_valign(gtk::Align::Center);
    button.set_child(Some(&label));
    (button, label)
}

mod popup;
use popup::popup_animation_progress;
use popup::{
    BAR_POPUP_TOP_MARGIN, BAR_POPUP_WIDTH, PopupReveal, attach_bar_click_dismiss,
    attach_popup_escape_handler, attach_popup_focus_dismiss, attach_popup_lifecycle,
    build_bar_popup, build_bar_popup_left, reset_hidden_popup_state, run_when_popup_visible,
};
mod inline_panel;
use inline_panel::{
    attach_inline_revealer_behavior, attach_scale_value_changed, attach_vertical_step_scroll,
    build_inline_panel,
};
mod background;
use background::{run_background, run_background_async};
mod timer;
pub(crate) use timer::Timer;
mod change_signal;
mod snapshot_cache;

pub mod audio;
mod audio_backend;
pub mod audio_spectrum;
mod audio_visual;
pub mod bar_features;
pub mod bluetooth;
mod bluetooth_backend;
pub mod brightness;
pub mod clock;
mod command;
mod css_transition;
mod frame_callback;
pub(crate) use frame_callback::FrameCallback;
mod dbus;
mod reorder;
#[cfg(test)]
mod test_support;
use css_transition::CssTransition;
pub mod keyboard;
pub mod launcher;
pub mod network;
mod network_backend;
pub mod osd;
mod smooth_scroll;
use smooth_scroll::{SmoothScrollConfig, install_smooth_scroll};
pub mod player;
pub mod power;
pub mod shutdown_timer;
pub mod system_monitor;
pub mod tooltip;
pub mod tray;
pub mod wallpaper;
pub mod workspace;

#[cfg(test)]
mod tests {
    use super::{Generation, RefreshGate, popup_animation_progress};

    #[test]
    #[ignore = "requires a GTK display; run with --ignored --test-threads=1"]
    fn detached_popup_releases_its_window_and_children() {
        use gtk::{gio, prelude::*};

        gtk::init().unwrap();
        let application = gtk::Application::builder()
            .application_id("dev.obsidian.WindowLifecycleTest")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        application.register(None::<&gio::Cancellable>).unwrap();
        for realized in [false, true] {
            let window = gtk::ApplicationWindow::builder()
                .application(&application)
                .build();
            window.set_hide_on_close(true);
            super::make_window_click_through(&window);
            let child = gtk::Button::new();
            window.set_child(Some(&child));
            let weak_window = window.downgrade();
            let weak_child = child.downgrade();
            if realized {
                WidgetExt::realize(&window);
            }

            super::detach_application_window(&window);
            assert!(application.windows().is_empty());
            drop(window);
            drop(child);
            assert!(weak_window.upgrade().is_none());
            assert!(weak_child.upgrade().is_none());
        }
    }

    #[test]
    fn refresh_requests_while_busy_produce_one_followup() {
        let gate = RefreshGate::default();
        assert!(gate.begin());
        for _ in 0..20 {
            assert!(!gate.begin());
        }
        assert!(gate.finish());
        assert!(gate.begin());
        assert!(!gate.finish());
        assert!(gate.begin());
    }

    #[test]
    fn generation_invalidates_older_values() {
        let generation = Generation::default();
        let first = generation.bump();
        assert!(generation.is_current(first));

        let second = generation.bump();
        assert!(!generation.is_current(first));
        assert!(generation.is_current(second));
    }

    #[test]
    fn popup_easing_has_stable_endpoints() {
        for closing in [false, true] {
            assert_eq!(popup_animation_progress(0.0, closing), 0.0);
            assert_eq!(popup_animation_progress(1.0, closing), 1.0);
        }
    }

    #[test]
    fn popup_opening_moves_early_and_closing_moves_late() {
        let opening_halfway = popup_animation_progress(0.5, false);
        let closing_halfway = popup_animation_progress(0.5, true);

        assert_eq!(opening_halfway, 0.875);
        assert_eq!(closing_halfway, 0.125);
    }
}
