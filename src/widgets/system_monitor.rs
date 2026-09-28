use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet, VecDeque},
    ffi::CString,
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use gtk::{gdk, glib, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use tracing::{info, warn};

use super::{Generation, clear_box, detach_application_window, run_background};

mod drawer;
mod gpu;
mod nvidia;
use drawer::MonitorDrawer;

const SETTINGS_GROUP: &str = "monitor";
const SETTINGS_FILE: &str = "system-monitor.ini";
const CONTENT_NAMESPACE: &str = "obsidian-system-monitor";
const SAMPLE_INTERVAL_DEFAULT_MS: i32 = 1_000;
const HIDE_DELAY_DEFAULT_MS: i32 = 2_000;
const SETTINGS_HIDE_DELAY: Duration = Duration::from_secs(1);
const HIDE_DELAY_MAX_MS: i32 = 30_000;
const NETWORK_HISTORY_LENGTH: usize = 64;
const PANEL_MIN_WIDTH: i32 = 180;
const PANEL_MAX_WIDTH: i32 = 800;
const PANEL_DEFAULT_WIDTH: i32 = 280;
const PANEL_MIN_HEIGHT: i32 = 160;
const PANEL_EDGE_MARGIN: i32 = 8;
const SETTINGS_TRIGGER_SIZE: i32 = 36;
const SETTINGS_PANEL_WIDTH: i32 = 330;
const SETTINGS_PANEL_PADDING: i32 = 12;
const SCALE_MILLI_DEFAULT: i32 = 1_000;
const SCALE_MILLI_MIN: i32 = 727;
const SCALE_MILLI_MAX: i32 = 2_182;

fn scaled_pixels(base: i32, scale_milli: i32) -> i32 {
    (base.saturating_mul(scale_milli) / SCALE_MILLI_DEFAULT).max(1)
}

const ICON_SETTINGS: &str = "\u{f0493}";
const ICON_MONITOR: &str = "\u{f0379}";
const ICON_UP: &str = "\u{f005d}";
const ICON_DOWN: &str = "\u{f0045}";
const ICON_VISIBLE: &str = "\u{f0208}";
const ICON_HIDDEN: &str = "\u{f0209}";
const ICON_EXPAND: &str = "\u{f0142}";
const ICON_COLLAPSE: &str = "\u{f0140}";
const METRIC_METER: &str = "usage-meter";
const METRIC_GRAPH: &str = "traffic-graph";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum MonitorSection {
    Cpu,
    Gpu,
    Memory,
    Network,
    Storage,
    Temperatures,
    Power,
    Cooling,
    Battery,
}

impl MonitorSection {
    const ALL: [Self; 9] = [
        Self::Cpu,
        Self::Gpu,
        Self::Memory,
        Self::Network,
        Self::Storage,
        Self::Temperatures,
        Self::Power,
        Self::Cooling,
        Self::Battery,
    ];

    const fn id(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Gpu => "gpu",
            Self::Memory => "memory",
            Self::Network => "network",
            Self::Storage => "storage",
            Self::Temperatures => "temperatures",
            Self::Power => "power",
            Self::Cooling => "cooling",
            Self::Battery => "battery",
        }
    }

    const fn title(self) -> &'static str {
        match self {
            Self::Cpu => "CPU",
            Self::Gpu => "GPU",
            Self::Memory => "Memory",
            Self::Network => "Network",
            Self::Storage => "Storage",
            Self::Temperatures => "Temperatures",
            Self::Power => "Power",
            Self::Cooling => "Cooling",
            Self::Battery => "Battery",
        }
    }

    fn from_id(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|section| section.id() == value.trim())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SectionPreference {
    section: MonitorSection,
    visible: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MonitorSettings {
    enabled: bool,
    sections: Vec<SectionPreference>,
    hidden_metrics: HashSet<String>,
    metric_names: HashMap<String, String>,
    output: Option<String>,
    network_interface: Option<String>,
    interval_ms: i32,
    economy: bool,
    position_y: Option<i32>,
    pinned: bool,
    scale_milli: i32,
    width: i32,
    height: Option<i32>,
    hide_delay_ms: i32,
}

impl Default for MonitorSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            sections: MonitorSection::ALL
                .into_iter()
                .map(|section| SectionPreference {
                    section,
                    visible: true,
                })
                .collect(),
            hidden_metrics: HashSet::new(),
            metric_names: HashMap::new(),
            output: None,
            network_interface: None,
            interval_ms: SAMPLE_INTERVAL_DEFAULT_MS,
            economy: true,
            position_y: None,
            pinned: false,
            scale_milli: SCALE_MILLI_DEFAULT,
            width: PANEL_DEFAULT_WIDTH,
            height: None,
            hide_delay_ms: HIDE_DELAY_DEFAULT_MS,
        }
    }
}

impl MonitorSettings {
    fn load() -> Self {
        let defaults = Self::default();
        let key_file = glib::KeyFile::new();
        if key_file
            .load_from_file(settings_path(), glib::KeyFileFlags::NONE)
            .is_err()
        {
            return defaults;
        }

        Self::from_key_file(&key_file)
    }

    fn from_key_file(key_file: &glib::KeyFile) -> Self {
        let defaults = Self::default();
        let enabled = key_file
            .boolean(SETTINGS_GROUP, "enabled")
            .unwrap_or(defaults.enabled);
        let order = key_file
            .string(SETTINGS_GROUP, "order")
            .ok()
            .map(|value| parse_section_list(&value))
            .unwrap_or_default();
        let hidden = key_file
            .string(SETTINGS_GROUP, "hidden")
            .ok()
            .map(|value| parse_section_list(&value).into_iter().collect())
            .unwrap_or_default();
        let position_y = key_file.integer(SETTINGS_GROUP, "position_y").ok();
        let pinned = key_file.boolean(SETTINGS_GROUP, "pinned").unwrap_or(false);
        let legacy_scale = key_file
            .integer(SETTINGS_GROUP, "scale")
            .unwrap_or(SCALE_MILLI_DEFAULT)
            .clamp(700, 2_000);
        let scale_milli = key_file
            .integer(SETTINGS_GROUP, "font_scale")
            .unwrap_or(legacy_scale)
            .clamp(SCALE_MILLI_MIN, SCALE_MILLI_MAX);
        let width = key_file
            .integer(SETTINGS_GROUP, "width")
            .unwrap_or_else(|_| scaled_pixels(PANEL_DEFAULT_WIDTH, legacy_scale))
            .clamp(PANEL_MIN_WIDTH, PANEL_MAX_WIDTH);
        let height = key_file
            .integer(SETTINGS_GROUP, "height")
            .ok()
            .map(|height| height.max(PANEL_MIN_HEIGHT));

        Self {
            enabled,
            sections: normalized_sections(order, &hidden),
            hidden_metrics: key_file
                .string(SETTINGS_GROUP, "hidden_metrics")
                .ok()
                .and_then(|value| serde_json::from_str(&value).ok())
                .unwrap_or_default(),
            metric_names: key_file
                .string(SETTINGS_GROUP, "metric_names")
                .ok()
                .and_then(|value| serde_json::from_str(&value).ok())
                .unwrap_or_default(),
            output: key_file
                .string(SETTINGS_GROUP, "output")
                .ok()
                .map(|value| value.to_string())
                .filter(|value| !value.is_empty()),
            network_interface: key_file
                .string(SETTINGS_GROUP, "network_interface")
                .ok()
                .map(|value| value.to_string())
                .filter(|value| !value.is_empty()),
            interval_ms: key_file
                .integer(SETTINGS_GROUP, "interval_ms")
                .unwrap_or(SAMPLE_INTERVAL_DEFAULT_MS)
                .clamp(500, 10_000),
            economy: key_file.boolean(SETTINGS_GROUP, "economy").unwrap_or(true),
            position_y,
            pinned,
            scale_milli,
            width,
            height,
            hide_delay_ms: key_file
                .integer(SETTINGS_GROUP, "hide_delay_ms")
                .unwrap_or(HIDE_DELAY_DEFAULT_MS)
                .clamp(0, HIDE_DELAY_MAX_MS),
        }
    }

    fn save(&self) -> Result<(), String> {
        let path = settings_path();
        let parent = path
            .parent()
            .ok_or_else(|| "failed to resolve system monitor state directory".to_owned())?;
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create system monitor state directory: {error}"))?;

        let key_file = self.to_key_file();
        let temporary = path.with_extension("ini.tmp");
        if let Err(error) = key_file.save_to_file(&temporary) {
            let _ = fs::remove_file(&temporary);
            return Err(format!("failed to save system monitor settings: {error}"));
        }
        if let Err(error) = fs::rename(&temporary, &path) {
            let _ = fs::remove_file(&temporary);
            return Err(format!(
                "failed to install system monitor settings: {error}"
            ));
        }
        Ok(())
    }

    fn to_key_file(&self) -> glib::KeyFile {
        let key_file = glib::KeyFile::new();
        key_file.set_boolean(SETTINGS_GROUP, "enabled", self.enabled);
        key_file.set_string(
            SETTINGS_GROUP,
            "order",
            &self
                .sections
                .iter()
                .map(|preference| preference.section.id())
                .collect::<Vec<_>>()
                .join(","),
        );
        key_file.set_string(
            SETTINGS_GROUP,
            "hidden",
            &self
                .sections
                .iter()
                .filter(|preference| !preference.visible)
                .map(|preference| preference.section.id())
                .collect::<Vec<_>>()
                .join(","),
        );
        if let Some(position_y) = self.position_y {
            key_file.set_integer(SETTINGS_GROUP, "position_y", position_y);
        }
        key_file.set_boolean(SETTINGS_GROUP, "pinned", self.pinned);
        let mut hidden_metrics = self.hidden_metrics.iter().collect::<Vec<_>>();
        hidden_metrics.sort_unstable();
        key_file.set_string(
            SETTINGS_GROUP,
            "hidden_metrics",
            &serde_json::to_string(&hidden_metrics).expect("metric IDs are JSON strings"),
        );
        key_file.set_integer(SETTINGS_GROUP, "font_scale", self.scale_milli);
        key_file.set_integer(SETTINGS_GROUP, "width", self.width);
        key_file.set_integer(SETTINGS_GROUP, "hide_delay_ms", self.hide_delay_ms);
        key_file.set_integer(SETTINGS_GROUP, "interval_ms", self.interval_ms);
        key_file.set_boolean(SETTINGS_GROUP, "economy", self.economy);
        key_file.set_string(
            SETTINGS_GROUP,
            "output",
            self.output.as_deref().unwrap_or_default(),
        );
        key_file.set_string(
            SETTINGS_GROUP,
            "network_interface",
            self.network_interface.as_deref().unwrap_or_default(),
        );
        let names = self
            .metric_names
            .iter()
            .collect::<std::collections::BTreeMap<_, _>>();
        key_file.set_string(
            SETTINGS_GROUP,
            "metric_names",
            &serde_json::to_string(&names).expect("metric names are strings"),
        );
        if let Some(height) = self.height {
            key_file.set_integer(SETTINGS_GROUP, "height", height);
        }
        key_file
    }

    fn metric_visible(&self, section: MonitorSection, id: &str) -> bool {
        let legacy_storage_bar_hidden = section == MonitorSection::Storage
            && id.ends_with(":usage-meter")
            && self.hidden_metrics.contains("storage:usage-meter");
        !legacy_storage_bar_hidden
            && !gpu::legacy_metric_id(section, id).is_some_and(|legacy| {
                self.hidden_metrics
                    .contains(&format!("{}:{legacy}", section.id()))
            })
            && !self
                .hidden_metrics
                .contains(&format!("{}:{id}", section.id()))
    }

    fn metric_name(&self, section: MonitorSection, id: &str, default: &str) -> String {
        self.metric_names
            .get(&format!("{}:{id}", section.id()))
            .cloned()
            .unwrap_or_else(|| default.to_owned())
    }

    fn wants(&self, section: MonitorSection, id: &str) -> bool {
        self.sections
            .iter()
            .any(|item| item.section == section && item.visible)
            && self.metric_visible(section, id)
    }

    fn effective_interval(&self, visible: bool) -> Duration {
        Duration::from_millis(if self.economy && !visible {
            self.interval_ms.max(5_000)
        } else {
            self.interval_ms
        } as u64)
    }

    fn set_metric_visible(&mut self, section: MonitorSection, id: &str, visible: bool) {
        let key = format!("{}:{id}", section.id());
        if visible {
            self.hidden_metrics.remove(&key);
        } else {
            self.hidden_metrics.insert(key);
        }
    }
}

fn parse_section_list(value: &str) -> Vec<MonitorSection> {
    value
        .split(',')
        .flat_map(|id| match id.trim() {
            "performance" => vec![MonitorSection::Cpu, MonitorSection::Gpu],
            "hardware" => vec![MonitorSection::Power, MonitorSection::Cooling],
            _ => MonitorSection::from_id(id).into_iter().collect(),
        })
        .collect()
}

fn normalized_sections(
    requested_order: Vec<MonitorSection>,
    hidden: &HashSet<MonitorSection>,
) -> Vec<SectionPreference> {
    let mut seen = HashSet::new();
    requested_order
        .into_iter()
        .chain(MonitorSection::ALL)
        .filter(|section| seen.insert(*section))
        .map(|section| SectionPreference {
            section,
            visible: !hidden.contains(&section),
        })
        .collect()
}

fn settings_path() -> PathBuf {
    glib::user_state_dir()
        .join("obsidian-bar")
        .join(SETTINGS_FILE)
}

#[derive(Clone, Debug, Default)]
struct GpuSnapshot {
    name: Option<String>,
    utilization_percent: Option<f64>,
    memory_used: Option<u64>,
    memory_total: Option<u64>,
    power_watts: Option<f64>,
    temperature_celsius: Option<f64>,
    frequency_mhz: Option<f64>,
    client_memory: bool,
    states: HashMap<&'static str, gpu::DataState>,
}

#[derive(Clone, Debug)]
struct SensorReading {
    id: String,
    label: String,
    value: f64,
    state: gpu::DataState,
}

impl SensorReading {
    fn formatted(&self, unit: &str, digits: usize) -> String {
        if self.state == gpu::DataState::Ready {
            format!("{:.digits$} {unit}", self.value)
        } else {
            self.state.label().to_owned()
        }
    }
}

#[derive(Clone, Debug)]
struct BatterySnapshot {
    percent: f64,
    status: String,
    power_watts: Option<f64>,
}

#[derive(Clone, Debug)]
struct DiskSnapshot {
    id: String,
    name: String,
    used: u64,
    total: u64,
    available: u64,
}

#[derive(Clone, Debug, Default)]
struct SystemSnapshot {
    ready: bool,
    cpu_percent: Option<f64>,
    cpu_frequency_mhz: Option<f64>,
    load_average: Option<f64>,
    running_processes: Option<u64>,
    total_processes: Option<u64>,
    uptime_seconds: Option<u64>,
    memory_used: Option<u64>,
    memory_total: Option<u64>,
    swap_used: Option<u64>,
    swap_total: Option<u64>,
    gpus: Vec<gpu::GpuDevice>,
    section_states: HashMap<MonitorSection, gpu::DataState>,
    network_choices: Vec<String>,
    network_available: bool,
    network_interfaces: Vec<String>,
    download_bytes_per_second: f64,
    upload_bytes_per_second: f64,
    network_received_bytes: u64,
    network_transmitted_bytes: u64,
    disks: Vec<DiskSnapshot>,
    temperatures: Vec<SensorReading>,
    power: Vec<SensorReading>,
    fans: Vec<SensorReading>,
    battery: Option<BatterySnapshot>,
}

type SnapshotSubscriber = Box<dyn Fn(&SystemSnapshot) -> bool>;
type SettingsSubscriber = Box<dyn Fn(&MonitorSettings) -> bool>;
type StateSubscriber = Box<dyn Fn(bool) -> bool>;

pub struct SystemMonitorController {
    settings: RefCell<MonitorSettings>,
    latest: RefCell<SystemSnapshot>,
    sampler: Arc<Mutex<SystemSampler>>,
    gpu_sampling_enabled: Arc<AtomicBool>,
    snapshot_subscribers: RefCell<Vec<SnapshotSubscriber>>,
    settings_subscribers: RefCell<Vec<SettingsSubscriber>>,
    state_subscribers: RefCell<Vec<StateSubscriber>>,
    timer: RefCell<Option<glib::SourceId>>,
    sampling: Cell<bool>,
    started: Cell<bool>,
    first_sample_logged: Cell<bool>,
    view_visible: Cell<bool>,
    sample_pending: Cell<bool>,
    sample_generation: Cell<u64>,
}

impl SystemMonitorController {
    pub fn new() -> Rc<Self> {
        let settings = MonitorSettings::load();
        let gpu_sampling_enabled = Arc::new(AtomicBool::new(settings.enabled));
        Rc::new(Self {
            settings: RefCell::new(settings),
            latest: RefCell::new(SystemSnapshot::default()),
            sampler: Arc::new(Mutex::new(SystemSampler {
                gpu_sampling_enabled: Arc::clone(&gpu_sampling_enabled),
                ..SystemSampler::default()
            })),
            gpu_sampling_enabled,
            snapshot_subscribers: RefCell::new(Vec::new()),
            settings_subscribers: RefCell::new(Vec::new()),
            state_subscribers: RefCell::new(Vec::new()),
            timer: RefCell::new(None),
            sampling: Cell::new(false),
            started: Cell::new(false),
            first_sample_logged: Cell::new(false),
            view_visible: Cell::new(false),
            sample_pending: Cell::new(false),
            sample_generation: Cell::new(0),
        })
    }

    pub fn start(self: &Rc<Self>) {
        if self.started.replace(true) {
            return;
        }
        if self.enabled() {
            self.start_sampling();
        }
    }

    fn start_sampling(self: &Rc<Self>) {
        if !self.started.get() || !self.enabled() || self.timer.borrow().is_some() {
            return;
        }
        self.sync_gpu_sampling();
        self.sample_now();
        self.arm_timer();
    }

    fn arm_timer(self: &Rc<Self>) {
        if let Some(timer) = self.timer.borrow_mut().take() {
            timer.remove();
        }
        if !self.started.get() || !self.enabled() {
            return;
        }
        let weak = Rc::downgrade(self);
        let source = glib::timeout_add_local_once(
            self.settings
                .borrow()
                .effective_interval(self.view_visible.get()),
            move || {
                let Some(controller) = weak.upgrade() else {
                    return;
                };
                controller.timer.borrow_mut().take();
                controller.sample_now();
                controller.arm_timer();
            },
        );
        self.timer.replace(Some(source));
    }

    pub fn shutdown(&self) {
        self.started.set(false);
        self.stop_sampling();
    }

    fn stop_sampling(&self) {
        self.gpu_sampling_enabled.store(false, Ordering::Release);
        if let Some(source) = self.timer.borrow_mut().take() {
            source.remove();
        }
    }

    fn sync_gpu_sampling(&self) {
        let settings = self.settings.borrow();
        self.gpu_sampling_enabled.store(
            settings.enabled && (!settings.economy || self.view_visible.get()),
            Ordering::Release,
        );
    }

    fn set_view_visible(self: &Rc<Self>, visible: bool) {
        if self.view_visible.replace(visible) == visible {
            return;
        }
        self.sync_gpu_sampling();
        self.sample_generation
            .set(self.sample_generation.get().wrapping_add(1));
        self.arm_timer();
        if visible {
            self.request_sample();
        }
    }

    fn request_sample(self: &Rc<Self>) {
        if self.sampling.get() {
            self.sample_pending.set(true);
        } else {
            self.sample_now();
        }
    }

    fn sample_now(self: &Rc<Self>) {
        if !self.started.get() || !self.enabled() || self.sampling.replace(true) {
            return;
        }

        let sampler = Arc::clone(&self.sampler);
        let settings = self.settings();
        let visible = self.view_visible.get();
        let generation = self.sample_generation.get();
        let weak = Rc::downgrade(self);
        run_background(
            move || {
                let mut sampler = sampler
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                sampler.sample_with(&settings, visible)
            },
            move |snapshot| {
                let Some(controller) = weak.upgrade() else {
                    return;
                };
                controller.sampling.set(false);
                if !controller.started.get() || !controller.enabled() {
                    return;
                }
                if generation != controller.sample_generation.get() {
                    controller.sample_pending.set(false);
                    controller.sample_now();
                    return;
                }
                if !controller.first_sample_logged.replace(true) {
                    info!(
                        cpu = snapshot.cpu_percent.is_some(),
                        gpu = snapshot.gpus.len(),
                        memory = snapshot.memory_used.is_some(),
                        network = snapshot.network_available,
                        storage = snapshot.disks.len(),
                        temperatures = snapshot.temperatures.len(),
                        power = snapshot.power.len(),
                        fans = snapshot.fans.len(),
                        battery = snapshot.battery.is_some(),
                        "system monitor received its first hardware snapshot"
                    );
                }
                controller.latest.replace(snapshot.clone());
                controller
                    .snapshot_subscribers
                    .borrow_mut()
                    .retain(|subscriber| subscriber(&snapshot));
                if controller.sample_pending.replace(false) {
                    controller.sample_now();
                }
            },
        );
    }

    fn subscribe_snapshot(&self, subscriber: impl Fn(&SystemSnapshot) -> bool + 'static) {
        if subscriber(&self.latest.borrow()) {
            self.snapshot_subscribers
                .borrow_mut()
                .push(Box::new(subscriber));
        }
    }

    fn subscribe_settings(&self, subscriber: impl Fn(&MonitorSettings) -> bool + 'static) {
        if subscriber(&self.settings.borrow()) {
            self.settings_subscribers
                .borrow_mut()
                .push(Box::new(subscriber));
        }
    }

    pub fn subscribe_state(&self, subscriber: impl Fn(bool) -> bool + 'static) {
        if subscriber(self.enabled()) {
            self.state_subscribers
                .borrow_mut()
                .push(Box::new(subscriber));
        }
    }

    pub fn enabled(&self) -> bool {
        self.settings.borrow().enabled
    }

    fn settings(&self) -> MonitorSettings {
        self.settings.borrow().clone()
    }

    fn latest(&self) -> SystemSnapshot {
        self.latest.borrow().clone()
    }

    fn update_settings(self: &Rc<Self>, update: impl FnOnce(&mut MonitorSettings)) -> bool {
        let previous = self.settings();
        let mut next = previous.clone();
        update(&mut next);
        if next == *self.settings.borrow() {
            return true;
        }
        if let Err(error) = next.save() {
            warn!(%error, "failed to update system monitor settings");
            return false;
        }

        self.settings.replace(next.clone());
        self.sync_gpu_sampling();
        self.settings_subscribers
            .borrow_mut()
            .retain(|subscriber| subscriber(&next));
        if previous.sections != next.sections
            || previous.hidden_metrics != next.hidden_metrics
            || previous.network_interface != next.network_interface
            || previous.interval_ms != next.interval_ms
            || previous.economy != next.economy
        {
            self.sample_generation
                .set(self.sample_generation.get().wrapping_add(1));
            self.arm_timer();
            self.request_sample();
        }
        true
    }

    pub fn set_enabled(self: &Rc<Self>, enabled: bool) -> bool {
        if enabled == self.enabled() {
            return true;
        }
        let mut next = self.settings();
        next.enabled = enabled;
        if let Err(error) = next.save() {
            warn!(%error, "failed to update system monitor state");
            return false;
        }
        self.settings.replace(next);
        self.sample_generation
            .set(self.sample_generation.get().wrapping_add(1));
        self.sync_gpu_sampling();
        if self.started.get() {
            if enabled {
                self.start_sampling();
            } else {
                self.stop_sampling();
            }
        }
        self.state_subscribers
            .borrow_mut()
            .retain(|subscriber| subscriber(enabled));
        true
    }

    fn set_section_visible(self: &Rc<Self>, section: MonitorSection, visible: bool) -> bool {
        self.update_settings(|settings| {
            if let Some(preference) = settings
                .sections
                .iter_mut()
                .find(|preference| preference.section == section)
            {
                preference.visible = visible;
            }
        })
    }

    fn move_section(self: &Rc<Self>, section: MonitorSection, direction: i32) -> bool {
        self.update_settings(|settings| {
            let Some(index) = settings
                .sections
                .iter()
                .position(|preference| preference.section == section)
            else {
                return;
            };
            let target = if direction < 0 {
                index.checked_sub(1)
            } else {
                index
                    .checked_add(1)
                    .filter(|target| *target < settings.sections.len())
            };
            if let Some(target) = target {
                settings.sections.swap(index, target);
            }
        })
    }

