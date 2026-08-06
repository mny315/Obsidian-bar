use std::{
    cell::Cell,
    panic::{AssertUnwindSafe, catch_unwind},
    rc::Rc,
    sync::OnceLock,
    time::Duration,
};

use gtk::{gdk, glib, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

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

type BackgroundJob = Box<dyn FnOnce() + Send + 'static>;

const BACKGROUND_QUEUE_CAPACITY: usize = 128;
const MAX_BACKGROUND_WORKERS: usize = 4;

fn background_queue() -> &'static async_channel::Sender<BackgroundJob> {
    static QUEUE: OnceLock<async_channel::Sender<BackgroundJob>> = OnceLock::new();
    QUEUE.get_or_init(|| {
        let (sender, receiver) = async_channel::bounded::<BackgroundJob>(BACKGROUND_QUEUE_CAPACITY);
        let worker_count = std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(2)
            .clamp(2, MAX_BACKGROUND_WORKERS);

        for worker_index in 0..worker_count {
            let receiver = receiver.clone();
            let spawn_result = std::thread::Builder::new()
                .name(format!("obsidian-worker-{worker_index}"))
                .spawn(move || {
                    while let Ok(job) = receiver.recv_blocking() {
                        if catch_unwind(AssertUnwindSafe(job)).is_err() {
                            tracing::error!(
                                worker_index,
                                "background job panicked; worker kept alive"
                            );
                        }
                    }
                });
            if let Err(error) = spawn_result {
                tracing::error!(%error, worker_index, "failed to start background worker");
            }
        }

        sender
    })
}

fn background_receiver<T, Job>(job: Job) -> async_channel::Receiver<T>
where
    T: Send + 'static,
    Job: FnOnce() -> T + Send + 'static,
{
    let (sender, receiver) = async_channel::bounded::<T>(1);
    let task: BackgroundJob = Box::new(move || {
        let _ = sender.send_blocking(job());
    });

    let queue = background_queue().clone();
    glib::MainContext::default().spawn_local(async move {
        if queue.send(task).await.is_err() {
            tracing::error!("background worker queue is unavailable; job was cancelled");
        }
    });
    receiver
}

fn run_background<T, Job, Done>(job: Job, done: Done)
where
    T: Send + 'static,
    Job: FnOnce() -> T + Send + 'static,
    Done: FnOnce(T) + 'static,
{
    let receiver = background_receiver(job);
    glib::MainContext::default().spawn_local(async move {
        if let Ok(result) = receiver.recv().await {
            done(result);
        }
    });
}

