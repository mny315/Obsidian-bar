use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet, VecDeque},
    ffi::CString,
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use gtk::{gdk, glib, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use tracing::{info, warn};

use super::{Generation, clear_box, detach_application_window, run_background};

mod drawer;
use drawer::MonitorDrawer;

const SETTINGS_GROUP: &str = "monitor";
const SETTINGS_FILE: &str = "system-monitor.ini";
const CONTENT_NAMESPACE: &str = "obsidian-system-monitor";
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const HIDE_DELAY: Duration = Duration::from_secs(2);
const NETWORK_HISTORY_LENGTH: usize = 64;
const PANEL_MIN_WIDTH: i32 = 220;
const PANEL_MAX_WIDTH: i32 = 280;
const PANEL_EDGE_MARGIN: i32 = 8;
const SETTINGS_TRIGGER_SIZE: i32 = 36;
const SETTINGS_PANEL_WIDTH: i32 = 330;
const SETTINGS_PANEL_PADDING: i32 = 12;
const SCALE_MILLI_DEFAULT: i32 = 1_000;
const SCALE_MILLI_MIN: i32 = 700;
const SCALE_MILLI_MAX: i32 = 2_000;

fn scaled_pixels(base: i32, scale_milli: i32) -> i32 {
    (base.saturating_mul(scale_milli) / SCALE_MILLI_DEFAULT).max(1)
}

const ICON_SETTINGS: &str = "\u{f0493}";
const ICON_MONITOR: &str = "\u{f0379}";
const ICON_UP: &str = "\u{f005d}";
const ICON_DOWN: &str = "\u{f0045}";
const ICON_VISIBLE: &str = "\u{f0208}";
const ICON_HIDDEN: &str = "\u{f0209}";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum MonitorSection {
    Performance,
    Memory,
    Network,
    Storage,
    Temperatures,
    Hardware,
    Battery,
}

impl MonitorSection {
    const ALL: [Self; 7] = [
        Self::Performance,
        Self::Memory,
        Self::Network,
        Self::Storage,
        Self::Temperatures,
        Self::Hardware,
        Self::Battery,
    ];

    const fn id(self) -> &'static str {
        match self {
            Self::Performance => "performance",
            Self::Memory => "memory",
            Self::Network => "network",
            Self::Storage => "storage",
            Self::Temperatures => "temperatures",
            Self::Hardware => "hardware",
            Self::Battery => "battery",
        }
    }

    const fn title(self) -> &'static str {
        match self {
            Self::Performance => "Processor & GPU",
            Self::Memory => "Memory",
            Self::Network => "Network",
            Self::Storage => "Storage",
            Self::Temperatures => "Temperatures",
            Self::Hardware => "Power & cooling",
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
    position_y: Option<i32>,
    pinned: bool,
    scale_milli: i32,
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
            position_y: None,
            pinned: false,
            scale_milli: SCALE_MILLI_DEFAULT,
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
        let scale_milli = key_file
            .integer(SETTINGS_GROUP, "scale")
            .unwrap_or(SCALE_MILLI_DEFAULT)
            .clamp(SCALE_MILLI_MIN, SCALE_MILLI_MAX);

        Self {
            enabled,
            sections: normalized_sections(order, &hidden),
            position_y,
            pinned,
            scale_milli,
        }
    }

    fn save(&self) -> Result<(), String> {
        let path = settings_path();
        let parent = path
            .parent()
            .ok_or_else(|| "failed to resolve system monitor state directory".to_owned())?;
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create system monitor state directory: {error}"))?;

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
        key_file.set_integer(SETTINGS_GROUP, "scale", self.scale_milli);

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
}

fn parse_section_list(value: &str) -> Vec<MonitorSection> {
    value
        .split(',')
        .filter_map(MonitorSection::from_id)
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
    utilization_percent: Option<f64>,
    memory_used: Option<u64>,
    memory_total: Option<u64>,
    power_watts: Option<f64>,
    temperature_celsius: Option<f64>,
}

impl GpuSnapshot {
    fn available(&self) -> bool {
        self.utilization_percent.is_some()
            || self.memory_used.is_some()
            || self.power_watts.is_some()
            || self.temperature_celsius.is_some()
    }
}

#[derive(Clone, Debug)]
struct SensorReading {
    id: String,
    label: String,
    value: f64,
}

#[derive(Clone, Debug)]
struct BatterySnapshot {
    percent: f64,
    status: String,
    power_watts: Option<f64>,
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
    gpu: Option<GpuSnapshot>,
    network_available: bool,
    network_interfaces: Vec<String>,
    download_bytes_per_second: f64,
    upload_bytes_per_second: f64,
    network_received_bytes: u64,
    network_transmitted_bytes: u64,
    disk_used: Option<u64>,
    disk_total: Option<u64>,
    temperatures: Vec<SensorReading>,
    power: Vec<SensorReading>,
    fans: Vec<SensorReading>,
    battery: Option<BatterySnapshot>,
}

impl SystemSnapshot {
    fn section_available(&self, section: MonitorSection) -> bool {
        match section {
            MonitorSection::Performance => {
                !self.ready
                    || self.cpu_percent.is_some()
                    || self.gpu.as_ref().is_some_and(GpuSnapshot::available)
            }
            MonitorSection::Memory => {
                self.memory_used.is_some()
                    || self
                        .gpu
                        .as_ref()
                        .is_some_and(|gpu| gpu.memory_used.is_some())
            }
            MonitorSection::Network => self.network_available,
            MonitorSection::Storage => self.disk_used.is_some(),
            MonitorSection::Temperatures => {
                !self.temperatures.is_empty()
                    || self
                        .gpu
                        .as_ref()
                        .is_some_and(|gpu| gpu.temperature_celsius.is_some())
            }
            MonitorSection::Hardware => !self.power.is_empty() || !self.fans.is_empty(),
            MonitorSection::Battery => self.battery.is_some(),
        }
    }
}

fn available_section_mask(snapshot: &SystemSnapshot) -> u8 {
    MonitorSection::ALL
        .into_iter()
        .enumerate()
        .fold(0, |mask, (index, section)| {
            if snapshot.section_available(section) {
                mask | (1 << index)
            } else {
                mask
            }
        })
}

type SnapshotSubscriber = Box<dyn Fn(&SystemSnapshot) -> bool>;
type SettingsSubscriber = Box<dyn Fn(&MonitorSettings) -> bool>;
type StateSubscriber = Box<dyn Fn(bool) -> bool>;

pub struct SystemMonitorController {
    settings: RefCell<MonitorSettings>,
    latest: RefCell<SystemSnapshot>,
    sampler: Arc<Mutex<SystemSampler>>,
    snapshot_subscribers: RefCell<Vec<SnapshotSubscriber>>,
    settings_subscribers: RefCell<Vec<SettingsSubscriber>>,
    state_subscribers: RefCell<Vec<StateSubscriber>>,
    timer: RefCell<Option<glib::SourceId>>,
    sampling: Cell<bool>,
    started: Cell<bool>,
    first_sample_logged: Cell<bool>,
}

impl SystemMonitorController {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            settings: RefCell::new(MonitorSettings::load()),
            latest: RefCell::new(SystemSnapshot::default()),
            sampler: Arc::new(Mutex::new(SystemSampler::default())),
            snapshot_subscribers: RefCell::new(Vec::new()),
            settings_subscribers: RefCell::new(Vec::new()),
            state_subscribers: RefCell::new(Vec::new()),
            timer: RefCell::new(None),
            sampling: Cell::new(false),
            started: Cell::new(false),
            first_sample_logged: Cell::new(false),
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
        self.sample_now();

        let weak = Rc::downgrade(self);
        let source = glib::timeout_add_local(SAMPLE_INTERVAL, move || {
            let Some(controller) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            controller.sample_now();
            glib::ControlFlow::Continue
        });
        self.timer.replace(Some(source));
    }

    pub fn shutdown(&self) {
        self.started.set(false);
        self.stop_sampling();
    }

    fn stop_sampling(&self) {
        if let Some(source) = self.timer.borrow_mut().take() {
            source.remove();
        }
    }

    fn sample_now(self: &Rc<Self>) {
        if !self.started.get() || !self.enabled() || self.sampling.replace(true) {
            return;
        }

        let sampler = Arc::clone(&self.sampler);
        let weak = Rc::downgrade(self);
        run_background(
            move || {
                let mut sampler = sampler
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                sampler.sample()
            },
            move |snapshot| {
                let Some(controller) = weak.upgrade() else {
                    return;
                };
                controller.sampling.set(false);
                if !controller.started.get() || !controller.enabled() {
                    return;
                }
                if !controller.first_sample_logged.replace(true) {
                    info!(
                        cpu = snapshot.cpu_percent.is_some(),
                        memory = snapshot.memory_used.is_some(),
                        network = snapshot.network_available,
                        storage = snapshot.disk_used.is_some(),
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

    fn update_settings(&self, update: impl FnOnce(&mut MonitorSettings)) -> bool {
        let mut next = self.settings();
        update(&mut next);
        if next == *self.settings.borrow() {
            return true;
        }
        if let Err(error) = next.save() {
            warn!(%error, "failed to update system monitor settings");
            return false;
        }

        self.settings.replace(next.clone());
        self.settings_subscribers
            .borrow_mut()
            .retain(|subscriber| subscriber(&next));
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

    fn set_section_visible(&self, section: MonitorSection, visible: bool) -> bool {
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

    fn move_section(&self, section: MonitorSection, direction: i32) -> bool {
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

    fn set_position(&self, y: i32) -> bool {
        self.update_settings(|settings| settings.position_y = Some(y))
    }

    fn set_scale(&self, scale_milli: i32) -> bool {
        self.update_settings(|settings| {
            settings.scale_milli = scale_milli.clamp(SCALE_MILLI_MIN, SCALE_MILLI_MAX)
        })
    }

    fn set_pinned(&self, pinned: bool) -> bool {
        self.update_settings(|settings| settings.pinned = pinned)
    }
}

#[derive(Clone)]
struct DisplayMetric {
    id: String,
    label: String,
    value: String,
}

impl DisplayMetric {
    fn new(id: impl Into<String>, label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            value: value.into(),
        }
    }
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
    value_labels: RefCell<Vec<gtk::Label>>,
    meter: Option<MeterView>,
    graph: Option<gtk::DrawingArea>,
    graph_state: Option<Rc<RefCell<NetworkGraphState>>>,
    available: Cell<bool>,
    scale_milli: Cell<i32>,
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

        let meter = matches!(
            section,
            MonitorSection::Memory | MonitorSection::Storage | MonitorSection::Battery
        )
        .then(|| {
            let meter = MeterView::new();
            root.append(&meter.area);
            meter
        });

        Rc::new(Self {
            root,
            rows,
            signature: RefCell::new(Vec::new()),
            value_labels: RefCell::new(Vec::new()),
            meter,
            graph,
            graph_state,
            available: Cell::new(false),
            scale_milli: Cell::new(SCALE_MILLI_DEFAULT),
        })
    }

    fn update(&self, metrics: Vec<DisplayMetric>, fraction: Option<f64>) {
        let signature = metrics
            .iter()
            .map(|metric| (metric.id.clone(), metric.label.clone()))
            .collect::<Vec<_>>();
        if *self.signature.borrow() != signature {
            clear_box(&self.rows);
            let mut value_labels = self.value_labels.borrow_mut();
            value_labels.clear();
            for metric in &metrics {
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
                label.set_ellipsize(gtk::pango::EllipsizeMode::End);

                let value = gtk::Label::new(Some(&metric.value));
                value.add_css_class("system-monitor-value");
                value.set_xalign(1.0);

                row.append(&accent);
                row.append(&label);
                row.append(&value);
                self.rows.append(&row);
                value_labels.push(value);
            }
            self.signature.replace(signature);
        }

        for (label, metric) in self.value_labels.borrow().iter().zip(&metrics) {
            label.set_label(&metric.value);
        }

        if let Some(meter) = self.meter.as_ref() {
            meter.set_fraction(fraction);
        }
        self.available.set(!metrics.is_empty());
    }

    fn apply_scale(&self, scale_milli: i32) {
        self.scale_milli.set(scale_milli);
        self.root.set_spacing(scaled_pixels(3, scale_milli));
        self.rows.set_spacing(scaled_pixels(1, scale_milli));
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
        root.set_can_target(false);

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
        self.refresh_visibility();
    }

    fn natural_height(&self, width: i32) -> i32 {
        let (_, natural, _, _) = self.root.measure(gtk::Orientation::Vertical, width.max(1));
        natural.max(1)
    }

    fn update_snapshot(&self, snapshot: &SystemSnapshot) {
        for section in MonitorSection::ALL {
            let Some(view) = self.sections.get(&section) else {
                continue;
            };
            let (metrics, fraction) = display_metrics(section, snapshot);
            view.update(metrics, fraction);
            if section == MonitorSection::Network {
                view.push_network_sample(
                    snapshot.download_bytes_per_second,
                    snapshot.upload_bytes_per_second,
                );
            }
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

        let has_selected_sections = settings
            .sections
            .iter()
            .any(|preference| preference.visible);
        if has_selected_sections {
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
        MonitorSection::Performance => {
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
            if let Some(gpu) = snapshot.gpu.as_ref() {
                if let Some(utilization) = gpu.utilization_percent {
                    metrics.push(DisplayMetric::new(
                        "gpu",
                        "GPU",
                        format_percent(utilization),
                    ));
                }
                if let Some(power) = gpu.power_watts {
                    metrics.push(DisplayMetric::new(
                        "gpu-power",
                        "GPU Power",
                        format!("{power:.1} W"),
                    ));
                }
            }
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
            if let Some(gpu) = snapshot.gpu.as_ref()
                && let Some(used) = gpu.memory_used
            {
                metrics.push(DisplayMetric::new(
                    "gpu-memory",
                    "GPU Memory",
                    format_bytes(used),
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
            if let Some(used) = snapshot.disk_used {
                metrics.push(DisplayMetric::new(
                    "disk-used",
                    "Disk Used",
                    format_bytes(used),
                ));
            }
            if let Some(total) = snapshot.disk_total {
                metrics.push(DisplayMetric::new(
                    "disk-total",
                    "Total",
                    format_bytes(total),
                ));
            }
            if let (Some(used), Some(total)) = (snapshot.disk_used, snapshot.disk_total) {
                metrics.push(DisplayMetric::new(
                    "disk-available",
                    "Available",
                    format_bytes(total.saturating_sub(used)),
                ));
            }
            ratio(snapshot.disk_used, snapshot.disk_total)
        }
        MonitorSection::Temperatures => {
            let mut seen_gpu = false;
            for sensor in &snapshot.temperatures {
                seen_gpu |= sensor.label.to_ascii_lowercase().contains("gpu");
                metrics.push(DisplayMetric::new(
                    &sensor.id,
                    &sensor.label,
                    format!("{:.1} °C", sensor.value),
                ));
            }
            if !seen_gpu
                && let Some(temperature) = snapshot
                    .gpu
                    .as_ref()
                    .and_then(|gpu| gpu.temperature_celsius)
            {
                metrics.push(DisplayMetric::new(
                    "gpu-runtime-temperature",
                    "GPU Temperature",
                    format!("{temperature:.1} °C"),
                ));
            }
            None
        }
        MonitorSection::Hardware => {
            for sensor in &snapshot.power {
                if snapshot
                    .gpu
                    .as_ref()
                    .is_some_and(|gpu| gpu.power_watts.is_some())
                    && sensor.label.to_ascii_lowercase().contains("gpu")
                {
                    continue;
                }
                metrics.push(DisplayMetric::new(
                    &sensor.id,
                    &sensor.label,
                    format!("{:.1} W", sensor.value),
                ));
            }
            for sensor in &snapshot.fans {
                metrics.push(DisplayMetric::new(
                    &sensor.id,
                    &sensor.label,
                    format!("{:.0} RPM", sensor.value),
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
}

impl PanelGeometry {
    fn for_monitor(monitor: &gdk::Monitor, scale_milli: i32) -> Self {
        let geometry = monitor.geometry();
        let screen_width = geometry.width().max(1);
        let screen_height = geometry.height().max(1);
        let base_width = ((f64::from(screen_width) * 0.18).round() as i32)
            .clamp(PANEL_MIN_WIDTH, PANEL_MAX_WIDTH);
        let panel_width = scaled_pixels(base_width, scale_milli)
            .min((screen_width - drawer::RIGHT_MARGIN).max(1));
        Self {
            screen_width,
            screen_height,
            panel_width,
        }
    }

    fn clamp_y(self, y: i32, height: i32) -> i32 {
        let maximum = (self.screen_height - height - PANEL_EDGE_MARGIN).max(PANEL_EDGE_MARGIN);
        y.clamp(PANEL_EDGE_MARGIN, maximum)
    }

    fn default_y(self) -> i32 {
        ((f64::from(self.screen_height) * 0.26).round() as i32).max(58)
    }
}

struct MonitorSettingsPanel {
    root: gtk::Box,
    list: gtk::Box,
    pin: gtk::ToggleButton,
    syncing_pin: Cell<bool>,
    scale: gtk::Scale,
    syncing_scale: Cell<bool>,
    rebuild_pending: Cell<bool>,
    available_sections: Cell<u8>,
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

        let pin = gtk::ToggleButton::with_label("Pin panel");
        pin.add_css_class("system-monitor-settings-pin");
        pin.set_tooltip_text(Some("Keep the monitor visible when the pointer leaves"));
        pin.set_active(controller.settings().pinned);

        let scale_row = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let scale_title = gtk::Label::new(Some("Scale"));
        scale_title.add_css_class("system-monitor-settings-section-title");
        scale_title.set_xalign(0.0);
        let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 70.0, 200.0, 5.0);
        scale.add_css_class("slider-control");
        scale.add_css_class("system-monitor-settings-scale");
        scale.set_value(f64::from(controller.settings().scale_milli) / 10.0);
        scale.set_digits(0);
        scale.set_draw_value(true);
        scale.set_value_pos(gtk::PositionType::Right);
        scale.set_format_value_func(|_, value| format!("{value:.0}%"));
        scale.add_mark(100.0, gtk::PositionType::Bottom, None);
        scale_row.append(&scale_title);
        scale_row.append(&scale);

        let section_title = gtk::Label::new(Some("Visible sections and order"));
        section_title.add_css_class("system-monitor-settings-section-title");
        section_title.set_xalign(0.0);

        let list = gtk::Box::new(gtk::Orientation::Vertical, 4);
        list.add_css_class("system-monitor-settings-list");

        let footer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        footer.add_css_class("system-monitor-settings-footer");
        let hint = gtk::Label::new(Some(
            "Hover over the monitor area at the right edge to show it. Drag the header up and down. Height follows the visible sections.",
        ));
        hint.add_css_class("system-monitor-settings-hint");
        hint.set_wrap(true);
        hint.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        hint.set_xalign(0.0);
        hint.set_hexpand(true);

        footer.append(&hint);

        root.append(&header);
        root.append(&pin);
        root.append(&scale_row);
        root.append(&section_title);
        root.append(&list);
        root.append(&footer);

        let panel = Rc::new(Self {
            root,
            list,
            pin,
            syncing_pin: Cell::new(false),
            scale,
            syncing_scale: Cell::new(false),
            rebuild_pending: Cell::new(false),
            available_sections: Cell::new(available_section_mask(&controller.latest())),
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

        {
            let weak = Rc::downgrade(&panel);
            panel.scale.connect_value_changed(move |scale| {
                let Some(panel) = weak.upgrade() else {
                    return;
                };
                if panel.syncing_scale.get() {
                    return;
                }
                if !panel
                    .controller
                    .set_scale((scale.value() * 10.0).round() as i32)
                {
                    panel.sync_scale(panel.controller.settings().scale_milli);
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
            controller.subscribe_snapshot(move |snapshot| {
                let Some(panel) = weak.upgrade() else {
                    return false;
                };
                let available_sections = available_section_mask(snapshot);
                let changed =
                    panel.available_sections.replace(available_sections) != available_sections;
                if changed && panel.root.is_mapped() {
                    panel.schedule_rebuild();
                }
                true
            });
        }

        panel
    }

    fn sync_scale(&self, scale_milli: i32) {
        self.syncing_scale.set(true);
        self.scale.set_value(f64::from(scale_milli) / 10.0);
        self.syncing_scale.set(false);
    }

    fn sync_pin(&self, pinned: bool) {
        self.syncing_pin.set(true);
        self.pin.set_active(pinned);
        self.pin
            .set_label(if pinned { "Unpin panel" } else { "Pin panel" });
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
        clear_box(&self.list);
        let settings = self.controller.settings();
        self.sync_pin(settings.pinned);
        self.sync_scale(settings.scale_milli);
        let snapshot = self.controller.latest();
        self.available_sections
            .set(available_section_mask(&snapshot));
        let last_index = settings.sections.len().saturating_sub(1);

        for (index, preference) in settings.sections.into_iter().enumerate() {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 7);
            row.add_css_class("system-monitor-settings-row");

            let copy = gtk::Box::new(gtk::Orientation::Vertical, 1);
            copy.set_hexpand(true);
            let title = gtk::Label::new(Some(preference.section.title()));
            title.add_css_class("system-monitor-settings-row-title");
            title.set_xalign(0.0);
            let available = snapshot.section_available(preference.section);
            let meta = gtk::Label::new(Some(if available {
                "Available"
            } else {
                "No compatible sensor detected"
            }));
            meta.add_css_class("system-monitor-settings-row-meta");
            if !available {
                meta.add_css_class("system-monitor-settings-row-unavailable");
            }
            meta.set_xalign(0.0);
            copy.append(&title);
            copy.append(&meta);

            let up = settings_move_button(ICON_UP, "Move up", index > 0);
            let down = settings_move_button(ICON_DOWN, "Move down", index < last_index);
            let visible_icon = gtk::Label::new(Some(if preference.visible {
                ICON_VISIBLE
            } else {
                ICON_HIDDEN
            }));
            visible_icon.add_css_class("system-monitor-settings-button-icon");
            let visible = gtk::ToggleButton::new();
            visible.add_css_class("system-monitor-settings-visibility");
            visible.set_active(preference.visible);
            visible.set_valign(gtk::Align::Center);
            visible.set_tooltip_text(Some(if preference.visible {
                "Hide section"
            } else {
                "Show section"
            }));
            visible.set_child(Some(&visible_icon));

            {
                let weak = Rc::downgrade(self);
                up.connect_clicked(move |_| {
                    if let Some(panel) = weak.upgrade() {
                        panel.controller.move_section(preference.section, -1);
                    }
                });
            }
            {
                let weak = Rc::downgrade(self);
                down.connect_clicked(move |_| {
                    if let Some(panel) = weak.upgrade() {
                        panel.controller.move_section(preference.section, 1);
                    }
                });
            }
            {
                let weak = Rc::downgrade(self);
                let reverting = Rc::new(Cell::new(false));
                visible.connect_toggled(move |toggle| {
                    let requested = toggle.is_active();
                    visible_icon.set_label(if requested { ICON_VISIBLE } else { ICON_HIDDEN });
                    toggle.set_tooltip_text(Some(if requested {
                        "Hide section"
                    } else {
                        "Show section"
                    }));
                    let Some(panel) = weak.upgrade() else {
                        return;
                    };
                    if reverting.replace(false) {
                        return;
                    }
                    if !panel
                        .controller
                        .set_section_visible(preference.section, requested)
                    {
                        reverting.set(true);
                        toggle.set_active(!requested);
                    }
                });
            }

            row.append(&copy);
            row.append(&up);
            row.append(&down);
            row.append(&visible);
            self.list.append(&row);
        }
    }
}

fn settings_move_button(icon: &str, tooltip: &str, sensitive: bool) -> gtk::Button {
    let label = gtk::Label::new(Some(icon));
    label.add_css_class("system-monitor-settings-button-icon");
    let button = gtk::Button::new();
    button.add_css_class("system-monitor-settings-move");
    button.set_tooltip_text(Some(tooltip));
    button.set_sensitive(sensitive);
    button.set_child(Some(&label));
    button
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
    y: Cell<i32>,
    pinned: Cell<bool>,
    scale_milli: Cell<i32>,
    hovered: Cell<bool>,
    hotspot_hovered: Cell<bool>,
    dragging: Cell<bool>,
    drag_start_y: Cell<i32>,
    hide_generation: Generation,
    panel_width: Cell<i32>,
    scale_geometry: Cell<Option<(i32, i32, i32)>>,
}

impl MonitorRuntime {
    fn apply_size(&self) {
        let geometry = PanelGeometry::for_monitor(&self.monitor, self.scale_milli.get());
        let frozen = self.scale_geometry.get();
        let settings_open =
            self.settings_reveal.reveals_child() || self.settings_reveal.is_child_revealed();
        let maximum_width = if settings_open {
            (geometry.screen_width
                - SETTINGS_PANEL_WIDTH
                - 1
                - drawer::RIGHT_MARGIN
                - PANEL_EDGE_MARGIN)
                .max(120)
        } else {
            geometry.panel_width
        };
        let width = frozen.map_or(geometry.panel_width.min(maximum_width), |(width, _, _)| {
            width
        });
        self.panel_width.set(width);
        self.card.set_size_request(width, -1);
        let (_, header_height, _, _) = self.header.measure(gtk::Orientation::Vertical, width);
        let available_height = frozen.map_or(
            geometry.screen_height - PANEL_EDGE_MARGIN * 2,
            |(_, height, _)| height,
        );
        let available = (available_height - header_height).max(1);
        let height = self.layout.natural_height(width).min(available);
        self.scroller.set_min_content_height(-1);
        self.scroller.set_max_content_height(height);
        self.scroller.set_min_content_height(height);
        let (_, natural, _, _) = self.card.measure(gtk::Orientation::Vertical, width);
        let mut height = natural.max(drawer::TAIL_TOP + drawer::TAIL_HEIGHT);
        if settings_open && frozen.is_none() {
            let (_, settings_height, _, _) = self.settings_panel.root.measure(
                gtk::Orientation::Vertical,
                SETTINGS_PANEL_WIDTH - SETTINGS_PANEL_PADDING * 2,
            );
            height = height.max(
                (settings_height + SETTINGS_PANEL_PADDING * 2)
                    .min(geometry.screen_height - PANEL_EDGE_MARGIN * 2),
            );
        }
        if let Some((_, fixed_height, _)) = frozen {
            height = fixed_height;
        }
        let settings_height = (height - SETTINGS_PANEL_PADDING * 2).max(1);
        self.settings_scroller.set_min_content_height(-1);
        self.settings_scroller
            .set_max_content_height(settings_height);
        self.settings_scroller
            .set_min_content_height(settings_height);
        let y = frozen.map_or_else(
            || geometry.clamp_y(self.requested_y.get(), height),
            |(_, _, y)| y,
        );
        self.y.set(y);
        self.window.set_default_size(1, height);
        self.window.set_margin(Edge::Top, y);
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
            // Keep one stable input region throughout the reveal. Changing it
            // under a stationary pointer can generate a leave without an enter.
            surface.set_input_region(None);
        }
    }

    fn header_hit(&self, x: f64, y: f64) -> bool {
        let width = self.panel_width.get();
        let (_, height, _, _) = self.header.measure(gtk::Orientation::Vertical, width);
        self.drawer.is_open()
            && y >= 0.0
            && y < f64::from(height)
            && x < f64::from(
                width - scaled_pixels(SETTINGS_TRIGGER_SIZE + 8, self.scale_milli.get()),
            )
    }

    fn keep_open(&self) -> bool {
        self.pinned.get() || self.hovered.get() || self.hotspot_hovered.get() || self.dragging.get()
    }

    fn sync_reveal(self: &Rc<Self>) {
        let generation = self.hide_generation.bump();
        if self.keep_open() {
            self.drawer.set_revealed(true);
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(HIDE_DELAY, move || {
            if let Some(runtime) = weak.upgrade()
                && runtime.hide_generation.is_current(generation)
                && !runtime.keep_open()
            {
                runtime.settings_reveal.set_reveal_child(false);
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
                        PanelGeometry::for_monitor(&runtime.monitor, runtime.scale_milli.get());
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
        let geometry = PanelGeometry::for_monitor(monitor, settings.scale_milli);
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
        let trigger = gtk::Button::new();
        trigger.add_css_class("system-monitor-settings-trigger");
        trigger.set_size_request(SETTINGS_TRIGGER_SIZE, SETTINGS_TRIGGER_SIZE);
        trigger.set_tooltip_text(Some("Configure system monitor"));
        let trigger_icon = gtk::Label::new(Some(ICON_SETTINGS));
        trigger_icon.add_css_class("system-monitor-settings-trigger-icon");
        trigger.set_child(Some(&trigger_icon));
        header.append(&drag_handle);
        header.append(&trigger);

        let scroller = gtk::ScrolledWindow::new();
        scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroller.set_propagate_natural_height(true);
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
        settings_scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
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
        let settings_trigger_pressed = Rc::new(Cell::new(false));

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
            y: Cell::new(0),
            pinned: Cell::new(settings.pinned),
            scale_milli: Cell::new(settings.scale_milli),
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
            // Keep the settings stationary under the pointer while the slider
            // changes font metrics. Apply the final outer size on release.
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
                        }
                        gdk::EventType::ButtonRelease
                        | gdk::EventType::TouchEnd
                        | gdk::EventType::TouchCancel
                            if runtime.scale_geometry.take().is_some() =>
                        {
                            runtime.apply_size();
                        }
                        _ => {}
                    }
                }
                glib::Propagation::Proceed
            });
            runtime.settings_panel.scale.add_controller(events);
            let weak = Rc::downgrade(&runtime);
            runtime.settings_panel.scale.connect_unmap(move |_| {
                if let Some(runtime) = weak.upgrade()
                    && runtime.scale_geometry.take().is_some()
                {
                    runtime.apply_size();
                }
            });
        }
        {
            let click = gtk::GestureClick::new();
            click.set_button(gdk::BUTTON_PRIMARY);
            let hit_trigger = {
                let weak = Rc::downgrade(&runtime);
                let weak_trigger = trigger.downgrade();
                Rc::new(move |x: f64, y: f64| {
                    let (Some(runtime), Some(trigger)) = (weak.upgrade(), weak_trigger.upgrade())
                    else {
                        return false;
                    };
                    runtime.drawer.is_open()
                        && trigger.compute_bounds(&runtime.card).is_some_and(|bounds| {
                            bounds.contains_point(&gtk::graphene::Point::new(x as f32, y as f32))
                        })
                })
            };
            {
                let pressed = Rc::clone(&settings_trigger_pressed);
                let hit_trigger = Rc::clone(&hit_trigger);
                click.connect_pressed(move |gesture, _, x, y| {
                    let hit = hit_trigger(x, y);
                    pressed.set(hit);
                    if hit {
                        gesture.set_state(gtk::EventSequenceState::Claimed);
                    }
                });
            }
            {
                let pressed = Rc::clone(&settings_trigger_pressed);
                let weak_trigger = trigger.downgrade();
                click.connect_released(move |_, _, x, y| {
                    if pressed.replace(false)
                        && hit_trigger(x, y)
                        && let Some(trigger) = weak_trigger.upgrade()
                    {
                        trigger.emit_clicked();
                    }
                });
            }
            {
                let pressed = Rc::clone(&settings_trigger_pressed);
                click.connect_cancel(move |_, _| {
                    pressed.set(false);
                });
            }
            hotspot.add_controller(click);
        }
        {
            let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
            let weak = Rc::downgrade(&runtime);
            scroll.connect_scroll(move |scroll, _, dy| {
                let Some(runtime) = weak.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                let adjustment = runtime.scroller.vadjustment();
                let step = if scroll.unit() == gdk::ScrollUnit::Surface {
                    1.0
                } else {
                    40.0
                };
                adjustment.set_value((adjustment.value() + dy * step).clamp(
                    adjustment.lower(),
                    (adjustment.upper() - adjustment.page_size()).max(adjustment.lower()),
                ));
                glib::Propagation::Stop
            });
            hotspot.add_controller(scroll);
        }

        {
            let weak = Rc::downgrade(&runtime);
            runtime.drawer.connect_settled(move || {
                if let Some(runtime) = weak.upgrade() {
                    runtime.sync_input_region();
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
                runtime.layout.apply_settings(settings);
                runtime.pinned.set(settings.pinned);
                runtime.scale_milli.set(settings.scale_milli);
                if !runtime.dragging.get() {
                    let geometry =
                        PanelGeometry::for_monitor(&runtime.monitor, runtime.scale_milli.get());
                    runtime
                        .requested_y
                        .set(settings.position_y.unwrap_or(geometry.default_y()));
                }
                runtime.apply_size();
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

#[derive(Default)]
struct SystemSampler {
    previous_cpu: Option<CpuTimes>,
    previous_network: Option<NetworkCounters>,
}

impl SystemSampler {
    fn sample(&mut self) -> SystemSnapshot {
        let cpu_times = read_cpu_times();
        let cpu_percent = cpu_times.map(|current| {
            let percent = self
                .previous_cpu
                .and_then(|previous| cpu_usage(previous, current))
                .unwrap_or(0.0);
            self.previous_cpu = Some(current);
            percent
        });
        let cpu_frequency_mhz = read_cpu_frequency_mhz();
        let (load_average, running_processes, total_processes) = read_load_snapshot();
        let uptime_seconds = read_uptime_seconds();

        let memory = read_memory_usage();
        let memory_used = memory.map(|memory| memory.used);
        let memory_total = memory.map(|memory| memory.total);
        let swap_used = memory
            .filter(|memory| memory.swap_total > 0)
            .map(|memory| memory.swap_used);
        let swap_total = memory
            .filter(|memory| memory.swap_total > 0)
            .map(|memory| memory.swap_total);
        let (disk_used, disk_total) = read_disk_usage(Path::new("/"))
            .map(|(used, total)| (Some(used), Some(total)))
            .unwrap_or((None, None));

        let current_network = read_network_counters();
        let (
            network_available,
            network_interfaces,
            download_bytes_per_second,
            upload_bytes_per_second,
            network_received_bytes,
            network_transmitted_bytes,
        ) = if let Some((current, interfaces)) = current_network {
            let rates = self
                .previous_network
                .as_ref()
                .and_then(|previous| network_rates(previous, &current))
                .unwrap_or((0.0, 0.0));
            let received = current.received;
            let transmitted = current.transmitted;
            self.previous_network = Some(current);
            (true, interfaces, rates.0, rates.1, received, transmitted)
        } else {
            self.previous_network = None;
            (false, Vec::new(), 0.0, 0.0, 0, 0)
        };

        let HwmonSnapshot {
            temperatures,
            power,
            fans,
        } = read_hwmon_snapshot();
        let mut gpu = read_drm_gpu_snapshot();
        let generic_gpu_temperature = temperatures
            .iter()
            .find(|sensor| is_gpu_label(&sensor.label))
            .map(|sensor| sensor.value);
        let generic_gpu_power = power
            .iter()
            .find(|sensor| is_gpu_label(&sensor.label))
            .map(|sensor| sensor.value);
        if generic_gpu_temperature.is_some() || generic_gpu_power.is_some() {
            let gpu = gpu.get_or_insert_with(GpuSnapshot::default);
            gpu.temperature_celsius = gpu.temperature_celsius.or(generic_gpu_temperature);
            gpu.power_watts = gpu.power_watts.or(generic_gpu_power);
        }
        if gpu.as_ref().is_some_and(|gpu| !gpu.available()) {
            gpu = None;
        }

        SystemSnapshot {
            ready: true,
            cpu_percent,
            cpu_frequency_mhz,
            load_average,
            running_processes,
            total_processes,
            uptime_seconds,
            memory_used,
            memory_total,
            swap_used,
            swap_total,
            gpu,
            network_available,
            network_interfaces,
            download_bytes_per_second,
            upload_bytes_per_second,
            network_received_bytes,
            network_transmitted_bytes,
            disk_used,
            disk_total,
            temperatures,
            power,
            fans,
            battery: read_battery_snapshot(),
        }
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

fn read_disk_usage(path: &Path) -> Option<(u64, u64)> {
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
    let used = total.saturating_sub(available);
    Some((
        used.min(u128::from(u64::MAX)) as u64,
        total.min(u128::from(u64::MAX)) as u64,
    ))
}

fn read_network_counters() -> Option<(NetworkCounters, Vec<String>)> {
    let content = fs::read_to_string("/proc/net/dev").ok()?;
    let default_interfaces = read_default_interfaces();
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
    let selected = if selected.is_empty() {
        candidates
            .iter()
            .filter(|(name, _, _)| network_interface_is_active(name))
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
    let mut interfaces = HashSet::new();
    let Ok(content) = fs::read_to_string("/proc/net/route") else {
        return interfaces;
    };
    for line in content.lines().skip(1) {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() >= 4 && fields[1] == "00000000" {
            interfaces.insert(fields[0].to_owned());
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

fn read_hwmon_snapshot() -> HwmonSnapshot {
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
                let Some(raw) = read_number(path) else {
                    continue;
                };
                let value = raw / 1000.0;
                if !(-40.0..=200.0).contains(&value) {
                    continue;
                }
                let label = sensor_display_label(
                    &device_label,
                    &chip,
                    read_trimmed(directory.join(format!("temp{index}_label"))).as_deref(),
                    SensorKind::Temperature,
                    &index,
                );
                temperatures.push(SensorReading {
                    id: format!("{}:temp{index}", directory.display()),
                    label,
                    value,
                });
            }
            if let Some(index) = sensor_index(name, "fan", "_input") {
                let Some(value) = read_number(path) else {
                    continue;
                };
                if !(0.0..=100_000.0).contains(&value) {
                    continue;
                }
                let label = sensor_display_label(
                    &device_label,
                    &chip,
                    read_trimmed(directory.join(format!("fan{index}_label"))).as_deref(),
                    SensorKind::Fan,
                    &index,
                );
                fans.push(SensorReading {
                    id: format!("{}:fan{index}", directory.display()),
                    label,
                    value,
                });
            }
            if let Some(index) = sensor_index(name, "power", "_average")
                && let Some(value) = read_number(path).map(|value| value / 1_000_000.0)
                && (0.0..=10_000.0).contains(&value)
            {
                power_indices.insert(index.clone());
                power.push(SensorReading {
                    id: format!("{}:power{index}", directory.display()),
                    label: sensor_display_label(
                        &device_label,
                        &chip,
                        read_trimmed(directory.join(format!("power{index}_label"))).as_deref(),
                        SensorKind::Power,
                        &index,
                    ),
                    value,
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
            if power_indices.contains(&index) {
                continue;
            }
            if let Some(value) = read_number(path).map(|value| value / 1_000_000.0)
                && (0.0..=10_000.0).contains(&value)
            {
                power.push(SensorReading {
                    id: format!("{}:power{index}", directory.display()),
                    label: sensor_display_label(
                        &device_label,
                        &chip,
                        read_trimmed(directory.join(format!("power{index}_label"))).as_deref(),
                        SensorKind::Power,
                        &index,
                    ),
                    value,
                });
            }
        }
    }

    append_thermal_zone_temperatures(&mut temperatures);
    temperatures.sort_by_key(|sensor| temperature_priority(&sensor.label));
    power.sort_by_key(|sensor| sensor_priority(&sensor.label));
    fans.sort_by_key(|sensor| sensor_priority(&sensor.label));
    temperatures.truncate(12);
    power.truncate(8);
    fans.truncate(8);
    uniquify_sensor_labels(&mut temperatures);
    uniquify_sensor_labels(&mut power);
    uniquify_sensor_labels(&mut fans);

    HwmonSnapshot {
        temperatures,
        power,
        fans,
    }
}

fn append_thermal_zone_temperatures(temperatures: &mut Vec<SensorReading>) {
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
        let Some(raw) = read_number(directory.join("temp")) else {
            continue;
        };
        let value = raw / 1000.0;
        if !(-40.0..=200.0).contains(&value) {
            continue;
        }
        let kind = read_trimmed(directory.join("type")).unwrap_or_else(|| name.to_owned());
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
            id: format!("{}:temp", directory.display()),
            label,
            value,
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

fn read_drm_gpu_snapshot() -> Option<GpuSnapshot> {
    let mut combined = None::<GpuSnapshot>;
    for card in sorted_directory_paths(Path::new("/sys/class/drm")) {
        let Some(name) = card.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(index) = name.strip_prefix("card") else {
            continue;
        };
        if index.is_empty() || !index.chars().all(|character| character.is_ascii_digit()) {
            continue;
        }
        let device = card.join("device");
        if !device.exists() {
            continue;
        }
        if device_is_nvidia(&device) || device_runtime_suspended(&device) {
            continue;
        }

        let utilization_percent = ["gpu_busy_percent", "gt_busy_percent"]
            .into_iter()
            .find_map(|file| read_number(device.join(file)))
            .filter(|value| (0.0..=100.0).contains(value));
        let memory_used = read_u64(device.join("mem_info_vram_used"));
        let memory_total = read_u64(device.join("mem_info_vram_total"));
        let snapshot = GpuSnapshot {
            utilization_percent,
            memory_used,
            memory_total,
            power_watts: None,
            temperature_celsius: None,
        };
        if snapshot.available() {
            merge_gpu_snapshot(&mut combined, snapshot);
        }
    }
    combined.filter(GpuSnapshot::available)
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

fn merge_gpu_snapshot(target: &mut Option<GpuSnapshot>, source: GpuSnapshot) {
    let target = target.get_or_insert_with(GpuSnapshot::default);
    target.utilization_percent = target.utilization_percent.or(source.utilization_percent);
    target.memory_used = target.memory_used.or(source.memory_used);
    target.memory_total = target.memory_total.or(source.memory_total);
    target.power_watts = target.power_watts.or(source.power_watts);
    target.temperature_celsius = target.temperature_celsius.or(source.temperature_celsius);
}

fn read_battery_snapshot() -> Option<BatterySnapshot> {
    let mut percentages = Vec::new();
    let mut powers = Vec::new();
    let mut statuses = Vec::new();
    for supply in sorted_directory_paths(Path::new("/sys/class/power_supply")) {
        if read_trimmed(supply.join("type")).as_deref() != Some("Battery") {
            continue;
        }
        let percent = read_number(supply.join("capacity")).or_else(|| {
            let now = read_number(supply.join("energy_now"))?;
            let full = read_number(supply.join("energy_full"))?;
            (full > 0.0).then_some(now * 100.0 / full)
        });
        if let Some(percent) = percent.filter(|percent| (0.0..=100.0).contains(percent)) {
            percentages.push(percent);
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
    if percentages.is_empty() {
        return None;
    }

    let percent = percentages.iter().sum::<f64>() / percentages.len() as f64;
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
    fn section_order_is_deduplicated_and_completed() {
        let hidden = HashSet::from([MonitorSection::Network]);
        let sections = normalized_sections(
            vec![
                MonitorSection::Storage,
                MonitorSection::Performance,
                MonitorSection::Storage,
            ],
            &hidden,
        );
        assert_eq!(sections.len(), MonitorSection::ALL.len());
        assert_eq!(sections[0].section, MonitorSection::Storage);
        assert_eq!(sections[1].section, MonitorSection::Performance);
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
        };
        assert_eq!(geometry.clamp_y(-100, 480), PANEL_EDGE_MARGIN);
        assert_eq!(geometry.clamp_y(500, 480), 232);
        assert_eq!(geometry.clamp_y(500, 120), 500);
        assert_eq!(geometry.clamp_y(500, 900), PANEL_EDGE_MARGIN);
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