    fn set_metric_visible(
        self: &Rc<Self>,
        section: MonitorSection,
        id: &str,
        visible: bool,
    ) -> bool {
        self.update_settings(|settings| {
            if let Some(legacy) = gpu::legacy_metric_id(section, id)
                && settings
                    .hidden_metrics
                    .remove(&format!("{}:{legacy}", section.id()))
            {
                for option in metric_options(section, &self.latest.borrow()) {
                    if gpu::legacy_metric_id(section, &option.id) == Some(legacy) {
                        settings.set_metric_visible(section, &option.id, false);
                    }
                }
            }
            if section == MonitorSection::Storage
                && id.ends_with(":usage-meter")
                && settings.hidden_metrics.remove("storage:usage-meter")
            {
                for disk in &self.latest.borrow().disks {
                    settings.set_metric_visible(
                        section,
                        &format!("{}:{METRIC_METER}", disk.id),
                        false,
                    );
                }
            }
            settings.set_metric_visible(section, id, visible);
        })
    }

    fn set_position(self: &Rc<Self>, y: i32) -> bool {
        self.update_settings(|settings| settings.position_y = Some(y))
    }

    fn set_dimensions(self: &Rc<Self>, width: i32, height: i32, scale_milli: i32) -> bool {
        self.update_settings(|settings| {
            settings.width = width.clamp(PANEL_MIN_WIDTH, PANEL_MAX_WIDTH);
            settings.height = Some(height.max(PANEL_MIN_HEIGHT));
            settings.scale_milli = scale_milli.clamp(SCALE_MILLI_MIN, SCALE_MILLI_MAX);
        })
    }

    fn set_pinned(self: &Rc<Self>, pinned: bool) -> bool {
        self.update_settings(|settings| settings.pinned = pinned)
    }

    fn set_hide_delay(self: &Rc<Self>, milliseconds: i32) -> bool {
        self.update_settings(|settings| {
            settings.hide_delay_ms = milliseconds.clamp(0, HIDE_DELAY_MAX_MS)
        })
    }

    fn set_metric_name(
        self: &Rc<Self>,
        section: MonitorSection,
        id: &str,
        name: &str,
        default: &str,
    ) -> bool {
        self.update_settings(|settings| {
            let key = format!("{}:{id}", section.id());
            let name = name.trim().chars().take(100).collect::<String>();
            if name.is_empty() || name == default {
                settings.metric_names.remove(&key);
            } else {
                settings.metric_names.insert(key, name);
            }
        })
    }

    pub fn preferred_output(&self) -> Option<String> {
        self.settings.borrow().output.clone()
    }

    fn set_output(self: &Rc<Self>, output: Option<String>) -> bool {
        if !self.update_settings(|settings| settings.output = output) {
            return false;
        }
        self.state_subscribers
            .borrow_mut()
            .retain(|subscriber| subscriber(self.enabled()));
        true
    }
}

#[derive(Clone)]
struct DisplayMetric {
    id: String,
    label: String,
    value: String,
    group: Option<MetricGroup>,
    fraction: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MetricGroup {
    id: String,
    label: String,
}

impl DisplayMetric {
    fn new(id: impl Into<String>, label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            value: value.into(),
            group: None,
            fraction: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MetricOption {
    id: String,
    label: String,
    group: Option<MetricGroup>,
}

fn metric_options(section: MonitorSection, snapshot: &SystemSnapshot) -> Vec<MetricOption> {
    let (metrics, fraction) = display_metrics(section, snapshot);
    let mut options = metrics
        .into_iter()
        .filter(|metric| metric.id != "loading")
        .map(|metric| MetricOption {
            id: metric.id,
            label: metric.label,
            group: metric.group,
        })
        .collect::<Vec<_>>();
    if section == MonitorSection::Network && snapshot.network_available {
        options.insert(
            0,
            MetricOption {
                id: METRIC_GRAPH.into(),
                label: "Traffic graph".into(),
                group: None,
            },
        );
    }
    if fraction.is_some() {
        options.push(MetricOption {
            id: METRIC_METER.into(),
            label: "Usage bar".into(),
            group: None,
        });
    }
    options
}

fn selected_metrics(
    section: MonitorSection,
    snapshot: &SystemSnapshot,
    settings: &MonitorSettings,
) -> (Vec<DisplayMetric>, Option<f64>, bool) {
    let (mut metrics, fraction) = display_metrics(section, snapshot);
    metrics.retain(|metric| settings.metric_visible(section, &metric.id));
    for metric in &mut metrics {
        metric.label = settings.metric_name(section, &metric.id, &metric.label);
    }
    let fraction = fraction.filter(|_| settings.metric_visible(section, METRIC_METER));
    let graph = section == MonitorSection::Network
        && snapshot.network_available
        && settings.metric_visible(section, METRIC_GRAPH);
    (metrics, fraction, graph)
}

#[derive(Default)]
struct NetworkGraphState {
    download: VecDeque<f64>,
    upload: VecDeque<f64>,
}

impl NetworkGraphState {
    fn push(&mut self, download: f64, upload: f64) {
        self.download.push_back(download.max(0.0));
        self.upload.push_back(upload.max(0.0));
        while self.download.len() > NETWORK_HISTORY_LENGTH {
            self.download.pop_front();
        }
        while self.upload.len() > NETWORK_HISTORY_LENGTH {
            self.upload.pop_front();
        }
    }
}

struct MeterView {
    area: gtk::DrawingArea,
    fraction: Rc<Cell<f64>>,
}

impl MeterView {
    fn new() -> Self {
        let area = gtk::DrawingArea::new();
        area.add_css_class("system-monitor-meter");
        area.set_content_height(3);
        area.set_hexpand(true);
        area.set_can_target(false);

        let fraction = Rc::new(Cell::new(0.0_f64));
        let draw_fraction = Rc::clone(&fraction);
        area.set_draw_func(move |area, context, width, height| {
            if width <= 0 || height <= 0 {
                return;
            }

            let width = f64::from(width);
            let height = f64::from(height);
            let middle = height / 2.0;
            let inset = middle.min(width / 2.0);
            let end = (width - inset).max(inset);
            let color = area.color();
            let red = f64::from(color.red());
            let green = f64::from(color.green());
            let blue = f64::from(color.blue());

            context.set_line_width(height.max(1.0));
            context.set_line_cap(gtk::cairo::LineCap::Round);
            context.move_to(inset, middle);
            context.line_to(end, middle);
            context.set_source_rgba(red, green, blue, 0.13);
            let _ = context.stroke();

            let filled = inset + (end - inset) * draw_fraction.get().clamp(0.0, 1.0);
            if filled > inset {
                context.move_to(inset, middle);
                context.line_to(filled, middle);
                context.set_source_rgba(red, green, blue, 0.82);
                let _ = context.stroke();
            }
        });

        Self { area, fraction }
    }

    fn set_fraction(&self, fraction: Option<f64>) {
        self.area.set_visible(fraction.is_some());
        self.fraction
            .set(fraction.unwrap_or_default().clamp(0.0, 1.0));
        self.area.queue_draw();
    }
}

struct SectionView {
    root: gtk::Box,
    rows: gtk::Box,
    signature: RefCell<Vec<(String, String)>>,
    metric_groups: RefCell<Vec<Option<MetricGroup>>>,
    metric_views: RefCell<Vec<MetricView>>,
    meter: Option<MeterView>,
    graph: Option<gtk::DrawingArea>,
    graph_state: Option<Rc<RefCell<NetworkGraphState>>>,
    available: Cell<bool>,
    scale_milli: Cell<i32>,
    minimum_width: Cell<i32>,
}

enum MetricView {
    Value(gtk::Label),
    Meter(MeterView),
}

impl SectionView {
    fn new(section: MonitorSection) -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 3);
        root.add_css_class("system-monitor-section");

        let divider = gtk::Separator::new(gtk::Orientation::Horizontal);
        divider.add_css_class("system-monitor-divider");
        root.append(&divider);

        let (graph, graph_state) = if section == MonitorSection::Network {
            let state = Rc::new(RefCell::new(NetworkGraphState::default()));
            let area = gtk::DrawingArea::new();
            area.add_css_class("system-monitor-network-graph");
            area.set_content_height(72);
            area.set_hexpand(true);
            area.set_can_target(false);
            {
                let state = Rc::clone(&state);
                area.set_draw_func(move |area, context, width, height| {
                    draw_network_graph(area, context, width, height, &state.borrow());
                });
            }
            root.append(&area);
            (Some(area), Some(state))
        } else {
            (None, None)
        };

        let rows = gtk::Box::new(gtk::Orientation::Vertical, 1);
        rows.add_css_class("system-monitor-rows");
        root.append(&rows);

        let meter =
            matches!(section, MonitorSection::Memory | MonitorSection::Battery).then(|| {
                let meter = MeterView::new();
                root.append(&meter.area);
                meter
            });

        Rc::new(Self {
            root,
            rows,
            signature: RefCell::new(Vec::new()),
            metric_groups: RefCell::new(Vec::new()),
            metric_views: RefCell::new(Vec::new()),
            meter,
            graph,
            graph_state,
            available: Cell::new(false),
            scale_milli: Cell::new(SCALE_MILLI_DEFAULT),
            minimum_width: Cell::new(0),
        })
    }

    fn update(&self, metrics: Vec<DisplayMetric>, fraction: Option<f64>, graph_visible: bool) {
        let signature = metrics
            .iter()
            .map(|metric| (metric.id.clone(), metric.label.clone()))
            .collect::<Vec<_>>();
        let groups = metrics
            .iter()
            .map(|metric| metric.group.clone())
            .collect::<Vec<_>>();
        if *self.signature.borrow() != signature || *self.metric_groups.borrow() != groups {
            self.minimum_width.set(0);
            clear_box(&self.rows);
            let mut views = self.metric_views.borrow_mut();
            views.clear();
            let mut previous_group = None;
            for metric in &metrics {
                if metric.group.as_ref() != previous_group {
                    if let Some(group) = &metric.group {
                        let title = gtk::Label::new(Some(&group.label));
                        title.add_css_class("system-monitor-storage-title");
                        title.set_xalign(0.0);
                        title.set_wrap(true);
                        title.set_wrap_mode(gtk::pango::WrapMode::WordChar);
                        title.set_natural_wrap_mode(gtk::NaturalWrapMode::None);
                        self.rows.append(&title);
                    }
                    previous_group = metric.group.as_ref();
                }
                if metric.fraction.is_some() {
                    let meter = MeterView::new();
                    meter
                        .area
                        .set_content_height(scaled_pixels(3, self.scale_milli.get()));
                    self.rows.append(&meter.area);
                    views.push(MetricView::Meter(meter));
                    continue;
                }
                let row = gtk::Box::new(
                    gtk::Orientation::Horizontal,
                    scaled_pixels(6, self.scale_milli.get()),
                );
                row.add_css_class("system-monitor-row");

                let accent = gtk::Box::new(gtk::Orientation::Vertical, 0);
                accent.add_css_class("system-monitor-row-accent");

                let label = gtk::Label::new(Some(&metric.label));
                label.add_css_class("system-monitor-label");
                label.set_xalign(0.0);
                label.set_hexpand(true);
                label.set_wrap(true);
                label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
                label.set_natural_wrap_mode(gtk::NaturalWrapMode::None);

                let value = gtk::Label::new(Some(&metric.value));
                value.add_css_class("system-monitor-value");
                value.set_xalign(1.0);
                value.set_wrap(true);
                value.set_wrap_mode(gtk::pango::WrapMode::Word);
                value.set_natural_wrap_mode(gtk::NaturalWrapMode::None);
                value.set_justify(gtk::Justification::Right);

                row.append(&accent);
                row.append(&label);
                row.append(&value);
                self.rows.append(&row);
                views.push(MetricView::Value(value));
            }
            self.signature.replace(signature);
            self.metric_groups.replace(groups);
        }

        for (view, metric) in self.metric_views.borrow().iter().zip(&metrics) {
            match view {
                MetricView::Value(label) => {
                    label.set_label(&metric.value);
                }
                MetricView::Meter(meter) => meter.set_fraction(metric.fraction),
            }
        }

        if let Some(meter) = self.meter.as_ref() {
            meter.set_fraction(fraction);
        }
        if let Some(graph) = &self.graph {
            graph.set_visible(graph_visible);
        }
        self.available
            .set(!metrics.is_empty() || fraction.is_some() || graph_visible);
    }

    fn apply_scale(&self, scale_milli: i32) {
        self.minimum_width.set(0);
        self.scale_milli.set(scale_milli);
        self.root.set_spacing(scaled_pixels(3, scale_milli));
        self.rows.set_spacing(scaled_pixels(1, scale_milli));
        for view in self.metric_views.borrow().iter() {
            if let MetricView::Meter(meter) = view {
                meter.area.set_content_height(scaled_pixels(3, scale_milli));
            }
        }
        let mut row = self.rows.first_child();
        while let Some(widget) = row {
            if let Some(row) = widget.downcast_ref::<gtk::Box>() {
                row.set_spacing(scaled_pixels(6, scale_milli));
            }
            row = widget.next_sibling();
        }
        if let Some(graph) = &self.graph {
            graph.set_content_height(scaled_pixels(72, scale_milli));
        }
        if let Some(meter) = &self.meter {
            meter.area.set_content_height(scaled_pixels(3, scale_milli));
        }
    }

    fn push_network_sample(&self, download: f64, upload: f64) {
        let Some(state) = self.graph_state.as_ref() else {
            return;
        };
        state.borrow_mut().push(download, upload);
        if let Some(graph) = self.graph.as_ref() {
            graph.queue_draw();
        }
    }
}

struct MonitorLayout {
    root: gtk::Box,
    sections: HashMap<MonitorSection, Rc<SectionView>>,
    empty_state: gtk::Box,
    empty_title: gtk::Label,
    empty_hint: gtk::Label,
    applied_scale: Cell<i32>,
    scale_provider: gtk::CssProvider,
    display: gdk::Display,
    settings: RefCell<MonitorSettings>,
    snapshot: RefCell<SystemSnapshot>,
}

impl MonitorLayout {
    fn new(
        display: &gdk::Display,
        settings: MonitorSettings,
        snapshot: SystemSnapshot,
    ) -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 7);
        root.add_css_class("system-monitor-surface");
        root.set_valign(gtk::Align::Start);

        let empty_state = gtk::Box::new(gtk::Orientation::Vertical, 3);
        empty_state.add_css_class("system-monitor-empty");
        empty_state.set_halign(gtk::Align::Fill);
        empty_state.set_can_target(false);
        let empty_icon = gtk::Label::new(Some(ICON_MONITOR));
        empty_icon.add_css_class("system-monitor-empty-icon");
        let empty_title = gtk::Label::new(None);
        empty_title.add_css_class("system-monitor-empty-title");
        empty_title.set_wrap(true);
        let empty_hint = gtk::Label::new(None);
        empty_hint.add_css_class("system-monitor-empty-hint");
        empty_hint.set_wrap(true);
        empty_hint.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        empty_state.append(&empty_icon);
        empty_state.append(&empty_title);
        empty_state.append(&empty_hint);

        let sections = MonitorSection::ALL
            .into_iter()
            .map(|section| (section, SectionView::new(section)))
            .collect();
        let scale_provider = gtk::CssProvider::new();
        gtk::style_context_add_provider_for_display(
            display,
            &scale_provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
        );
        let layout = Rc::new(Self {
            root,
            sections,
            empty_state,
            empty_title,
            empty_hint,
            applied_scale: Cell::new(0),
            scale_provider,
            display: display.clone(),
            settings: RefCell::new(settings.clone()),
            snapshot: RefCell::new(snapshot.clone()),
        });
        layout.apply_settings(&settings);
        layout.update_snapshot(&snapshot);
        layout
    }

    fn apply_settings(&self, settings: &MonitorSettings) {
        self.settings.replace(settings.clone());
        if self.applied_scale.replace(settings.scale_milli) != settings.scale_milli {
            self.scale_provider.load_from_data(&format!(
                ".system-monitor-body {{ font-size: {:.2}px; }}",
                11.0 * f64::from(settings.scale_milli) / 1000.0
            ));
            self.root
                .set_spacing(scaled_pixels(7, settings.scale_milli));
            self.empty_state
                .set_spacing(scaled_pixels(3, settings.scale_milli));
            for section in self.sections.values() {
                section.apply_scale(settings.scale_milli);
            }
        }
        clear_box(&self.root);
        for preference in &settings.sections {
            if let Some(section) = self.sections.get(&preference.section) {
                self.root.append(&section.root);
            }
        }
        self.root.append(&self.empty_state);
        self.update_metrics(&self.snapshot.borrow());
    }

    fn minimum_width(&self) -> i32 {
        let mut current = 0;
        let mut reserved = 0;
        for section in self
            .sections
            .values()
            .filter(|section| section.root.is_visible())
        {
            let (_, natural, _, _) = section.root.measure(gtk::Orientation::Horizontal, -1);
            // Keep the widest sampled value until the rows or font change, so
            // ordinary telemetry updates cannot repeatedly shrink the panel.
            let minimum = natural.max(section.minimum_width.get());
            section.minimum_width.set(minimum);
            current = current.max(natural);
            reserved = reserved.max(minimum);
        }
        let (_, natural, _, _) = self.root.measure(gtk::Orientation::Horizontal, -1);
        // The root adds the body's CSS padding (or the empty-state message).
        natural + reserved - current
    }

    fn natural_height(&self, width: i32) -> i32 {
        let (_, natural, _, _) = self.root.measure(gtk::Orientation::Vertical, width.max(1));
        natural.max(1)
    }

    fn update_snapshot(&self, snapshot: &SystemSnapshot) {
        self.snapshot.replace(snapshot.clone());
        if let Some(view) = self.sections.get(&MonitorSection::Network) {
            view.push_network_sample(
                snapshot.download_bytes_per_second,
                snapshot.upload_bytes_per_second,
            );
        }
        self.update_metrics(snapshot);
    }

    fn update_metrics(&self, snapshot: &SystemSnapshot) {
        let settings = self.settings.borrow();
        for section in MonitorSection::ALL {
            let Some(view) = self.sections.get(&section) else {
                continue;
            };
            let (metrics, fraction, graph) = selected_metrics(section, snapshot, &settings);
            view.update(metrics, fraction, graph);
        }
        self.refresh_visibility();
    }

    fn refresh_visibility(&self) {
        let settings = self.settings.borrow();
        self.root.set_visible(settings.enabled);
        let mut has_visible_metrics = false;
        for preference in &settings.sections {
            if let Some(section) = self.sections.get(&preference.section) {
                let visible = settings.enabled && preference.visible && section.available.get();
                section.root.set_visible(visible);
                has_visible_metrics |= visible;
            }
        }

        let selected_options = settings
            .sections
            .iter()
            .filter(|preference| preference.visible)
            .flat_map(|preference| {
                metric_options(preference.section, &self.snapshot.borrow())
                    .into_iter()
                    .map(move |metric| (preference.section, metric.id))
            })
            .collect::<Vec<_>>();
        let has_selected_sections = settings
            .sections
            .iter()
            .any(|preference| preference.visible);
        let all_items_hidden = !selected_options.is_empty()
            && selected_options
                .iter()
                .all(|(section, id)| !settings.metric_visible(*section, id));
        if has_selected_sections && !all_items_hidden {
            self.empty_title.set_label("No compatible metrics");
            self.empty_hint
                .set_label("This device did not report data for the selected sections.");
        } else {
            self.empty_title.set_label("No metrics selected");
            self.empty_hint
                .set_label("Open settings to choose what should be shown.");
        }
        self.empty_state
            .set_visible(settings.enabled && !has_visible_metrics);
    }
}

impl Drop for MonitorLayout {
    fn drop(&mut self) {
        gtk::style_context_remove_provider_for_display(&self.display, &self.scale_provider);
    }
}

fn display_metrics(
    section: MonitorSection,
    snapshot: &SystemSnapshot,
) -> (Vec<DisplayMetric>, Option<f64>) {
    let mut metrics = Vec::new();
    let fraction = match section {
        MonitorSection::Cpu => {
            if !snapshot.ready {
                metrics.push(DisplayMetric::new("loading", "System Monitor", "Loading…"));
                return (metrics, None);
            }
            if let Some(cpu) = snapshot.cpu_percent {
                metrics.push(DisplayMetric::new("cpu", "CPU", format_percent(cpu)));
            }
            if let Some(frequency) = snapshot.cpu_frequency_mhz {
                metrics.push(DisplayMetric::new(
                    "cpu-frequency",
                    "CPU Frequency",
                    if frequency >= 1000.0 {
                        format!("{:.2} GHz", frequency / 1000.0)
                    } else {
                        format!("{frequency:.0} MHz")
                    },
                ));
            }
            if let Some(load) = snapshot.load_average {
                metrics.push(DisplayMetric::new(
                    "load-average",
                    "Load (1 min)",
                    format!("{load:.2}"),
                ));
            }
            if let (Some(running), Some(total)) =
                (snapshot.running_processes, snapshot.total_processes)
            {
                metrics.push(DisplayMetric::new(
                    "processes",
                    "Processes",
                    format!("{running} / {total}"),
                ));
            }
            if let Some(seconds) = snapshot.uptime_seconds {
                metrics.push(DisplayMetric::new(
                    "uptime",
                    "Uptime",
                    format_duration(seconds),
                ));
            }
            None
        }
        MonitorSection::Gpu => {
            metrics.extend(gpu::display(section, &snapshot.gpus));
            None
        }
        MonitorSection::Memory => {
            if let Some(used) = snapshot.memory_used {
                metrics.push(DisplayMetric::new(
                    "memory-used",
                    "Memory Used",
                    format_bytes(used),
                ));
            }
            if let Some(total) = snapshot.memory_total {
                metrics.push(DisplayMetric::new(
                    "memory-total",
                    "Memory Total",
                    format_bytes(total),
                ));
            }
            if let (Some(used), Some(total)) = (snapshot.memory_used, snapshot.memory_total) {
                metrics.push(DisplayMetric::new(
                    "memory-available",
                    "Available",
                    format_bytes(total.saturating_sub(used)),
                ));
            }
            if let Some(used) = snapshot.swap_used {
                metrics.push(DisplayMetric::new(
                    "swap-used",
                    "Swap Used",
                    format_bytes(used),
                ));
            }
            if let Some(total) = snapshot.swap_total {
                metrics.push(DisplayMetric::new(
                    "swap-total",
                    "Swap Total",
                    format_bytes(total),
                ));
            }
            ratio(snapshot.memory_used, snapshot.memory_total)
        }
        MonitorSection::Network => {
            if snapshot.network_available {
                if !snapshot.network_interfaces.is_empty() {
                    metrics.push(DisplayMetric::new(
                        "network-interfaces",
                        "Interfaces",
                        snapshot.network_interfaces.join(", "),
                    ));
                }
                metrics.push(DisplayMetric::new(
                    "download",
                    "Download Rate",
                    format_rate(snapshot.download_bytes_per_second),
                ));
                metrics.push(DisplayMetric::new(
                    "upload",
                    "Upload Rate",
                    format_rate(snapshot.upload_bytes_per_second),
                ));
                metrics.push(DisplayMetric::new(
                    "network-received",
                    "Received",
                    format_bytes(snapshot.network_received_bytes),
                ));
                metrics.push(DisplayMetric::new(
                    "network-sent",
                    "Sent",
                    format_bytes(snapshot.network_transmitted_bytes),
                ));
            }
            None
        }
        MonitorSection::Storage => {
            for disk in &snapshot.disks {
                let group = MetricGroup {
                    id: disk.id.clone(),
                    label: disk.name.clone(),
                };
                for (id, label, bytes) in [
                    ("used", "Used", disk.used),
                    ("total", "Total", disk.total),
                    ("available", "Available", disk.available),
                ] {
                    let mut metric =
                        DisplayMetric::new(format!("{}:{id}", disk.id), label, format_bytes(bytes));
                    metric.group = Some(group.clone());
                    metrics.push(metric);
                }
                if let Some(fraction) = ratio(Some(disk.used), Some(disk.total)) {
                    let mut metric =
                        DisplayMetric::new(format!("{}:{METRIC_METER}", disk.id), "Usage bar", "");
                    metric.group = Some(group);
                    metric.fraction = Some(fraction);
                    metrics.push(metric);
                }
            }
            None
        }
        MonitorSection::Temperatures => {
            for sensor in &snapshot.temperatures {
                metrics.push(DisplayMetric::new(
                    &sensor.id,
                    &sensor.label,
                    sensor.formatted("°C", 1),
                ));
            }
            metrics.extend(gpu::display(section, &snapshot.gpus));
            None
        }
        MonitorSection::Power => {
            for sensor in &snapshot.power {
                metrics.push(DisplayMetric::new(
                    &sensor.id,
                    &sensor.label,
                    sensor.formatted("W", 1),
                ));
            }
            metrics.extend(gpu::display(section, &snapshot.gpus));
            None
        }
        MonitorSection::Cooling => {
            for sensor in &snapshot.fans {
                metrics.push(DisplayMetric::new(
                    &sensor.id,
                    &sensor.label,
                    sensor.formatted("RPM", 0),
                ));
            }
            None
        }
        MonitorSection::Battery => {
            if let Some(battery) = snapshot.battery.as_ref() {
                metrics.push(DisplayMetric::new(
                    "battery",
                    "Battery",
                    format!("{:.0}%", battery.percent),
                ));
                metrics.push(DisplayMetric::new(
                    "battery-status",
                    "Status",
                    &battery.status,
                ));
                if let Some(power) = battery.power_watts {
                    let label = if battery.status.eq_ignore_ascii_case("charging") {
                        "Charging Rate"
                    } else {
                        "Power Draw"
                    };
                    metrics.push(DisplayMetric::new(
                        "battery-power",
                        label,
                        format!("{power:.1} W"),
                    ));
                }
                Some(battery.percent / 100.0)
            } else {
                None
            }
        }
    };
    (metrics, fraction)
}

