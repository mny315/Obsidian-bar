use std::{
    cell::{Cell, RefCell},
    fs,
    path::Path,
    rc::Rc,
    time::Duration,
};

use gtk::{gio, glib, prelude::*};
use tracing::{debug, warn};

use super::tooltip::BarTooltipExt;
use super::{
    Generation, RefreshGate, Timer, attach_inline_revealer_behavior, attach_scale_value_changed,
    attach_vertical_step_scroll, build_inline_panel, build_quick_toggle_button, run_background,
};

mod backend;
use backend::{
    BACKLIGHT_CLASS_PATH, BRIGHTNESS_MIN, BrightnessBackend, BrightnessState, clamp_brightness,
    invalidate_brightness_cache, percentage, read_brightness, write_brightness,
};

const BRIGHTNESS_STEP: f64 = 0.05;
const INLINE_HIDE_DELAY: Duration = Duration::from_secs(5);
const PERCENT_FLASH_DELAY: Duration = Duration::from_millis(1200);
const BACKLIGHT_WRITE_DEBOUNCE: Duration = Duration::from_millis(60);
const DDC_WRITE_DEBOUNCE: Duration = Duration::from_millis(250);
const UNKNOWN_WRITE_DEBOUNCE: Duration = Duration::from_millis(90);
const INLINE_REVEAL_DURATION_MS: u32 = 300;

const ICON_BRIGHTNESS: &str = "\u{f00df}";

struct BrightnessController {
    revealer: gtk::Revealer,
    slider: gtk::Scale,
    percent: gtk::Label,
    trigger: gtk::Button,
    trigger_label: gtk::Label,

    state: RefCell<BrightnessState>,
    updating_slider: Cell<bool>,
    refresh: RefreshGate,
    refresh_revision: Generation,
    write_timer: Timer,
    write_busy: Cell<bool>,
    pending_write: Cell<Option<f64>>,
    percent_flash_timer: Timer,
    backlight_monitors: RefCell<Vec<gio::FileMonitor>>,
    backlight_refresh_pending: Cell<bool>,
    backlight_monitors_dirty: Cell<bool>,
}

pub struct BrightnessIndicator {
    root: gtk::Box,
    revealer: gtk::Revealer,
    _controller: Rc<BrightnessController>,
}

impl BrightnessIndicator {
    pub fn new() -> Self {
        let (root, revealer, panel) =
            build_inline_panel(INLINE_REVEAL_DURATION_MS, 8, "slider-panel");

        let panel_icon = gtk::Label::new(Some(ICON_BRIGHTNESS));
        panel_icon.add_css_class("module-icon");
        panel_icon.add_css_class("brightness-icon");

        let slider =
            gtk::Scale::with_range(gtk::Orientation::Horizontal, BRIGHTNESS_MIN, 1.0, 0.01);
        slider.add_css_class("slider-control");
        slider.set_draw_value(false);
        slider.set_hexpand(true);
        slider.set_value(BRIGHTNESS_MIN);

        let percent = gtk::Label::new(Some("5%"));
        percent.add_css_class("slider-value");

        panel.append(&panel_icon);
        panel.append(&slider);
        panel.append(&percent);
        attach_inline_revealer_behavior(&root, &revealer, INLINE_HIDE_DELAY);

        let (trigger, trigger_label) =
            build_quick_toggle_button(ICON_BRIGHTNESS, "brightness-trigger", &["brightness-icon"]);
        trigger.set_bar_tooltip_text(Some("Brightness"));

        root.append(&revealer);
        root.append(&trigger);

        let controller = Rc::new(BrightnessController {
            revealer,
            slider,
            percent,
            trigger,
            trigger_label,
            state: RefCell::new(BrightnessState::default()),
            updating_slider: Cell::new(false),
            refresh: RefreshGate::default(),
            refresh_revision: Generation::default(),
            write_timer: Timer::default(),
            write_busy: Cell::new(false),
            pending_write: Cell::new(None),
            percent_flash_timer: Timer::default(),
            backlight_monitors: RefCell::new(Vec::new()),
            backlight_refresh_pending: Cell::new(false),
            backlight_monitors_dirty: Cell::new(false),
        });

        BrightnessController::connect(&controller);
        BrightnessController::install_backlight_monitors(&controller);
        controller.refresh();

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
    }
}

impl BrightnessController {
    fn connect(this: &Rc<Self>) {
        let weak = Rc::downgrade(this);
        this.trigger.connect_clicked(move |_| {
            let Some(this) = weak.upgrade() else {
                return;
            };

            let opening = !this.revealer.reveals_child();
            this.revealer.set_reveal_child(opening);
            if opening {
                invalidate_brightness_cache();
                this.refresh();
            }
        });

        attach_vertical_step_scroll(
            &this.trigger,
            Rc::downgrade(this),
            BRIGHTNESS_STEP,
            |this, delta| this.adjust_brightness(delta),
            |this| this.flash_percent(),
        );
        attach_scale_value_changed(
            &this.slider,
            Rc::downgrade(this),
            |this| this.updating_slider.get(),
            |this, value| this.set_brightness(value),
        );
    }

    fn install_backlight_monitors(this: &Rc<Self>) {
        let mut monitors = Vec::new();
        let class_path = Path::new(BACKLIGHT_CLASS_PATH);

        let class_file = gio::File::for_path(class_path);
        if let Ok(monitor) =
            class_file.monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
        {
            let weak = Rc::downgrade(this);
            monitor.connect_changed(move |_, _, _, _| {
                if let Some(this) = weak.upgrade() {
                    this.schedule_backlight_refresh(true);
                }
            });
            monitors.push(monitor);
        }

        if let Ok(entries) = fs::read_dir(class_path) {
            for entry in entries.flatten() {
                for attribute in ["brightness", "actual_brightness", "max_brightness"] {
                    let path = entry.path().join(attribute);
                    if !path.exists() {
                        continue;
                    }
                    let file = gio::File::for_path(&path);
                    let Ok(monitor) =
                        file.monitor_file(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE)
                    else {
                        continue;
                    };
                    let weak = Rc::downgrade(this);
                    monitor.connect_changed(move |_, _, _, _| {
                        if let Some(this) = weak.upgrade() {
                            this.schedule_backlight_refresh(false);
                        }
                    });
                    monitors.push(monitor);
                }
            }
        }

        this.backlight_monitors.replace(monitors);
    }

