use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    sync::Arc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use gtk::{gdk, glib, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use pipewire as pw;
use pw::{
    metadata::Metadata,
    node::Node,
    proxy::{Listener, ProxyT},
    spa::param::ParamType,
    types::ObjectType,
};
use tracing::{debug, warn};

use super::{
    Generation, PopupReveal, RefreshGate,
    audio_backend::{AudioBackend, VolumeState, default_audio_backend, invalidate_audio_caches},
    audio_visual::{ICON_HIGH, volume_icon},
    detach_application_window, run_background,
};

const NAMESPACE: &str = "obsidian-bar-osd";
const AUTO_HIDE_DELAY: Duration = Duration::from_millis(1200);
const LOCAL_CHANGE_SUPPRESS: Duration = Duration::from_millis(900);
const EVENT_MONITOR_RETRY_BASE_DELAY: Duration = Duration::from_secs(1);
const EVENT_MONITOR_RETRY_MAX_DELAY: Duration = Duration::from_secs(30);
const VALUE_ANIMATION_MS: f64 = 180.0;
const BOTTOM_MARGIN: i32 = 140;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VolumeKey {
    percent: i32,
    muted: bool,
}

impl From<VolumeState> for VolumeKey {
    fn from(state: VolumeState) -> Self {
        Self {
            percent: (state.volume * 100.0).round() as i32,
            muted: state.muted,
        }
    }
}

struct OsdState {
    window: gtk::ApplicationWindow,
    reveal: PopupReveal,
    icon: gtk::Label,
    title: gtk::Label,
    percent: gtk::Label,
    slider: gtk::Scale,
    backend: Arc<dyn AudioBackend>,
    refresh: RefreshGate,
    last_volume: Cell<Option<VolumeKey>>,
    suppress_until: Cell<Option<Instant>>,
    hide_generation: Generation,
    value_generation: Generation,
    event_refresh_pending: Cell<bool>,
    event_monitor: RefCell<Option<AudioEventMonitor>>,
    event_monitor_retry_pending: Cell<bool>,
    event_monitor_retry_attempt: Cell<u32>,
    audio_change_subscribers: RefCell<Vec<Box<dyn Fn() -> bool>>>,
    running: Cell<bool>,
}

#[derive(Clone)]
pub struct OsdController(Rc<OsdState>);

impl OsdController {
    pub fn new(application: &gtk::Application, monitor: &gdk::Monitor) -> Self {
        let window = gtk::ApplicationWindow::builder()
            .application(application)
            .decorated(false)
            .resizable(false)
            .build();
        window.add_css_class("osd-window");
        window.init_layer_shell();
        window.set_namespace(Some(NAMESPACE));
        window.set_layer(Layer::Overlay);
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_monitor(Some(monitor));
        window.set_anchor(Edge::Bottom, true);
        window.set_anchor(Edge::Top, false);
        window.set_anchor(Edge::Left, false);
        window.set_anchor(Edge::Right, false);
        window.set_exclusive_zone(-1);
        window.set_margin(Edge::Bottom, BOTTOM_MARGIN);
        window.set_hide_on_close(true);
        window.set_focusable(false);
        window.set_can_target(false);

        let body = gtk::Box::new(gtk::Orientation::Vertical, 10);
        body.add_css_class("osd-body");
        body.set_can_target(false);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        header.add_css_class("osd-header");
        header.set_valign(gtk::Align::Center);
        header.set_can_target(false);

        let icon = gtk::Label::new(Some(ICON_HIGH));
        icon.add_css_class("osd-icon");
        icon.set_can_target(false);

        let title = gtk::Label::new(Some("Volume"));
        title.add_css_class("osd-title");
        title.set_xalign(0.0);
        title.set_hexpand(true);
        title.set_can_target(false);

        let percent = gtk::Label::new(Some("0%"));
        percent.add_css_class("osd-percent");
        percent.set_can_target(false);

        header.append(&icon);
        header.append(&title);
        header.append(&percent);

        let slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.01);
        slider.add_css_class("slider-control");
        slider.add_css_class("osd-slider");
        slider.set_draw_value(false);
        slider.set_hexpand(true);
        slider.set_can_target(false);
        slider.set_focusable(false);

        body.append(&header);
        body.append(&slider);

        let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
        frame.add_css_class("osd-frame");
        frame.set_size_request(300, 82);
        frame.set_overflow(gtk::Overflow::Hidden);
        frame.set_can_target(false);
        frame.append(&body);

        let reveal = PopupReveal::masked(frame.upcast::<gtk::Widget>());
        reveal.widget().add_css_class("osd-placement");
        reveal.widget().set_can_target(false);
        window.set_child(Some(reveal.widget()));

        let state = Rc::new(OsdState {
            window,
            reveal,
            icon,
            title,
            percent,
            slider,
            backend: default_audio_backend(),
            refresh: RefreshGate::default(),
            last_volume: Cell::new(None),
            suppress_until: Cell::new(None),
            hide_generation: Generation::default(),
            value_generation: Generation::default(),
            event_refresh_pending: Cell::new(false),
            event_monitor: RefCell::new(None),
            event_monitor_retry_pending: Cell::new(false),
            event_monitor_retry_attempt: Cell::new(0),
            audio_change_subscribers: RefCell::new(Vec::new()),
            running: Cell::new(true),
        });
        let controller = Self(state);
        controller.start_event_monitor();
        controller.refresh_volume(false);
        controller
    }

    pub fn set_monitor(&self, monitor: &gdk::Monitor) {
        self.0.window.set_monitor(Some(monitor));
    }

    pub fn subscribe_audio_changes(&self, callback: impl Fn() -> bool + 'static) {
        self.0
            .audio_change_subscribers
            .borrow_mut()
            .push(Box::new(callback));
    }

    pub fn suppress_local_audio_change(&self) {
        self.0
            .suppress_until
            .set(Some(Instant::now() + LOCAL_CHANGE_SUPPRESS));
    }

    pub fn shutdown(&self) {
        if !self.0.running.replace(false) {
            return;
        }

        self.0.hide_generation.bump();
        self.0.value_generation.bump();
        self.0.event_refresh_pending.set(false);
        drop(self.0.event_monitor.borrow_mut().take());
        self.0.reveal.reset_hidden();
        detach_application_window(&self.0.window);
    }

    fn start_event_monitor(&self) {
        if !self.0.running.get() || self.0.event_monitor.borrow().is_some() {
            return;
        }

        let (sender, receiver) = async_channel::bounded::<()>(1);
        let (stopped_sender, stopped_receiver) = async_channel::bounded::<()>(1);
        match AudioEventMonitor::spawn(sender, stopped_sender) {
            Ok(monitor) => {
                self.0.event_monitor.replace(Some(monitor));
            }
            Err(error) => {
                warn!(%error, "audio OSD event monitor unavailable");
                self.schedule_event_monitor_restart();
                return;
            }
        };

        let weak = Rc::downgrade(&self.0);
        glib::MainContext::default().spawn_local(async move {
            while receiver.recv().await.is_ok() {
                let Some(state) = weak.upgrade() else {
                    break;
                };
                if !state.running.get() {
                    break;
                }
                state.event_monitor_retry_attempt.set(0);
                invalidate_audio_caches();
                let controller = Self(state);
                controller.notify_audio_change_subscribers();
                controller.schedule_event_refresh();
            }
        });

        let weak = Rc::downgrade(&self.0);
        glib::MainContext::default().spawn_local(async move {
            if stopped_receiver.recv().await.is_err() {
                return;
            }
            let Some(state) = weak.upgrade() else {
                return;
            };
            if !state.running.get() {
                return;
            }

            let controller = Self(state);
            drop(controller.0.event_monitor.borrow_mut().take());
            controller.schedule_event_monitor_restart();
        });
    }

    fn schedule_event_monitor_restart(&self) {
        if !self.0.running.get() || self.0.event_monitor_retry_pending.replace(true) {
            return;
        }

        let attempt = self.0.event_monitor_retry_attempt.get();
        self.0
            .event_monitor_retry_attempt
            .set(attempt.saturating_add(1));
        let multiplier = 1_u32 << attempt.min(5);
        let delay = EVENT_MONITOR_RETRY_BASE_DELAY
            .saturating_mul(multiplier)
            .min(EVENT_MONITOR_RETRY_MAX_DELAY);

        let weak = Rc::downgrade(&self.0);
        glib::timeout_add_local_once(delay, move || {
            let Some(state) = weak.upgrade() else {
                return;
            };
            state.event_monitor_retry_pending.set(false);
            if state.running.get() {
                Self(state).start_event_monitor();
            }
        });
    }

    fn notify_audio_change_subscribers(&self) {
        self.0
            .audio_change_subscribers
            .borrow_mut()
            .retain(|callback| callback());
    }

    fn schedule_event_refresh(&self) {
        if self.0.event_refresh_pending.replace(true) {
            return;
        }

        let weak = Rc::downgrade(&self.0);
        glib::idle_add_local_once(move || {
            let Some(state) = weak.upgrade() else {
                return;
            };
            state.event_refresh_pending.set(false);
            if state.running.get() {
                Self(state).refresh_volume(true);
            }
        });
    }

    fn refresh_volume(&self, notify: bool) {
        if !self.0.refresh.begin() {
            return;
        }

        let backend = Arc::clone(&self.0.backend);
        let weak = Rc::downgrade(&self.0);
        run_background(
            move || backend.volume(),
            move |result| {
                let Some(state) = weak.upgrade() else {
                    return;
                };
                let retry = state.refresh.finish();
                let controller = Self(state);

                match result {
                    Ok(volume) => controller.apply_volume(volume, notify),
                    Err(error) => debug!(%error, "failed to refresh audio OSD volume"),
                }

                if retry {
                    controller.refresh_volume(true);
                }
            },
        );
    }

    fn apply_volume(&self, volume: VolumeState, notify: bool) {
        let key = VolumeKey::from(volume);
        let previous = self.0.last_volume.replace(Some(key));

        if previous.is_none() || previous == Some(key) {
            self.set_static_value(volume);
            return;
        }

        if notify && !self.is_suppressed() {
            self.present(volume);
        } else {
            self.set_static_value(volume);
        }
    }

    fn is_suppressed(&self) -> bool {
        let now = Instant::now();
        match self.0.suppress_until.get() {
            Some(value) if now < value => true,
            Some(_) => {
                self.0.suppress_until.set(None);
                false
            }
            None => false,
        }
    }

    fn present(&self, volume: VolumeState) {
        self.update_content(volume);
        self.animate_value(volume.volume);

        self.0.reveal.show(&self.0.window);
        let generation = self.0.hide_generation.bump();
        let weak = Rc::downgrade(&self.0);
        glib::timeout_add_local_once(AUTO_HIDE_DELAY, move || {
            let Some(state) = weak.upgrade() else {
                return;
            };
            if state.running.get() && state.hide_generation.is_current(generation) {
                state.reveal.hide(&state.window);
            }
        });
    }

    fn set_static_value(&self, volume: VolumeState) {
        self.0.value_generation.bump();
        self.0.slider.set_value(volume.volume);
        self.update_content(volume);
    }

    fn update_content(&self, volume: VolumeState) {
        self.0
            .icon
            .set_text(volume_icon(volume.volume, volume.muted));
        self.0.title.set_text(if volume.muted {
            "Sound muted"
        } else {
            "Volume"
        });
        self.0
            .percent
            .set_text(&format!("{}%", VolumeKey::from(volume).percent));
    }

    fn animate_value(&self, target: f64) {
        let target = target.clamp(0.0, 1.0);
        let start = self.0.slider.value();
        if (start - target).abs() < 0.001 {
            self.0.value_generation.bump();
            self.0.slider.set_value(target);
            return;
        }

        let generation = self.0.value_generation.bump();
        let weak_state = Rc::downgrade(&self.0);
        let weak_slider = self.0.slider.downgrade();
        let start_time_us = Cell::new(None::<i64>);

        self.0.slider.add_tick_callback(move |_, frame_clock| {
            let Some(state) = weak_state.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if !state.running.get() || !state.value_generation.is_current(generation) {
                return glib::ControlFlow::Break;
            }

            let now_us = frame_clock.frame_time();
            let started_us = match start_time_us.get() {
                Some(value) => value,
                None => {
                    start_time_us.set(Some(now_us));
                    now_us
                }
            };
            let elapsed_ms = (now_us - started_us).max(0) as f64 / 1_000.0;
            let progress = (elapsed_ms / VALUE_ANIMATION_MS).clamp(0.0, 1.0);
            let eased = 1.0 - (1.0 - progress).powi(3);

            let Some(slider) = weak_slider.upgrade() else {
                return glib::ControlFlow::Break;
            };
            slider.set_value(start + (target - start) * eased);

            if progress < 1.0 {
                glib::ControlFlow::Continue
            } else {
                slider.set_value(target);
                glib::ControlFlow::Break
            }
        });
    }
}