fn ratio(value: Option<u64>, total: Option<u64>) -> Option<f64> {
    value
        .zip(total)
        .filter(|(_, total)| *total > 0)
        .map(|(value, total)| value as f64 / total as f64)
}

fn format_percent(value: f64) -> String {
    if value >= 10.0 {
        format!("{value:.0}%")
    } else {
        format!("{value:.1}%")
    }
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    const TIB: f64 = GIB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= TIB {
        format!("{:.1} TiB", bytes / TIB)
    } else if bytes >= GIB {
        format!("{:.1} GiB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.0} MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.0} KiB", bytes / KIB)
    } else {
        format!("{bytes:.0} B")
    }
}

fn format_rate(bytes_per_second: f64) -> String {
    format!("{}/s", format_bytes(bytes_per_second.max(0.0) as u64))
}

fn format_duration(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = seconds % 86_400 / 3_600;
    let minutes = seconds % 3_600 / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

fn draw_network_graph(
    area: &gtk::DrawingArea,
    context: &gtk::cairo::Context,
    width: i32,
    height: i32,
    state: &NetworkGraphState,
) {
    if width <= 2 || height <= 2 {
        return;
    }
    let width = f64::from(width);
    let height = f64::from(height);
    let color = area.color();
    let red = f64::from(color.red());
    let green = f64::from(color.green());
    let blue = f64::from(color.blue());
    let visual_scale = (height / 72.0).clamp(0.7, 3.0);

    let _ = context.save();
    context.set_line_width(visual_scale);
    context.set_source_rgba(red, green, blue, 0.10);
    for row in 1..4 {
        let y = height * f64::from(row) / 4.0;
        context.move_to(0.0, y);
        context.line_to(width, y);
    }
    let _ = context.stroke();

    let peak = state
        .download
        .iter()
        .chain(&state.upload)
        .copied()
        .fold(1024.0_f64, f64::max);
    draw_graph_series(context, width, height, &state.download, peak, 0.82, true);
    draw_graph_series(context, width, height, &state.upload, peak, 0.48, false);
    let _ = context.restore();
}

fn draw_graph_series(
    context: &gtk::cairo::Context,
    width: f64,
    height: f64,
    values: &VecDeque<f64>,
    peak: f64,
    alpha: f64,
    fill: bool,
) {
    if values.len() < 2 || peak <= 0.0 {
        return;
    }
    let visual_scale = (height / 72.0).clamp(0.7, 3.0);
    let step = width / (NETWORK_HISTORY_LENGTH.saturating_sub(1)) as f64;
    let x_offset = width - step * values.len().saturating_sub(1) as f64;
    let mut first = true;
    for (index, value) in values.iter().enumerate() {
        let x = x_offset + index as f64 * step;
        let normalized = (value / peak).clamp(0.0, 1.0).sqrt();
        let y = height - 2.0 * visual_scale - normalized * (height - 5.0 * visual_scale).max(1.0);
        if first {
            context.move_to(x, y);
            first = false;
        } else {
            context.line_to(x, y);
        }
    }

    if fill {
        let _ = context.copy_path().map(|path| {
            context.line_to(width, height);
            context.line_to(x_offset, height);
            context.close_path();
            context.set_source_rgba(1.0, 1.0, 1.0, 0.055);
            let _ = context.fill();
            context.append_path(&path);
        });
    }
    context.set_source_rgba(1.0, 1.0, 1.0, alpha);
    context.set_line_width((if fill { 1.35 } else { 1.0 }) * visual_scale);
    context.set_line_join(gtk::cairo::LineJoin::Round);
    context.set_line_cap(gtk::cairo::LineCap::Round);
    let _ = context.stroke();
}

#[derive(Clone, Copy)]
struct PanelGeometry {
    screen_width: i32,
    screen_height: i32,
    panel_width: i32,
    top_margin: i32,
}

impl PanelGeometry {
    fn for_monitor(monitor: &gdk::Monitor, requested_width: i32) -> Self {
        let geometry = monitor.geometry();
        let screen_width = geometry.width().max(1);
        let screen_height = geometry.height().max(1);
        let panel_width = requested_width
            .clamp(PANEL_MIN_WIDTH, PANEL_MAX_WIDTH)
            .min((screen_width - drawer::RIGHT_MARGIN).max(1));
        Self {
            screen_width,
            screen_height,
            panel_width,
            top_margin: crate::ui::bar::BAR_VISIBLE_TOP_MARGIN
                + crate::ui::bar::BAR_FALLBACK_HEIGHT
                + PANEL_EDGE_MARGIN,
        }
    }

    fn height_limits(self) -> (i32, i32) {
        let maximum = (self.screen_height - self.top_margin - PANEL_EDGE_MARGIN).max(1);
        (PANEL_MIN_HEIGHT.min(maximum), maximum)
    }

    fn clamp_y(self, y: i32, height: i32) -> i32 {
        let minimum = self.top_margin.min((self.screen_height - 1).max(0));
        let maximum = (self.screen_height - height - PANEL_EDGE_MARGIN).max(minimum);
        y.clamp(minimum, maximum)
    }

    fn default_y(self) -> i32 {
        ((f64::from(self.screen_height) * 0.26).round() as i32).max(self.top_margin)
    }
}

fn monitor_dimension_slider(
    title: &str,
    min: f64,
    max: f64,
    step: f64,
    value: f64,
    digits: i32,
    unit: &'static str,
) -> (gtk::Box, gtk::Scale) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.add_css_class("system-monitor-settings-slider-row");
    let label = gtk::Label::new(Some(title));
    label.add_css_class("system-monitor-settings-section-title");
    label.set_xalign(0.0);
    label.set_size_request(88, -1);
    let slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, min, max, step);
    slider.set_hexpand(true);
    slider.add_css_class("slider-control");
    slider.add_css_class("system-monitor-settings-scale");
    slider.set_value(value);
    slider.set_digits(digits);
    slider.set_draw_value(true);
    slider.set_value_pos(gtk::PositionType::Right);
    slider.set_format_value_func(move |_, value| {
        format!("{value:.precision$} {unit}", precision = digits as usize)
    });
    // Let wheel and touchpad scrolling reach the settings scroller. Keep the
    // range's pointer, touch and keyboard controllers intact.
    let controllers = slider.observe_controllers();
    for index in 0..controllers.n_items() {
        if let Some(scroll) = controllers
            .item(index)
            .and_then(|item| item.downcast::<gtk::EventControllerScroll>().ok())
        {
            scroll.set_propagation_phase(gtk::PropagationPhase::None);
        }
    }
    row.append(&label);
    row.append(&slider);
    (row, slider)
}

pub(crate) fn monitor_output_id(monitor: &gdk::Monitor) -> String {
    monitor
        .connector()
        .map(|name| name.to_string())
        .unwrap_or_else(|| {
            let geometry = monitor.geometry();
            format!(
                "{}-{}-{}",
                monitor.model().unwrap_or_default(),
                geometry.x(),
                geometry.y()
            )
        })
}

fn settings_control_row(title: &str, control: &impl IsA<gtk::Widget>) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Vertical, 5);
    let label = gtk::Label::new(Some(title));
    label.set_xalign(0.0);
    label.add_css_class("system-monitor-settings-section-title");
    row.append(&label);
    row.append(control);
    row
}

fn settings_dropdown() -> gtk::DropDown {
    let dropdown = gtk::DropDown::from_strings(&["Automatic"]);
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let label = gtk::Label::new(None);
        label.set_xalign(0.0);
        label.set_width_chars(1);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        item.set_child(Some(&label));
    });
    factory.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let label = item.child().unwrap().downcast::<gtk::Label>().unwrap();
        if let Some(value) = item
            .item()
            .and_then(|item| item.downcast::<gtk::StringObject>().ok())
        {
            label.set_text(&value.string());
        }
    });
    dropdown.set_factory(Some(&factory));
    dropdown.add_css_class("system-monitor-settings-dropdown");
    dropdown
}

fn constrain_metric_name(widget: &impl IsA<gtk::Widget>) {
    let widget = widget.as_ref();
    if let Some(label) = widget.downcast_ref::<gtk::Label>() {
        label.set_width_chars(1);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    }
    if let Some(text) = widget.downcast_ref::<gtk::Text>() {
        text.set_width_chars(1);
    }
    let mut child = widget.first_child();
    while let Some(widget) = child {
        constrain_metric_name(&widget);
        child = widget.next_sibling();
    }
}

fn settings_popover(widget: &impl IsA<gtk::Widget>) -> Option<gtk::Popover> {
    let widget = widget.as_ref();
    if let Some(popover) = widget.downcast_ref::<gtk::Popover>() {
        return Some(popover.clone());
    }
    let mut child = widget.first_child();
    while let Some(widget) = child {
        if let Some(popover) = settings_popover(&widget) {
            return Some(popover);
        }
        child = widget.next_sibling();
    }
    None
}

struct MonitorSettingsPanel {
    root: gtk::Box,
    list: gtk::Box,
    pin: gtk::ToggleButton,
    syncing_pin: Cell<bool>,
    scale: gtk::Scale,
    width: gtk::Scale,
    height: gtk::Scale,
    hide_delay: gtk::Scale,
    interval: gtk::Scale,
    economy: gtk::ToggleButton,
    output: gtk::DropDown,
    network: gtk::DropDown,
    popovers: Vec<gtk::Popover>,
    output_choices: RefCell<Vec<(String, String)>>,
    network_choices: RefCell<Vec<(String, String)>>,
    editing_changed: RefCell<Option<Box<dyn Fn()>>>,
    syncing_scale: Cell<bool>,
    rebuild_pending: Cell<bool>,
    syncing_selection: Cell<bool>,
    groups: RefCell<HashMap<MonitorSection, SettingsGroupView>>,
    order: RefCell<Vec<MonitorSection>>,
    controller: Rc<SystemMonitorController>,
}

impl MonitorSettingsPanel {
    fn new(controller: &Rc<SystemMonitorController>) -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
        root.add_css_class("system-monitor-settings-panel");
        root.set_size_request(SETTINGS_PANEL_WIDTH - SETTINGS_PANEL_PADDING * 2, -1);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 9);
        header.add_css_class("system-monitor-settings-header");
        let icon = gtk::Label::new(Some(ICON_MONITOR));
        icon.add_css_class("system-monitor-settings-icon");
        let title = gtk::Label::new(Some("System monitor"));
        title.add_css_class("system-monitor-settings-title");
        title.set_xalign(0.0);
        title.set_hexpand(true);
        header.append(&icon);
        header.append(&title);

        let pin = gtk::ToggleButton::new();
        let pin_icon = gtk::Label::new(Some("\u{f0403}"));
        pin_icon.add_css_class("system-monitor-settings-button-icon");
        pin.set_child(Some(&pin_icon));
        pin.add_css_class("system-monitor-settings-pin");
        pin.update_property(&[gtk::accessible::Property::Label("Pin panel")]);
        pin.set_active(controller.settings().pinned);
        let economy = gtk::ToggleButton::new();
        let economy_icon = gtk::Label::new(Some("\u{f032a}"));
        economy_icon.add_css_class("system-monitor-settings-button-icon");
        economy.set_child(Some(&economy_icon));
        economy.add_css_class("system-monitor-settings-pin");
        economy.update_property(&[gtk::accessible::Property::Label(
            "Reduce updates when hidden",
        )]);
        economy.set_active(controller.settings().economy);
        header.append(&economy);
        header.append(&pin);

        let settings = controller.settings();
        let (width_row, width) = monitor_dimension_slider(
            "Width",
            f64::from(PANEL_MIN_WIDTH),
            f64::from(PANEL_MAX_WIDTH),
            1.0,
            f64::from(settings.width),
            0,
            "px",
        );
        let (height_row, height) = monitor_dimension_slider(
            "Height",
            f64::from(PANEL_MIN_HEIGHT),
            2160.0,
            1.0,
            f64::from(settings.height.unwrap_or(720)),
            0,
            "px",
        );
        let (scale_row, scale) = monitor_dimension_slider(
            "Font size",
            8.0,
            24.0,
            0.5,
            11.0 * f64::from(settings.scale_milli) / 1000.0,
            1,
            "px",
        );
        let (delay_row, hide_delay) = monitor_dimension_slider(
            "Hide delay",
            0.0,
            f64::from(HIDE_DELAY_MAX_MS) / 1000.0,
            0.5,
            f64::from(settings.hide_delay_ms) / 1000.0,
            1,
            "s",
        );

        let section_title = gtk::Label::new(Some("Groups and metrics"));
        section_title.add_css_class("system-monitor-settings-section-title");
        section_title.set_xalign(0.0);

        let list = gtk::Box::new(gtk::Orientation::Vertical, 4);
        list.add_css_class("system-monitor-settings-list");

        root.append(&header);
        let (interval_row, interval) = monitor_dimension_slider(
            "Update interval",
            0.5,
            10.0,
            0.5,
            f64::from(settings.interval_ms) / 1000.0,
            1,
            "s",
        );
        let output = settings_dropdown();
        let network = settings_dropdown();
        let popovers = [&output, &network]
            .into_iter()
            .filter_map(settings_popover)
            .collect();
        let adjustments = gtk::Expander::new(Some("Size and timing"));
        adjustments.add_css_class("system-monitor-settings-adjustments");
        let sliders = gtk::Box::new(gtk::Orientation::Vertical, 2);
        sliders.set_margin_top(6);
        for row in [
            &width_row,
            &height_row,
            &scale_row,
            &delay_row,
            &interval_row,
        ] {
            sliders.append(row);
        }
        adjustments.set_child(Some(&sliders));
        root.append(&adjustments);
        root.append(&settings_control_row("Display", &output));
        root.append(&settings_control_row("Network interface", &network));
        root.append(&section_title);
        root.append(&list);

        let panel = Rc::new(Self {
            root,
            list,
            pin,
            syncing_pin: Cell::new(false),
            scale,
            width,
            height,
            hide_delay,
            interval,
            economy,
            output,
            network,
            popovers,
            output_choices: RefCell::new(Vec::new()),
            network_choices: RefCell::new(Vec::new()),
            editing_changed: RefCell::new(None),
            syncing_scale: Cell::new(false),
            rebuild_pending: Cell::new(false),
            syncing_selection: Cell::new(false),
            groups: RefCell::new(HashMap::new()),
            order: RefCell::new(Vec::new()),
            controller: Rc::clone(controller),
        });

        {
            let weak = Rc::downgrade(&panel);
            panel.pin.connect_toggled(move |pin| {
                let Some(panel) = weak.upgrade() else {
                    return;
                };
                if panel.syncing_pin.get() {
                    return;
                }
                if !panel.controller.set_pinned(pin.is_active()) {
                    panel.sync_pin(panel.controller.settings().pinned);
                }
            });
        }

        for slider in [&panel.width, &panel.height, &panel.scale] {
            let weak = Rc::downgrade(&panel);
            slider.connect_value_changed(move |_| {
                let Some(panel) = weak.upgrade() else {
                    return;
                };
                if panel.syncing_scale.get() {
                    return;
                }
                if !panel.controller.set_dimensions(
                    panel.width.value().round() as i32,
                    panel.height.value().round() as i32,
                    (panel.scale.value() * 1000.0 / 11.0).round() as i32,
                ) {
                    panel.sync_dimensions(&panel.controller.settings());
                }
            });
        }

        {
            let weak = Rc::downgrade(&panel);
            panel.hide_delay.connect_value_changed(move |slider| {
                if let Some(panel) = weak.upgrade()
                    && !panel.syncing_scale.get()
                    && !panel
                        .controller
                        .set_hide_delay((slider.value() * 1000.0).round() as i32)
                {
                    panel.sync_dimensions(&panel.controller.settings());
                }
            });
        }

        {
            let weak = Rc::downgrade(&panel);
            panel.interval.connect_value_changed(move |slider| {
                if let Some(panel) = weak.upgrade()
                    && !panel.syncing_scale.get()
                    && !panel.controller.update_settings(|settings| {
                        settings.interval_ms = (slider.value() * 1000.0).round() as i32
                    })
                {
                    panel.sync_dimensions(&panel.controller.settings());
                }
            });
        }
        {
            let weak = Rc::downgrade(&panel);
            panel.economy.connect_toggled(move |button| {
                if let Some(panel) = weak.upgrade()
                    && !panel.syncing_scale.get()
                    && !panel
                        .controller
                        .update_settings(|settings| settings.economy = button.is_active())
                {
                    panel.sync_dimensions(&panel.controller.settings());
                }
            });
        }
        {
            let weak = Rc::downgrade(&panel);
            panel.output.connect_selected_notify(move |dropdown| {
                if let Some(panel) = weak.upgrade()
                    && !panel.syncing_scale.get()
                {
                    let output = panel
                        .output_choices
                        .borrow()
                        .get(dropdown.selected() as usize)
                        .map(|(id, _)| id.clone())
                        .filter(|id| !id.is_empty());
                    if !panel.controller.set_output(output) {
                        panel.schedule_rebuild();
                    }
                }
            });
        }
        {
            let weak = Rc::downgrade(&panel);
            panel.network.connect_selected_notify(move |dropdown| {
                if let Some(panel) = weak.upgrade()
                    && !panel.syncing_scale.get()
                {
                    let network = panel
                        .network_choices
                        .borrow()
                        .get(dropdown.selected() as usize)
                        .map(|(id, _)| id.clone())
                        .filter(|id| !id.is_empty());
                    if !panel
                        .controller
                        .update_settings(|settings| settings.network_interface = network)
                    {
                        panel.schedule_rebuild();
                    }
                }
            });
        }
        if let Some(display) = gdk::Display::default() {
            let weak = Rc::downgrade(&panel);
            display.monitors().connect_items_changed(move |_, _, _, _| {
                if let Some(panel) = weak.upgrade() {
                    panel.schedule_rebuild();
                }
            });
        }
        {
            let weak = Rc::downgrade(&panel);
            controller.subscribe_settings(move |_| {
                let Some(panel) = weak.upgrade() else {
                    return false;
                };
                panel.schedule_rebuild();
                true
            });
        }

        {
            let weak = Rc::downgrade(&panel);
            controller.subscribe_snapshot(move |_| {
                let Some(panel) = weak.upgrade() else {
                    return false;
                };
                if panel.root.is_mapped() {
                    // Values update in place; refreshing also tracks device state
                    // and interface changes without replacing expanded groups.
                    panel.schedule_rebuild();
                }
                true
            });
        }

        panel
    }

    fn is_interacting(&self) -> bool {
        self.popovers.iter().any(|popover| popover.is_visible())
            || self
                .groups
                .borrow()
                .values()
                .any(|group| group.renamers.iter().any(|name| name.is_editing()))
    }

    fn sync_dimensions(&self, settings: &MonitorSettings) {
        self.syncing_scale.set(true);
        self.width.set_value(f64::from(settings.width));
        if let Some(height) = settings.height {
            self.height.set_value(f64::from(height));
        }
        self.scale
            .set_value(11.0 * f64::from(settings.scale_milli) / 1000.0);
        self.hide_delay
            .set_value(f64::from(settings.hide_delay_ms) / 1000.0);
        self.interval
            .set_value(f64::from(settings.interval_ms) / 1000.0);
        self.economy.set_active(settings.economy);
        self.syncing_scale.set(false);
    }

    fn sync_sources(&self, settings: &MonitorSettings, snapshot: &SystemSnapshot) {
        self.syncing_scale.set(true);
        let mut outputs = vec![(String::new(), "Automatic".to_owned())];
        if let Some(display) = gdk::Display::default() {
            let monitors = display.monitors();
            for index in 0..monitors.n_items() {
                if let Some(monitor) = monitors
                    .item(index)
                    .and_then(|item| item.downcast::<gdk::Monitor>().ok())
                {
                    let id = monitor_output_id(&monitor);
                    outputs.push((
                        id.clone(),
                        format!("{id} · {}", monitor.model().unwrap_or_default()),
                    ));
                }
            }
        }
        if let Some(output) = &settings.output
            && !outputs.iter().any(|(id, _)| id == output)
        {
            outputs.push((output.clone(), format!("{output} · disconnected")));
        }
        if *self.output_choices.borrow() != outputs {
            let strings = gtk::StringList::new(
                &outputs
                    .iter()
                    .map(|(_, label)| label.as_str())
                    .collect::<Vec<_>>(),
            );
            self.output.set_model(Some(&strings));
            self.output_choices.replace(outputs.clone());
        }
        self.output.set_selected(
            outputs
                .iter()
                .position(|(id, _)| Some(id.as_str()) == settings.output.as_deref())
                .unwrap_or(0) as u32,
        );
        let mut networks = vec![String::new()];
        networks.extend(snapshot.network_choices.clone());
        if let Some(network) = &settings.network_interface
            && !networks.contains(network)
        {
            networks.push(network.clone());
        }
        let networks = networks
            .iter()
            .map(|name| {
                let label = if name.is_empty() {
                    "Automatic".to_owned()
                } else if !snapshot.network_choices.contains(name) {
                    format!("{name} · unavailable")
                } else {
                    name.clone()
                };
                (name.clone(), label)
            })
            .collect::<Vec<_>>();
        if *self.network_choices.borrow() != networks {
            self.network.set_model(Some(&gtk::StringList::new(
                &networks
                    .iter()
                    .map(|(_, label)| label.as_str())
                    .collect::<Vec<_>>(),
            )));
            self.network_choices.replace(networks.clone());
        }
        self.network.set_selected(
            networks
                .iter()
                .position(|(id, _)| Some(id.as_str()) == settings.network_interface.as_deref())
                .unwrap_or(0) as u32,
        );
        self.syncing_scale.set(false);
    }

    fn sync_limits(&self, width_limits: (i32, i32), height_limits: (i32, i32), height: i32) {
        self.syncing_scale.set(true);
        self.width
            .set_range(f64::from(width_limits.0), f64::from(width_limits.1));
        self.width
            .set_value(f64::from(self.controller.settings().width));
        self.height
            .set_range(f64::from(height_limits.0), f64::from(height_limits.1));
        self.height.set_value(f64::from(
            self.controller.settings().height.unwrap_or(height),
        ));
        self.syncing_scale.set(false);
    }

    fn sync_pin(&self, pinned: bool) {
        self.syncing_pin.set(true);
        self.pin.set_active(pinned);
        self.pin
            .update_property(&[gtk::accessible::Property::Label(if pinned {
                "Unpin panel"
            } else {
                "Pin panel"
            })]);
        self.syncing_pin.set(false);
    }

    fn schedule_rebuild(self: &Rc<Self>) {
        if self.rebuild_pending.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            let Some(panel) = weak.upgrade() else {
                return;
            };
            panel.rebuild_pending.set(false);
            panel.rebuild();
        });
    }

    fn rebuild(self: &Rc<Self>) {
        let settings = self.controller.settings();
        self.sync_pin(settings.pinned);
        self.sync_dimensions(&settings);
        let snapshot = self.controller.latest();
        self.sync_sources(&settings, &snapshot);
        let last_index = settings.sections.len().saturating_sub(1);
        self.syncing_selection.set(true);
        let mut groups = self.groups.borrow_mut();

        for (index, preference) in settings.sections.iter().enumerate() {
            let section = preference.section;
            let group = groups
                .entry(section)
                .or_insert_with(|| SettingsGroupView::new(self, section));
            let options = metric_options(section, &snapshot);
            if group.options.as_ref() != Some(&options) {
                clear_box(&group.choices);
                group.toggles.clear();
                group.renamers.clear();
                let mut previous_group = None;
                for option in &options {
                    if option.group.as_ref() != previous_group {
                        if let Some(device) = &option.group {
                            let heading = gtk::Label::new(Some(&device.label));
                            heading.set_xalign(0.0);
                            heading.set_width_chars(1);
                            heading.set_ellipsize(gtk::pango::EllipsizeMode::End);
                            heading.add_css_class("system-monitor-settings-row-title");
                            heading.set_margin_top(6);
                            group.choices.append(&heading);
                        }
                        previous_group = option.group.as_ref();
                    }
                    let weak = Rc::downgrade(self);
                    let id = option.id.clone();
                    let toggle = settings_visibility_button(
                        settings.metric_visible(section, &id),
                        &option.label,
                        move |visible| {
                            weak.upgrade().is_some_and(|panel| {
                                panel.syncing_selection.get()
                                    || panel.controller.set_metric_visible(section, &id, visible)
                            })
                        },
                    );
                    let row = gtk::Box::new(gtk::Orientation::Horizontal, 5);
                    row.add_css_class("system-monitor-settings-metric-row");
                    let name = gtk::EditableLabel::new(&settings.metric_name(
                        section,
                        &option.id,
                        &option.label,
                    ));
                    name.set_hexpand(true);
                    name.set_size_request(0, -1);
                    name.add_css_class("system-monitor-settings-name");
                    constrain_metric_name(&name);
                    let weak = Rc::downgrade(self);
                    let id = option.id.clone();
                    let default = option.label.clone();
                    name.connect_notify_local(Some("editing"), move |name, _| {
                        let Some(panel) = weak.upgrade() else { return };
                        if let Some(changed) = panel.editing_changed.borrow().as_ref() {
                            changed();
                        }
                        if !name.is_editing() && !panel.syncing_selection.get() {
                            panel.controller.set_metric_name(
                                section,
                                &id,
                                name.text().as_str(),
                                &default,
                            );
                            panel.schedule_rebuild();
                        }
                    });
                    let edit = settings_move_button("\u{f03eb}", "Rename metric", true);
                    let field = name.clone();
                    edit.connect_clicked(move |_| field.start_editing());
                    row.append(&name);
                    row.append(&edit);
                    row.append(&toggle);
                    group.choices.append(&row);
                    group.toggles.push(toggle);
                    group.renamers.push(name);
                }
                if options.is_empty() {
                    let label = gtk::Label::new(Some(if snapshot.ready {
                        "Not reported by driver"
                    } else {
                        "Waiting for sensor data…"
                    }));
                    label.add_css_class("system-monitor-settings-row-meta");
                    label.set_wrap(true);
                    label.set_xalign(0.0);
                    group.choices.append(&label);
                }
                group.options = Some(options.clone());
            }
            let selected = options
                .iter()
                .filter(|option| settings.metric_visible(section, &option.id))
                .count();
            group.meta.set_label(
                &if let Some(state) = snapshot.section_states.get(&section) {
                    state.label().into()
                } else if section == MonitorSection::Gpu
                    && snapshot
                        .gpus
                        .iter()
                        .any(|device| device.state != gpu::DataState::Ready)
                {
                    snapshot
                        .gpus
                        .iter()
                        .filter(|device| device.state != gpu::DataState::Ready)
                        .map(|device| format!("{}: {}", device.name, device.state.label()))
                        .collect::<Vec<_>>()
                        .join(" · ")
                } else if options.is_empty() {
                    "Not reported by driver".into()
                } else if preference.visible {
                    format!("{selected} of {} selected", options.len())
                } else {
                    format!("Hidden · {selected}/{} selected", options.len())
                },
            );
            group.visible.set_active(preference.visible);
            group.expand.set_sensitive(!options.is_empty());
            group.up.set_sensitive(index > 0);
            group.down.set_sensitive(index < last_index);
            for ((option, toggle), name) in options.iter().zip(&group.toggles).zip(&group.renamers)
            {
                toggle.set_active(settings.metric_visible(section, &option.id));
                if !name.is_editing() {
                    name.set_text(&settings.metric_name(section, &option.id, &option.label));
                }
            }
        }
        let order = settings
            .sections
            .iter()
            .map(|preference| preference.section)
            .collect::<Vec<_>>();
        if *self.order.borrow() != order {
            clear_box(&self.list);
            for section in &order {
                self.list.append(&groups[section].root);
            }
            self.order.replace(order);
        }
        self.syncing_selection.set(false);
    }
}

