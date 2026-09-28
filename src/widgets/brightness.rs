use std::{
    cell::{Cell, RefCell},
    cmp::Ordering,
    fs,
    path::Path,
    rc::Rc,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering as AtomicOrdering},
    },
    time::{Duration, Instant},
};

use gtk::{gio, glib, prelude::*};
use tracing::{debug, warn};

use super::tooltip::BarTooltipExt;
use super::{
    Generation, RefreshGate, attach_inline_revealer_behavior, attach_scale_value_changed,
    attach_vertical_step_scroll, build_inline_panel, build_quick_toggle_button, command,
    run_background,
};

const BRIGHTNESS_MIN: f64 = 0.05;
const BRIGHTNESS_STEP: f64 = 0.05;
const INLINE_HIDE_DELAY: Duration = Duration::from_secs(5);
const PERCENT_FLASH_DELAY: Duration = Duration::from_millis(1200);
const BACKLIGHT_WRITE_DEBOUNCE: Duration = Duration::from_millis(60);
const DDC_WRITE_DEBOUNCE: Duration = Duration::from_millis(250);
const UNKNOWN_WRITE_DEBOUNCE: Duration = Duration::from_millis(90);
const BRIGHTNESS_CACHE_TTL: Duration = Duration::from_millis(500);
static BRIGHTNESS_CACHE: OnceLock<Mutex<Option<(Instant, BrightnessState)>>> = OnceLock::new();
static BRIGHTNESS_READ_LOCK: Mutex<()> = Mutex::new(());
static BRIGHTNESS_WRITE_LOCK: Mutex<()> = Mutex::new(());
static BRIGHTNESS_CACHE_REVISION: AtomicU64 = AtomicU64::new(0);
const INLINE_REVEAL_DURATION_MS: u32 = 300;
const BACKLIGHT_CLASS_PATH: &str = "/sys/class/backlight";
const BRIGHTNESSCTL_TIMEOUT: Duration = Duration::from_secs(2);
const DDCUTIL_TIMEOUT: Duration = Duration::from_secs(4);
const DDC_DISCOVERY_TTL: Duration = Duration::from_secs(30);
type DisplayTopology = Vec<(String, Vec<u8>)>;
struct DdcDiscovery {
    checked_at: Instant,
    topology: DisplayTopology,
    bus: Option<u32>,
}
static DDC_DISCOVERY: Mutex<Option<DdcDiscovery>> = Mutex::new(None);
static BRIGHTNESSCTL: command::ExternalProgram = command::ExternalProgram::new(
    "OBSIDIAN_BAR_BRIGHTNESSCTL_BIN",
    option_env!("OBSIDIAN_BAR_BRIGHTNESSCTL_BIN"),
    "brightnessctl",
);
static DDCUTIL: command::ExternalProgram = command::ExternalProgram::new(
    "OBSIDIAN_BAR_DDCUTIL_BIN",
    option_env!("OBSIDIAN_BAR_DDCUTIL_BIN"),
    "ddcutil",
);

const ICON_BRIGHTNESS: &str = "\u{f00df}";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum BrightnessBackend {
    #[default]
    Unknown,
    Backlight,
    Ddc {
        maximum: u16,
        bus: Option<u32>,
    },
    None,
}

#[derive(Clone, Copy, Debug)]
struct BrightnessState {
    backend: BrightnessBackend,
    value: f64,
}