async fn run_background_async<T, Job>(job: Job) -> Option<T>
where
    T: Send + 'static,
    Job: FnOnce() -> T + Send + 'static,
{
    let receiver = background_receiver(job);
    receiver.recv().await.ok()
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
    if let Some(application) = window.application() {
        application.remove_window(window);
    }
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

fn reset_hidden_popup_state(
    reveal: &PopupReveal,
    focus_armed: &Cell<bool>,
    trigger: &gtk::Button,
    open_css_class: &str,
) {
    reveal.reset_hidden();
    focus_armed.set(false);
    trigger.remove_css_class(open_css_class);
}

fn attach_bar_click_dismiss(
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

fn attach_popup_focus_dismiss(
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

fn attach_popup_escape_handler<T: 'static>(
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

fn run_when_popup_visible<T: 'static>(
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

fn attach_popup_lifecycle<T: 'static>(
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
const BAR_POPUP_WIDTH: i32 = 392;

fn build_bar_popup(
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

fn build_bar_popup_left(
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

const POPUP_FADE_OPEN_MS: f64 = 220.0;
const POPUP_FADE_CLOSE_MS: f64 = 170.0;
const POPUP_SAFE_OPACITY_FLOOR: f64 = 0.42;

struct PopupRevealState {
    generation: Generation,
    revealed: Cell<bool>,
    root: gtk::Box,
    child: gtk::Widget,
}

#[derive(Clone)]
struct PopupReveal(Rc<PopupRevealState>);

impl PopupReveal {
    fn masked(child: gtk::Widget) -> Self {
        child.set_opacity(0.0);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("widget-popup-revealer");
        root.append(&child);

        Self(Rc::new(PopupRevealState {
            generation: Generation::default(),
            revealed: Cell::new(false),
            root,
            child,
        }))
    }

    fn widget(&self) -> &gtk::Box {
        &self.0.root
    }

    fn show(&self, window: &gtk::ApplicationWindow) -> u64 {
        let generation = self.0.generation.bump();
        self.0.revealed.set(true);

        if !window.is_visible() {
            self.0.child.set_opacity(POPUP_SAFE_OPACITY_FLOOR);
            window.present();
        }

        self.animate_opacity(window, generation, 1.0, POPUP_FADE_OPEN_MS, false);
        generation
    }

    fn hide(&self, window: &gtk::ApplicationWindow) {
        if !window.is_visible() {
            self.reset_hidden();
            return;
        }

        let generation = self.0.generation.bump();
        self.0.revealed.set(false);
        self.animate_opacity(window, generation, 0.0, POPUP_FADE_CLOSE_MS, true);
    }

    fn animate_opacity(
        &self,
        window: &gtk::ApplicationWindow,
        generation: u64,
        target: f64,
        duration_ms: f64,
        hide_on_finish: bool,
    ) {
        let start_opacity = self.0.child.opacity();
        let weak_child = self.0.child.downgrade();
        let weak_window = window.downgrade();
        let weak_state = Rc::downgrade(&self.0);
        let start_time_us = Cell::new(None::<i64>);

        self.0.root.add_tick_callback(move |_, frame_clock| {
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
            let eased = if hide_on_finish {
                t * t * t
            } else {
                1.0 - (1.0 - t).powi(3)
            };
            let opacity = start_opacity + (target - start_opacity) * eased;

            let Some(child) = weak_child.upgrade() else {
                return glib::ControlFlow::Break;
            };

            if hide_on_finish && opacity <= POPUP_SAFE_OPACITY_FLOOR {
                if !state.revealed.get()
                    && let Some(window) = weak_window.upgrade()
                {
                    window.set_visible(false);
                }
                return glib::ControlFlow::Break;
            }

            child.set_opacity(opacity.clamp(0.0, 1.0));

            if t < 1.0 {
                glib::ControlFlow::Continue
            } else {
                child.set_opacity(target);
                glib::ControlFlow::Break
            }
        });
    }

    fn reset_hidden(&self) {
        self.0.generation.bump();
        self.0.revealed.set(false);
        self.0.child.set_opacity(0.0);
    }

    fn is_current(&self, generation: u64) -> bool {
        self.0.generation.is_current(generation)
    }

    fn is_revealed(&self) -> bool {
        self.0.revealed.get()
    }
}

fn build_inline_panel(
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

fn attach_vertical_step_scroll<T: 'static>(
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

fn attach_scale_value_changed<T: 'static>(
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

fn attach_inline_revealer_behavior(
    root: &gtk::Box,
    revealer: &gtk::Revealer,
    hide_delay: Duration,
) {
    let hide_generation = Rc::new(Generation::default());
    let motion = gtk::EventControllerMotion::new();

    let generation = Rc::clone(&hide_generation);
    motion.connect_enter(move |_, _, _| {
        generation.bump();
    });

    let generation = hide_generation;
    let weak_revealer = revealer.downgrade();
    motion.connect_leave(move |_| {
        if !weak_revealer
            .upgrade()
            .is_some_and(|revealer| revealer.reveals_child())
        {
            return;
        }

        let token = generation.bump();
        let generation = Rc::clone(&generation);
        let weak_revealer = weak_revealer.clone();
        glib::timeout_add_local_once(hide_delay, move || {
            if !generation.is_current(token) {
                return;
            }

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
mod dbus;
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
pub mod tooltip;
pub mod tray;
pub mod wallpaper;
pub mod workspace;

#[cfg(test)]
mod tests {
    use super::Generation;

    #[test]
    fn generation_invalidates_older_values() {
        let generation = Generation::default();
        let first = generation.bump();
        assert!(generation.is_current(first));

        let second = generation.bump();
        assert!(!generation.is_current(first));
        assert!(generation.is_current(second));
    }
}