struct SettingsGroupView {
    root: gtk::Box,
    expand: gtk::ToggleButton,
    meta: gtk::Label,
    up: gtk::Button,
    down: gtk::Button,
    visible: gtk::ToggleButton,
    choices: gtk::Box,
    options: Option<Vec<MetricOption>>,
    toggles: Vec<gtk::ToggleButton>,
    renamers: Vec<gtk::EditableLabel>,
}

impl SettingsGroupView {
    fn new(panel: &Rc<MonitorSettingsPanel>, section: MonitorSection) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("system-monitor-settings-row");
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let copy = gtk::Box::new(gtk::Orientation::Vertical, 1);
        copy.set_hexpand(true);
        let title = gtk::Label::new(Some(section.title()));
        title.add_css_class("system-monitor-settings-row-title");
        title.set_xalign(0.0);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let meta = gtk::Label::new(None);
        meta.add_css_class("system-monitor-settings-row-meta");
        meta.set_xalign(0.0);
        meta.set_ellipsize(gtk::pango::EllipsizeMode::End);
        copy.append(&title);
        copy.append(&meta);
        let chevron = gtk::Label::new(Some(ICON_EXPAND));
        chevron.add_css_class("system-monitor-settings-button-icon");
        let heading = gtk::Box::new(gtk::Orientation::Horizontal, 5);
        heading.append(&chevron);
        heading.append(&copy);
        let expand = gtk::ToggleButton::new();
        expand.add_css_class("system-monitor-settings-expand");
        expand.set_hexpand(true);
        expand.set_child(Some(&heading));
        let choices = gtk::Box::new(gtk::Orientation::Vertical, 2);
        choices.add_css_class("system-monitor-settings-choices");
        let reveal = gtk::Revealer::new();
        reveal.set_transition_type(gtk::RevealerTransitionType::SlideDown);
        reveal.set_transition_duration(160);
        reveal.set_child(Some(&choices));
        let expansion = reveal.clone();
        expand.connect_toggled(move |toggle| {
            expansion.set_reveal_child(toggle.is_active());
            chevron.set_label(if toggle.is_active() {
                ICON_COLLAPSE
            } else {
                ICON_EXPAND
            });
        });
        let up = settings_move_button(ICON_UP, "Move up", true);
        let down = settings_move_button(ICON_DOWN, "Move down", true);
        for (button, direction) in [(&up, -1), (&down, 1)] {
            let weak = Rc::downgrade(panel);
            button.connect_clicked(move |_| {
                if let Some(panel) = weak.upgrade() {
                    panel.controller.move_section(section, direction);
                }
            });
        }
        let weak = Rc::downgrade(panel);
        let visible = settings_visibility_button(true, section.title(), move |visible| {
            weak.upgrade().is_some_and(|panel| {
                panel.syncing_selection.get()
                    || panel.controller.set_section_visible(section, visible)
            })
        });
        header.append(&expand);
        header.append(&up);
        header.append(&down);
        header.append(&visible);
        root.append(&header);
        root.append(&reveal);
        Self {
            root,
            expand,
            meta,
            up,
            down,
            visible,
            choices,
            options: None,
            toggles: Vec::new(),
            renamers: Vec::new(),
        }
    }
}

fn settings_visibility_button(
    active: bool,
    name: &str,
    update: impl Fn(bool) -> bool + 'static,
) -> gtk::ToggleButton {
    let button = gtk::ToggleButton::new();
    button.add_css_class("system-monitor-settings-visibility");
    button.set_valign(gtk::Align::Center);
    let icon = gtk::Label::new(Some(if active { ICON_VISIBLE } else { ICON_HIDDEN }));
    icon.add_css_class("system-monitor-settings-button-icon");
    button.set_child(Some(&icon));
    let name = name.to_owned();
    button.update_property(&[gtk::accessible::Property::Label(&format!(
        "{} {name}",
        if active { "Hide" } else { "Show" }
    ))]);
    button.set_active(active);
    let reverting = Cell::new(false);
    button.connect_toggled(move |toggle| {
        let active = toggle.is_active();
        icon.set_label(if active { ICON_VISIBLE } else { ICON_HIDDEN });
        toggle.update_property(&[gtk::accessible::Property::Label(&format!(
            "{} {name}",
            if active { "Hide" } else { "Show" }
        ))]);
        if !reverting.replace(false) && !update(active) {
            reverting.set(true);
            toggle.set_active(!active);
        }
    });
    button
}

fn settings_move_button(icon: &str, accessible_name: &str, sensitive: bool) -> gtk::Button {
    let label = gtk::Label::new(Some(icon));
    label.add_css_class("system-monitor-settings-button-icon");
    let button = gtk::Button::new();
    button.add_css_class("system-monitor-settings-move");
    button.update_property(&[gtk::accessible::Property::Label(accessible_name)]);
    button.set_sensitive(sensitive);
    button.set_child(Some(&label));
    button
}

fn monitor_input_region(
    width: i32,
    height: i32,
    open: bool,
    header_width: i32,
    header_height: i32,
) -> gtk::cairo::Region {
    let edge_width = drawer::EDGE_TRIGGER_WIDTH.min(width.max(0));
    let region = gtk::cairo::Region::create_rectangle(&gtk::cairo::RectangleInt::new(
        width - edge_width,
        0,
        edge_width,
        height.max(0),
    ));
    if open {
        // Only the drag handle needs a stationary input surface. Let GTK route
        // button and scrolling events directly to the content below.
        let _ = region.union_rectangle(&gtk::cairo::RectangleInt::new(
            0,
            0,
            header_width.clamp(0, width.max(0)),
            header_height.clamp(0, height.max(0)),
        ));
    }
    region
}

struct MonitorRuntime {
    monitor: gdk::Monitor,
    window: gtk::ApplicationWindow,
    surface: gtk::Box,
    settings_reveal: gtk::Revealer,
    settings_scroller: gtk::ScrolledWindow,
    settings_panel: Rc<MonitorSettingsPanel>,
    drawer: Rc<MonitorDrawer>,
    hotspot_window: gtk::ApplicationWindow,
    hotspot: gtk::Box,
    card: gtk::Box,
    header: gtk::Box,
    scroller: gtk::ScrolledWindow,
    layout: Rc<MonitorLayout>,
    requested_y: Cell<i32>,
    requested_width: Cell<i32>,
    requested_height: Cell<Option<i32>>,
    y: Cell<i32>,
    pinned: Cell<bool>,
    hide_delay: Cell<Duration>,
    settings_hovered: Cell<bool>,
    settings_hide_generation: Generation,
    hovered: Cell<bool>,
    hotspot_hovered: Cell<bool>,
    dragging: Cell<bool>,
    drag_start_y: Cell<i32>,
    hide_generation: Generation,
    panel_width: Cell<i32>,
    scale_geometry: Cell<Option<(i32, i32, i32)>>,
}

impl MonitorRuntime {
    fn measure_after_font_update(self: &Rc<Self>) {
        // Inherited CSS fonts settle during layout. Measure after that frame,
        // including the first map with a font restored from saved settings.
        let weak = Rc::downgrade(self);
        self.window.add_tick_callback(move |_, _| {
            let weak = weak.clone();
            glib::idle_add_local_once(move || {
                if let Some(runtime) = weak.upgrade() {
                    for section in runtime.layout.sections.values() {
                        section.minimum_width.set(0);
                    }
                    runtime.apply_size();
                }
            });
            glib::ControlFlow::Break
        });
    }

    fn geometry(&self) -> PanelGeometry {
        let mut geometry = PanelGeometry::for_monitor(&self.monitor, self.requested_width.get());
        if let Some(application) = self.window.application() {
            for window in application.windows() {
                if window.has_css_class("bar-window")
                    && window.monitor().as_ref() == Some(&self.monitor)
                {
                    geometry.top_margin = geometry.top_margin.max(
                        window.height()
                            + crate::ui::bar::BAR_VISIBLE_TOP_MARGIN
                            + PANEL_EDGE_MARGIN,
                    );
                }
            }
        }
        geometry
    }

    fn apply_size(&self) {
        let geometry = self.geometry();
        let (min_height, max_height) = geometry.height_limits();
        let frozen = self.scale_geometry.get();
        let settings_open =
            self.settings_reveal.reveals_child() || self.settings_reveal.is_child_revealed();
        let settings_width = if settings_open {
            SETTINGS_PANEL_WIDTH + 1 + PANEL_EDGE_MARGIN
        } else {
            0
        };
        let maximum_width = (geometry.screen_width - drawer::RIGHT_MARGIN - settings_width)
            .clamp(1, PANEL_MAX_WIDTH);
        let (_, header_width, _, _) = self.header.measure(gtk::Orientation::Horizontal, -1);
        let minimum_width = self
            .layout
            .minimum_width()
            .max(header_width)
            .max(PANEL_MIN_WIDTH)
            .min(maximum_width);
        let width = frozen.map_or_else(
            || geometry.panel_width.clamp(minimum_width, maximum_width),
            |(width, _, _)| width.min(maximum_width),
        );
        self.panel_width.set(width);
        self.card.set_size_request(width, -1);
        let (_, header_height, _, _) = self.header.measure(gtk::Orientation::Vertical, width);
        // The body measurement includes wrapped lines, meters, group gaps and
        // bottom padding. A saved height must not hide any of that content.
        let minimum_height =
            (header_height + self.layout.natural_height(width)).clamp(min_height, max_height);
        let height = frozen.map_or_else(
            || {
                self.requested_height
                    .get()
                    .unwrap_or(minimum_height)
                    .clamp(minimum_height, max_height)
            },
            |(_, height, _)| height.min(max_height),
        );
        // Scrolling is only necessary when the content exceeds the screen (or
        // temporarily while the settings are held still during a slider drag).
        self.scroller.set_propagate_natural_height(false);
        self.scroller.set_min_content_height(1);
        self.scroller.set_max_content_height(-1);
        self.card.set_size_request(width, height);
        let (_, natural, _, _) = self.card.measure(gtk::Orientation::Vertical, width);
        self.settings_panel.sync_limits(
            (minimum_width, maximum_width),
            (minimum_height, max_height),
            natural,
        );
        let mut height = natural.max(drawer::TAIL_TOP + drawer::TAIL_HEIGHT);
        if let Some((_, fixed_height, _)) = frozen {
            height = fixed_height.min(max_height);
        }
        let settings_height = (height - SETTINGS_PANEL_PADDING * 2).max(1);
        self.settings_scroller.set_min_content_height(-1);
        self.settings_scroller
            .set_max_content_height(settings_height);
        self.settings_scroller
            .set_min_content_height(settings_height);
        let y = frozen.map_or_else(
            || geometry.clamp_y(self.requested_y.get(), height),
            |(_, _, y)| geometry.clamp_y(y, height),
        );
        self.y.set(y);
        self.window.set_default_size(1, height);
        self.window.set_margin(Edge::Top, y);
        self.drawer.sync_position();
        if !self.dragging.get() {
            self.hotspot_window.set_margin(Edge::Top, y);
            self.hotspot
                .set_size_request(width + drawer::RIGHT_MARGIN, height);
            self.hotspot_window
                .set_default_size(width + drawer::RIGHT_MARGIN, height);
            self.sync_input_region();
        }
    }

    fn set_settings_open(self: &Rc<Self>, open: bool) {
        if open {
            self.settings_panel.rebuild();
        }
        self.settings_reveal.set_reveal_child(open);
        self.window.set_keyboard_mode(if open {
            KeyboardMode::OnDemand
        } else {
            KeyboardMode::None
        });
        self.apply_size();
        self.sync_reveal();
    }

    fn sync_input_region(&self) {
        if let Some(surface) = self.hotspot_window.surface() {
            let width = surface.width();
            let (header_width, header_height) = self.drag_handle_size();
            let region = monitor_input_region(
                width,
                surface.height(),
                self.drawer.is_open(),
                header_width,
                header_height,
            );
            surface.set_input_region(Some(&region));
        }
    }

    fn drag_handle_size(&self) -> (i32, i32) {
        let width = self
            .header
            .first_child()
            .and_then(|handle| handle.compute_bounds(&self.card))
            .map_or(0, |bounds| (bounds.x() + bounds.width()).floor() as i32);
        (width, self.header.height())
    }

    fn header_hit(&self, x: f64, y: f64) -> bool {
        let (width, height) = self.drag_handle_size();
        self.drawer.is_open()
            && x >= 0.0
            && y >= 0.0
            && y < f64::from(height)
            && x < f64::from(width)
    }

    fn keep_open(&self) -> bool {
        self.pinned.get()
            || self.hovered.get()
            || self.hotspot_hovered.get()
            || self.dragging.get()
            || self.scale_geometry.get().is_some()
            || self.settings_hovered.get()
            || self.settings_panel.is_interacting()
    }

    fn keep_settings_open(&self) -> bool {
        self.settings_hovered.get()
            || self.dragging.get()
            || self.scale_geometry.get().is_some()
            || self.settings_panel.is_interacting()
    }

