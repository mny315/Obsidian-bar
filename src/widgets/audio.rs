use std::{
    cell::{Cell, RefCell},
    collections::BTreeSet,
    fs,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use gtk::{gdk, glib, prelude::*};
use gtk4_layer_shell::{Layer, LayerShell};
use tracing::{debug, warn};

use super::tooltip::BarTooltipExt;
use super::{
    BAR_POPUP_WIDTH, CssTransition, Generation, PopupReveal, RefreshGate,
    attach_inline_revealer_behavior, attach_popup_escape_handler, attach_popup_lifecycle,
    attach_scale_value_changed, attach_vertical_step_scroll,
    audio_backend::{AudioBackend, SinkInfo, SinkKind, default_audio_backend},
    audio_visual::{ICON_HIGH, volume_icon},
    build_bar_popup, build_inline_panel, build_quick_toggle_button, clear_box,
    detach_application_window,
    osd::OsdController,
    reset_hidden_popup_state, run_background, run_when_popup_visible,
};

const VOLUME_STEP: f64 = 0.05;
const INLINE_HIDE_DELAY: Duration = Duration::from_secs(5);
const PERCENT_FLASH_DELAY: Duration = Duration::from_millis(1200);
const VOLUME_WRITE_DEBOUNCE: Duration = Duration::from_millis(45);
const INLINE_REVEAL_DURATION_MS: u32 = 300;
const VIEW_TRANSITION_DURATION: Duration = Duration::from_millis(190);
const AUDIO_POPUP_NAMESPACE: &str = "obsidian-bar-audio";
const AUDIO_LIST_MIN_HEIGHT: i32 = 96;
const AUDIO_LIST_MAX_HEIGHT: i32 = 242;

static HIDDEN_KEYS: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
static HIDDEN_WRITE_LOCK: Mutex<()> = Mutex::new(());
static HIDDEN_WRITE_SERIAL: AtomicU64 = AtomicU64::new(0);

const ICON_HIDE: &str = "\u{f06d1}";
const ICON_RESTORE: &str = "\u{f05e1}";
const ICON_BACK: &str = "\u{f004d}";
const ICON_HEADSET: &str = "\u{f02cb}";
const ICON_HDMI: &str = "\u{f0f5f}";
const ICON_SPEAKER: &str = "\u{f04c3}";
const ICON_CHECK: &str = "\u{f012c}";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AudioViewTransition {
    HiddenForward,
    HiddenBack,
}

impl AudioViewTransition {
    const CLASSES: &[&str] = &["audio-view-hidden-forward", "audio-view-hidden-back"];

    fn css_class(self) -> &'static str {
        match self {
            Self::HiddenForward => "audio-view-hidden-forward",
            Self::HiddenBack => "audio-view-hidden-back",
        }
    }
}

#[derive(Default)]
struct AudioState {
    volume: f64,
    muted: bool,
    sinks: Vec<SinkInfo>,
    show_hidden: bool,
}

struct AudioController {
    revealer: gtk::Revealer,
    mute_button: gtk::Button,
    mute_icon: gtk::Label,
    slider: gtk::Scale,
    percent: gtk::Label,
    trigger: gtk::Button,
    trigger_label: gtk::Label,

    popup: gtk::ApplicationWindow,
    popup_root: gtk::Box,
    popup_title: gtk::Label,
    popup_status: gtk::Label,
    hidden_toggle: gtk::Button,
    hidden_toggle_label: gtk::Label,
    list_section_title: gtk::Label,
    list: gtk::Box,

    backend: Arc<dyn AudioBackend>,
    state: RefCell<AudioState>,
    updating_slider: Cell<bool>,
    volume_read: RefreshGate,
    sinks_read: RefreshGate,
    volume_revision: Generation,
    sinks_revision: Generation,
    volume_write_serial: Generation,
    volume_write_busy: Cell<bool>,
    pending_volume_write: Cell<Option<f64>>,
    mute_write_busy: Cell<bool>,
    pending_mute_write: Cell<Option<bool>>,
    sink_switch_busy: Cell<bool>,
    pending_sink_switch: RefCell<Option<SinkInfo>>,
    percent_flash_serial: Generation,
    popup_reveal: PopupReveal,
    view_transition: CssTransition,
    focus_armed: Rc<Cell<bool>>,
    osd: OsdController,
}