impl Default for BrightnessState {
    fn default() -> Self {
        Self {
            backend: BrightnessBackend::Unknown,
            value: BRIGHTNESS_MIN,
        }
    }
}

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
    write_serial: Generation,
    write_scheduled: Cell<bool>,
    write_busy: Cell<bool>,
    pending_write: Cell<Option<f64>>,
    percent_flash_serial: Generation,
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
            write_serial: Generation::default(),
            write_scheduled: Cell::new(false),
            write_busy: Cell::new(false),
            pending_write: Cell::new(None),
            percent_flash_serial: Generation::default(),
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
                && !this.write_scheduled.get()
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
        let generation = self.percent_flash_serial.bump();
        let percentage = percentage(self.state.borrow().value);

        self.trigger_label.remove_css_class("module-icon");
        self.trigger_label.remove_css_class("brightness-icon");
        self.trigger_label.add_css_class("module-percent");
        self.trigger_label.add_css_class("brightness-percent");
        self.trigger_label.set_text(&format!("{percentage}%"));

        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(PERCENT_FLASH_DELAY, move || {
            let Some(this) = weak.upgrade() else {
                return;
            };
            if this.percent_flash_serial.is_current(generation) {
                this.show_trigger_icon();
            }
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
        self.write_scheduled.set(true);
        let generation = self.write_serial.bump();
        let delay = match self.state.borrow().backend {
            BrightnessBackend::Backlight => BACKLIGHT_WRITE_DEBOUNCE,
            BrightnessBackend::Ddc { .. } => DDC_WRITE_DEBOUNCE,
            BrightnessBackend::Unknown | BrightnessBackend::None => UNKNOWN_WRITE_DEBOUNCE,
        };
        let weak = Rc::downgrade(self);

        glib::timeout_add_local_once(delay, move || {
            let Some(this) = weak.upgrade() else {
                return;
            };
            if this.write_serial.is_current(generation) {
                this.write_scheduled.set(false);
                this.write_now(value);
            }
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

fn read_brightness() -> Result<BrightnessState, String> {
    if let Some(state) = cached_brightness() {
        return Ok(state);
    }
    let _read_guard = BRIGHTNESS_READ_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(state) = cached_brightness() {
        return Ok(state);
    }

    let revision = BRIGHTNESS_CACHE_REVISION.load(AtomicOrdering::Acquire);
    let state = read_brightness_uncached()?;
    let mut cache = brightness_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if BRIGHTNESS_CACHE_REVISION.load(AtomicOrdering::Acquire) == revision {
        *cache = Some((Instant::now(), state));
    }
    Ok(state)
}

fn cached_brightness() -> Option<BrightnessState> {
    brightness_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .filter(|(loaded_at, _)| loaded_at.elapsed() < BRIGHTNESS_CACHE_TTL)
        .map(|(_, state)| *state)
}

fn read_brightness_uncached() -> Result<BrightnessState, String> {
    read_brightness_at(Path::new(BACKLIGHT_CLASS_PATH), read_ddc)
}

fn read_brightness_at(
    backlight_path: &Path,
    ddc: impl FnOnce() -> Result<(f64, u16, Option<u32>), String>,
) -> Result<BrightnessState, String> {
    match read_backlight(backlight_path) {
        Ok(value) => {
            return Ok(BrightnessState {
                backend: BrightnessBackend::Backlight,
                value,
            });
        }
        Err(error) => debug!(%error, "backlight brightness probe failed"),
    }

    match ddc() {
        Ok((value, maximum, bus)) => Ok(BrightnessState {
            backend: BrightnessBackend::Ddc { maximum, bus },
            value,
        }),
        Err(error) => Err(format!("no brightness backend available: {error}")),
    }
}

fn brightness_cache() -> &'static Mutex<Option<(Instant, BrightnessState)>> {
    BRIGHTNESS_CACHE.get_or_init(|| Mutex::new(None))
}

fn invalidate_brightness_cache() {
    let mut cache = brightness_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    BRIGHTNESS_CACHE_REVISION.fetch_add(1, AtomicOrdering::AcqRel);
    *cache = None;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BacklightKind {
    Firmware,
    Platform,
    Raw,
    Unknown,
}

impl BacklightKind {
    fn parse(value: &str) -> Self {
        match value.trim() {
            "firmware" => Self::Firmware,
            "platform" => Self::Platform,
            "raw" => Self::Raw,
            _ => Self::Unknown,
        }
    }

    fn priority(self) -> u8 {
        match self {
            Self::Firmware => 0,
            Self::Platform => 1,
            Self::Raw => 2,
            Self::Unknown => 3,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BacklightDevice {
    name: String,
    kind: BacklightKind,
    brightness: u64,
    maximum: u64,
}

impl BacklightDevice {
    fn read(path: &Path) -> Result<Self, String> {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("invalid backlight device path: {}", path.display()))?
            .to_owned();
        let brightness = read_sysfs_number(&path.join("brightness"))?;
        let maximum = read_sysfs_number(&path.join("max_brightness"))?;
        if maximum == 0 {
            return Err(format!("backlight device {name} reports max_brightness=0"));
        }

        let kind = fs::read_to_string(path.join("type"))
            .map(|value| BacklightKind::parse(&value))
            .unwrap_or(BacklightKind::Unknown);

        Ok(Self {
            name,
            kind,
            brightness,
            maximum,
        })
    }

    fn value(&self) -> f64 {
        self.brightness.min(self.maximum) as f64 / self.maximum as f64
    }
}

fn read_backlight(class_path: &Path) -> Result<f64, String> {
    let device = find_backlight_device(class_path)?;
    Ok(clamp_brightness(device.value()))
}

fn find_backlight_device(class_path: &Path) -> Result<BacklightDevice, String> {
    let entries = fs::read_dir(class_path)
        .map_err(|error| format!("failed to read {}: {error}", class_path.display()))?;
    let mut devices = Vec::new();
    let mut failures = Vec::new();

    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(error) => {
                failures.push(format!("failed to inspect backlight entry: {error}"));
                continue;
            }
        };

        match BacklightDevice::read(&path) {
            Ok(device) => devices.push(device),
            Err(error) => failures.push(error),
        }
    }

    select_backlight_device(devices).ok_or_else(|| {
        if failures.is_empty() {
            format!("no backlight devices found in {}", class_path.display())
        } else {
            format!(
                "no usable backlight devices found in {}: {}",
                class_path.display(),
                failures.join("; ")
            )
        }
    })
}

fn select_backlight_device(devices: Vec<BacklightDevice>) -> Option<BacklightDevice> {
    devices.into_iter().min_by(compare_backlight_devices)
}

fn compare_backlight_devices(left: &BacklightDevice, right: &BacklightDevice) -> Ordering {
    left.kind
        .priority()
        .cmp(&right.kind.priority())
        .then_with(|| left.name.cmp(&right.name))
}

fn read_sysfs_number(path: &Path) -> Result<u64, String> {
    let raw = fs::read_to_string(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    raw.trim()
        .parse::<u64>()
        .map_err(|error| format!("invalid value in {}: {error}", path.display()))
}

fn display_topology() -> DisplayTopology {
    let mut topology = Vec::new();
    if let Ok(entries) = fs::read_dir("/sys/class/drm") {
        for entry in entries.flatten() {
            let path = entry.path();
            if fs::read_to_string(path.join("status"))
                .is_ok_and(|status| status.trim() == "connected")
            {
                topology.push((
                    entry.file_name().to_string_lossy().into_owned(),
                    fs::read(path.join("edid")).unwrap_or_default(),
                ));
            }
        }
    }
    topology.sort_unstable();
    topology
}

fn ddc_bus() -> Option<u32> {
    let topology = display_topology();
    let mut cache = DDC_DISCOVERY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(discovery) = cache.as_ref()
        && discovery.topology == topology
        && discovery.checked_at.elapsed() < DDC_DISCOVERY_TTL
    {
        return discovery.bus;
    }
    // An unqualified ddcutil command scans all buses before every operation.
    // Discover once, then use the validated bus directly while dragging.
    let bus = command::output(DDCUTIL.get(), &["detect", "--brief"], DDCUTIL_TIMEOUT)
        .ok()
        .and_then(|output| parse_ddc_bus(&output));
    *cache = Some(DdcDiscovery {
        checked_at: Instant::now(),
        topology,
        bus,
    });
    bus
}

fn parse_ddc_bus(output: &str) -> Option<u32> {
    let mut first_display = false;
    for line in output.lines().map(str::trim) {
        if let Some(display) = line.strip_prefix("Display ") {
            if first_display {
                break;
            }
            first_display = display.parse::<u32>().ok() == Some(1);
        } else if first_display && let Some(bus) = line.strip_prefix("I2C bus:") {
            return bus.trim().strip_prefix("/dev/i2c-")?.parse().ok();
        }
    }
    None
}

fn read_ddc() -> Result<(f64, u16, Option<u32>), String> {
    let read = |bus| {
        let args = ddc_arguments(&["getvcp", "10", "--brief"], bus);
        let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
        let output = command::output(DDCUTIL.get(), &refs, DDCUTIL_TIMEOUT)?;
        parse_ddc_brightness(&output)
            .map(|(value, maximum)| (value, maximum, bus))
            .ok_or_else(|| format!("unexpected ddcutil output: {output}"))
    };
    if let Some(bus) = ddc_bus() {
        match read(Some(bus)) {
            Ok(value) => return Ok(value),
            Err(error) => {
                debug!(%error, bus, "direct DDC read failed; retrying normal discovery");
                // Keep the original path for older ddcutil and USB-only displays.
                if let Some(cache) = DDC_DISCOVERY
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .as_mut()
                {
                    cache.bus = None;
                }
            }
        }
    }
    read(None)
}

fn ddc_arguments(command: &[&str], bus: Option<u32>) -> Vec<String> {
    let mut args = command
        .iter()
        .map(|arg| (*arg).to_owned())
        .collect::<Vec<_>>();
    if let Some(bus) = bus {
        args.extend(["--bus".into(), bus.to_string(), "--skip-ddc-checks".into()]);
    }
    args
}

fn write_brightness(backend: BrightnessBackend, value: f64) -> Result<BrightnessBackend, String> {
    let _write_guard = match BRIGHTNESS_WRITE_LOCK.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            return Err("another brightness change is already in progress".to_owned());
        }
    };
    let percent = percentage(value).clamp(5, 100);

    let result = match backend {
        BrightnessBackend::Backlight => {
            write_backlight(percent).map(|_| BrightnessBackend::Backlight)
        }
        BrightnessBackend::Ddc { maximum, bus } => {
            write_ddc(percent, maximum, bus).map(|_| BrightnessBackend::Ddc { maximum, bus })
        }
        BrightnessBackend::Unknown | BrightnessBackend::None => match write_backlight(percent) {
            Ok(()) => Ok(BrightnessBackend::Backlight),
            Err(backlight_error) => match read_ddc().and_then(|(_, maximum, bus)| {
                write_ddc(percent, maximum, bus)?;
                Ok(BrightnessBackend::Ddc { maximum, bus })
            }) {
                Ok(backend) => Ok(backend),
                Err(ddc_error) => Err(format!(
                    "backlight failed ({backlight_error}); DDC/CI failed ({ddc_error})"
                )),
            },
        },
    };

    if let Ok(resolved_backend) = result.as_ref() {
        let mut cache = brightness_cache()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        BRIGHTNESS_CACHE_REVISION.fetch_add(1, AtomicOrdering::AcqRel);
        *cache = Some((
            Instant::now(),
            BrightnessState {
                backend: *resolved_backend,
                value: clamp_brightness(value),
            },
        ));
    } else {
        // An error must not leave an optimistic cached value/old display target
        // in place when the controller reads back the actual state.
        invalidate_brightness_cache();
    }
    result
}

fn write_backlight(percent: i32) -> Result<(), String> {
    let device = find_backlight_device(Path::new(BACKLIGHT_CLASS_PATH))?;
    let device_arg = format!("--device={}", device.name);
    let value = native_brightness_value(percent, device.maximum).to_string();
    command::status(
        BRIGHTNESSCTL.get(),
        &[&device_arg, "--class=backlight", "set", &value],
        BRIGHTNESSCTL_TIMEOUT,
    )
}

fn write_ddc(percent: i32, maximum: u16, bus: Option<u32>) -> Result<(), String> {
    if bus.is_some() {
        let topology = display_topology();
        let cache = DDC_DISCOVERY
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if cache
            .as_ref()
            .is_none_or(|cached| cached.topology != topology || cached.bus != bus)
        {
            return Err(
                "display connection changed; refresh brightness before trying again".into(),
            );
        }
    }
    let value = ddc_raw_value(percent, maximum).to_string();
    // Retain verification of the value; skip only redundant discovery checks.
    let args = ddc_arguments(&["setvcp", "10", &value], bus);
    let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    let result = command::status(DDCUTIL.get(), &refs, DDCUTIL_TIMEOUT);
    if result.is_err() {
        DDC_DISCOVERY
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
    }
    result
}

fn ddc_raw_value(percent: i32, maximum: u16) -> u16 {
    // setvcp accepts native VCP units, not a percentage. Some displays use a
    // range such as 0..255 instead of 0..100.
    native_brightness_value(percent, u64::from(maximum)) as u16
}

fn native_brightness_value(percent: i32, maximum: u64) -> u64 {
    let maximum = u128::from(maximum);
    let minimum = (5 * maximum).div_ceil(100);
    ((percent.clamp(5, 100) as u128 * maximum + 50) / 100).max(minimum) as u64
}

fn parse_ddc_brightness(output: &str) -> Option<(f64, u16)> {
    for line in output.lines() {
        let mut fields = line.split_whitespace();
        let (Some(vcp), Some(code), Some(kind), Some(current), Some(maximum)) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            continue;
        };
        if !vcp.eq_ignore_ascii_case("VCP")
            || !code.eq_ignore_ascii_case("10")
            || !kind.eq_ignore_ascii_case("C")
        {
            continue;
        }

        let (Ok(current), Ok(maximum)) = (current.parse::<u16>(), maximum.parse::<u16>()) else {
            continue;
        };
        if maximum == 0 {
            continue;
        }
        return Some((
            clamp_brightness(f64::from(current) / f64::from(maximum)),
            maximum,
        ));
    }
    None
}

fn percentage(value: f64) -> i32 {
    (clamp_brightness(value) * 100.0).round() as i32
}

fn clamp_brightness(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(BRIGHTNESS_MIN, 1.0)
    } else {
        BRIGHTNESS_MIN
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BacklightDevice, BacklightKind, ddc_raw_value, parse_ddc_brightness,
        select_backlight_device,
    };

    fn backlight(name: &str, kind: BacklightKind) -> BacklightDevice {
        BacklightDevice {
            name: name.to_owned(),
            kind,
            brightness: 40,
            maximum: 100,
        }
    }

    #[test]
    fn selects_backlight_using_kernel_interface_priority() {
        let selected = select_backlight_device(vec![
            backlight("intel_backlight", BacklightKind::Raw),
            backlight("acpi_video0", BacklightKind::Firmware),
            backlight("vendor", BacklightKind::Platform),
        ])
        .expect("a backlight should be selected");

        assert_eq!(selected.name, "acpi_video0");
    }

    #[test]
    fn selects_backlight_deterministically_within_same_kind() {
        let selected = select_backlight_device(vec![
            backlight("backlight_b", BacklightKind::Raw),
            backlight("backlight_a", BacklightKind::Raw),
        ])
        .expect("a backlight should be selected");

        assert_eq!(selected.name, "backlight_a");
    }

    #[test]
    fn parses_ddcutil_brief_continuous_value() {
        assert_eq!(parse_ddc_brightness("VCP 10 C 57 100"), Some((0.57, 100)));
        assert_eq!(
            parse_ddc_brightness("Display 1\nVCP 10 C 80 100"),
            Some((0.8, 100))
        );
        assert_eq!(
            parse_ddc_brightness("VCP 10 C invalid 100\nVCP 10 C 65 100"),
            Some((0.65, 100))
        );
        assert_eq!(parse_ddc_brightness("VCP 10 C NaN 100"), None);
        assert_eq!(parse_ddc_brightness("VCP 10 C 20 NaN"), None);
        assert_eq!(parse_ddc_brightness("VCP 10 C -1 100"), None);
        assert_eq!(parse_ddc_brightness("VCP 10 C 1 0"), None);
        assert_eq!(parse_ddc_brightness("VCP 10 C 1 65536"), None);
    }

    #[test]
    fn ddc_percentages_use_the_displays_native_range() {
        assert_eq!(
            parse_ddc_brightness("VCP 10 C 64 255"),
            Some((64.0 / 255.0, 255))
        );
        assert_eq!(ddc_raw_value(50, 100), 50);
        assert_eq!(ddc_raw_value(50, 255), 128);
        assert_eq!(ddc_raw_value(100, 255), 255);
        assert_eq!(ddc_raw_value(5, 255), 13);
        assert_eq!(ddc_raw_value(0, 100), 5);
        assert_eq!(ddc_raw_value(-100, 100), 5);
        assert_eq!(ddc_raw_value(5, 127), 7);
        assert_eq!(ddc_raw_value(100, u16::MAX), u16::MAX);
    }

    #[test]
    fn no_supported_ddc_range_can_round_below_five_percent() {
        for maximum in 1..=u16::MAX {
            let minimum = ddc_raw_value(-1, maximum);
            assert!(u32::from(minimum) * 100 >= u32::from(maximum) * 5);
            assert!(minimum <= maximum);
        }
        for input in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 0.0, 0.049] {
            assert_eq!(super::clamp_brightness(input), super::BRIGHTNESS_MIN);
        }
        assert_eq!(super::native_brightness_value(100, u64::MAX), u64::MAX);
        assert_eq!(super::native_brightness_value(0, 127), 7);
    }

    #[test]
    fn laptop_backlight_does_not_probe_ddc() {
        let root = std::env::temp_dir().join(format!("obsidian-backlight-{}", std::process::id()));
        let device = root.join("intel_backlight");
        std::fs::create_dir_all(&device).unwrap();
        for (name, value) in [
            ("brightness", "400"),
            ("max_brightness", "800"),
            ("type", "raw"),
        ] {
            std::fs::write(device.join(name), value).unwrap();
        }
        let state =
            super::read_brightness_at(&root, || panic!("laptop must not start ddcutil")).unwrap();
        assert_eq!(state.backend, super::BrightnessBackend::Backlight);
        assert_eq!(state.value, 0.5);
        std::fs::write(device.join("max_brightness"), "0").unwrap();
        let fallback = super::read_brightness_at(&root, || Err("DDC unavailable".into()));
        assert!(fallback.is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ddc_discovery_respects_the_first_valid_display() {
        assert_eq!(
            super::parse_ddc_bus(
                "Invalid display\n I2C bus: /dev/i2c-2\nDisplay 1\n I2C bus: /dev/i2c-37\nDisplay 2\n I2C bus: /dev/i2c-42"
            ),
            Some(37)
        );
        assert_eq!(
            super::parse_ddc_bus("Display 1\n USB bus: 1.2\nDisplay 2\n I2C bus: /dev/i2c-42"),
            None
        );
        assert_eq!(super::parse_ddc_bus("No displays found"), None);
        assert_eq!(
            super::parse_ddc_bus("Display 1\n I2C bus: /dev/i2c-invalid"),
            None
        );
    }

    #[test]
    fn ddc_fast_path_preserves_fallbacks_verification_and_error_recovery() {
        use super::*;
        use std::os::unix::fs::PermissionsExt;
        const ROOT_VARIABLE: &str = "OBSIDIAN_BRIGHTNESS_TEST_ROOT";
        if let Some(root) = std::env::var_os(ROOT_VARIABLE) {
            let root = std::path::PathBuf::from(root);
            for _ in 0..2 {
                assert_eq!(read_ddc().unwrap(), (128.0 / 255.0, 255, Some(37)));
            }
            let backend = BrightnessBackend::Ddc {
                maximum: 255,
                bus: Some(37),
            };
            write_brightness(backend, 0.0).unwrap();
            let calls = fs::read_to_string(root.join("calls")).unwrap();
            assert_eq!(
                calls
                    .lines()
                    .filter(|line| line.starts_with("detect "))
                    .count(),
                1
            );
            assert!(calls.contains("setvcp 10 13 --bus 37 --skip-ddc-checks"));
            assert!(!calls.contains("--noverify"));

            fs::write(root.join("fail-write"), "").unwrap();
            assert!(write_brightness(backend, 0.5).is_err());
            assert!(cached_brightness().is_none());
            assert!(DDC_DISCOVERY.lock().unwrap().is_none());
            fs::remove_file(root.join("fail-write")).unwrap();
            read_ddc().unwrap();
            DDC_DISCOVERY
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .topology
                .push(("removed-output".into(), vec![]));
            let calls_before = fs::read_to_string(root.join("calls")).unwrap();
            assert!(write_brightness(backend, 0.5).is_err());
            assert_eq!(
                fs::read_to_string(root.join("calls")).unwrap(),
                calls_before
            );

            DDC_DISCOVERY.lock().unwrap().take();
            fs::write(root.join("legacy"), "").unwrap();
            assert_eq!(read_ddc().unwrap().2, None);
            assert_eq!(read_ddc().unwrap().2, None);
            write_brightness(
                BrightnessBackend::Ddc {
                    maximum: 255,
                    bus: None,
                },
                0.5,
            )
            .unwrap();
            assert!(
                fs::read_to_string(root.join("calls"))
                    .unwrap()
                    .lines()
                    .any(|line| line == "setvcp 10 128")
            );
            return;
        }
        let root =
            std::env::temp_dir().join(format!("obsidian-ddc-fixture-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let program = root.join("ddcutil-fixture");
        fs::write(
            &program,
            r#"#!/bin/sh
printf '%s\n' "$*" >> "$OBSIDIAN_BRIGHTNESS_TEST_ROOT/calls"
case "$1" in
detect) printf 'Display 1\n I2C bus: /dev/i2c-37\n' ;;
getvcp)
  if [ -f "$OBSIDIAN_BRIGHTNESS_TEST_ROOT/legacy" ]; then
    case " $* " in *' --skip-ddc-checks '*) exit 2 ;; esac
  fi
  printf 'VCP 10 C 128 255\n'
  ;;
setvcp) if [ -f "$OBSIDIAN_BRIGHTNESS_TEST_ROOT/fail-write" ]; then exit 1; fi ;;
*) exit 2 ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "widgets::brightness::tests::ddc_fast_path_preserves_fallbacks_verification_and_error_recovery", "--test-threads=1", "--nocapture"])
            .env(ROOT_VARIABLE, &root)
            .env("OBSIDIAN_BAR_DDCUTIL_BIN", &program)
            .output().unwrap();
        fs::remove_dir_all(root).unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