    fn sync_settings_timeout(self: &Rc<Self>) {
        let generation = self.settings_hide_generation.bump();
        if !self.settings_reveal.reveals_child() || self.keep_settings_open() {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(SETTINGS_HIDE_DELAY, move || {
            if let Some(runtime) = weak.upgrade()
                && runtime.settings_hide_generation.is_current(generation)
                && runtime.settings_reveal.reveals_child()
                && !runtime.keep_settings_open()
            {
                runtime.set_settings_open(false);
            }
        });
    }

    fn sync_reveal(self: &Rc<Self>) {
        self.sync_settings_timeout();
        let generation = self.hide_generation.bump();
        if self.keep_open() {
            self.drawer.set_revealed(true);
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(self.hide_delay.get(), move || {
            if let Some(runtime) = weak.upgrade()
                && runtime.hide_generation.is_current(generation)
                && !runtime.keep_open()
            {
                runtime.window.set_keyboard_mode(KeyboardMode::None);
                runtime.drawer.set_revealed(false);
                runtime.sync_input_region();
            }
        });
    }

    fn install_drag(
        self: &Rc<Self>,
        widget: &impl IsA<gtk::Widget>,
        controller: &Rc<SystemMonitorController>,
    ) {
        let drag = gtk::GestureDrag::new();
        drag.set_button(gdk::BUTTON_PRIMARY);
        {
            let weak = Rc::downgrade(self);
            drag.connect_drag_begin(move |gesture, x, y| {
                let Some(runtime) = weak.upgrade() else {
                    return;
                };
                if !runtime.header_hit(x, y) {
                    gesture.set_state(gtk::EventSequenceState::Denied);
                    return;
                }
                gesture.set_state(gtk::EventSequenceState::Claimed);
                runtime.drag_start_y.set(runtime.y.get());
                runtime.dragging.set(true);
                runtime.sync_reveal();
            });
        }
        {
            let weak = Rc::downgrade(self);
            drag.connect_drag_update(move |_, _, offset_y| {
                let Some(runtime) = weak.upgrade() else {
                    return;
                };
                if !runtime.dragging.get() {
                    return;
                }
                // Keep the input surface stationary until release, so GTK's
                // offsets stay relative to one origin while the panel moves.
                runtime.requested_y.set(
                    runtime
                        .drag_start_y
                        .get()
                        .saturating_add(offset_y.round() as i32),
                );
                runtime.apply_size();
            });
        }
        {
            let weak = Rc::downgrade(self);
            let controller = Rc::clone(controller);
            drag.connect_drag_end(move |_, _, offset_y| {
                let Some(runtime) = weak.upgrade() else {
                    return;
                };
                if !runtime.dragging.replace(false) {
                    return;
                }
                runtime.requested_y.set(
                    runtime
                        .drag_start_y
                        .get()
                        .saturating_add(offset_y.round() as i32),
                );
                runtime.apply_size();
                if !controller.set_position(runtime.y.get()) {
                    let geometry =
                        PanelGeometry::for_monitor(&runtime.monitor, runtime.requested_width.get());
                    runtime.requested_y.set(
                        controller
                            .settings()
                            .position_y
                            .unwrap_or(geometry.default_y()),
                    );
                    runtime.apply_size();
                }
                runtime.sync_reveal();
            });
        }
        {
            let weak = Rc::downgrade(self);
            drag.connect_cancel(move |_, _| {
                if let Some(runtime) = weak.upgrade()
                    && runtime.dragging.replace(false)
                {
                    runtime.requested_y.set(runtime.drag_start_y.get());
                    runtime.apply_size();
                    runtime.sync_reveal();
                }
            });
        }
        widget.add_controller(drag);
    }
}

pub struct SystemMonitorView {
    runtime: Rc<MonitorRuntime>,
    geometry_handler: Option<glib::SignalHandlerId>,
}

impl SystemMonitorView {
    pub fn new(
        application: &gtk::Application,
        monitor: &gdk::Monitor,
        controller: &Rc<SystemMonitorController>,
    ) -> Self {
        let settings = controller.settings();
        let geometry = PanelGeometry::for_monitor(monitor, settings.width);
        let layout = MonitorLayout::new(&monitor.display(), settings.clone(), controller.latest());
        let content_window = gtk::ApplicationWindow::builder()
            .application(application)
            .decorated(false)
            .resizable(false)
            .build();
        content_window.add_css_class("system-monitor-window");
        content_window.init_layer_shell();
        content_window.set_namespace(Some(CONTENT_NAMESPACE));
        content_window.set_layer(Layer::Bottom);
        content_window.set_keyboard_mode(KeyboardMode::None);
        content_window.set_monitor(Some(monitor));
        content_window.set_anchor(Edge::Top, true);
        content_window.set_anchor(Edge::Right, true);
        content_window.set_exclusive_zone(-1);

        let tail = gtk::Box::new(gtk::Orientation::Vertical, 0);
        tail.add_css_class("system-monitor-tail");
        tail.set_size_request(drawer::TAIL_WIDTH, drawer::TAIL_HEIGHT);
        tail.set_valign(gtk::Align::Start);
        tail.set_halign(gtk::Align::End);
        tail.set_hexpand(true);
        tail.set_can_target(false);
        tail.set_margin_top(drawer::TAIL_TOP);
        let grip = gtk::Box::new(gtk::Orientation::Vertical, 0);
        grip.add_css_class("system-monitor-tail-grip");
        grip.set_halign(gtk::Align::Center);
        grip.set_valign(gtk::Align::Center);
        grip.set_vexpand(true);
        tail.append(&grip);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        header.add_css_class("system-monitor-header");
        let drag_handle = gtk::Label::new(Some("System monitor"));
        drag_handle.add_css_class("system-monitor-header-title");
        drag_handle.set_xalign(0.0);
        drag_handle.set_hexpand(true);
        drag_handle.set_ellipsize(gtk::pango::EllipsizeMode::End);
        drag_handle.set_width_chars(1);
        let trigger = gtk::Button::new();
        trigger.add_css_class("system-monitor-settings-trigger");
        trigger.set_size_request(SETTINGS_TRIGGER_SIZE, SETTINGS_TRIGGER_SIZE);
        trigger.update_property(&[gtk::accessible::Property::Label("Configure system monitor")]);
        let trigger_icon = gtk::Label::new(Some(ICON_SETTINGS));
        trigger_icon.add_css_class("system-monitor-settings-trigger-icon");
        trigger.set_child(Some(&trigger_icon));
        header.append(&drag_handle);
        header.append(&trigger);

        let scroller = gtk::ScrolledWindow::new();
        scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::External);
        scroller.set_propagate_natural_height(true);
        scroller.set_vexpand(true);
        scroller.set_child(Some(&layout.root));
        let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
        card.add_css_class("system-monitor-body");
        card.set_valign(gtk::Align::Start);
        card.append(&header);
        card.append(&scroller);
        let hotspot_window = gtk::ApplicationWindow::builder()
            .application(application)
            .decorated(false)
            .resizable(false)
            .build();
        hotspot_window.add_css_class("system-monitor-hotspot-window");
        hotspot_window.init_layer_shell();
        hotspot_window.set_namespace(Some("obsidian-system-monitor-hotspot"));
        hotspot_window.set_layer(Layer::Bottom);
        hotspot_window.set_keyboard_mode(KeyboardMode::None);
        hotspot_window.set_monitor(Some(monitor));
        hotspot_window.set_anchor(Edge::Top, true);
        hotspot_window.set_anchor(Edge::Right, true);
        hotspot_window.set_exclusive_zone(-1);
        let hotspot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        hotspot.append(&tail);
        hotspot_window.set_child(Some(&hotspot));
        let settings_panel = MonitorSettingsPanel::new(controller);
        let settings_scroller = gtk::ScrolledWindow::new();
        settings_scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::External);
        settings_scroller.set_child(Some(&settings_panel.root));
        let settings_surface = gtk::Box::new(gtk::Orientation::Vertical, 0);
        settings_surface.add_css_class("system-monitor-settings-surface");
        settings_surface.append(&settings_scroller);
        let settings_reveal = gtk::Revealer::new();
        settings_reveal.set_transition_type(gtk::RevealerTransitionType::SlideRight);
        settings_reveal.set_transition_duration(220);
        settings_reveal.set_child(Some(&settings_surface));
        let surface = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        surface.add_css_class("system-monitor-card");
        surface.set_overflow(gtk::Overflow::Hidden);
        surface.append(&settings_reveal);
        surface.append(&card);
        let drawer = MonitorDrawer::new(&content_window, &hotspot_window, &surface);

        let runtime = Rc::new(MonitorRuntime {
            monitor: monitor.clone(),
            window: content_window.clone(),
            surface,
            settings_reveal: settings_reveal.clone(),
            settings_scroller,
            settings_panel,
            drawer,
            hotspot_window: hotspot_window.clone(),
            hotspot: hotspot.clone(),
            card,
            header,
            scroller,
            layout,
            requested_y: Cell::new(settings.position_y.unwrap_or(geometry.default_y())),
            requested_width: Cell::new(settings.width),
            requested_height: Cell::new(settings.height),
            y: Cell::new(0),
            pinned: Cell::new(settings.pinned),
            hide_delay: Cell::new(Duration::from_millis(settings.hide_delay_ms as u64)),
            settings_hovered: Cell::new(false),
            settings_hide_generation: Generation::default(),
            hovered: Cell::new(false),
            hotspot_hovered: Cell::new(false),
            dragging: Cell::new(false),
            drag_start_y: Cell::new(0),
            hide_generation: Generation::default(),
            panel_width: Cell::new(geometry.panel_width),
            scale_geometry: Cell::new(None),
        });
        runtime.install_drag(&hotspot, controller);
        {
            let motion = gtk::EventControllerMotion::new();
            let weak = Rc::downgrade(&runtime);
            motion.connect_enter(move |_, _, _| {
                if let Some(runtime) = weak.upgrade() {
                    runtime.settings_hovered.set(true);
                    runtime.sync_reveal();
                }
            });
            let weak = Rc::downgrade(&runtime);
            motion.connect_leave(move |_| {
                if let Some(runtime) = weak.upgrade() {
                    runtime.settings_hovered.set(false);
                    runtime.sync_reveal();
                }
            });
            settings_surface.add_controller(motion);
            let weak = Rc::downgrade(&runtime);
            settings_surface.connect_unmap(move |_| {
                if let Some(runtime) = weak.upgrade() {
                    runtime.settings_hovered.set(false);
                    runtime.settings_hide_generation.bump();
                }
            });
        }
        for popover in &runtime.settings_panel.popovers {
            let weak = Rc::downgrade(&runtime);
            popover.connect_visible_notify(move |_| {
                let weak = weak.clone();
                glib::idle_add_local_once(move || {
                    if let Some(runtime) = weak.upgrade() {
                        runtime.sync_reveal();
                    }
                });
            });
        }
        {
            let weak = Rc::downgrade(&runtime);
            runtime
                .settings_panel
                .editing_changed
                .replace(Some(Box::new(move || {
                    let weak = weak.clone();
                    glib::idle_add_local_once(move || {
                        if let Some(runtime) = weak.upgrade() {
                            runtime.sync_reveal();
                        }
                    });
                })));
        }
        for slider in [
            &runtime.settings_panel.width,
            &runtime.settings_panel.height,
            &runtime.settings_panel.scale,
            &runtime.settings_panel.hide_delay,
            &runtime.settings_panel.interval,
        ] {
            // Keep the settings stationary under the pointer while the slider
            // changes dimensions or font metrics. Apply outer size on release.
            let events = gtk::EventControllerLegacy::new();
            events.set_propagation_phase(gtk::PropagationPhase::Capture);
            let weak = Rc::downgrade(&runtime);
            events.connect_event(move |_, event| {
                if let Some(runtime) = weak.upgrade() {
                    match event.event_type() {
                        gdk::EventType::ButtonPress | gdk::EventType::TouchBegin => {
                            runtime.scale_geometry.set(Some((
                                runtime.panel_width.get(),
                                runtime.surface.height().max(1),
                                runtime.y.get(),
                            )));
                            runtime.sync_reveal();
                        }
                        gdk::EventType::ButtonRelease
                        | gdk::EventType::TouchEnd
                        | gdk::EventType::TouchCancel
                            if runtime.scale_geometry.take().is_some() =>
                        {
                            runtime.apply_size();
                            runtime.sync_reveal();
                        }
                        _ => {}
                    }
                }
                glib::Propagation::Proceed
            });
            slider.add_controller(events);
            let weak = Rc::downgrade(&runtime);
            slider.connect_unmap(move |_| {
                if let Some(runtime) = weak.upgrade()
                    && runtime.scale_geometry.take().is_some()
                {
                    runtime.apply_size();
                }
            });
        }
        {
            let weak = Rc::downgrade(&runtime);
            runtime.drawer.connect_settled(move || {
                if let Some(runtime) = weak.upgrade() {
                    if !runtime.window.is_visible() {
                        runtime.settings_hide_generation.bump();
                        runtime.settings_reveal.set_transition_duration(0);
                        runtime.settings_reveal.set_reveal_child(false);
                        runtime.settings_reveal.set_transition_duration(220);
                        runtime.apply_size();
                    }
                    runtime.sync_input_region();
                }
            });
        }
        for window in [&hotspot_window, &content_window] {
            let weak = Rc::downgrade(&runtime);
            window.connect_realize(move |window| {
                if let Some(runtime) = weak.upgrade() {
                    runtime.sync_input_region();
                }
                if let Some(surface) = window.surface() {
                    let weak = weak.clone();
                    surface.connect_layout(move |_, _, _| {
                        if let Some(runtime) = weak.upgrade() {
                            runtime.sync_input_region();
                        }
                    });
                }
            });
        }
        {
            let weak = Rc::downgrade(&runtime);
            hotspot_window.connect_map(move |_| {
                if let Some(runtime) = weak.upgrade() {
                    runtime.sync_input_region();
                }
            });
        }
        {
            let controller = Rc::downgrade(controller);
            content_window.connect_visible_notify(move |window| {
                if let Some(controller) = controller.upgrade() {
                    controller.set_view_visible(window.is_visible());
                }
            });
        }
        {
            let weak = Rc::downgrade(&runtime);
            let first_map = Cell::new(true);
            content_window.connect_map(move |_| {
                if first_map.replace(false)
                    && let Some(runtime) = weak.upgrade()
                {
                    runtime.measure_after_font_update();
                }
            });
        }
        for (widget, is_tail) in [(runtime.surface.clone(), false), (hotspot.clone(), true)] {
            let motion = gtk::EventControllerMotion::new();
            let weak = Rc::downgrade(&runtime);
            motion.connect_enter(move |_, _, _| {
                if let Some(runtime) = weak.upgrade() {
                    if is_tail {
                        runtime.hotspot_hovered.set(true);
                    } else {
                        runtime.hovered.set(true);
                    }
                    runtime.sync_reveal();
                }
            });
            if is_tail {
                let weak = Rc::downgrade(&runtime);
                motion.connect_motion(move |_, x, y| {
                    if let Some(runtime) = weak.upgrade() {
                        runtime
                            .hotspot
                            .set_cursor_from_name(if runtime.header_hit(x, y) {
                                Some("ns-resize")
                            } else {
                                None
                            });
                    }
                });
            }
            let weak = Rc::downgrade(&runtime);
            motion.connect_leave(move |_| {
                if let Some(runtime) = weak.upgrade() {
                    if is_tail {
                        runtime.hotspot_hovered.set(false);
                    } else {
                        runtime.hovered.set(false);
                    }
                    runtime.sync_reveal();
                }
            });
            widget.add_controller(motion);
        }
        {
            let weak = Rc::downgrade(&runtime);
            controller.subscribe_snapshot(move |snapshot| {
                let Some(runtime) = weak.upgrade() else {
                    return false;
                };
                runtime.layout.update_snapshot(snapshot);
                runtime.apply_size();
                true
            });
        }
        {
            let weak = Rc::downgrade(&runtime);
            controller.subscribe_settings(move |settings| {
                let Some(runtime) = weak.upgrade() else {
                    return false;
                };
                let font_changed = runtime.layout.applied_scale.get() != settings.scale_milli;
                runtime.layout.apply_settings(settings);
                runtime.pinned.set(settings.pinned);
                runtime
                    .hide_delay
                    .set(Duration::from_millis(settings.hide_delay_ms as u64));
                runtime.requested_width.set(settings.width);
                runtime.requested_height.set(settings.height);
                if !runtime.dragging.get() {
                    let geometry =
                        PanelGeometry::for_monitor(&runtime.monitor, runtime.requested_width.get());
                    runtime
                        .requested_y
                        .set(settings.position_y.unwrap_or(geometry.default_y()));
                }
                if font_changed {
                    runtime.measure_after_font_update();
                } else {
                    runtime.apply_size();
                }
                runtime.sync_reveal();
                true
            });
        }
        let geometry_handler = {
            let weak = Rc::downgrade(&runtime);
            monitor.connect_geometry_notify(move |_| {
                if let Some(runtime) = weak.upgrade() {
                    runtime.apply_size();
                }
            })
        };
        {
            let weak = Rc::downgrade(&runtime);
            settings_reveal.connect_child_revealed_notify(move |_| {
                if let Some(runtime) = weak.upgrade() {
                    runtime.apply_size();
                }
            });
        }
        {
            let weak = Rc::downgrade(&runtime);
            trigger.connect_clicked(move |_| {
                if let Some(runtime) = weak.upgrade() {
                    runtime.set_settings_open(!runtime.settings_reveal.reveals_child());
                }
            });
        }
        {
            let weak = Rc::downgrade(&runtime);
            let key = gtk::EventControllerKey::new();
            key.connect_key_pressed(move |_, key, _, _| {
                if key == gdk::Key::Escape
                    && let Some(runtime) = weak.upgrade()
                    && runtime.settings_reveal.reveals_child()
                {
                    runtime.set_settings_open(false);
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
            content_window.add_controller(key);
        }

        runtime.apply_size();
        controller.set_view_visible(content_window.is_visible());
        hotspot_window.present();
        Self {
            runtime,
            geometry_handler: Some(geometry_handler),
        }
    }

    pub fn monitor(&self) -> &gdk::Monitor {
        &self.runtime.monitor
    }
}

impl Drop for SystemMonitorView {
    fn drop(&mut self) {
        self.runtime
            .settings_panel
            .controller
            .set_view_visible(false);
        if let Some(handler) = self.geometry_handler.take() {
            self.runtime.monitor.disconnect(handler);
        }
        for window in [&self.runtime.window, &self.runtime.hotspot_window] {
            detach_application_window(window);
        }
    }
}

#[derive(Clone, Copy)]
struct CpuTimes {
    total: u64,
    idle: u64,
}

#[derive(Clone)]
struct NetworkCounters {
    read_at: Instant,
    received: u64,
    transmitted: u64,
    interfaces: Vec<String>,
}

struct SystemSampler {
    previous_cpu: Option<CpuTimes>,
    previous_network: Option<NetworkCounters>,
    gpu_sampling_enabled: Arc<AtomicBool>,
    cached: SystemSnapshot,
    gpus: gpu::GpuSampler,
}

impl Default for SystemSampler {
    fn default() -> Self {
        Self {
            previous_cpu: None,
            previous_network: None,
            gpu_sampling_enabled: Arc::new(AtomicBool::new(true)),
            cached: SystemSnapshot::default(),
            gpus: gpu::GpuSampler::default(),
        }
    }
}

impl SystemSampler {
    #[cfg(test)]
    fn sample(&mut self) -> SystemSnapshot {
        self.sample_with(&MonitorSettings::default(), true)
    }

    fn sample_with(&mut self, settings: &MonitorSettings, visible: bool) -> SystemSnapshot {
        let first = !self.cached.ready;
        let wants = |section| {
            let options = metric_options(section, &self.cached);
            first
                || (options.is_empty()
                    && settings
                        .sections
                        .iter()
                        .any(|item| item.section == section && item.visible))
                || options
                    .iter()
                    .any(|metric| settings.wants(section, &metric.id))
        };
        let mut next = self.cached.clone();
        next.ready = true;
        next.section_states.clear();
        if wants(MonitorSection::Cpu) {
            let cpu_times = read_cpu_times();
            next.cpu_percent = cpu_times.map(|current| {
                let percent = self
                    .previous_cpu
                    .and_then(|previous| cpu_usage(previous, current))
                    .unwrap_or(0.0);
                self.previous_cpu = Some(current);
                percent
            });
            next.cpu_frequency_mhz = read_cpu_frequency_mhz();
            (
                next.load_average,
                next.running_processes,
                next.total_processes,
            ) = read_load_snapshot();
            next.uptime_seconds = read_uptime_seconds();
        } else {
            next.section_states
                .insert(MonitorSection::Cpu, gpu::DataState::Paused);
        }
        if wants(MonitorSection::Memory) {
            let memory = read_memory_usage();
            next.memory_used = memory.map(|memory| memory.used);
            next.memory_total = memory.map(|memory| memory.total);
            next.swap_used = memory
                .filter(|memory| memory.swap_total > 0)
                .map(|memory| memory.swap_used);
            next.swap_total = memory
                .filter(|memory| memory.swap_total > 0)
                .map(|memory| memory.swap_total);
        } else {
            next.section_states
                .insert(MonitorSection::Memory, gpu::DataState::Paused);
        }
        if wants(MonitorSection::Storage) {
            next.disks = read_disk_snapshots();
        } else {
            next.section_states
                .insert(MonitorSection::Storage, gpu::DataState::Paused);
        }
        if wants(MonitorSection::Battery) {
            next.battery = read_battery_snapshot();
        } else {
            next.section_states
                .insert(MonitorSection::Battery, gpu::DataState::Paused);
        }
        next.network_choices = network_interface_names();
        if wants(MonitorSection::Network) {
            if let Some((current, interfaces)) =
                read_network_counters(settings.network_interface.as_deref())
            {
                let rates = self
                    .previous_network
                    .as_ref()
                    .and_then(|previous| network_rates(previous, &current))
                    .unwrap_or_default();
                next.network_available = true;
                next.network_interfaces = interfaces;
                next.download_bytes_per_second = rates.0;
                next.upload_bytes_per_second = rates.1;
                next.network_received_bytes = current.received;
                next.network_transmitted_bytes = current.transmitted;
                self.previous_network = Some(current);
            } else {
                self.previous_network = None;
                next.network_available = false;
                next.section_states
                    .insert(MonitorSection::Network, gpu::DataState::Unavailable);
            }
        } else {
            next.section_states
                .insert(MonitorSection::Network, gpu::DataState::Paused);
        }
        let hwmon = read_hwmon_snapshot(settings, first);
        next.temperatures = hwmon.temperatures;
        next.power = hwmon.power;
        next.fans = hwmon.fans;
        next.gpus = self
            .gpus
            .sample(settings, visible, &self.gpu_sampling_enabled);
        self.cached = next.clone();
        next
    }
}

fn read_cpu_times() -> Option<CpuTimes> {
    let content = fs::read_to_string("/proc/stat").ok()?;
    parse_cpu_times(content.lines().find(|line| line.starts_with("cpu "))?)
}

fn parse_cpu_times(line: &str) -> Option<CpuTimes> {
    let mut fields = line.split_whitespace();
    if fields.next()? != "cpu" {
        return None;
    }
    let (mut total, mut idle, mut count) = (0_u64, 0_u64, 0);
    // Guest and guest_nice are already included in user and nice.
    for (index, field) in fields.take(8).enumerate() {
        let value = field.parse::<u64>().ok()?;
        total = total.checked_add(value)?;
        if matches!(index, 3 | 4) {
            idle = idle.checked_add(value)?;
        }
        count += 1;
    }
    (count >= 4).then_some(CpuTimes { total, idle })
}

fn cpu_usage(previous: CpuTimes, current: CpuTimes) -> Option<f64> {
    let total = current.total.checked_sub(previous.total)?;
    if total == 0 {
        return Some(0.0);
    }
    let idle = current.idle.saturating_sub(previous.idle).min(total);
    Some((total - idle) as f64 * 100.0 / total as f64)
}

fn read_cpu_frequency_mhz() -> Option<f64> {
    let mut frequencies = sorted_directory_paths(Path::new("/sys/devices/system/cpu/cpufreq"))
        .into_iter()
        .filter_map(|policy| read_number(policy.join("scaling_cur_freq")))
        .map(|kilohertz| kilohertz / 1000.0)
        .filter(|megahertz| (10.0..=20_000.0).contains(megahertz))
        .collect::<Vec<_>>();

    if frequencies.is_empty()
        && let Ok(content) = fs::read_to_string("/proc/cpuinfo")
    {
        frequencies.extend(content.lines().filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key.trim() == "cpu MHz")
                .then(|| value.trim().parse::<f64>().ok())
                .flatten()
                .filter(|megahertz| (10.0..=20_000.0).contains(megahertz))
        }));
    }

    (!frequencies.is_empty()).then(|| frequencies.iter().sum::<f64>() / frequencies.len() as f64)
}

fn read_load_snapshot() -> (Option<f64>, Option<u64>, Option<u64>) {
    let Ok(content) = fs::read_to_string("/proc/loadavg") else {
        return (None, None, None);
    };
    let fields = content.split_whitespace().collect::<Vec<_>>();
    let load = fields.first().and_then(|value| value.parse().ok());
    let processes = fields.get(3).and_then(|value| value.split_once('/'));
    let running = processes.and_then(|(running, _)| running.parse().ok());
    let total = processes.and_then(|(_, total)| total.parse().ok());
    (load, running, total)
}

fn read_uptime_seconds() -> Option<u64> {
    fs::read_to_string("/proc/uptime")
        .ok()?
        .split_whitespace()
        .next()?
        .parse::<f64>()
        .ok()
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        .map(|seconds| seconds as u64)
}

#[derive(Clone, Copy)]
struct MemoryUsage {
    used: u64,
    total: u64,
    swap_used: u64,
    swap_total: u64,
}

fn read_memory_usage() -> Option<MemoryUsage> {
    let content = fs::read_to_string("/proc/meminfo").ok()?;
    let mut total_kib = None;
    let mut available_kib = None;
    let mut swap_total_kib = None;
    let mut swap_free_kib = None;
    for line in content.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let Some(value) = value
            .split_whitespace()
            .next()
            .and_then(|value| value.parse::<u64>().ok())
        else {
            continue;
        };
        match key {
            "MemTotal" => total_kib = Some(value),
            "MemAvailable" => available_kib = Some(value),
            "SwapTotal" => swap_total_kib = Some(value),
            "SwapFree" => swap_free_kib = Some(value),
            _ => {}
        }
        if total_kib.is_some()
            && available_kib.is_some()
            && swap_total_kib.is_some()
            && swap_free_kib.is_some()
        {
            break;
        }
    }
    let total = total_kib?.saturating_mul(1024);
    let available = available_kib?.saturating_mul(1024).min(total);
    let swap_total = swap_total_kib.unwrap_or_default().saturating_mul(1024);
    let swap_free = swap_free_kib
        .unwrap_or_default()
        .saturating_mul(1024)
        .min(swap_total);
    Some(MemoryUsage {
        used: total.saturating_sub(available),
        total,
        swap_used: swap_total.saturating_sub(swap_free),
        swap_total,
    })
}

fn decode_mount_field(value: &str) -> String {
    value
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

fn disk_mounts(mountinfo: &str) -> Vec<(String, PathBuf)> {
    let mut mounts = mountinfo
        .lines()
        .filter_map(|line| {
            let (mount, filesystem) = line.split_once(" - ")?;
            let fields = mount.split_whitespace().collect::<Vec<_>>();
            let mut filesystem = filesystem.split_whitespace();
            let kind = filesystem.next()?;
            let source = decode_mount_field(filesystem.next()?);
            // Include mounted block filesystems, not tmpfs, network mounts or
            // loop-backed application images. statvfs stays local to the machine.
            if !source.starts_with("/dev/")
                || ["/dev/loop", "/dev/ram", "/dev/zram"]
                    .iter()
                    .any(|prefix| source.starts_with(prefix))
            {
                return None;
            }
            let device = *fields.get(2)?;
            let mount_point = PathBuf::from(decode_mount_field(fields.get(4)?));
            // Btrfs subvolumes may have distinct device numbers but share space on
            // the same source. Ordinary bind mounts share the device number.
            let id = if kind == "btrfs" {
                source
            } else {
                device.to_owned()
            };
            Some((id, mount_point))
        })
        .collect::<Vec<_>>();
    mounts.sort_by(|a, b| {
        a.1.components()
            .count()
            .cmp(&b.1.components().count())
            .then_with(|| a.1.cmp(&b.1))
    });
    let mut seen = HashSet::new();
    mounts.retain(|(id, _)| seen.insert(id.clone()));
    mounts
}

fn read_disk_snapshots() -> Vec<DiskSnapshot> {
    let Ok(mountinfo) = fs::read_to_string("/proc/self/mountinfo") else {
        return Vec::new();
    };
    disk_mounts(&mountinfo)
        .into_iter()
        .filter_map(|(device, mount_point)| {
            let (used, total, available) = read_disk_usage(&mount_point)?;
            (total > 0).then_some(DiskSnapshot {
                // Device numbers can change after reboot; the mount identifies
                // the volume whose metrics the user configured.
                id: format!("mount:{}", mount_point.display()),
                name: disk_display_name(&device, &mount_point),
                used,
                total,
                available,
            })
        })
        .collect()
}

fn disk_display_name(device: &str, mount: &Path) -> String {
    use std::os::unix::fs::MetadataExt;
    let device = if device.starts_with("/dev/") {
        fs::metadata(device)
            .ok()
            .map(|metadata| {
                format!(
                    "{}:{}",
                    libc::major(metadata.rdev()),
                    libc::minor(metadata.rdev())
                )
            })
            .unwrap_or_default()
    } else {
        device.to_owned()
    };
    let properties = fs::read_to_string(format!("/run/udev/data/b{device}")).unwrap_or_default();
    let property = |name: &str| properties.lines().find_map(|line| line.strip_prefix(name));
    let sysfs = PathBuf::from(format!("/sys/dev/block/{device}"));
    let model = property("E:ID_MODEL=")
        .map(str::to_owned)
        .or_else(|| read_trimmed(sysfs.join("device/model")))
        .or_else(|| read_trimmed(sysfs.join("../device/model")))
        .and_then(|model| compact_storage_model(&model));
    storage_display_name(model.as_deref(), property("E:ID_FS_LABEL="), mount)
}

fn storage_display_name(model: Option<&str>, label: Option<&str>, mount: &Path) -> String {
    let partition = label
        .filter(|label| !label.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| match mount.to_str() {
            Some("/") => "System".into(),
            Some("/boot" | "/boot/efi" | "/efi") => "Boot".into(),
            _ => mount
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Volume".into()),
        });
    model.map_or(partition.clone(), |model| format!("{model} · {partition}"))
}

fn read_disk_usage(path: &Path) -> Option<(u64, u64, u64)> {
    let path = CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return None;
    }
    let stats = unsafe { stats.assume_init() };
    let block_size = if stats.f_frsize > 0 {
        stats.f_frsize
    } else {
        stats.f_bsize
    } as u128;
    let total = u128::from(stats.f_blocks).saturating_mul(block_size);
    let available = u128::from(stats.f_bavail).saturating_mul(block_size);
    let free = u128::from(stats.f_bfree).saturating_mul(block_size);
    let used = total.saturating_sub(free);
    Some((
        used.min(u128::from(u64::MAX)) as u64,
        total.min(u128::from(u64::MAX)) as u64,
        available.min(total).min(u128::from(u64::MAX)) as u64,
    ))
}

fn network_interface_names() -> Vec<String> {
    fs::read_to_string("/proc/net/dev")
        .unwrap_or_default()
        .lines()
        .skip(2)
        .filter_map(|line| line.split_once(':').map(|(name, _)| name.trim().to_owned()))
        .filter(|name| name != "lo")
        .collect()
}

fn read_network_counters(
    selected_interface: Option<&str>,
) -> Option<(NetworkCounters, Vec<String>)> {
    let content = fs::read_to_string("/proc/net/dev").ok()?;
    let default_interfaces = read_default_interfaces();
    parse_network_counters(
        &content,
        selected_interface,
        &default_interfaces,
        network_interface_is_active,
    )
}