pub struct AudioIndicator {
    root: gtk::Box,
    revealer: gtk::Revealer,
    _controller: Rc<AudioController>,
}

impl Drop for AudioIndicator {
    fn drop(&mut self) {
        detach_application_window(&self._controller.popup);
    }
}

impl AudioIndicator {
    pub fn new(
        application: &gtk::Application,
        bar_window: &gtk::ApplicationWindow,
        monitor: &gdk::Monitor,
        osd: &OsdController,
    ) -> Self {
        let (root, revealer, panel) =
            build_inline_panel(INLINE_REVEAL_DURATION_MS, 8, "slider-panel");

        let mute_icon = gtk::Label::new(Some(ICON_HIGH));
        mute_icon.add_css_class("module-icon");

        let mute_button = gtk::Button::new();
        mute_button.add_css_class("icon-button");
        mute_button.add_css_class("panel-icon-button");
        mute_button.set_valign(gtk::Align::Center);
        mute_button.set_child(Some(&mute_icon));

        let slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.01);
        slider.add_css_class("slider-control");
        slider.set_draw_value(false);
        slider.set_hexpand(true);
        slider.set_value(0.0);

        let percent = gtk::Label::new(Some("0%"));
        percent.add_css_class("slider-value");

        panel.append(&mute_button);
        panel.append(&slider);
        panel.append(&percent);
        attach_inline_revealer_behavior(&root, &revealer, INLINE_HIDE_DELAY);

        let (trigger, trigger_label) = build_quick_toggle_button(ICON_HIGH, "audio-trigger", &[]);

        root.append(&revealer);
        root.append(&trigger);

        let popup = build_bar_popup(
            application,
            monitor,
            AUDIO_POPUP_NAMESPACE,
            "audio-popup-window",
        );
        popup.set_layer(Layer::Overlay);

        let popup_root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        popup_root.add_css_class("widget-popup-root");
        popup_root.set_focusable(true);

        let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
        frame.add_css_class("widget-popup-frame");
        frame.add_css_class("audio-popover-window");
        frame.set_overflow(gtk::Overflow::Hidden);
        frame.set_size_request(BAR_POPUP_WIDTH, -1);

        let popup_content = gtk::Box::new(gtk::Orientation::Vertical, 10);
        popup_content.add_css_class("audio-popover");

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        header.add_css_class("audio-header");
        header.set_valign(gtk::Align::Center);

        let header_text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        header_text.set_hexpand(true);
        header_text.set_valign(gtk::Align::Center);

        let popup_title = gtk::Label::new(Some("Audio outputs"));
        popup_title.add_css_class("audio-header-title");
        popup_title.set_xalign(0.0);
        popup_title.set_ellipsize(gtk::pango::EllipsizeMode::End);

        let popup_status = gtk::Label::new(Some("Output devices"));
        popup_status.add_css_class("audio-header-meta");
        popup_status.set_xalign(0.0);

        header_text.append(&popup_title);
        header_text.append(&popup_status);

        let hidden_toggle_label = gtk::Label::new(None);
        hidden_toggle_label.add_css_class("audio-hidden-toggle-label");
        hidden_toggle_label.add_css_class("audio-material-icon");
        hidden_toggle_label.set_xalign(0.5);
        hidden_toggle_label.set_yalign(0.5);
        hidden_toggle_label.set_halign(gtk::Align::Center);
        hidden_toggle_label.set_valign(gtk::Align::Center);
        hidden_toggle_label.set_size_request(20, 20);

        let hidden_toggle = gtk::Button::new();
        hidden_toggle.add_css_class("audio-hidden-toggle");
        hidden_toggle.set_halign(gtk::Align::Center);
        hidden_toggle.set_valign(gtk::Align::Center);
        hidden_toggle.set_child(Some(&hidden_toggle_label));
        hidden_toggle.set_visible(false);

        header.append(&header_text);
        header.append(&hidden_toggle);

        let list_section = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let list_section_title = gtk::Label::new(Some("Outputs"));
        list_section_title.add_css_class("audio-section-title");
        list_section_title.set_xalign(0.0);