struct AudioEventMonitor {
    stop: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl AudioEventMonitor {
    fn spawn(
        sender: async_channel::Sender<()>,
        stopped_sender: async_channel::Sender<()>,
    ) -> Result<Self, String> {
        let (stop, stop_receiver) = pw::channel::channel();
        let thread = thread::Builder::new()
            .name("pipewire-audio-monitor".to_owned())
            .spawn(move || {
                if let Err(error) = run_audio_event_monitor(sender, stop_receiver) {
                    warn!(%error, "PipeWire audio event monitor stopped");
                }
                let _ = stopped_sender.try_send(());
            })
            .map_err(|error| format!("failed to start PipeWire audio monitor thread: {error}"))?;

        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for AudioEventMonitor {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Default)]
struct PipeWireSubscriptions {
    proxies: HashMap<u32, Box<dyn ProxyT>>,
    listeners: HashMap<u32, Box<dyn Listener>>,
}

impl PipeWireSubscriptions {
    fn insert(&mut self, global_id: u32, proxy: Box<dyn ProxyT>, listener: Box<dyn Listener>) {
        self.remove(global_id);
        self.proxies.insert(global_id, proxy);
        self.listeners.insert(global_id, listener);
    }

    fn remove(&mut self, global_id: u32) {
        self.listeners.remove(&global_id);
        self.proxies.remove(&global_id);
    }
}

fn run_audio_event_monitor(
    sender: async_channel::Sender<()>,
    stop_receiver: pw::channel::Receiver<()>,
) -> Result<(), String> {
    pw::init();

    let main_loop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|error| format!("failed to create PipeWire main loop: {error}"))?;
    let context = pw::context::ContextRc::new(&main_loop, None)
        .map_err(|error| format!("failed to create PipeWire context: {error}"))?;
    let core = context
        .connect_rc(None)
        .map_err(|error| format!("failed to connect to PipeWire: {error}"))?;
    let registry = core
        .get_registry_rc()
        .map_err(|error| format!("failed to get PipeWire registry: {error}"))?;

    let _stop_receiver = stop_receiver.attach(main_loop.loop_(), {
        let main_loop = main_loop.clone();
        move |_| main_loop.quit()
    });

    let subscriptions = Rc::new(RefCell::new(PipeWireSubscriptions::default()));
    let registry_weak = registry.downgrade();
    let added_subscriptions = Rc::clone(&subscriptions);
    let removed_subscriptions = Rc::clone(&subscriptions);

    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            let Some(registry) = registry_weak.upgrade() else {
                return;
            };

            match global.type_ {
                ObjectType::Node
                    if is_audio_sink_class(
                        global
                            .props
                            .as_ref()
                            .and_then(|props| props.get("media.class")),
                    ) =>
                {
                    let node: Node = match registry.bind(global) {
                        Ok(node) => node,
                        Err(error) => {
                            warn!(
                                global_id = global.id,
                                %error,
                                "failed to bind PipeWire audio sink"
                            );
                            return;
                        }
                    };

                    let event_sender = sender.clone();
                    let listener = node
                        .add_listener_local()
                        .param(move |_sequence, id, _index, _next, _param| {
                            if id == ParamType::Props {
                                let _ = event_sender.try_send(());
                            }
                        })
                        .register();
                    node.subscribe_params(&[ParamType::Props]);

                    added_subscriptions.borrow_mut().insert(
                        global.id,
                        Box::new(node),
                        Box::new(listener),
                    );
                }
                ObjectType::Metadata
                    if is_default_metadata(
                        global
                            .props
                            .as_ref()
                            .and_then(|props| props.get("metadata.name")),
                    ) =>
                {
                    let metadata: Metadata = match registry.bind(global) {
                        Ok(metadata) => metadata,
                        Err(error) => {
                            warn!(
                                global_id = global.id,
                                %error,
                                "failed to bind PipeWire metadata"
                            );
                            return;
                        }
                    };

                    let event_sender = sender.clone();
                    let listener = metadata
                        .add_listener_local()
                        .property(move |_subject, key, _type, _value| {
                            if is_default_sink_key(key) {
                                let _ = event_sender.try_send(());
                            }
                            0
                        })
                        .register();

                    added_subscriptions.borrow_mut().insert(
                        global.id,
                        Box::new(metadata),
                        Box::new(listener),
                    );
                }
                _ => {}
            }
        })
        .global_remove(move |global_id| {
            removed_subscriptions.borrow_mut().remove(global_id);
        })
        .register();

    main_loop.run();
    Ok(())
}