    fn schedule_backlight_refresh(self: &Rc<Self>, rebuild_monitors: bool) {
        if rebuild_monitors {
            self.backlight_monitors_dirty.set(true);
        }
        if self.backlight_refresh_pending.replace(true) {
            return;
        }

        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            let Some(this) = weak.upgrade() else {
                return;
            };
            this.backlight_refresh_pending.set(false);
            if this.backlight_monitors_dirty.replace(false) {
                Self::install_backlight_monitors(&this);
            }
            invalidate_brightness_cache();
            this.refresh();
        });
    }

    fn refresh(self: &Rc<Self>) {
        if !self.refresh.begin() {
            return;
        }

        let revision = self.refresh_revision.current();
        let weak = Rc::downgrade(self);
        run_background(read_brightness, move |result| {
            let Some(this) = weak.upgrade() else {
                return;
            };

            let retry = this.refresh.finish();

            if this.refresh_revision.is_current(revision)
                && !this.write_timer.is_pending()
                && !this.write_busy.get()
            {
                match result {
                    Ok(state) => {
                        *this.state.borrow_mut() = state;
                        this.update_ui(true);
                    }
                    Err(error) => {
                        debug!(%error, "failed to refresh brightness");
                        this.state.borrow_mut().backend = BrightnessBackend::None;
                        this.update_ui(true);
                    }
                }
            }

            if retry {
                this.refresh();
            }
        });
    }

    fn update_ui(&self, preserve_flash: bool) {
        let state = *self.state.borrow();
        let percentage = percentage(state.value);

        self.percent.set_text(&format!("{percentage}%"));
        self.updating_slider.set(true);
        self.slider.set_value(state.value);
        self.updating_slider.set(false);
        self.slider
            .set_sensitive(state.backend != BrightnessBackend::None);

        if !preserve_flash || !self.trigger_label.has_css_class("module-percent") {
            self.show_trigger_icon();
        }

        let tooltip = match state.backend {
            BrightnessBackend::Backlight => format!("Brightness {percentage}%"),
            BrightnessBackend::Ddc { .. } => format!("Brightness {percentage}% • DDC/CI"),
            BrightnessBackend::Unknown => "Brightness".to_owned(),
            BrightnessBackend::None => "Brightness unavailable".to_owned(),
        };
        self.trigger.set_bar_tooltip_text(Some(&tooltip));
    }

    fn show_trigger_icon(&self) {
        self.trigger_label.remove_css_class("module-percent");
        self.trigger_label.remove_css_class("brightness-percent");
        self.trigger_label.add_css_class("module-icon");
        self.trigger_label.add_css_class("brightness-icon");
        self.trigger_label.set_text(ICON_BRIGHTNESS);
    }

    fn flash_percent(self: &Rc<Self>) {
        let percentage = percentage(self.state.borrow().value);

        self.trigger_label.remove_css_class("module-icon");
        self.trigger_label.remove_css_class("brightness-icon");
        self.trigger_label.add_css_class("module-percent");
        self.trigger_label.add_css_class("brightness-percent");
        self.trigger_label.set_text(&format!("{percentage}%"));

        let weak = Rc::downgrade(self);
        self.percent_flash_timer
            .schedule(PERCENT_FLASH_DELAY, move || {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                this.show_trigger_icon();
            });
    }

    fn set_brightness(self: &Rc<Self>, value: f64) {
        let value = clamp_brightness(value);
        self.state.borrow_mut().value = value;
        self.refresh_revision.bump();
        self.update_ui(true);
        self.schedule_write(value);
    }

    fn adjust_brightness(self: &Rc<Self>, delta: f64) {
        let next = clamp_brightness(self.state.borrow().value + delta);
        self.set_brightness(next);
    }

    fn schedule_write(self: &Rc<Self>, value: f64) {
        let delay = match self.state.borrow().backend {
            BrightnessBackend::Backlight => BACKLIGHT_WRITE_DEBOUNCE,
            BrightnessBackend::Ddc { .. } => DDC_WRITE_DEBOUNCE,
            BrightnessBackend::Unknown | BrightnessBackend::None => UNKNOWN_WRITE_DEBOUNCE,
        };
        let weak = Rc::downgrade(self);

        self.write_timer.schedule(delay, move || {
            let Some(this) = weak.upgrade() else {
                return;
            };
            this.write_now(value);
        });
    }

    fn write_now(self: &Rc<Self>, value: f64) {
        if self.write_busy.replace(true) {
            self.pending_write.set(Some(value));
            return;
        }

        let backend = self.state.borrow().backend;
        let weak = Rc::downgrade(self);
        run_background(
            move || write_brightness(backend, value),
            move |result| {
                let Some(this) = weak.upgrade() else {
                    return;
                };

                this.write_busy.set(false);
                match result {
                    Ok(backend) => {
                        this.state.borrow_mut().backend = backend;
                        this.update_ui(true);
                    }
                    Err(error) => warn!(%error, "failed to set brightness"),
                }

                if let Some(pending) = this.pending_write.take() {
                    this.write_now(pending);
                    return;
                }

                this.refresh();
            },
        );
    }
}