        let list_capsule = gtk::Box::new(gtk::Orientation::Vertical, 0);
        list_capsule.add_css_class("audio-list-capsule");

        let scroller = gtk::ScrolledWindow::new();
        scroller.add_css_class("audio-list-scroller");
        scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Never);
        scroller.set_kinetic_scrolling(true);
        scroller.set_propagate_natural_height(true);
        scroller.set_min_content_height(AUDIO_LIST_MIN_HEIGHT);
        scroller.set_max_content_height(AUDIO_LIST_MAX_HEIGHT);

        let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
        list.add_css_class("audio-list-inner");
        scroller.set_child(Some(&list));
        list_capsule.append(&scroller);
        list_section.append(&list_section_title);
        list_section.append(&list_capsule);

        let view = gtk::Box::new(gtk::Orientation::Vertical, 10);
        view.add_css_class("audio-view");
        view.set_overflow(gtk::Overflow::Hidden);
        view.append(&header);
        view.append(&list_section);
        popup_content.append(&view);

        frame.append(&popup_content);
        let popup_reveal = PopupReveal::masked(frame.clone().upcast::<gtk::Widget>());
        popup_root.append(popup_reveal.widget());
        popup.set_child(Some(&popup_root));

        let view_transition = CssTransition::new(
            view.clone().upcast::<gtk::Widget>(),
            AudioViewTransition::CLASSES,
            VIEW_TRANSITION_DURATION,
        );

        let controller = Rc::new(AudioController {
            revealer,
            mute_button,
            mute_icon,
            slider,
            percent,
            trigger,
            trigger_label,
            popup,
            popup_root,
            popup_title,
            popup_status,
            hidden_toggle,
            hidden_toggle_label,
            list_section_title,
            list,
            backend: default_audio_backend(),
            state: RefCell::new(AudioState::default()),
            updating_slider: Cell::new(false),
            volume_read: RefreshGate::default(),
            sinks_read: RefreshGate::default(),
            volume_revision: Generation::default(),
            sinks_revision: Generation::default(),
            volume_write_serial: Generation::default(),
            volume_write_busy: Cell::new(false),
            pending_volume_write: Cell::new(None),
            mute_write_busy: Cell::new(false),
            pending_mute_write: Cell::new(None),
            sink_switch_busy: Cell::new(false),
            pending_sink_switch: RefCell::new(None),
            percent_flash_serial: Generation::default(),
            popup_reveal,
            view_transition,
            focus_armed: Rc::new(Cell::new(false)),
            osd: osd.clone(),
        });

        AudioController::connect(&controller, bar_window);
        let weak = Rc::downgrade(&controller);
        osd.subscribe_audio_changes(move || {
            let Some(this) = weak.upgrade() else {
                return false;
            };
            this.refresh_volume();
            if this.popup_reveal.is_revealed() && this.popup.is_active() {
                this.refresh_sinks();
            }
            true
        });
        controller.refresh_volume();
        controller.refresh_sinks();

        Self {
            root,
            revealer: controller.revealer.clone(),
            _controller: controller,
        }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    pub fn revealer(&self) -> &gtk::Revealer {
        &self.revealer
    }

    pub fn dismiss(&self) {
        self.revealer.set_reveal_child(false);
        self._controller.close_popup();
    }
}