fn parse_network_counters(
    content: &str,
    selected_interface: Option<&str>,
    default_interfaces: &HashSet<String>,
    is_active: impl Fn(&str) -> bool,
) -> Option<(NetworkCounters, Vec<String>)> {
    let mut candidates = Vec::new();
    for line in content.lines().skip(2) {
        let Some((name, values)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name == "lo" {
            continue;
        }
        let fields = values.split_whitespace().collect::<Vec<_>>();
        if fields.len() < 16 {
            continue;
        }
        let (Ok(received), Ok(transmitted)) = (fields[0].parse::<u64>(), fields[8].parse::<u64>())
        else {
            continue;
        };
        candidates.push((name.to_owned(), received, transmitted));
    }

    let selected = candidates
        .iter()
        .filter(|(name, _, _)| default_interfaces.contains(name))
        .collect::<Vec<_>>();
    let selected = if let Some(name) = selected_interface {
        candidates
            .iter()
            .filter(|(candidate, _, _)| candidate == name && is_active(name))
            .collect::<Vec<_>>()
    } else if selected.is_empty() {
        candidates
            .iter()
            .filter(|(name, _, _)| is_active(name))
            .collect::<Vec<_>>()
    } else {
        selected
    };
    if selected.is_empty() {
        return None;
    }

    let interfaces = selected.iter().map(|(name, _, _)| name.clone()).collect();
    Some((
        NetworkCounters {
            read_at: Instant::now(),
            received: selected
                .iter()
                .fold(0_u64, |total, (_, value, _)| total.saturating_add(*value)),
            transmitted: selected
                .iter()
                .fold(0_u64, |total, (_, _, value)| total.saturating_add(*value)),
            interfaces: selected.iter().map(|(name, _, _)| name.clone()).collect(),
        },
        interfaces,
    ))
}

fn read_default_interfaces() -> HashSet<String> {
    parse_default_interfaces(
        &fs::read_to_string("/proc/net/route").unwrap_or_default(),
        &fs::read_to_string("/proc/net/ipv6_route").unwrap_or_default(),
    )
}

fn parse_default_interfaces(ipv4: &str, ipv6: &str) -> HashSet<String> {
    let mut interfaces = HashSet::new();
    for line in ipv4.lines().skip(1) {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() >= 4 && fields[1] == "00000000" {
            interfaces.insert(fields[0].to_owned());
        }
    }
    for line in ipv6.lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() >= 10
            && fields[0] == "00000000000000000000000000000000"
            && fields[1] == "00"
            && fields[9] != "lo"
        {
            interfaces.insert(fields[9].to_owned());
        }
    }
    interfaces
}

fn network_interface_is_active(name: &str) -> bool {
    let path = Path::new("/sys/class/net").join(name);
    let state = read_trimmed(path.join("operstate"));
    let carrier = read_trimmed(path.join("carrier"));
    matches!(state.as_deref(), Some("up") | Some("unknown")) || carrier.as_deref() == Some("1")
}

fn network_rates(previous: &NetworkCounters, current: &NetworkCounters) -> Option<(f64, f64)> {
    if previous.interfaces != current.interfaces {
        return None;
    }
    let seconds = current
        .read_at
        .duration_since(previous.read_at)
        .as_secs_f64();
    if seconds <= f64::EPSILON {
        return None;
    }
    Some((
        current.received.saturating_sub(previous.received) as f64 / seconds,
        current.transmitted.saturating_sub(previous.transmitted) as f64 / seconds,
    ))
}

struct HwmonSnapshot {
    temperatures: Vec<SensorReading>,
    power: Vec<SensorReading>,
    fans: Vec<SensorReading>,
}

fn hwmon_device_key(directory: &Path, chip: &str) -> String {
    if let Some(serial) = read_trimmed(directory.join("device/serial")) {
        return format!("{chip}:{serial}");
    }
    let path = fs::canonicalize(directory.join("device"))
        .or_else(|_| fs::canonicalize(directory))
        .unwrap_or_else(|_| directory.to_path_buf());
    // hwmon indices depend on driver initialization order, unlike the device path.
    let device = if path
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == "hwmon")
    {
        path.parent().and_then(Path::parent).unwrap_or(&path)
    } else {
        &path
    };
    format!("{chip}:{}", device.display())
}

fn sensor_value(
    path: &Path,
    settings: &MonitorSettings,
    section: MonitorSection,
    id: &str,
    discover: bool,
    divisor: f64,
    range: std::ops::RangeInclusive<f64>,
) -> (f64, gpu::DataState) {
    if !discover && !settings.wants(section, id) {
        return (0.0, gpu::DataState::Paused);
    }
    match fs::read_to_string(path) {
        Ok(value) => match value
            .trim()
            .parse::<f64>()
            .ok()
            .map(|value| value / divisor)
            .filter(|value| range.contains(value))
        {
            Some(value) => (value, gpu::DataState::Ready),
            None => (0.0, gpu::DataState::Unsupported),
        },
        Err(error) => (
            0.0,
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                gpu::DataState::PermissionDenied
            } else {
                gpu::DataState::Unavailable
            },
        ),
    }
}

fn read_hwmon_snapshot(settings: &MonitorSettings, discover: bool) -> HwmonSnapshot {
    let mut temperatures = Vec::new();
    let mut power = Vec::new();
    let mut fans = Vec::new();

    let devices = sorted_directory_paths(Path::new("/sys/class/hwmon"))
        .into_iter()
        .filter_map(|directory| {
            if device_is_nvidia(&directory) || device_runtime_suspended(&directory) {
                return None;
            }
            let chip = read_trimmed(directory.join("name")).unwrap_or_else(|| "sensor".to_owned());
            let chip_lower = chip.to_ascii_lowercase();
            if chip_lower.contains("nvidia") || chip_lower.contains("nouveau") {
                return None;
            }
            Some((directory, chip))
        })
        .collect::<Vec<_>>();
    let identities = devices
        .iter()
        .map(|(directory, chip)| sensor_device_identity(directory, chip))
        .collect::<Vec<_>>();
    let mut identity_totals = HashMap::<String, usize>::new();
    for identity in &identities {
        *identity_totals.entry(identity.clone()).or_default() += 1;
    }
    let mut identity_occurrences = HashMap::<String, usize>::new();

    for ((directory, chip), identity) in devices.into_iter().zip(identities) {
        let device_id = hwmon_device_key(&directory, &chip);
        let is_gpu = gpu::is_gpu_device(&directory);
        let occurrence = identity_occurrences.entry(identity.clone()).or_default();
        *occurrence += 1;
        let device_label = if identity_totals.get(&identity).copied().unwrap_or(1) > 1 {
            format!("{identity} {occurrence}")
        } else {
            identity
        };
        let files = sorted_directory_paths(&directory);
        let preferred_nvme_temperature = chip
            .eq_ignore_ascii_case("nvme")
            .then(|| preferred_nvme_temperature_index(&directory, &files))
            .flatten();
        let mut power_indices = HashSet::new();

        for path in &files {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if let Some(index) = sensor_index(name, "temp", "_input") {
                if preferred_nvme_temperature
                    .as_ref()
                    .is_some_and(|preferred| preferred != &index)
                {
                    continue;
                }
                if is_gpu {
                    continue;
                }
                let id = format!("{device_id}:temp{index}");
                let (value, state) = sensor_value(
                    path,
                    settings,
                    MonitorSection::Temperatures,
                    &id,
                    discover,
                    1000.0,
                    -40.0..=200.0,
                );
                let label = sensor_display_label(
                    &device_label,
                    &chip,
                    read_trimmed(directory.join(format!("temp{index}_label"))).as_deref(),
                    SensorKind::Temperature,
                    &index,
                );
                temperatures.push(SensorReading {
                    id,
                    label,
                    value,
                    state,
                });
            }
            if let Some(index) = sensor_index(name, "fan", "_input") {
                let id = format!("{device_id}:fan{index}");
                let (value, state) = sensor_value(
                    path,
                    settings,
                    MonitorSection::Cooling,
                    &id,
                    discover,
                    1.0,
                    0.0..=100_000.0,
                );
                let label = sensor_display_label(
                    &device_label,
                    &chip,
                    read_trimmed(directory.join(format!("fan{index}_label"))).as_deref(),
                    SensorKind::Fan,
                    &index,
                );
                fans.push(SensorReading {
                    id,
                    label,
                    value,
                    state,
                });
            }
            if let Some(index) = sensor_index(name, "power", "_average")
                && !is_gpu
            {
                let id = format!("{device_id}:power{index}");
                let (value, state) = sensor_value(
                    path,
                    settings,
                    MonitorSection::Power,
                    &id,
                    discover,
                    1_000_000.0,
                    0.0..=10_000.0,
                );
                power_indices.insert(index.clone());
                power.push(SensorReading {
                    id,
                    label: sensor_display_label(
                        &device_label,
                        &chip,
                        read_trimmed(directory.join(format!("power{index}_label"))).as_deref(),
                        SensorKind::Power,
                        &index,
                    ),
                    value,
                    state,
                });
            }
        }

        for path in &files {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(index) = sensor_index(name, "power", "_input") else {
                continue;
            };
            if power_indices.contains(&index) || is_gpu {
                continue;
            }
            {
                let id = format!("{device_id}:power{index}");
                let (value, state) = sensor_value(
                    path,
                    settings,
                    MonitorSection::Power,
                    &id,
                    discover,
                    1_000_000.0,
                    0.0..=10_000.0,
                );
                power.push(SensorReading {
                    id,
                    label: sensor_display_label(
                        &device_label,
                        &chip,
                        read_trimmed(directory.join(format!("power{index}_label"))).as_deref(),
                        SensorKind::Power,
                        &index,
                    ),
                    value,
                    state,
                });
            }
        }
    }

    append_thermal_zone_temperatures(&mut temperatures, settings, discover);
    temperatures.sort_by_key(|sensor| temperature_priority(&sensor.label));
    power.sort_by_key(|sensor| sensor_priority(&sensor.label));
    fans.sort_by_key(|sensor| sensor_priority(&sensor.label));
    uniquify_sensor_labels(&mut temperatures);
    uniquify_sensor_labels(&mut power);
    uniquify_sensor_labels(&mut fans);

    HwmonSnapshot {
        temperatures,
        power,
        fans,
    }
}

fn append_thermal_zone_temperatures(
    temperatures: &mut Vec<SensorReading>,
    settings: &MonitorSettings,
    discover: bool,
) {
    for directory in sorted_directory_paths(Path::new("/sys/class/thermal")) {
        let Some(name) = directory.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with("thermal_zone")
            || device_is_nvidia(&directory)
            || device_runtime_suspended(&directory)
        {
            continue;
        }
        let id = format!("{}:temp", directory.display());
        let (value, state) = sensor_value(
            &directory.join("temp"),
            settings,
            MonitorSection::Temperatures,
            &id,
            discover,
            1000.0,
            -40.0..=200.0,
        );
        let kind = read_trimmed(directory.join("type")).unwrap_or_else(|| name.to_owned());
        if is_gpu_label(&kind) {
            continue;
        }
        let kind_lower = kind.to_ascii_lowercase();
        if (kind_lower.contains("cpu") || kind_lower.contains("x86_pkg"))
            && temperatures
                .iter()
                .any(|sensor| sensor.label.to_ascii_lowercase().contains("cpu"))
        {
            continue;
        }
        let mut label = thermal_zone_device_label(&kind);
        if !label.to_ascii_lowercase().contains("temperature") {
            label.push_str(" Temperature");
        }
        temperatures.push(SensorReading {
            id,
            label,
            value,
            state,
        });
    }
}

fn thermal_zone_device_label(kind: &str) -> String {
    let normalized = kind.to_ascii_lowercase();
    if normalized.contains("x86_pkg")
        || normalized.contains("cpu_thermal")
        || normalized == "cpu-thermal"
    {
        "CPU Package".to_owned()
    } else if normalized.contains("acpitz") {
        "ACPI Thermal Zone".to_owned()
    } else if normalized.starts_with("pch") || normalized.contains("chipset") {
        "Chipset".to_owned()
    } else if normalized.contains("iwlwifi")
        || normalized.contains("wifi")
        || normalized.contains("wlan")
    {
        "Wi-Fi Adapter".to_owned()
    } else if normalized.contains("soc") {
        "SoC".to_owned()
    } else if is_gpu_label(kind) {
        "GPU".to_owned()
    } else {
        humanize_label(kind)
    }
}

#[derive(Clone, Copy)]
enum SensorKind {
    Temperature,
    Power,
    Fan,
}

fn sensor_index(name: &str, prefix: &str, suffix: &str) -> Option<String> {
    let index = name.strip_prefix(prefix)?.strip_suffix(suffix)?;
    (!index.is_empty() && index.chars().all(|character| character.is_ascii_digit()))
        .then(|| index.to_owned())
}

fn preferred_nvme_temperature_index(directory: &Path, files: &[PathBuf]) -> Option<String> {
    let mut first = None;
    for path in files {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(index) = sensor_index(name, "temp", "_input") else {
            continue;
        };
        first.get_or_insert_with(|| index.clone());
        if read_trimmed(directory.join(format!("temp{index}_label")))
            .is_some_and(|label| label.eq_ignore_ascii_case("composite"))
        {
            return Some(index);
        }
    }
    first
}

fn sensor_display_label(
    device: &str,
    chip: &str,
    label: Option<&str>,
    kind: SensorKind,
    index: &str,
) -> String {
    let raw_label = label.filter(|label| !label.trim().is_empty()).unwrap_or("");
    let label_lower = raw_label.to_ascii_lowercase();
    let base = if is_cpu_chip(chip) {
        cpu_sensor_label(device, raw_label, index)
    } else if raw_label.is_empty() {
        device.to_owned()
    } else if label_lower == "composite" {
        format!("{device} Overall")
    } else if label_lower == device.to_ascii_lowercase() {
        device.to_owned()
    } else {
        format!("{device} {}", humanize_label(raw_label))
    };

    match kind {
        SensorKind::Temperature if !base.to_ascii_lowercase().contains("temperature") => {
            format!("{base} Temperature")
        }
        SensorKind::Power if !base.to_ascii_lowercase().contains("power") => {
            format!("{base} Power")
        }
        SensorKind::Fan if !base.to_ascii_lowercase().contains("fan") => {
            format!("{base} Fan")
        }
        _ => base,
    }
}

fn cpu_sensor_label(device: &str, raw_label: &str, index: &str) -> String {
    let normalized = raw_label.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return format!("{device} Sensor {index}");
    }
    if normalized == "tctl" || normalized.starts_with("package id") {
        return format!("{device} Package");
    }
    if normalized == "tdie" {
        return format!("{device} Die");
    }
    if let Some(ccd) = normalized
        .strip_prefix("tccd")
        .or_else(|| normalized.strip_prefix("ccd"))
        .filter(|ccd| !ccd.is_empty() && ccd.chars().all(|character| character.is_ascii_digit()))
    {
        return format!("{device} CCD {ccd}");
    }
    if normalized.starts_with("core ") {
        return format!("{device} {}", humanize_label(raw_label));
    }
    format!("{device} {}", humanize_label(raw_label))
}

fn sensor_device_identity(directory: &Path, chip: &str) -> String {
    let chip_lower = chip.to_ascii_lowercase();
    if is_cpu_chip(chip) {
        return "CPU".to_owned();
    }
    if chip_lower == "nvme" {
        return read_trimmed(directory.join("device/model"))
            .and_then(|model| compact_storage_model(&model))
            .map(|model| format!("{model} SSD"))
            .unwrap_or_else(|| "NVMe SSD".to_owned());
    }
    if chip_lower.contains("drivetemp") {
        return read_trimmed(directory.join("device/model"))
            .and_then(|model| compact_storage_model(&model))
            .map(|model| format!("{model} Drive"))
            .unwrap_or_else(|| "Storage Drive".to_owned());
    }
    if chip_lower.contains("spd5118") || chip_lower.contains("jc42") {
        return "RAM Module".to_owned();
    }
    if is_gpu_label(chip) {
        return "GPU".to_owned();
    }
    let path = directory.to_string_lossy().to_ascii_lowercase();
    if path.contains("/ieee80211/")
        || chip_lower.starts_with("iwlwifi")
        || chip_lower.starts_with("mt76")
        || chip_lower.starts_with("mt79")
        || chip_lower.starts_with("ath")
    {
        return "Wi-Fi Adapter".to_owned();
    }

    let friendly = friendly_chip_name(chip);
    if friendly != "Sensor" {
        return friendly;
    }
    sysfs_link_name(directory.join("device/driver"))
        .map(|driver| humanize_label(&driver))
        .unwrap_or_else(|| "Hardware".to_owned())
}

fn compact_storage_model(model: &str) -> Option<String> {
    let words = model
        .split_whitespace()
        .filter(|word| {
            let normalized = word
                .trim_matches(|character: char| !character.is_ascii_alphanumeric())
                .to_ascii_lowercase();
            !matches!(
                normalized.as_str(),
                "ssd" | "nvme" | "with" | "heatsink" | "solid" | "state" | "drive"
            ) && !is_storage_capacity(word)
        })
        .map(|word| word.replace('_', " "))
        .collect::<Vec<_>>();
    (!words.is_empty()).then(|| words.join(" "))
}

fn is_storage_capacity(word: &str) -> bool {
    let normalized = word
        .trim_matches(|character: char| !character.is_ascii_alphanumeric() && character != '.')
        .to_ascii_lowercase();
    ["kib", "mib", "gib", "tib", "kb", "mb", "gb", "tb"]
        .into_iter()
        .find_map(|suffix| normalized.strip_suffix(suffix))
        .is_some_and(|number| {
            !number.is_empty()
                && number
                    .chars()
                    .all(|character| character.is_ascii_digit() || character == '.')
        })
}

fn sysfs_link_name(path: impl AsRef<Path>) -> Option<String> {
    fs::read_link(path)
        .ok()?
        .file_name()?
        .to_str()
        .map(str::to_owned)
}

fn is_cpu_chip(chip: &str) -> bool {
    let chip = chip.to_ascii_lowercase();
    chip.contains("coretemp")
        || chip.contains("k10temp")
        || chip.contains("zenpower")
        || chip.contains("cpu_thermal")
        || chip.contains("fam15h_power")
}

fn friendly_chip_name(chip: &str) -> String {
    let lower = chip.to_ascii_lowercase();
    if is_cpu_chip(chip) {
        "CPU".to_owned()
    } else if lower.contains("amdgpu") || lower.contains("nouveau") || lower.contains("nvidia") {
        "GPU".to_owned()
    } else if lower.contains("nvme") {
        "NVMe".to_owned()
    } else {
        humanize_label(chip)
    }
}

fn humanize_label(label: &str) -> String {
    let words = label
        .replace(['_', '-'], " ")
        .split_whitespace()
        .map(|word| {
            let mut characters = word.chars();
            characters.next().map_or_else(String::new, |first| {
                first.to_uppercase().collect::<String>() + characters.as_str()
            })
        })
        .collect::<Vec<_>>();
    if words.is_empty() {
        "Sensor".to_owned()
    } else {
        words.join(" ")
    }
}

fn temperature_priority(label: &str) -> u8 {
    let label = label.to_ascii_lowercase();
    if label.contains("cpu") {
        0
    } else if label.contains("gpu") {
        1
    } else if label.contains("nvme") {
        3
    } else {
        2
    }
}

fn sensor_priority(label: &str) -> u8 {
    let label = label.to_ascii_lowercase();
    if label.contains("gpu") {
        0
    } else if label.contains("cpu") {
        1
    } else {
        2
    }
}

fn uniquify_sensor_labels(readings: &mut [SensorReading]) {
    let mut counts = HashMap::<String, usize>::new();
    for reading in readings {
        let count = counts.entry(reading.label.clone()).or_default();
        *count += 1;
        if *count > 1 {
            reading.label = format!("{} {}", reading.label, count);
        }
    }
}

fn is_gpu_label(label: &str) -> bool {
    let label = label.to_ascii_lowercase();
    label.contains("gpu")
        || label.contains("amdgpu")
        || label.contains("nvidia")
        || label.contains("nouveau")
}

fn device_is_nvidia(path: &Path) -> bool {
    [path.join("vendor"), path.join("device/vendor")]
        .into_iter()
        .filter_map(read_trimmed)
        .any(|vendor| vendor.eq_ignore_ascii_case("0x10de"))
}

fn device_runtime_suspended(path: &Path) -> bool {
    [
        path.join("power/runtime_status"),
        path.join("device/power/runtime_status"),
    ]
    .into_iter()
    .filter_map(read_trimmed)
    .any(|status| matches!(status.as_str(), "suspended" | "suspending"))
}

fn read_battery_snapshot() -> Option<BatterySnapshot> {
    read_battery_snapshot_at(Path::new("/sys/class/power_supply"))
}

fn battery_capacity(supply: &Path) -> Option<(f64, Option<f64>)> {
    let positive =
        |name| read_number(supply.join(name)).filter(|value| value.is_finite() && *value > 0.0);
    let energy_full = positive("energy_full");
    let charge_full = positive("charge_full");
    let percent = read_number(supply.join("capacity"))
        .filter(|value| (0.0..=100.0).contains(value))
        .or_else(|| Some(read_number(supply.join("energy_now"))? / energy_full? * 100.0))
        .or_else(|| Some(read_number(supply.join("charge_now"))? / charge_full? * 100.0))
        .filter(|value| value.is_finite() && *value >= 0.0)?
        .min(100.0);
    // Linux exports energy in µWh and charge in µAh. Convert charge-only
    // batteries before combining capacities from different drivers.
    // https://docs.kernel.org/power/power_supply_class.html
    let energy_full = energy_full
        .or_else(|| {
            let voltage = positive("voltage_min_design").or_else(|| positive("voltage_now"))?;
            Some(charge_full? * voltage / 1_000_000.0)
        })
        .filter(|value| value.is_finite() && *value > 0.0);
    Some((percent, energy_full))
}

fn read_battery_snapshot_at(power_supplies: &Path) -> Option<BatterySnapshot> {
    let mut capacities = Vec::new();
    let mut powers = Vec::new();
    let mut statuses = Vec::new();
    for supply in sorted_directory_paths(power_supplies) {
        if read_trimmed(supply.join("type")).as_deref() != Some("Battery")
            || read_trimmed(supply.join("scope")).as_deref() == Some("Device")
            || read_trimmed(supply.join("present")).as_deref() == Some("0")
        {
            continue;
        }
        if let Some(capacity) = battery_capacity(&supply) {
            capacities.push(capacity);
        }
        let power = read_number(supply.join("power_now"))
            .map(|value| value / 1_000_000.0)
            .or_else(|| {
                let current = read_number(supply.join("current_now"))? / 1_000_000.0;
                let voltage = read_number(supply.join("voltage_now"))? / 1_000_000.0;
                Some(current * voltage)
            });
        if let Some(power) = power.filter(|power| power.is_finite() && *power >= 0.0) {
            powers.push(power);
        }
        if let Some(status) = read_trimmed(supply.join("status")) {
            statuses.push(status);
        }
    }
    if capacities.is_empty() {
        return None;
    }

    let percent = if capacities.iter().all(|(_, energy)| energy.is_some()) {
        let largest = capacities
            .iter()
            .filter_map(|(_, energy)| *energy)
            .fold(0.0_f64, f64::max);
        let (weighted, total) =
            capacities
                .iter()
                .fold((0.0, 0.0), |(weighted, total), (percent, energy)| {
                    let weight = energy.unwrap_or_default() / largest;
                    (weighted + percent * weight, total + weight)
                });
        weighted / total
    } else {
        // Some drivers expose only a percentage, without an energy capacity.
        capacities.iter().map(|(percent, _)| percent).sum::<f64>() / capacities.len() as f64
    };
    let status = if statuses
        .iter()
        .any(|status| status.eq_ignore_ascii_case("charging"))
    {
        "Charging"
    } else if statuses
        .iter()
        .any(|status| status.eq_ignore_ascii_case("discharging"))
    {
        "Discharging"
    } else if statuses
        .iter()
        .any(|status| status.eq_ignore_ascii_case("full"))
    {
        "Full"
    } else {
        "Idle"
    }
    .to_owned();
    Some(BatterySnapshot {
        percent,
        status,
        power_watts: (!powers.is_empty()).then(|| powers.iter().sum()),
    })
}