fn is_audio_sink_class(media_class: Option<&str>) -> bool {
    media_class.is_some_and(|media_class| media_class.starts_with("Audio/Sink"))
}

fn is_default_metadata(metadata_name: Option<&str>) -> bool {
    metadata_name.is_none_or(|metadata_name| metadata_name == "default")
}

fn is_default_sink_key(key: Option<&str>) -> bool {
    key.is_none_or(|key| matches!(key, "default.audio.sink" | "default.configured.audio.sink"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_audio_sink_media_classes() {
        assert!(is_audio_sink_class(Some("Audio/Sink")));
        assert!(is_audio_sink_class(Some("Audio/Sink/Virtual")));
        assert!(!is_audio_sink_class(Some("Audio/Source")));
        assert!(!is_audio_sink_class(Some("Stream/Output/Audio")));
        assert!(!is_audio_sink_class(None));
    }

    #[test]
    fn filters_default_metadata() {
        assert!(is_default_metadata(Some("default")));
        assert!(is_default_metadata(None));
        assert!(!is_default_metadata(Some("settings")));
    }

    #[test]
    fn filters_default_sink_metadata_keys() {
        assert!(is_default_sink_key(Some("default.audio.sink")));
        assert!(is_default_sink_key(Some("default.configured.audio.sink")));
        assert!(is_default_sink_key(None));
        assert!(!is_default_sink_key(Some("default.audio.source")));
    }
}