impl AudioController {
    fn connect(this: &Rc<Self>, bar_window: &gtk::ApplicationWindow) {
        let weak = Rc::downgrade(this);
        this.trigger.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.revealer
                    .set_reveal_child(!this.revealer.reveals_child());
            }
        });

        let middle_click = gtk::GestureClick::new();
        middle_click.set_button(2);
        middle_click.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(this);
        middle_click.connect_released(move |_, _, _, _| {
            if let Some(this) = weak.upgrade() {
                this.toggle_mute();
            }
        });
        this.trigger.add_controller(middle_click);

        let right_click = gtk::GestureClick::new();
        right_click.set_button(3);
        right_click.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(this);
        right_click.connect_released(move |_, _, _, _| {
            if let Some(this) = weak.upgrade() {
                this.toggle_popup();
            }
        });
        this.trigger.add_controller(right_click);

        attach_vertical_step_scroll(
            &this.trigger,
            Rc::downgrade(this),
            VOLUME_STEP,
            |this, delta| this.adjust_volume(delta),
            |this| this.flash_percent(),
        );
        attach_scale_value_changed(
            &this.slider,
            Rc::downgrade(this),
            |this| this.updating_slider.get(),
            |this, value| this.set_volume(value),
        );

        let weak = Rc::downgrade(this);
        this.mute_button.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.toggle_mute();
            }
        });

        let weak = Rc::downgrade(this);
        this.hidden_toggle.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                let entering_hidden = {
                    let mut state = this.state.borrow_mut();
                    state.show_hidden = !state.show_hidden;
                    state.show_hidden
                };
                this.rebuild_sink_list();
                this.replay_view_transition(if entering_hidden {
                    AudioViewTransition::HiddenForward
                } else {
                    AudioViewTransition::HiddenBack
                });
            }
        });

        attach_popup_escape_handler(&this.popup, Rc::downgrade(this), |this| {
            if !this.popup.is_visible() {
                return false;
            }
            this.close_popup();
            true
        });

        attach_popup_lifecycle(
            bar_window,
            &this.trigger,
            &this.popup,
            &this.focus_armed,
            Rc::downgrade(this),
            |this| this.close_popup(),
            |this| {
                reset_hidden_popup_state(
                    &this.popup_reveal,
                    &this.focus_armed,
                    &this.trigger,
                    "audio-popup-open",
                );
                this.state.borrow_mut().show_hidden = false;
                this.clear_view_transition();
            },
        );
    }

    fn refresh_volume(self: &Rc<Self>) {
        if !self.volume_read.begin() {
            return;
        }

        let revision = self.volume_revision.current();
        let backend = Arc::clone(&self.backend);
        let weak = Rc::downgrade(self);
        run_background(
            move || backend.volume(),
            move |result| {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                let retry = this.volume_read.finish();

                if !this.volume_revision.is_current(revision) {
                    if retry {
                        this.refresh_volume();
                    }
                    return;
                }

                match result {
                    Ok(volume) => {
                        {
                            let mut state = this.state.borrow_mut();
                            state.volume = volume.volume;
                            state.muted = volume.muted;
                        }
                        this.update_volume_ui(true);
                    }
                    Err(error) => debug!(%error, "failed to refresh audio volume"),
                }

                if retry {
                    this.refresh_volume();
                }
            },
        );
    }

    fn refresh_sinks(self: &Rc<Self>) {
        if !self.sinks_read.begin() {
            return;
        }

        let revision = self.sinks_revision.current();
        let backend = Arc::clone(&self.backend);
        let weak = Rc::downgrade(self);
        run_background(
            move || backend.sinks(),
            move |result| {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                let retry = this.sinks_read.finish();

                if !this.sinks_revision.is_current(revision) {
                    if retry {
                        this.refresh_sinks();
                    }
                    return;
                }

                match result {
                    Ok(sinks) => {
                        let migrated_hidden_keys = sync_hidden_keys(&sinks);
                        if let Some(keys) = migrated_hidden_keys {
                            persist_hidden_keys(keys);
                        }
                        let sinks_changed = this.state.borrow().sinks != sinks;
                        if sinks_changed {
                            this.state.borrow_mut().sinks = sinks;
                            this.rebuild_sink_list();
                        }
                        this.update_volume_ui(true);
                    }
                    Err(error) => {
                        debug!(%error, "failed to refresh audio outputs");
                        this.popup_status.set_text("No sinks found");
                    }
                }

                if retry {
                    this.refresh_sinks();
                }
            },
        );
    }

    fn update_volume_ui(&self, preserve_flash: bool) {
        let state = self.state.borrow();
        let icon = volume_icon(state.volume, state.muted);
        let percentage = (state.volume * 100.0).round() as i32;

        self.mute_icon.set_text(icon);
        self.percent.set_text(&format!("{percentage}%"));

        self.updating_slider.set(true);
        self.slider.set_value(state.volume);
        self.updating_slider.set(false);

        if state.muted || !preserve_flash || !self.trigger_label.has_css_class("module-percent") {
            self.show_trigger_icon();
        }

        let sink_name = state
            .sinks
            .iter()
            .find(|sink| sink.current)
            .map(|sink| sink.name.as_str())
            .unwrap_or("Audio output");
        let status = if state.muted { "Muted" } else { "Sound" };
        self.trigger.set_bar_tooltip_text(Some(&format!(
            "{status} {percentage}% • LMB volume • MMB mute • RMB devices • {sink_name}"
        )));
        self.mute_button.set_bar_tooltip_text(Some(if state.muted {
            "Unmute sound"
        } else {
            "Mute sound"
        }));
    }

    fn show_trigger_icon(&self) {
        self.trigger_label.remove_css_class("module-percent");
        self.trigger_label.remove_css_class("volume-percent");
        self.trigger_label.add_css_class("module-icon");
        let state = self.state.borrow();
        self.trigger_label
            .set_text(volume_icon(state.volume, state.muted));
    }

    fn flash_percent(self: &Rc<Self>) {
        let generation = self.percent_flash_serial.bump();

        let percentage = (self.state.borrow().volume * 100.0).round() as i32;
        self.trigger_label.remove_css_class("module-icon");
        self.trigger_label.add_css_class("module-percent");
        self.trigger_label.add_css_class("volume-percent");
        self.trigger_label.set_text(&format!("{percentage}%"));

        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(PERCENT_FLASH_DELAY, move || {
            let Some(this) = weak.upgrade() else {
                return;
            };
            if !this.percent_flash_serial.is_current(generation) {
                return;
            }
            this.show_trigger_icon();
        });
    }

    fn set_volume(self: &Rc<Self>, value: f64) {
        self.osd.suppress_local_audio_change();
        let value = value.clamp(0.0, 1.0);
        self.state.borrow_mut().volume = value;
        self.volume_revision.bump();
        self.update_volume_ui(true);
        self.schedule_volume_write(value);
    }

    fn adjust_volume(self: &Rc<Self>, delta: f64) {
        let next = (self.state.borrow().volume + delta).clamp(0.0, 1.0);
        self.set_volume(next);
    }

    fn schedule_volume_write(self: &Rc<Self>, value: f64) {
        let generation = self.volume_write_serial.bump();
        let weak = Rc::downgrade(self);

        glib::timeout_add_local_once(VOLUME_WRITE_DEBOUNCE, move || {
            let Some(this) = weak.upgrade() else {
                return;
            };
            if !this.volume_write_serial.is_current(generation) {
                return;
            }
            this.write_volume_now(value);
        });
    }

    fn write_volume_now(self: &Rc<Self>, value: f64) {
        if self.volume_write_busy.replace(true) {
            self.pending_volume_write.set(Some(value));
            return;
        }

        let backend = Arc::clone(&self.backend);
        let weak = Rc::downgrade(self);
        run_background(
            move || backend.set_volume(value),
            move |result| {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                this.volume_write_busy.set(false);
                if let Err(error) = result {
                    warn!(%error, "failed to set audio volume");
                }

                if let Some(pending) = this.pending_volume_write.take() {
                    this.write_volume_now(pending);
                    return;
                }

                this.refresh_volume();
            },
        );
    }

    fn toggle_mute(self: &Rc<Self>) {
        self.osd.suppress_local_audio_change();
        let target = !self.state.borrow().muted;
        self.state.borrow_mut().muted = target;
        self.volume_revision.bump();
        self.update_volume_ui(true);
        self.write_mute_now(target);
    }

    fn write_mute_now(self: &Rc<Self>, muted: bool) {
        if self.mute_write_busy.replace(true) {
            self.pending_mute_write.set(Some(muted));
            return;
        }

        let backend = Arc::clone(&self.backend);
        let weak = Rc::downgrade(self);
        run_background(
            move || backend.set_mute(muted),
            move |result| {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                this.mute_write_busy.set(false);
                if let Err(error) = result {
                    warn!(%error, "failed to set audio mute state");
                }

                if let Some(pending) = this.pending_mute_write.take() {
                    this.write_mute_now(pending);
                    return;
                }
                this.refresh_volume();
            },
        );
    }

    fn toggle_popup(self: &Rc<Self>) {
        if self.popup_reveal.is_revealed() {
            self.close_popup();
        } else {
            self.open_popup();
        }
    }

    fn open_popup(self: &Rc<Self>) {
        self.focus_armed.set(false);
        self.state.borrow_mut().show_hidden = false;
        self.clear_view_transition();
        self.rebuild_sink_list();
        let generation = self.popup_reveal.show(&self.popup);
        self.trigger.add_css_class("audio-popup-open");
        self.refresh_sinks();

        run_when_popup_visible(
            &self.popup,
            &self.popup_reveal,
            generation,
            Rc::downgrade(self),
            |this| {
                this.popup_root.grab_focus();
            },
        );
    }

    fn clear_view_transition(&self) {
        self.view_transition.clear();
    }

    fn replay_view_transition(&self, transition: AudioViewTransition) {
        self.view_transition.replay(transition.css_class());
    }

    fn close_popup(self: &Rc<Self>) {
        if !self.popup.is_visible() {
            return;
        }

        self.focus_armed.set(false);
        self.trigger.remove_css_class("audio-popup-open");
        self.clear_view_transition();
        self.popup_reveal.hide(&self.popup);
        self.state.borrow_mut().show_hidden = false;
    }

    fn rebuild_sink_list(self: &Rc<Self>) {
        clear_box(&self.list);

        let state = self.state.borrow();
        let hidden_keys = hidden_keys_snapshot();
        let hidden_count = state
            .sinks
            .iter()
            .filter(|sink| sink_hidden(sink, &hidden_keys))
            .count();

        let title = if state.show_hidden {
            format!("Hidden outputs {hidden_count}")
        } else {
            "Audio outputs".to_owned()
        };
        let status = if state.sinks.is_empty() {
            "No sinks found".to_owned()
        } else {
            format!("{} outputs", state.sinks.len())
        };
        self.popup_title.set_text(&title);
        self.popup_status.set_text(&status);
        self.list_section_title.set_text(if state.show_hidden {
            "Hidden"
        } else {
            "Outputs"
        });
        self.hidden_toggle
            .set_visible(hidden_count > 0 || state.show_hidden);
        if state.show_hidden {
            self.hidden_toggle_label.set_text(ICON_BACK);
        } else {
            self.hidden_toggle_label.set_text(ICON_HIDE);
        }

        let visible: Vec<SinkInfo> = state
            .sinks
            .iter()
            .filter(|sink| sink_hidden(sink, &hidden_keys) == state.show_hidden)
            .cloned()
            .collect();
        let show_hidden = state.show_hidden;
        drop(state);

        if visible.is_empty() {
            let empty = gtk::Label::new(Some(if show_hidden {
                "No hidden outputs"
            } else {
                "No sinks found"
            }));
            empty.add_css_class("audio-empty");
            empty.set_margin_top(18);
            empty.set_margin_bottom(18);
            self.list.append(&empty);
            return;
        }

        for sink in visible {
            self.list.append(&self.build_sink_row(sink, show_hidden));
        }
    }

    fn build_sink_row(self: &Rc<Self>, sink: SinkInfo, show_hidden: bool) -> gtk::Box {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        row.add_css_class("audio-sink-row");
        if sink.current {
            row.add_css_class("audio-sink-current");
        }
        row.set_valign(gtk::Align::Center);

        let main = gtk::Button::new();
        main.add_css_class("audio-sink-main");
        main.set_hexpand(true);
        main.set_halign(gtk::Align::Fill);

        let body = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        body.add_css_class("audio-sink-body");
        body.set_hexpand(true);
        body.set_valign(gtk::Align::Center);

        let icon_frame = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        icon_frame.add_css_class("audio-sink-icon-frame");
        icon_frame.set_halign(gtk::Align::Center);
        icon_frame.set_valign(gtk::Align::Center);

        let icon = gtk::Label::new(Some(sink_icon(sink.kind)));
        icon.add_css_class("audio-sink-icon");
        icon.set_halign(gtk::Align::Center);
        icon.set_valign(gtk::Align::Center);
        icon_frame.append(&icon);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 2);
        content.add_css_class("audio-sink-content");
        content.set_hexpand(true);
        content.set_valign(gtk::Align::Center);

        let name = gtk::Label::new(Some(&sink.name));
        name.add_css_class("audio-sink-name");
        name.set_xalign(0.0);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        name.set_max_width_chars(28);

        let meta = gtk::Label::new(Some(if show_hidden {
            "Hidden output"
        } else {
            &sink.meta
        }));
        meta.add_css_class("audio-sink-meta");
        meta.set_xalign(0.0);
        meta.set_ellipsize(gtk::pango::EllipsizeMode::End);
        meta.set_max_width_chars(38);

        content.append(&name);
        content.append(&meta);

        let status = gtk::Label::new(Some(if sink.current { ICON_CHECK } else { "" }));
        status.add_css_class("audio-sink-status");
        status.set_halign(gtk::Align::Center);
        status.set_valign(gtk::Align::Center);

        body.append(&icon_frame);
        body.append(&content);
        body.append(&status);
        main.set_child(Some(&body));

        let weak = Rc::downgrade(self);
        let selected_sink = sink.clone();
        main.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.choose_sink(selected_sink.clone());
            }
        });

        row.append(&main);

        if show_hidden || !sink.current {
            let side_icon =
                gtk::Label::new(Some(if show_hidden { ICON_RESTORE } else { ICON_HIDE }));
            side_icon.add_css_class("audio-sink-side-icon");

            let side = gtk::Button::new();
            side.add_css_class("audio-sink-side-button");
            side.set_valign(gtk::Align::Center);
            side.set_child(Some(&side_icon));

            let weak = Rc::downgrade(self);
            side.connect_clicked(move |_| {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                if show_hidden {
                    this.restore_sink(&sink);
                } else {
                    this.hide_sink(&sink);
                }
            });
            row.append(&side);
        }

        row
    }

    fn choose_sink(self: &Rc<Self>, sink: SinkInfo) {
        self.osd.suppress_local_audio_change();
        self.popup_status
            .set_text(&format!("Switching to {}", sink.name));
        self.volume_revision.bump();
        self.sinks_revision.bump();

        if self.sink_switch_busy.replace(true) {
            self.pending_sink_switch.replace(Some(sink));
            return;
        }

        self.switch_sink_now(sink);
    }

    fn switch_sink_now(self: &Rc<Self>, sink: SinkInfo) {
        let sink_id = sink.id.clone();
        let backend = Arc::clone(&self.backend);
        let weak = Rc::downgrade(self);

        run_background(
            move || backend.set_default_sink(&sink_id),
            move |result| {
                let Some(this) = weak.upgrade() else {
                    return;
                };

                if let Err(error) = result {
                    warn!(%error, sink = %sink.name, "failed to switch audio output");
                }

                let pending = this.pending_sink_switch.borrow_mut().take();
                if let Some(pending) = pending {
                    this.switch_sink_now(pending);
                    return;
                }

                this.sink_switch_busy.set(false);
                this.refresh_volume();
                this.refresh_sinks();
            },
        );
    }

    fn hide_sink(self: &Rc<Self>, sink: &SinkInfo) {
        if sink.current {
            return;
        }

        let keys = update_hidden_keys(|hidden| {
            hidden.extend(sink.persist_keys.iter().cloned());
        });
        self.rebuild_sink_list();
        persist_hidden_keys(keys);
    }

    fn restore_sink(self: &Rc<Self>, sink: &SinkInfo) {
        let keys = update_hidden_keys(|hidden| {
            for key in &sink.keys {
                hidden.remove(key);
            }
        });
        self.rebuild_sink_list();
        persist_hidden_keys(keys);
    }
}

