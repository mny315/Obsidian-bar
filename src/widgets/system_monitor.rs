use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use gtk::glib;
use tracing::{info, warn};

use super::run_background;

mod drawer;
mod gpu;
mod nvidia;

mod metrics;
use metrics::*;
mod settings_ui;
pub(crate) use drawer::monitor_output_id;
mod view;
#[cfg(test)]
use view::{MonitorRuntime, monitor_input_region};
mod sampler;
use sampler::*;
pub use view::SystemMonitorView;

const SETTINGS_GROUP: &str = "monitor";
const SETTINGS_FILE: &str = "system-monitor.ini";
const SAMPLE_INTERVAL_DEFAULT_MS: i32 = 1_000;
const HIDE_DELAY_DEFAULT_MS: i32 = 2_000;
const HIDE_DELAY_MAX_MS: i32 = 30_000;
const PANEL_MIN_WIDTH: i32 = 180;
const PANEL_MAX_WIDTH: i32 = 800;
const PANEL_DEFAULT_WIDTH: i32 = 280;
const PANEL_MIN_HEIGHT: i32 = 160;
const PANEL_EDGE_MARGIN: i32 = 8;
const SETTINGS_PANEL_WIDTH: i32 = 330;
const SETTINGS_PANEL_PADDING: i32 = 12;
const SCALE_MILLI_DEFAULT: i32 = 1_000;
const SCALE_MILLI_MIN: i32 = 727;
const SCALE_MILLI_MAX: i32 = 2_182;

fn scaled_pixels(base: i32, scale_milli: i32) -> i32 {
    (base.saturating_mul(scale_milli) / SCALE_MILLI_DEFAULT).max(1)
}

const ICON_MONITOR: &str = "\u{f0379}";
const METRIC_METER: &str = "usage-meter";

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

#[cfg(test)]
mod tests;