fn sorted_directory_paths(path: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(path) else {
        return Vec::new();
    };
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

fn read_trimmed(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn read_number(path: impl AsRef<Path>) -> Option<f64> {
    read_trimmed(path)?.parse().ok()
}

fn read_u64(path: impl AsRef<Path>) -> Option<u64> {
    read_trimmed(path)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_total_uses_capacities_and_excludes_peripherals_and_absent_packs() {
        let root = std::env::temp_dir().join(format!("obsidian-batteries-{}", std::process::id()));
        let battery = |name: &str, fields: &[(&str, &str)]| {
            let path = root.join(name);
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("type"), "Battery").unwrap();
            for (name, value) in fields {
                fs::write(path.join(name), value).unwrap();
            }
            path
        };
        battery(
            "BAT0",
            &[
                ("energy_now", "20000000"),
                ("energy_full", "20000000"),
                ("status", "Full"),
                ("power_now", "0"),
            ],
        );
        battery(
            "BAT1",
            &[
                ("energy_now", "0"),
                ("energy_full", "80000000"),
                ("status", "Discharging"),
                ("power_now", "10000000"),
            ],
        );
        battery(
            "mouse",
            &[
                ("scope", "Device"),
                ("capacity", "95"),
                ("status", "Charging"),
                ("power_now", "5000000"),
            ],
        );
        battery(
            "removed",
            &[("present", "0"), ("capacity", "95"), ("status", "Charging")],
        );
        let snapshot = read_battery_snapshot_at(&root).unwrap();
        assert_eq!(snapshot.percent, 20.0);
        assert_eq!(snapshot.status, "Discharging");
        assert_eq!(snapshot.power_watts, Some(10.0));

        let charge = battery(
            "charge",
            &[
                ("charge_now", "1000000"),
                ("charge_full", "2000000"),
                ("voltage_min_design", "10000000"),
            ],
        );
        assert_eq!(battery_capacity(&charge), Some((50.0, Some(20_000_000.0))));
        assert!((read_battery_snapshot_at(&root).unwrap().percent - 25.0).abs() < 0.001);
        fs::write(charge.join("charge_full"), "0").unwrap();
        assert!(battery_capacity(&charge).is_none());
        fs::write(charge.join("capacity"), "70").unwrap();
        assert_eq!(battery_capacity(&charge), Some((70.0, None)));
        fs::remove_dir_all(root).unwrap();
    }

    fn metric_selection_snapshot() -> SystemSnapshot {
        SystemSnapshot {
            ready: true,
            cpu_percent: Some(25.0),
            cpu_frequency_mhz: Some(3500.0),
            uptime_seconds: Some(7200),
            memory_used: Some(4 * 1024 * 1024 * 1024),
            memory_total: Some(16 * 1024 * 1024 * 1024),
            network_available: true,
            network_interfaces: vec!["enp1s0".into()],
            disks: ["/", "/mnt/games"]
                .into_iter()
                .map(|mount| DiskSnapshot {
                    id: format!("mount:{mount}"),
                    name: storage_display_name(Some("Test SSD"), None, Path::new(mount)),
                    used: 256 * 1024 * 1024 * 1024,
                    total: 1024 * 1024 * 1024 * 1024,
                    available: 768 * 1024 * 1024 * 1024,
                })
                .collect(),
            temperatures: vec![
                SensorReading {
                    id: "cpu:temp1".into(),
                    label: "CPU Package Temperature".into(),
                    value: 50.0,
                    state: gpu::DataState::Ready,
                },
                SensorReading {
                    id: "ssd:temp1".into(),
                    label: "Samsung 990 PRO SSD Overall Temperature".into(),
                    value: 35.0,
                    state: gpu::DataState::Ready,
                },
            ],
            fans: vec![SensorReading {
                id: "board:fan1".into(),
                label: "CPU Fan".into(),
                value: 1200.0,
                state: gpu::DataState::Ready,
            }],
            ..SystemSnapshot::default()
        }
    }

    #[test]
    fn individual_metric_choices_survive_save_and_group_toggles() {
        let mut settings = MonitorSettings::default();
        let id = "disk:games,cache;данные\\\"\n:used";
        settings.set_metric_visible(MonitorSection::Storage, id, false);
        settings.set_metric_visible(MonitorSection::Cpu, "cpu-frequency", false);
        settings.sections[0].visible = false;
        let saved = settings.to_key_file().to_data();
        let key_file = glib::KeyFile::new();
        key_file
            .load_from_data(&saved, glib::KeyFileFlags::NONE)
            .unwrap();
        let mut restored = MonitorSettings::from_key_file(&key_file);
        assert_eq!(restored, settings);
        restored.sections[0].visible = true;
        assert!(!restored.metric_visible(MonitorSection::Cpu, "cpu-frequency"));
        assert!(!restored.metric_visible(MonitorSection::Storage, id));
        assert!(restored.metric_visible(MonitorSection::Network, id));
        assert!(restored.metric_visible(MonitorSection::Cpu, "uptime"));
        restored.set_metric_visible(MonitorSection::Cpu, "cpu-frequency", true);
        assert!(restored.metric_visible(MonitorSection::Cpu, "cpu-frequency"));
    }

    #[test]
    fn metrics_sensors_volumes_and_charts_are_selected_independently() {
        let snapshot = metric_selection_snapshot();
        let mut settings = MonitorSettings::default();
        settings.set_metric_visible(MonitorSection::Temperatures, "cpu:temp1", false);
        settings.set_metric_visible(MonitorSection::Storage, "mount:/:used", false);
        let (temperatures, _, _) =
            selected_metrics(MonitorSection::Temperatures, &snapshot, &settings);
        assert_eq!(temperatures.len(), 1);
        assert_eq!(temperatures[0].id, "ssd:temp1");
        let (disks, _, _) = selected_metrics(MonitorSection::Storage, &snapshot, &settings);
        assert_eq!(disks.len(), 7);
        assert!(
            disks
                .iter()
                .any(|metric| metric.id == "mount:/mnt/games:used")
        );
        for section in [MonitorSection::Network, MonitorSection::Memory] {
            for metric in display_metrics(section, &snapshot).0 {
                settings.set_metric_visible(section, &metric.id, false);
            }
        }
        let (rows, _, graph) = selected_metrics(MonitorSection::Network, &snapshot, &settings);
        assert!(rows.is_empty());
        assert!(graph);
        let (rows, meter, _) = selected_metrics(MonitorSection::Memory, &snapshot, &settings);
        assert!(rows.is_empty());
        assert_eq!(meter, Some(0.25));
        settings.set_metric_visible(MonitorSection::Network, METRIC_GRAPH, false);
        settings.set_metric_visible(MonitorSection::Memory, METRIC_METER, false);
        assert!(!selected_metrics(MonitorSection::Network, &snapshot, &settings).2);
        assert!(
            selected_metrics(MonitorSection::Memory, &snapshot, &settings)
                .1
                .is_none()
        );
        assert!(metric_options(MonitorSection::Cpu, &SystemSnapshot::default()).is_empty());
    }

    #[test]
    fn sensor_preferences_do_not_depend_on_hwmon_enumeration() {
        let first = Path::new("/sys/devices/test-board/hwmon/hwmon1");
        let renumbered = Path::new("/sys/devices/test-board/hwmon/hwmon42");
        let other = Path::new("/sys/devices/other-board/hwmon/hwmon1");
        assert_eq!(
            hwmon_device_key(first, "nct6775"),
            hwmon_device_key(renumbered, "nct6775")
        );
        assert_ne!(
            hwmon_device_key(first, "nct6775"),
            hwmon_device_key(other, "nct6775")
        );
    }

    #[test]
    fn storage_bars_belong_to_individual_named_volumes() {
        let mut snapshot = metric_selection_snapshot();
        snapshot.disks[1].used = snapshot.disks[1].total * 3 / 4;
        let mut settings = MonitorSettings::default();
        let (metrics, aggregate, _) =
            selected_metrics(MonitorSection::Storage, &snapshot, &settings);
        assert!(aggregate.is_none());
        assert_eq!(
            metrics
                .iter()
                .filter_map(|metric| metric.fraction)
                .collect::<Vec<_>>(),
            [0.25, 0.75]
        );
        assert_eq!(
            metrics[0].group.as_ref().unwrap().label,
            "Test SSD · System"
        );
        settings.set_metric_visible(MonitorSection::Storage, "mount:/:usage-meter", false);
        let (metrics, _, _) = selected_metrics(MonitorSection::Storage, &snapshot, &settings);
        assert_eq!(
            metrics
                .iter()
                .filter_map(|metric| metric.fraction)
                .collect::<Vec<_>>(),
            [0.75]
        );
        settings.set_metric_visible(MonitorSection::Storage, METRIC_METER, false);
        assert!(
            selected_metrics(MonitorSection::Storage, &snapshot, &settings)
                .0
                .iter()
                .all(|metric| metric.fraction.is_none())
        );
        assert_eq!(
            storage_display_name(Some("Samsung 990 PRO"), None, Path::new("/boot")),
            "Samsung 990 PRO · Boot"
        );
        assert_eq!(
            storage_display_name(
                Some("WD BLACK SN850X"),
                Some("Games"),
                Path::new("/mnt/games")
            ),
            "WD BLACK SN850X · Games"
        );
    }

    fn assert_monitor_text_fits(widget: &gtk::Widget) {
        if !widget.is_visible() {
            return;
        }
        if let Some(label) = widget.downcast_ref::<gtk::Label>() {
            let layout = label.layout();
            let (width, height) = layout.pixel_size();
            assert!(!layout.is_ellipsized(), "truncated text: {}", label.text());
            assert!(
                width <= label.width() + 1,
                "text exceeds width: {} ({width} > {})",
                label.text(),
                label.width()
            );
            assert!(
                height <= label.height() + 1,
                "text exceeds height: {} ({height} > {})",
                label.text(),
                label.height()
            );
        }
        let mut child = widget.first_child();
        while let Some(widget) = child {
            assert_monitor_text_fits(&widget);
            child = widget.next_sibling();
        }
    }

    fn last_monitor_label(widget: &gtk::Widget) -> Option<gtk::Label> {
        if !widget.is_visible() {
            return None;
        }
        let mut child = widget.last_child();
        while let Some(widget) = child {
            if let Some(label) = last_monitor_label(&widget) {
                return Some(label);
            }
            child = widget.prev_sibling();
        }
        widget.downcast_ref::<gtk::Label>().cloned()
    }

    fn assert_monitor_bottom_visible(runtime: &MonitorRuntime) {
        let last = last_monitor_label(runtime.layout.root.upcast_ref()).unwrap();
        let bounds = last.compute_bounds(&runtime.scroller).unwrap();
        assert!(bounds.y() >= 0.0, "last line must be inside the viewport");
        assert!(
            bounds.y() + bounds.height() < runtime.scroller.height() as f32,
            "last line needs room for its bottom padding: {}",
            last.text(),
        );
    }

    fn assert_monitor_height_fits(runtime: &MonitorRuntime) {
        let scroll = runtime.scroller.vadjustment();
        if scroll.upper() > scroll.page_size() + 1.0 {
            assert_eq!(
                runtime.card.height(),
                runtime.geometry().height_limits().1,
                "content may scroll only after reaching the screen height limit"
            );
        } else {
            assert_eq!(scroll.value(), 0.0);
            assert_monitor_bottom_visible(runtime);
        }
    }

    #[test]
    #[ignore = "requires Wayland layer-shell and XDG_STATE_HOME=/tmp/obsidian-monitor-items-test-state"]
    fn monitor_metric_groups_keep_selection_and_scroll_position() {
        assert!(settings_path().starts_with("/tmp/obsidian-monitor-items-test-state"));
        gtk::init().unwrap();
        let application = gtk::Application::builder()
            .application_id("dev.obsidian.MonitorItemsTest")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        application.register(None::<&gio::Cancellable>).unwrap();
        let display = gdk::Display::default().unwrap();
        let css = gtk::CssProvider::new();
        css.load_from_data(include_str!("../../assets/window.css"));
        gtk::style_context_add_provider_for_display(
            &display,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        let monitor = display
            .monitors()
            .item(0)
            .unwrap()
            .downcast::<gdk::Monitor>()
            .unwrap();
        let controller = SystemMonitorController::new();
        assert!(
            controller.update_settings(|settings| *settings = MonitorSettings {
                width: 364,
                height: Some(650),
                pinned: true,
                ..MonitorSettings::default()
            })
        );
        controller.latest.replace(metric_selection_snapshot());
        let view = SystemMonitorView::new(&application, &monitor, &controller);
        let runtime = &view.runtime;
        runtime.window.set_layer(Layer::Overlay);
        let pump = |ms: u64| {
            let until = Instant::now() + Duration::from_millis(ms);
            while Instant::now() < until {
                glib::MainContext::default().iteration(false);
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        pump(350);
        runtime.set_settings_open(true);
        runtime.settings_hovered.set(true);
        pump(300);
        runtime
            .settings_panel
            .height
            .set_value(f64::from(PANEL_MIN_HEIGHT));
        pump(50);
        assert_eq!(
            runtime.settings_panel.height.value(),
            runtime.settings_panel.height.adjustment().lower()
        );
        assert!(runtime.card.height() > PANEL_MIN_HEIGHT);
        assert_monitor_height_fits(runtime);
        controller.set_dimensions(364, 650, SCALE_MILLI_DEFAULT);
        pump(50);
        let height = runtime.window.height();
        let panel = &runtime.settings_panel;
        let width = runtime.window.width();
        panel.interval.set_value(2.5);
        panel.economy.set_active(false);
        pump(50);
        assert_eq!(controller.settings().interval_ms, 2500);
        assert!(!controller.settings().economy);
        controller.set_view_visible(false);
        assert!(controller.gpu_sampling_enabled.load(Ordering::Acquire));
        panel.economy.set_active(true);
        assert!(!controller.gpu_sampling_enabled.load(Ordering::Acquire));
        controller.set_view_visible(true);
        assert!(controller.gpu_sampling_enabled.load(Ordering::Acquire));
        panel.output.set_selected(1);
        assert_eq!(
            controller.preferred_output(),
            Some(monitor_output_id(&monitor))
        );
        let expand = panel.groups.borrow()[&MonitorSection::Cpu].expand.clone();
        expand.set_active(true);
        pump(200);
        let name = panel.groups.borrow()[&MonitorSection::Cpu].renamers[0].clone();
        name.start_editing();
        name.set_text("Мой процессор — длинное имя показателя для проверки размеров панели");
        name.stop_editing(true);
        let resize_deadline = Instant::now() + Duration::from_secs(2);
        while runtime.window.width() <= width && Instant::now() < resize_deadline {
            pump(5);
        }
        assert_eq!(
            controller
                .settings()
                .metric_name(MonitorSection::Cpu, "cpu", "CPU"),
            name.text()
        );
        assert!(
            runtime.window.width() > width,
            "long names raise the minimum width: old={width}, new={}, card={}, requested={}",
            runtime.window.width(),
            runtime.card.width(),
            runtime.panel_width.get()
        );
        assert_eq!(runtime.window.height(), height);
        assert_monitor_text_fits(runtime.card.upcast_ref());
        let minimum = panel.width.adjustment().lower();
        panel.width.set_value(180.0);
        pump(50);
        assert_eq!(panel.width.value(), minimum);
        assert_eq!(runtime.card.width(), minimum as i32);
        controller.set_dimensions(364, 650, SCALE_MILLI_MAX);
        pump(100);
        assert_eq!(runtime.card.width(), PANEL_MAX_WIDTH);
        assert!(
            runtime.window.height() > height,
            "larger fonts also raise the minimum height"
        );
        assert_monitor_height_fits(runtime);
        assert_monitor_text_fits(runtime.card.upcast_ref());
        assert!(
            runtime.layout.sections[&MonitorSection::Cpu]
                .rows
                .first_child()
                .unwrap()
                .height()
                > 24
        );
        controller.set_dimensions(364, 650, SCALE_MILLI_DEFAULT);
        pump(100);
        let mut snapshot = metric_selection_snapshot();
        snapshot.cpu_percent = Some(100.0);
        runtime.layout.update_snapshot(&snapshot);
        runtime.apply_size();
        pump(30);
        let reserved_width = runtime.card.width();
        snapshot.cpu_percent = Some(0.0);
        runtime.layout.update_snapshot(&snapshot);
        runtime.apply_size();
        pump(30);
        assert_eq!(
            runtime.card.width(),
            reserved_width,
            "sampling must not shrink the panel"
        );
        assert_monitor_text_fits(runtime.card.upcast_ref());
        let toggle = |section: MonitorSection, id: &str| {
            let groups = panel.groups.borrow();
            let group = &groups[&section];
            let index = group
                .options
                .as_ref()
                .unwrap()
                .iter()
                .position(|option| option.id == id)
                .unwrap();
            group.toggles[index].clone()
        };
        let frequency = toggle(MonitorSection::Cpu, "cpu-frequency");
        let storage_bar_count = || {
            runtime.layout.sections[&MonitorSection::Storage]
                .metric_views
                .borrow()
                .iter()
                .filter(|view| matches!(view, MetricView::Meter(_)))
                .count()
        };
        assert_eq!(storage_bar_count(), 2);
        toggle(MonitorSection::Storage, "mount:/:usage-meter").set_active(false);
        assert_eq!(storage_bar_count(), 1);
        toggle(MonitorSection::Storage, "mount:/:usage-meter").set_active(true);
        let scroll = runtime.settings_scroller.vadjustment();
        scroll.set_value(100.0);
        pump(30);
        let position = scroll.value();
        frequency.set_active(false);
        pump(80);
        assert!(expand.is_active());
        assert_eq!(panel.groups.borrow()[&MonitorSection::Cpu].expand, expand);
        assert_eq!(toggle(MonitorSection::Cpu, "cpu-frequency"), frequency);
        assert_eq!(scroll.value(), position);
        assert_eq!(runtime.window.height(), height);
        assert!(!MonitorSettings::load().metric_visible(MonitorSection::Cpu, "cpu-frequency"));
        assert_eq!(
            runtime.layout.sections[&MonitorSection::Cpu]
                .signature
                .borrow()
                .len(),
            2
        );
        for id in ["cpu", "uptime"] {
            toggle(MonitorSection::Cpu, id).set_active(false);
        }
        pump(30);
        assert!(
            !runtime.layout.sections[&MonitorSection::Cpu]
                .root
                .is_visible()
        );
        assert_eq!(
            runtime.window.width(),
            width,
            "hidden labels do not reserve width"
        );
        toggle(MonitorSection::Cpu, "uptime").set_active(true);
        let group_visible = panel.groups.borrow()[&MonitorSection::Cpu].visible.clone();
        group_visible.set_active(false);
        pump(30);
        group_visible.set_active(true);
        pump(30);
        assert!(
            runtime.layout.sections[&MonitorSection::Cpu]
                .root
                .is_visible()
        );
        assert_eq!(
            runtime.layout.sections[&MonitorSection::Cpu]
                .signature
                .borrow()[0]
                .0,
            "uptime"
        );
        assert!(!frequency.is_active());
        panel.groups.borrow()[&MonitorSection::Temperatures]
            .expand
            .set_active(true);
        toggle(MonitorSection::Temperatures, "cpu:temp1").set_active(false);
        let mut snapshot = metric_selection_snapshot();
        snapshot.temperatures.remove(0);
        let publish = |snapshot: &SystemSnapshot| {
            controller.latest.replace(snapshot.clone());
            controller
                .snapshot_subscribers
                .borrow_mut()
                .retain(|subscriber| subscriber(snapshot));
            pump(60);
        };
        publish(&snapshot);
        publish(&metric_selection_snapshot());
        assert!(
            controller
                .update_settings(|settings| settings.network_interface = Some("test0".into()))
        );
        pump(50);
        assert!(
            panel
                .network_choices
                .borrow()
                .iter()
                .any(|(id, label)| id == "test0" && label.contains("unavailable"))
        );
        let mut connected = metric_selection_snapshot();
        connected.network_choices = vec!["test0".into()];
        publish(&connected);
        assert!(
            panel
                .network_choices
                .borrow()
                .iter()
                .any(|(id, label)| id == "test0" && label == "test0")
        );
        assert!(!toggle(MonitorSection::Temperatures, "cpu:temp1").is_active());
        assert!(
            panel.groups.borrow()[&MonitorSection::Temperatures]
                .expand
                .is_active()
        );
        assert_eq!(
            runtime.layout.sections[&MonitorSection::Temperatures]
                .signature
                .borrow()
                .len(),
            1
        );
        scroll.set_value(200.0);
        pump(200);
        if let Ok(path) = std::env::var("OBSIDIAN_MONITOR_TEST_SNAPSHOT") {
            let snapshot = gtk::Snapshot::new();
            let paintable = gtk::WidgetPaintable::new(Some(&runtime.surface));
            paintable.snapshot(
                &snapshot,
                f64::from(runtime.surface.width()),
                f64::from(runtime.surface.height()),
            );
            runtime
                .window
                .renderer()
                .unwrap()
                .render_texture(snapshot.to_node().unwrap(), None)
                .save_to_png(path)
                .unwrap();
        }
        let toggles = panel
            .groups
            .borrow()
            .values()
            .flat_map(|group| group.toggles.clone())
            .collect::<Vec<_>>();
        for toggle in toggles {
            toggle.set_active(false);
        }
        pump(50);
        assert!(runtime.layout.empty_state.is_visible());
        assert_eq!(runtime.layout.empty_title.text(), "No metrics selected");
        runtime.set_settings_open(false);
        pump(300);
        runtime.set_settings_open(true);
        pump(300);
        assert!(expand.is_active());
        assert!(!frequency.is_active());
        assert_eq!(MonitorSettings::load(), controller.settings());
        // Settings have their own timeout even when the monitor is pinned.
        runtime.settings_hovered.set(false);
        runtime.sync_reveal();
        pump(400);
        assert!(runtime.settings_reveal.reveals_child());
        runtime.settings_hovered.set(true);
        runtime.sync_reveal();
        pump(1100);
        assert!(
            runtime.settings_reveal.reveals_child(),
            "returning cancels settings timeout"
        );
        let popup = settings_popover(&panel.output).unwrap();
        popup.set_autohide(false);
        popup.popup();
        runtime.settings_hovered.set(false);
        runtime.sync_reveal();
        pump(1100);
        assert!(
            runtime.settings_reveal.reveals_child(),
            "an open menu keeps settings available"
        );
        popup.popdown();
        // The one-second delay starts when the menu finishes hiding, not when
        // popdown starts its closing animation. Allow for compositor latency.
        let menu_deadline = Instant::now() + Duration::from_secs(1);
        while popup.is_visible() && Instant::now() < menu_deadline {
            pump(5);
        }
        assert!(!popup.is_visible(), "the output menu must finish closing");
        pump(400);
        assert!(runtime.settings_reveal.reveals_child());
        pump(750);
        assert!(!runtime.settings_reveal.reveals_child());
        pump(250);
        assert!(
            runtime.window.is_visible(),
            "pin keeps monitoring visible after settings close"
        );
        assert!(controller.set_enabled(false));
        controller.start();
        assert!(!controller.gpu_sampling_enabled.load(Ordering::Acquire));
        assert!(controller.timer.borrow().is_none());
        drop(view);
        pump(20);
    }

    #[test]
    fn network_selection_does_not_fall_back_when_selected_device_disappears() {
        let content = "header\nheader\n eth0: 100 0 0 0 0 0 0 0 200 0 0 0 0 0 0 0\n wlan0: 300 0 0 0 0 0 0 0 400 0 0 0 0 0 0 0\n";
        let defaults = HashSet::from(["eth0".to_owned()]);
        let (automatic, _) = parse_network_counters(content, None, &defaults, |_| true).unwrap();
        let (manual, _) =
            parse_network_counters(content, Some("wlan0"), &defaults, |_| true).unwrap();
        assert_eq!(automatic.received, 100);
        assert_eq!(manual.received, 300);
        assert_eq!(manual.transmitted, 400);
        assert!(network_rates(&automatic, &manual).is_none());
        assert!(parse_network_counters(content, Some("missing0"), &defaults, |_| true).is_none());
        assert!(parse_network_counters(content, Some("wlan0"), &defaults, |_| false).is_none());
        let ipv6 = "00000000000000000000000000000000 00 00000000000000000000000000000000 00 fe800000000000000000000000000001 00000400 00000001 00000000 00000003 wlan0";
        assert_eq!(
            parse_default_interfaces("", ipv6),
            HashSet::from(["wlan0".to_owned()])
        );
    }

    #[test]
    fn economy_interval_and_hidden_sensor_queries_are_independent() {
        let mut settings = MonitorSettings {
            interval_ms: 500,
            ..MonitorSettings::default()
        };
        assert_eq!(
            settings.effective_interval(true),
            Duration::from_millis(500)
        );
        assert_eq!(settings.effective_interval(false), Duration::from_secs(5));
        settings.economy = false;
        assert_eq!(
            settings.effective_interval(false),
            Duration::from_millis(500)
        );
        settings.interval_ms = 10_000;
        settings.economy = true;
        assert_eq!(settings.effective_interval(false), Duration::from_secs(10));
        settings.set_metric_visible(MonitorSection::Temperatures, "test", false);
        // A hidden metric must skip the filesystem read, even if it would fail.
        assert_eq!(
            sensor_value(
                Path::new("/no-such-monitor-sensor"),
                &settings,
                MonitorSection::Temperatures,
                "test",
                false,
                1000.0,
                -50.0..=200.0
            )
            .1,
            gpu::DataState::Paused
        );
    }

    #[test]
    fn monitor_dimensions_migrate_and_save_independently() {
        let old = glib::KeyFile::new();
        old.set_integer(SETTINGS_GROUP, "scale", 1090);
        let mut settings = MonitorSettings::from_key_file(&old);
        assert_eq!(settings.width, 305);
        assert_eq!(settings.scale_milli, 1090);
        assert_eq!(settings.height, None);
        settings.width = 420;
        settings.height = Some(600);
        settings.scale_milli = 1500;
        settings.hide_delay_ms = 8500;
        settings.interval_ms = 2500;
        settings.economy = false;
        settings.output = Some("DP-2".into());
        settings.network_interface = Some("wlan0".into());
        settings
            .metric_names
            .insert("cpu:cpu".into(), "Процессор · загрузка".into());
        let restored = MonitorSettings::from_key_file(&settings.to_key_file());
        assert_eq!(restored, settings);
        assert_eq!(
            MonitorSettings::from_key_file(&glib::KeyFile::new()),
            MonitorSettings::default()
        );
    }

    #[test]
    fn grouped_sections_migrate_to_independent_switches() {
        let key_file = glib::KeyFile::new();
        key_file.set_string(SETTINGS_GROUP, "order", "performance,hardware,memory");
        key_file.set_string(SETTINGS_GROUP, "hidden", "hardware");
        let settings = MonitorSettings::from_key_file(&key_file);
        assert_eq!(
            settings.sections[..4]
                .iter()
                .map(|item| item.section)
                .collect::<Vec<_>>(),
            [
                MonitorSection::Cpu,
                MonitorSection::Gpu,
                MonitorSection::Power,
                MonitorSection::Cooling
            ]
        );
        assert!(settings.sections[..2].iter().all(|item| item.visible));
        assert!(settings.sections[2..4].iter().all(|item| !item.visible));
        assert_eq!(settings.hide_delay_ms, 2000);
        assert_eq!(
            MonitorSettings::from_key_file(&settings.to_key_file()),
            settings
        );
    }

    #[test]
    fn gpu_power_and_cooling_are_independent_of_cpu_and_memory() {
        let snapshot = SystemSnapshot {
            ready: true,
            gpus: vec![gpu::GpuDevice {
                id: "0000:01:00.0".into(),
                name: "Test GPU".into(),
                state: gpu::DataState::Ready,
                data: GpuSnapshot {
                    utilization_percent: Some(50.0),
                    memory_used: Some(1024),
                    power_watts: Some(75.0),
                    ..GpuSnapshot::default()
                },
            }],
            fans: vec![SensorReading {
                id: "fan1".into(),
                label: "CPU Fan".into(),
                value: 1200.0,
                state: gpu::DataState::Ready,
            }],
            battery: Some(BatterySnapshot {
                percent: 80.0,
                status: "Discharging".into(),
                power_watts: None,
            }),
            ..SystemSnapshot::default()
        };
        assert!(display_metrics(MonitorSection::Cpu, &snapshot).0.is_empty());
        assert!(
            display_metrics(MonitorSection::Memory, &snapshot)
                .0
                .is_empty()
        );
        assert_eq!(display_metrics(MonitorSection::Gpu, &snapshot).0.len(), 4);
        assert_eq!(display_metrics(MonitorSection::Power, &snapshot).0.len(), 1);
        assert_eq!(
            display_metrics(MonitorSection::Cooling, &snapshot).0.len(),
            1
        );
        assert!(!metric_options(MonitorSection::Battery, &snapshot).is_empty());
    }

    #[test]
    fn disk_mounts_include_second_ssd_without_counting_bind_mounts_twice() {
        let mounts = disk_mounts(
            "1 0 259:2 / / rw - ext4 /dev/nvme0n1p2 rw\n2 1 259:2 /nix/store /nix/store ro - ext4 /dev/nvme0n1p2 ro\n3 0 259:4 / /mnt/games rw - ext4 /dev/nvme1n1p1 rw\n4 0 0:7 / /tmp rw - tmpfs tmpfs rw\n5 0 7:0 / /app ro - squashfs /dev/loop0 ro\n6 0 8:1 / /mnt/My\\040Disk rw - ext4 /dev/sda1 rw",
        );
        assert_eq!(
            mounts
                .iter()
                .map(|(_, path)| path.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            ["/", "/mnt/My Disk", "/mnt/games"]
        );
        let btrfs = disk_mounts(
            "1 0 0:21 /@ / rw - btrfs /dev/nvme0n1p2 rw\n2 0 0:22 /@home /home rw - btrfs /dev/nvme0n1p2 rw",
        );
        assert_eq!(btrfs.len(), 1);
        assert_eq!(btrfs[0].1, Path::new("/"));
    }

    #[test]
    fn hidden_monitor_only_captures_last_ten_pixels() {
        for width in [159, 285, 565] {
            let region = monitor_input_region(width, 600, false, width - 48, 40);
            for y in [0, 39, 300, 599] {
                assert!(!region.contains_point(width - 11, y));
                assert!(region.contains_point(width - 10, y));
                assert!(region.contains_point(width - 1, y));
            }
            assert!(!region.contains_point(width, 300));
            assert!(!region.contains_point(width - 1, 600));
        }
    }

    #[test]
    fn open_monitor_passes_buttons_and_scrolls_to_content() {
        let region = monitor_input_region(285, 600, true, 236, 40);
        assert!(
            region.contains_point(100, 20),
            "drag handle stays stationary"
        );
        assert!(
            !region.contains_point(260, 20),
            "settings button gets native input"
        );
        assert!(
            !region.contains_point(100, 200),
            "body gets native scrolling"
        );
        assert!(region.contains_point(280, 200), "edge remains active");
    }

    #[test]
    #[ignore = "requires Wayland layer-shell and XDG_STATE_HOME=/tmp/obsidian-monitor-items-test-state"]
    fn monitor_drawer_animation_keeps_width_and_survives_reversal() {
        assert!(settings_path().starts_with("/tmp/obsidian-monitor-items-test-state"));
        gtk::init().unwrap();
        let application = gtk::Application::builder()
            .application_id("dev.obsidian.MonitorDrawerTest")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        application.register(None::<&gio::Cancellable>).unwrap();
        let display = gdk::Display::default().unwrap();
        let css = gtk::CssProvider::new();
        css.load_from_data(include_str!("../../assets/window.css"));
        gtk::style_context_add_provider_for_display(
            &display,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        let monitor = display
            .monitors()
            .item(0)
            .unwrap()
            .downcast::<gdk::Monitor>()
            .unwrap();
        let controller = SystemMonitorController::new();
        // The metric-selection test deliberately hides every row. Never inherit
        // those saved settings (or the user's real settings) in a layout test.
        controller.settings.replace(MonitorSettings::default());
        controller.latest.replace(SystemSampler::default().sample());
        let view = SystemMonitorView::new(&application, &monitor, &controller);
        let runtime = &view.runtime;
        // Isolate animation assertions from the user's pointer and hover timer.
        for widget in [&runtime.surface, &runtime.hotspot] {
            let controllers = widget.observe_controllers();
            while let Some(controller) = controllers.item(0) {
                widget.remove_controller(&controller.downcast::<gtk::EventController>().unwrap());
            }
        }
        runtime.window.set_layer(Layer::Overlay);
        runtime.window.settings().set_gtk_enable_animations(true);
        runtime.pinned.set(true);
        runtime.hide_generation.bump();
        let pump = |ms: u64| {
            let until = Instant::now() + Duration::from_millis(ms);
            while Instant::now() < until {
                glib::MainContext::default().iteration(false);
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        // Warm up the renderer before checking intermediate animation frames.
        runtime.drawer.set_revealed(true);
        pump(800);
        runtime.drawer.set_revealed(false);
        pump(300);
        let frames = Rc::new(RefCell::new(Vec::new()));
        let sampled_frames = Rc::clone(&frames);
        let tick = runtime.window.add_tick_callback(move |window, clock| {
            if let Some(surface) = window.surface() {
                sampled_frames
                    .borrow_mut()
                    .push((clock.frame_time(), surface.width()));
            }
            glib::ControlFlow::Continue
        });
        runtime.drawer.set_revealed(true);
        pump(80);
        let width = runtime.window.surface().unwrap().width();
        let opening_margin = runtime.window.margin(Edge::Right);
        assert!(opening_margin < drawer::RIGHT_MARGIN);
        pump(80);
        assert_eq!(runtime.window.surface().unwrap().width(), width);
        assert!(runtime.window.margin(Edge::Right) > opening_margin);
        tick.remove();
        runtime.drawer.set_revealed(false);
        pump(45);
        let reversing_margin = runtime.window.margin(Edge::Right);
        runtime.drawer.set_revealed(true);
        assert_eq!(runtime.window.margin(Edge::Right), reversing_margin);
        pump(350);
        assert!(runtime.drawer.is_open());
        assert!(runtime.window.is_visible());
        assert_eq!(runtime.window.margin(Edge::Right), drawer::RIGHT_MARGIN);
        assert_eq!(runtime.window.surface().unwrap().width(), width);
        let frames = frames.borrow();
        let mut intervals = frames
            .windows(2)
            .map(|pair| pair[1].0 - pair[0].0)
            .collect::<Vec<_>>();
        assert!(frames.len() > 2);
        intervals.sort_unstable();
        let median = intervals[intervals.len() / 2];
        eprintln!(
            "Monitor refresh: {} mHz; animation frames: {}; median interval: {} us; longest interval: {} us",
            monitor.refresh_rate(),
            frames.len(),
            median,
            intervals.last().unwrap(),
        );
        if monitor.refresh_rate() > 0 {
            let refresh_interval = 1_000_000_000 / i64::from(monitor.refresh_rate());
            assert!(
                median < refresh_interval * 3 / 2,
                "animation must follow the output refresh rate"
            );
        }
        assert!(frames.iter().all(|(_, frame_width)| *frame_width == width));
        let height = runtime.window.surface().unwrap().height();
        let y = runtime.window.margin(Edge::Top);
        runtime.set_settings_open(true);
        runtime.settings_hovered.set(true);
        pump(300);
        assert!(runtime.settings_reveal.is_child_revealed());
        assert!(runtime.window.surface().unwrap().width() > width);
        assert_eq!(runtime.window.surface().unwrap().height(), height);
        assert_eq!(runtime.window.margin(Edge::Top), y);
        assert_monitor_height_fits(runtime);
        if let Ok(path) = std::env::var("OBSIDIAN_MONITOR_TEST_SNAPSHOT") {
            fn descendants(widget: &gtk::Widget) -> Vec<gtk::Widget> {
                let mut result = vec![widget.clone()];
                let mut child = widget.first_child();
                while let Some(widget) = child {
                    result.extend(descendants(&widget));
                    child = widget.next_sibling();
                }
                result
            }
            let capture = |widget: &gtk::Widget, path: &str| {
                let snapshot = gtk::Snapshot::new();
                gtk::WidgetPaintable::new(Some(widget)).snapshot(
                    &snapshot,
                    f64::from(widget.width()),
                    f64::from(widget.height()),
                );
                runtime
                    .window
                    .renderer()
                    .unwrap()
                    .render_texture(
                        snapshot.to_node().unwrap_or_else(|| {
                            panic!(
                                "empty snapshot {path}: visible={}, mapped={}, size={}x{}",
                                widget.is_visible(),
                                widget.is_mapped(),
                                widget.width(),
                                widget.height()
                            )
                        }),
                        None,
                    )
                    .save_to_png(path)
                    .unwrap();
            };
            capture(
                runtime.surface.upcast_ref(),
                &format!("{path}.collapsed.png"),
            );
            let adjustments = descendants(runtime.settings_panel.root.upcast_ref())
                .into_iter()
                .find_map(|widget| widget.downcast::<gtk::Expander>().ok())
                .unwrap();
            assert!(!adjustments.is_expanded());
            adjustments.set_expanded(true);
            pump(200);
            assert_eq!(runtime.window.surface().unwrap().height(), height);
            for hovered in [false, true] {
                for slider in [
                    &runtime.settings_panel.width,
                    &runtime.settings_panel.height,
                    &runtime.settings_panel.scale,
                    &runtime.settings_panel.hide_delay,
                    &runtime.settings_panel.interval,
                ] {
                    if hovered {
                        slider.set_state_flags(gtk::StateFlags::PRELIGHT, false);
                    }
                }
                pump(50);
                let output = if hovered {
                    format!("{path}.hover.png")
                } else {
                    path.clone()
                };
                capture(runtime.surface.upcast_ref(), &output);
            }
            for slider in [
                &runtime.settings_panel.width,
                &runtime.settings_panel.height,
                &runtime.settings_panel.scale,
                &runtime.settings_panel.hide_delay,
                &runtime.settings_panel.interval,
            ] {
                slider.unset_state_flags(gtk::StateFlags::PRELIGHT);
                let controllers = slider.observe_controllers();
                for index in 0..controllers.n_items() {
                    if let Some(scroll) = controllers
                        .item(index)
                        .and_then(|item| item.downcast::<gtk::EventControllerScroll>().ok())
                    {
                        eprintln!(
                            "Monitor slider scroll phase: {:?}",
                            scroll.propagation_phase()
                        );
                    }
                }
            }
            for (name, dropdown) in [
                ("display", &runtime.settings_panel.output),
                ("network", &runtime.settings_panel.network),
            ] {
                let popup = descendants(dropdown.upcast_ref())
                    .into_iter()
                    .find_map(|widget| widget.downcast::<gtk::Popover>().ok())
                    .unwrap();
                // Keep manual desktop input from dismissing the visual fixture.
                popup.set_autohide(false);
                popup.popup();
                pump(150);
                eprintln!(
                    "Monitor {name} menu: visible={}, mapped={}, size={}x{}, settings={}",
                    popup.is_visible(),
                    popup.is_mapped(),
                    popup.width(),
                    popup.height(),
                    runtime.settings_reveal.reveals_child()
                );
                if std::env::var_os("OBSIDIAN_MONITOR_TEST_DESKTOP").is_some() {
                    let output = format!("{path}.{name}.desktop.png");
                    assert!(
                        std::process::Command::new("niri")
                            .args([
                                "msg",
                                "action",
                                "screenshot-screen",
                                "--path",
                                &output,
                                "--show-pointer",
                                "false"
                            ])
                            .status()
                            .unwrap()
                            .success()
                    );
                    pump(150);
                }
                capture(popup.upcast_ref(), &format!("{path}.{name}.png"));
                if let Some(row) = descendants(popup.upcast_ref()).into_iter().find(|widget| {
                    widget.css_name() == "row"
                        && !widget.state_flags().contains(gtk::StateFlags::SELECTED)
                }) {
                    row.set_state_flags(gtk::StateFlags::PRELIGHT, false);
                    pump(150);
                    capture(popup.upcast_ref(), &format!("{path}.{name}.hover.png"));
                }
                for label in descendants(popup.upcast_ref())
                    .into_iter()
                    .filter_map(|widget| widget.downcast::<gtk::Label>().ok())
                {
                    if label.is_mapped() {
                        let color = label.color();
                        assert!(
                            color.red() > 0.9 && color.green() > 0.9 && color.blue() > 0.9,
                            "popup text must remain readable: {} ({color})",
                            label.text()
                        );
                    }
                }
                popup.popdown();
                pump(150);
            }
        }
        let settings_width = runtime.window.surface().unwrap().width();
        runtime.drawer.set_revealed(false);
        pump(80);
        assert_eq!(runtime.window.surface().unwrap().width(), settings_width);
        pump(300);
        assert!(!runtime.window.is_visible());
        assert!(!runtime.settings_reveal.reveals_child());
        assert!(runtime.hotspot_window.is_visible());
        runtime.drawer.set_revealed(true);
        pump(300);
        for (width, height, font_scale) in [
            (420, 500, 1000),
            (356, 851, 1090),
            (420, 500, 2000),
            (240, 350, 1000),
            (180, 160, 2182),
            (800, 9999, 1000),
        ] {
            let mut settings = controller.settings();
            settings.width = width;
            settings.height = Some(height);
            settings.scale_milli = font_scale;
            settings.pinned = true;
            controller.settings.replace(settings.clone());
            controller
                .settings_subscribers
                .borrow_mut()
                .retain(|subscriber| subscriber(&settings));
            pump(100);
            assert!(runtime.card.width() >= width);
            assert!(runtime.card.width() <= PANEL_MAX_WIDTH);
            assert_monitor_text_fits(runtime.card.upcast_ref());
            let geometry = runtime.geometry();
            assert!(runtime.card.height() >= height.min(geometry.height_limits().1));
            assert!(runtime.card.height() <= geometry.height_limits().1);
            assert_monitor_height_fits(runtime);
            let height = runtime.card.height();
            let scroll = runtime.scroller.vadjustment();
            scroll.set_value(scroll.upper() - scroll.page_size());
            pump(30);
            assert_monitor_bottom_visible(runtime);
            scroll.set_value(0.0);
            assert_eq!(
                runtime.settings_panel.width.value(),
                f64::from(runtime.card.width())
            );
            assert_eq!(runtime.settings_panel.height.value(), f64::from(height));
            assert!(runtime.y.get() >= geometry.top_margin);
            assert!(
                runtime.y.get() + runtime.window.height()
                    <= geometry.screen_height - PANEL_EDGE_MARGIN
            );
            let trigger = runtime
                .header
                .last_child()
                .unwrap()
                .compute_bounds(&runtime.card)
                .unwrap();
            assert!(runtime.drag_handle_size().0 as f32 <= trigger.x());
            let window_height = runtime.window.surface().unwrap().height();
            let y = runtime.window.margin(Edge::Top);
            for open in [true, false] {
                runtime.set_settings_open(open);
                for elapsed in [80, 220] {
                    pump(elapsed);
                    assert_eq!(runtime.window.surface().unwrap().height(), window_height);
                    assert_eq!(runtime.card.height(), height);
                    assert_eq!(runtime.window.margin(Edge::Top), y);
                }
                if open && height == PANEL_MIN_HEIGHT {
                    let scroll = runtime.settings_scroller.vadjustment();
                    assert!(scroll.upper() > scroll.page_size());
                    let bottom = scroll.upper() - scroll.page_size();
                    scroll.set_value(bottom);
                    pump(20);
                    assert_eq!(scroll.value(), bottom, "all settings must remain reachable");
                    scroll.set_value(0.0);
                }
            }
        }
        runtime.hide_delay.set(Duration::from_millis(150));
        runtime.hovered.set(false);
        runtime.hotspot_hovered.set(false);
        runtime.pinned.set(false);
        runtime.sync_reveal();
        pump(60);
        assert!(runtime.window.is_visible());
        runtime.hovered.set(true);
        runtime.sync_reveal();
        pump(400);
        assert!(
            runtime.window.is_visible(),
            "returning to the panel cancels the timeout"
        );
        runtime.hovered.set(false);
        runtime.sync_reveal();
        pump(450);
        assert!(
            !runtime.window.is_visible(),
            "panel hides after the configured delay"
        );
        runtime.window.settings().set_gtk_enable_animations(false);
        runtime.drawer.set_revealed(true);
        assert!(runtime.drawer.is_open());
        runtime.drawer.set_revealed(false);
        assert!(!runtime.window.is_visible());
        drop(view);
        pump(20);
    }

    #[test]
    fn section_order_is_deduplicated_and_completed() {
        let hidden = HashSet::from([MonitorSection::Network]);
        let sections = normalized_sections(
            vec![
                MonitorSection::Storage,
                MonitorSection::Cpu,
                MonitorSection::Storage,
            ],
            &hidden,
        );
        assert_eq!(sections.len(), MonitorSection::ALL.len());
        assert_eq!(sections[0].section, MonitorSection::Storage);
        assert_eq!(sections[1].section, MonitorSection::Cpu);
        assert!(
            !sections
                .iter()
                .find(|preference| preference.section == MonitorSection::Network)
                .unwrap()
                .visible
        );
    }

    #[test]
    fn cpu_usage_uses_deltas_and_includes_iowait_as_idle() {
        let previous = parse_cpu_times("cpu  100 0 50 800 50 0 0 0").unwrap();
        let current = parse_cpu_times("cpu  150 0 70 850 70 0 0 0").unwrap();
        assert!((cpu_usage(previous, current).unwrap() - 50.0).abs() < 0.001);
    }

    #[test]
    fn guest_cpu_time_is_not_counted_twice() {
        let previous = parse_cpu_times("cpu 100 0 50 800 50 0 0 0 80 0").unwrap();
        let current = parse_cpu_times("cpu 150 0 70 850 70 0 0 0 120 0").unwrap();
        assert!((cpu_usage(previous, current).unwrap() - 50.0).abs() < 0.001);
        assert!(parse_cpu_times("cpu 18446744073709551615 1 0 0").is_none());
        assert!(parse_cpu_times("cpu 1 2").is_none());
    }

    #[test]
    fn rates_ignore_counter_resets() {
        let previous = NetworkCounters {
            read_at: Instant::now(),
            received: 10_000,
            transmitted: 20_000,
            interfaces: vec!["wlan0".to_owned()],
        };
        let current = NetworkCounters {
            read_at: previous.read_at + Duration::from_secs(2),
            received: 100,
            transmitted: 200,
            interfaces: vec!["wlan0".to_owned()],
        };
        assert_eq!(network_rates(&previous, &current), Some((0.0, 0.0)));
    }

    #[test]
    fn rates_reset_when_the_interface_set_changes() {
        let previous = NetworkCounters {
            read_at: Instant::now(),
            received: 10_000,
            transmitted: 20_000,
            interfaces: vec!["wlan0".to_owned()],
        };
        let current = NetworkCounters {
            read_at: previous.read_at + Duration::from_secs(1),
            received: 50_000,
            transmitted: 80_000,
            interfaces: vec!["eth0".to_owned()],
        };

        assert_eq!(network_rates(&previous, &current), None);
    }

    #[test]
    fn byte_format_is_compact_and_binary() {
        assert_eq!(format_bytes(1536), "2 KiB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn panel_stays_on_screen_as_content_grows_and_shrinks() {
        let geometry = PanelGeometry {
            screen_width: 1280,
            screen_height: 720,
            panel_width: 230,
            top_margin: 54,
        };
        assert_eq!(geometry.clamp_y(-100, 480), 54);
        assert_eq!(geometry.clamp_y(500, 480), 232);
        assert_eq!(geometry.clamp_y(500, 120), 500);
        assert_eq!(geometry.clamp_y(500, 900), 54);
        assert_eq!(geometry.height_limits(), (160, 658));
        assert_eq!(geometry.clamp_y(500, geometry.height_limits().1), 54);
    }

    #[test]
    fn common_kernel_sensor_labels_keep_their_device_context() {
        assert_eq!(
            sensor_display_label(
                "CPU",
                "k10temp",
                Some("Tccd2"),
                SensorKind::Temperature,
                "4",
            ),
            "CPU CCD 2 Temperature"
        );
        assert_eq!(
            sensor_display_label(
                "Example FastDrive SSD",
                "nvme",
                Some("Sensor 1"),
                SensorKind::Temperature,
                "2",
            ),
            "Example FastDrive SSD Sensor 1 Temperature"
        );
        assert_eq!(
            sensor_display_label(
                "RAM Module 2",
                "spd5118",
                None,
                SensorKind::Temperature,
                "1",
            ),
            "RAM Module 2 Temperature"
        );
    }

    #[test]
    fn storage_models_are_compacted_without_model_specific_rules() {
        assert_eq!(
            compact_storage_model("ExampleCorp SSD FastDrive with Heatsink 2TB"),
            Some("ExampleCorp FastDrive".to_owned())
        );
        assert_eq!(
            compact_storage_model("Vendor_Model NVMe 1000GB"),
            Some("Vendor Model".to_owned())
        );
    }

    #[test]
    fn laptop_thermal_zone_names_are_humanized() {
        assert_eq!(thermal_zone_device_label("x86_pkg_temp"), "CPU Package");
        assert_eq!(thermal_zone_device_label("acpitz"), "ACPI Thermal Zone");
        assert_eq!(thermal_zone_device_label("iwlwifi_1"), "Wi-Fi Adapter");
    }
}