fn sink_icon(kind: SinkKind) -> &'static str {
    match kind {
        SinkKind::Headset => ICON_HEADSET,
        SinkKind::Display => ICON_HDMI,
        SinkKind::Digital | SinkKind::Analog | SinkKind::Other => ICON_SPEAKER,
    }
}

fn sink_hidden(sink: &SinkInfo, hidden: &BTreeSet<String>) -> bool {
    !sink.current && sink.keys.iter().any(|key| hidden.contains(key))
}

fn hidden_keys() -> &'static Mutex<BTreeSet<String>> {
    HIDDEN_KEYS.get_or_init(|| Mutex::new(read_hidden_keys()))
}

fn hidden_keys_snapshot() -> BTreeSet<String> {
    hidden_keys()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn update_hidden_keys(update: impl FnOnce(&mut BTreeSet<String>)) -> BTreeSet<String> {
    let mut hidden = hidden_keys()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    update(&mut hidden);
    hidden.clone()
}

fn sync_hidden_keys(sinks: &[SinkInfo]) -> Option<BTreeSet<String>> {
    let mut hidden = hidden_keys()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let before = hidden.clone();

    let matched = sinks
        .iter()
        .filter(|sink| sink.keys.iter().any(|key| before.contains(key)))
        .collect::<Vec<_>>();

    for sink in &matched {
        hidden.extend(sink.persist_keys.iter().cloned());
    }
    for sink in matched {
        for key in &sink.keys {
            if !sink.persist_keys.contains(key) {
                hidden.remove(key);
            }
        }
    }

    (*hidden != before).then(|| hidden.clone())
}

fn normalize_hidden_keys(keys: Vec<String>) -> BTreeSet<String> {
    keys.into_iter()
        .map(|key| migrate_legacy_key(&key))
        .filter(|key| !key.is_empty())
        .collect()
}

fn migrate_legacy_key(key: &str) -> String {
    let key = key.trim();
    if let Some((prefix, rest)) = key.split_once(':')
        && !prefix.is_empty()
        && prefix.bytes().all(|byte| byte.is_ascii_digit())
    {
        return rest.trim().to_owned();
    }
    key.to_owned()
}

fn state_home() -> PathBuf {
    glib::user_state_dir()
}

fn hidden_sinks_path() -> PathBuf {
    state_home().join("obsidian-bar/audio-hidden-sinks.json")
}

fn read_hidden_keys() -> BTreeSet<String> {
    let current = hidden_sinks_path();
    if let Ok(contents) = fs::read_to_string(&current) {
        return serde_json::from_str::<Vec<String>>(&contents)
            .map(normalize_hidden_keys)
            .unwrap_or_default();
    }

    let legacy = state_home().join("ags/audio-hidden-sinks.json");
    let Ok(contents) = fs::read_to_string(legacy) else {
        return BTreeSet::new();
    };
    let keys = serde_json::from_str::<Vec<String>>(&contents)
        .map(normalize_hidden_keys)
        .unwrap_or_default();

    if !keys.is_empty() {
        let _ = write_hidden_keys_unchecked(&keys);
    }
    keys
}

fn persist_hidden_keys(keys: BTreeSet<String>) {
    let serial = HIDDEN_WRITE_SERIAL
        .fetch_add(1, Ordering::SeqCst)
        .wrapping_add(1);
    run_background(
        move || write_hidden_keys(serial, &keys),
        |result| {
            if let Err(error) = result {
                warn!(%error, "failed to persist hidden audio outputs");
            }
        },
    );
}

fn write_hidden_keys(serial: u64, keys: &BTreeSet<String>) -> Result<(), String> {
    let _write_guard = HIDDEN_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if HIDDEN_WRITE_SERIAL.load(Ordering::SeqCst) != serial {
        return Ok(());
    }
    write_hidden_keys_unchecked(keys)
}

fn write_hidden_keys_unchecked(keys: &BTreeSet<String>) -> Result<(), String> {
    let path = hidden_sinks_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    }

    let json = serde_json::to_vec(keys)
        .map_err(|error| format!("failed to serialize hidden audio outputs: {error}"))?;
    let temp_path = path.with_extension("json.tmp");
    if let Err(error) = fs::write(&temp_path, json) {
        let _ = fs::remove_file(&temp_path);
        return Err(format!("failed to write {}: {error}", temp_path.display()));
    }
    if let Err(error) = fs::rename(&temp_path, &path) {
        let _ = fs::remove_file(&temp_path);
        return Err(format!(
            "failed to replace {} using {}: {error}",
            path.display(),
            temp_path.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_legacy_hidden_keys() {
        assert_eq!(
            normalize_hidden_keys(vec![
                "42:usb headset".to_owned(),
                "usb headset".to_owned(),
                "  node:alsa_output.card  ".to_owned(),
            ]),
            BTreeSet::from(["node:alsa_output.card".to_owned(), "usb headset".to_owned(),])
        );
    }
}
