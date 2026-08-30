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

use super::{PopupReveal, clear_box, run_background};

const SETTINGS_GROUP: &str = "monitor";
const SETTINGS_FILE: &str = "system-monitor.ini";
const CONTENT_NAMESPACE: &str = "obsidian-system-monitor";
const TRIGGER_NAMESPACE: &str = "obsidian-system-monitor-settings-trigger";
const HANDLE_NAMESPACE: &str = "obsidian-system-monitor-drag-handle";
const HEIGHT_HANDLE_NAMESPACE: &str = "obsidian-system-monitor-height-handle";
const SETTINGS_NAMESPACE: &str = "obsidian-system-monitor-settings";
const CONTENT_WIDGET_NAME: &str = "obsidian-system-monitor-content";
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const NETWORK_HISTORY_LENGTH: usize = 64;
const PANEL_MIN_WIDTH: i32 = 220;
const PANEL_MAX_WIDTH: i32 = 280;
const PANEL_MIN_HEIGHT: i32 = 380;
const PANEL_MAX_HEIGHT: i32 = 620;
const PANEL_EDGE_MARGIN: i32 = 8;
const SETTINGS_TRIGGER_SIZE: i32 = 36;
const DRAG_HANDLE_WIDTH: i32 = 18;
const HEIGHT_HANDLE_SIZE: i32 = 18;
const RESIZE_CORNER_SIZE: i32 = 18;
const SETTINGS_WINDOW_WIDTH: i32 = 330;
const SETTINGS_WINDOW_HEIGHT: i32 = 510;
const SCALE_MILLI_DEFAULT: i32 = 1_000;
const SCALE_MILLI_MIN: i32 = 700;
const SCALE_MILLI_MAX: i32 = 3_000;

const ICON_SETTINGS: &str = "\u{f0493}";
const ICON_MONITOR: &str = "\u{f0379}";
const ICON_UP: &str = "\u{f005d}";
const ICON_DOWN: &str = "\u{f0045}";
const ICON_VISIBLE: &str = "\u{f0208}";
const ICON_HIDDEN: &str = "\u{f0209}";

fn scaled_pixels(base: i32, scale_milli: i32) -> i32 {
    (base.saturating_mul(scale_milli) / SCALE_MILLI_DEFAULT).max(1)
}

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
    position_x: Option<i32>,
    position_y: Option<i32>,
    scale_milli: i32,
    panel_width: Option<i32>,
    legacy_width: Option<i32>,
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
            position_x: None,
            position_y: None,
            scale_milli: SCALE_MILLI_DEFAULT,
            panel_width: None,
            legacy_width: None,
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
        let position_x = key_file.integer(SETTINGS_GROUP, "position_x").ok();
        let position_y = key_file.integer(SETTINGS_GROUP, "position_y").ok();
        let saved_scale = key_file.integer(SETTINGS_GROUP, "scale").ok();
        let scale_milli = saved_scale
            .unwrap_or(defaults.scale_milli)
            .clamp(SCALE_MILLI_MIN, SCALE_MILLI_MAX);
        let panel_width = key_file.integer(SETTINGS_GROUP, "panel_width").ok();
        let legacy_width = saved_scale
            .is_none()
            .then(|| key_file.integer(SETTINGS_GROUP, "width").ok())
            .flatten();

        Self {
            enabled,
            sections: normalized_sections(order, &hidden),
            position_x,
            position_y,
            scale_milli,
            panel_width,
            legacy_width,
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
        if let Some(position_x) = self.position_x {
            key_file.set_integer(SETTINGS_GROUP, "position_x", position_x);
        }
        if let Some(position_y) = self.position_y {
            key_file.set_integer(SETTINGS_GROUP, "position_y", position_y);
        }
        if let Some(panel_width) = self.panel_width {
            key_file.set_integer(SETTINGS_GROUP, "panel_width", panel_width);
        }
        if let Some(width) = self.legacy_width {
            key_file.set_integer(SETTINGS_GROUP, "width", width);
        } else {
            key_file.set_integer(SETTINGS_GROUP, "scale", self.scale_milli);
        }

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

    fn set_position(&self, x: i32, y: i32) -> bool {
        self.update_settings(|settings| {
            settings.position_x = Some(x);
            settings.position_y = Some(y);
        })
    }

    fn set_dimensions(&self, scale_milli: i32, panel_width: i32) -> bool {
        self.update_settings(|settings| {
            settings.scale_milli = scale_milli;
            settings.panel_width = Some(panel_width);
            settings.legacy_width = None;
        })
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

struct MonitorScaleStyle {
    provider: gtk::CssProvider,
    display: gdk::Display,
}

impl MonitorScaleStyle {
    fn new(display: &gdk::Display) -> Self {
        let provider = gtk::CssProvider::new();
        provider.connect_parsing_error(|_, _, error| {
            warn!(%error, "system monitor scale css parsing error");
        });
        gtk::style_context_add_provider_for_display(
            display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
        );
        Self {
            provider,
            display: display.clone(),
        }
    }

    fn apply(&self, scale_milli: i32) {
        let scale = f64::from(scale_milli) / f64::from(SCALE_MILLI_DEFAULT);
        self.provider.load_from_data(&format!(
            "#{CONTENT_WIDGET_NAME}.system-monitor-surface, window.system-monitor-trigger-window, window.system-monitor-handle-window, window.system-monitor-height-handle-window {{ font-size: {:.2}px; }}",
            11.0 * scale
        ));
    }
}

impl Drop for MonitorScaleStyle {
    fn drop(&mut self) {
        gtk::style_context_remove_provider_for_display(&self.display, &self.provider);
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

    fn apply_scale(&self, scale_milli: i32) {
        self.area.set_content_height(scaled_pixels(3, scale_milli));
    }
}

struct SectionView {
    root: gtk::Box,
    rows: gtk::Box,
    signature: RefCell<Vec<(String, String)>>,
    value_labels: RefCell<Vec<gtk::Label>>,
    row_boxes: RefCell<Vec<gtk::Box>>,
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
            row_boxes: RefCell::new(Vec::new()),
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
            let mut row_boxes = self.row_boxes.borrow_mut();
            value_labels.clear();
            row_boxes.clear();
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
                row_boxes.push(row);
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

    fn push_network_sample(&self, download: f64, upload: f64) {
        let Some(state) = self.graph_state.as_ref() else {
            return;
        };
        state.borrow_mut().push(download, upload);
        if let Some(graph) = self.graph.as_ref() {
            graph.queue_draw();
        }
    }

    fn apply_scale(&self, scale_milli: i32) {
        self.scale_milli.set(scale_milli);
        self.root.set_spacing(scaled_pixels(3, scale_milli));
        self.rows.set_spacing(scaled_pixels(1, scale_milli));
        for row in self.row_boxes.borrow().iter() {
            row.set_spacing(scaled_pixels(6, scale_milli));
        }
        if let Some(graph) = self.graph.as_ref() {
            graph.set_content_height(scaled_pixels(72, scale_milli));
        }
        if let Some(meter) = self.meter.as_ref() {
            meter.apply_scale(scale_milli);
        }
    }
}

struct MonitorLayout {
    root: gtk::Box,
    sections: HashMap<MonitorSection, Rc<SectionView>>,
    empty_state: gtk::Box,
    empty_title: gtk::Label,
    empty_hint: gtk::Label,
    scale_style: MonitorScaleStyle,
    applied_scale: Cell<i32>,
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
        root.set_widget_name(CONTENT_WIDGET_NAME);
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
        let layout = Rc::new(Self {
            root,
            sections,
            empty_state,
            empty_title,
            empty_hint,
            scale_style: MonitorScaleStyle::new(display),
            applied_scale: Cell::new(0),
            settings: RefCell::new(settings.clone()),
            snapshot: RefCell::new(snapshot.clone()),
        });
        layout.apply_settings(&settings);
        layout.update_snapshot(&snapshot);
        layout
    }

    fn apply_settings(&self, settings: &MonitorSettings) {
        self.settings.replace(settings.clone());
        self.apply_scale(settings.scale_milli);
        clear_box(&self.root);
        for preference in &settings.sections {
            if let Some(section) = self.sections.get(&preference.section) {
                self.root.append(&section.root);
            }
        }
        self.root.append(&self.empty_state);
        self.refresh_visibility();
    }

    fn apply_scale(&self, scale_milli: i32) {
        if self.applied_scale.replace(scale_milli) == scale_milli {
            return;
        }
        self.scale_style.apply(scale_milli);
        self.root.set_spacing(scaled_pixels(7, scale_milli));
        self.empty_state.set_spacing(scaled_pixels(3, scale_milli));
        for section in self.sections.values() {
            section.apply_scale(scale_milli);
        }
    }

    fn natural_height(&self, width: i32) -> i32 {
        let (_, natural, _, _) = self.root.measure(gtk::Orientation::Vertical, width.max(1));
        natural.max(1)
    }

    fn update_snapshot(&self, snapshot: &SystemSnapshot) {
        self.snapshot.replace(snapshot.clone());
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
    panel_height: i32,
}

impl PanelGeometry {
    fn for_monitor(monitor: &gdk::Monitor) -> Self {
        let geometry = monitor.geometry();
        let screen_width = geometry.width().max(1);
        let screen_height = geometry.height().max(1);
        let panel_width = ((f64::from(screen_width) * 0.18).round() as i32)
            .clamp(PANEL_MIN_WIDTH, PANEL_MAX_WIDTH)
            .min((screen_width - PANEL_EDGE_MARGIN * 2).max(1));
        let panel_height = ((f64::from(screen_height) * 0.68).round() as i32)
            .clamp(PANEL_MIN_HEIGHT, PANEL_MAX_HEIGHT)
            .min((screen_height - PANEL_EDGE_MARGIN * 2).max(1));
        Self {
            screen_width,
            screen_height,
            panel_width,
            panel_height,
        }
    }

    fn resolve_scale(self, settings: &MonitorSettings) -> i32 {
        let scale_milli = settings
            .legacy_width
            .map(|width| width.saturating_mul(SCALE_MILLI_DEFAULT) / self.panel_width)
            .unwrap_or(settings.scale_milli);
        self.clamp_scale(scale_milli)
    }

    fn clamp_scale(self, scale_milli: i32) -> i32 {
        let available_width = (self.screen_width - PANEL_EDGE_MARGIN * 2).max(1);
        let width_limit = available_width.saturating_mul(SCALE_MILLI_DEFAULT) / self.panel_width;
        let minimum = SCALE_MILLI_MIN.min(width_limit);
        let maximum = SCALE_MILLI_MAX.min(width_limit).max(minimum);
        scale_milli.clamp(minimum, maximum)
    }

    fn width_for_scale(self, scale_milli: i32) -> i32 {
        (self.panel_width.saturating_mul(scale_milli) / SCALE_MILLI_DEFAULT).max(1)
    }

    fn resolve_width(self, settings: &MonitorSettings, scale_milli: i32) -> i32 {
        let maximum = (self.screen_width - PANEL_EDGE_MARGIN * 2).max(1);
        let minimum = scaled_pixels(PANEL_MIN_WIDTH, scale_milli).min(maximum);
        settings
            .panel_width
            .unwrap_or_else(|| self.width_for_scale(scale_milli))
            .clamp(minimum, maximum)
    }

    fn height_for_scale(self, scale_milli: i32) -> i32 {
        (self.panel_height.saturating_mul(scale_milli) / SCALE_MILLI_DEFAULT).max(1)
    }

    fn resolve(self, settings: &MonitorSettings, width: i32, height: i32) -> (i32, i32) {
        let default_x = ((f64::from(self.screen_width) * 0.025).round() as i32).max(18);
        let default_y = ((f64::from(self.screen_height) * 0.26).round() as i32).max(58);
        self.clamp(
            settings.position_x.unwrap_or(default_x),
            settings.position_y.unwrap_or(default_y),
            width,
            height,
        )
    }

    fn clamp(self, x: i32, y: i32, width: i32, height: i32) -> (i32, i32) {
        let max_x = (self.screen_width - width - PANEL_EDGE_MARGIN).max(PANEL_EDGE_MARGIN);
        let max_y = (self.screen_height - height - PANEL_EDGE_MARGIN).max(PANEL_EDGE_MARGIN);
        (
            x.clamp(PANEL_EDGE_MARGIN, max_x),
            y.clamp(PANEL_EDGE_MARGIN, max_y),
        )
    }
}

struct PlacementRuntime {
    x: Cell<i32>,
    y: Cell<i32>,
    width: Cell<i32>,
    height: Cell<i32>,
    scale_milli: Cell<i32>,
    drag_start_x: Cell<i32>,
    drag_start_y: Cell<i32>,
    resize_start_width: Cell<i32>,
    resize_start_height: Cell<i32>,
    resize_start_scale: Cell<i32>,
    geometry: PanelGeometry,
}

impl PlacementRuntime {
    fn new(geometry: PanelGeometry, settings: &MonitorSettings) -> Rc<Self> {
        let scale_milli = geometry.resolve_scale(settings);
        let width = geometry.resolve_width(settings, scale_milli);
        let height = geometry.height_for_scale(scale_milli);
        let (x, y) = geometry.resolve(settings, width, height);
        Rc::new(Self {
            x: Cell::new(x),
            y: Cell::new(y),
            width: Cell::new(width),
            height: Cell::new(height),
            scale_milli: Cell::new(scale_milli),
            drag_start_x: Cell::new(x),
            drag_start_y: Cell::new(y),
            resize_start_width: Cell::new(width),
            resize_start_height: Cell::new(height),
            resize_start_scale: Cell::new(scale_milli),
            geometry,
        })
    }

    fn set(&self, x: i32, y: i32) -> (i32, i32) {
        let (x, y) = self
            .geometry
            .clamp(x, y, self.width.get(), self.height.get());
        self.x.set(x);
        self.y.set(y);
        (x, y)
    }

    fn begin_resize(&self, measured_width: i32, measured_height: i32) {
        self.resize_start_width.set(measured_width.max(1));
        self.resize_start_height.set(measured_height.max(1));
        self.resize_start_scale.set(self.scale_milli.get());
        self.width.set(measured_width.max(1));
        self.height.set(measured_height.max(1));
    }

    fn begin_width_resize(&self, measured_width: i32) {
        self.resize_start_width.set(measured_width.max(1));
        self.width.set(measured_width.max(1));
    }

    fn resize_width(&self, offset_x: f64) {
        let available = (self.geometry.screen_width - self.x.get() - PANEL_EDGE_MARGIN).max(1);
        let minimum = scaled_pixels(PANEL_MIN_WIDTH, self.scale_milli.get()).min(available);
        let requested = self
            .resize_start_width
            .get()
            .saturating_add(offset_x.round() as i32);
        self.width.set(requested.clamp(minimum, available));
    }

    fn resize(&self, offset_x: f64, offset_y: f64) {
        let start_width = f64::from(self.resize_start_width.get().max(1));
        let start_height = f64::from(self.resize_start_height.get().max(1));
        let projection = (offset_x * start_width + offset_y * start_height)
            / (start_width.mul_add(start_width, start_height * start_height));
        let requested =
            (f64::from(self.resize_start_scale.get()) * (1.0 + projection)).round() as i32;

        let available_width =
            (self.geometry.screen_width - self.x.get() - PANEL_EDGE_MARGIN).max(1);
        let available_height =
            (self.geometry.screen_height - self.y.get() - PANEL_EDGE_MARGIN).max(1);
        let maximum_from_width = self
            .resize_start_scale
            .get()
            .saturating_mul(available_width)
            / self.resize_start_width.get().max(1);
        let maximum_from_height = self
            .resize_start_scale
            .get()
            .saturating_mul(available_height)
            / self.resize_start_height.get().max(1);
        let maximum = SCALE_MILLI_MAX
            .min(maximum_from_width)
            .min(maximum_from_height)
            .max(SCALE_MILLI_MIN);
        let scale_milli = requested.clamp(SCALE_MILLI_MIN.min(maximum), maximum);
        let factor = f64::from(scale_milli) / f64::from(self.resize_start_scale.get().max(1));

        self.scale_milli.set(scale_milli);
        self.width
            .set((start_width * factor).round().max(1.0) as i32);
        self.height
            .set((start_height * factor).round().max(1.0) as i32);
    }

    fn resize_height(&self, offset_y: f64) {
        let start_width = self.resize_start_width.get().max(1);
        let start_height = self.resize_start_height.get().max(1);
        let start_scale = self.resize_start_scale.get().max(1);
        let requested_height = start_height.saturating_add(offset_y.round() as i32).max(1);
        let requested_scale = start_scale.saturating_mul(requested_height) / start_height;
        let available_height =
            (self.geometry.screen_height - self.y.get() - PANEL_EDGE_MARGIN).max(1);
        let maximum_from_height = start_scale.saturating_mul(available_height) / start_height;
        let maximum = SCALE_MILLI_MAX
            .min(maximum_from_height)
            .max(SCALE_MILLI_MIN);
        let scale_milli = requested_scale.clamp(SCALE_MILLI_MIN.min(maximum), maximum);

        self.scale_milli.set(scale_milli);
        self.width.set(start_width);
        self.height
            .set(start_height.saturating_mul(scale_milli) / start_scale);
    }

    fn set_measured_height(&self, height: i32) {
        if height > 1 {
            self.height.set(height);
        }
    }

    fn apply_settings(&self, settings: &MonitorSettings) {
        let old_scale = self.scale_milli.get().max(1);
        let scale_milli = self.geometry.resolve_scale(settings);
        let width = self.geometry.resolve_width(settings, scale_milli);
        let height = self.height.get().saturating_mul(scale_milli) / old_scale;
        let (x, y) = self.geometry.resolve(settings, width, height);
        self.scale_milli.set(scale_milli);
        self.width.set(width);
        self.height.set(height.max(1));
        self.x.set(x);
        self.y.set(y);
    }
}

#[derive(Default)]
struct PointerDragState {
    active: Cell<bool>,
    offset_x: Cell<f64>,
    offset_y: Cell<f64>,
}

impl PointerDragState {
    fn begin(&self) {
        self.offset_x.set(0.0);
        self.offset_y.set(0.0);
        self.active.set(true);
    }

    fn update(&self, offset_x: f64, offset_y: f64) {
        self.offset_x.set(offset_x);
        self.offset_y.set(offset_y);
    }

    fn end(&self, offset_x: f64, offset_y: f64) {
        self.update(offset_x, offset_y);
        self.active.set(false);
    }
}

struct MonitorSettingsPanel {
    root: gtk::Box,
    list: gtk::Box,
    rebuild_pending: Cell<bool>,
    available_sections: Cell<u8>,
    controller: Rc<SystemMonitorController>,
}

impl MonitorSettingsPanel {
    fn new(controller: &Rc<SystemMonitorController>) -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
        root.add_css_class("system-monitor-settings-panel");
        root.set_size_request(SETTINGS_WINDOW_WIDTH, -1);

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

        let section_title = gtk::Label::new(Some("Visible sections and order"));
        section_title.add_css_class("system-monitor-settings-section-title");
        section_title.set_xalign(0.0);

        let list = gtk::Box::new(gtk::Orientation::Vertical, 4);
        list.add_css_class("system-monitor-settings-list");

        let footer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        footer.add_css_class("system-monitor-settings-footer");
        let hint = gtk::Label::new(Some(
            "Left-drag the right edge to move. Right-drag the right or bottom edge to resize one axis; use the corner to scale everything.",
        ));
        hint.add_css_class("system-monitor-settings-hint");
        hint.set_wrap(true);
        hint.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        hint.set_xalign(0.0);
        hint.set_hexpand(true);

        footer.append(&hint);

        root.append(&header);
        root.append(&section_title);
        root.append(&list);
        root.append(&footer);

        let panel = Rc::new(Self {
            root,
            list,
            rebuild_pending: Cell::new(false),
            available_sections: Cell::new(available_section_mask(&controller.latest())),
            controller: Rc::clone(controller),
        });

        {
            let weak = Rc::downgrade(&panel);
            controller.subscribe_settings(move |settings| {
                let Some(panel) = weak.upgrade() else {
                    return false;
                };
                let _ = settings;
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

pub struct SystemMonitorView {
    monitor: gdk::Monitor,
    content_window: gtk::ApplicationWindow,
    trigger_window: gtk::ApplicationWindow,
    handle_window: gtk::ApplicationWindow,
    height_handle_window: gtk::ApplicationWindow,
    settings_window: gtk::ApplicationWindow,
    _layout: Rc<MonitorLayout>,
    _settings_panel: Rc<MonitorSettingsPanel>,
}

impl SystemMonitorView {
    pub fn new(
        application: &gtk::Application,
        monitor: &gdk::Monitor,
        controller: &Rc<SystemMonitorController>,
    ) -> Self {
        let geometry = PanelGeometry::for_monitor(monitor);
        let settings = controller.settings();
        let placement = PlacementRuntime::new(geometry, &settings);

        let layout = MonitorLayout::new(&monitor.display(), settings.clone(), controller.latest());
        layout.root.set_size_request(placement.width.get(), -1);
        placement.set_measured_height(layout.natural_height(placement.width.get()));
        placement.set(placement.x.get(), placement.y.get());

        let content_window = desktop_window(
            application,
            monitor,
            CONTENT_NAMESPACE,
            "system-monitor-window",
            false,
        );
        content_window.set_default_size(placement.width.get(), -1);
        content_window.set_can_target(false);
        content_window.set_child(Some(&layout.root));

        let trigger_window = desktop_window(
            application,
            monitor,
            TRIGGER_NAMESPACE,
            "system-monitor-trigger-window",
            true,
        );
        trigger_window.set_default_size(SETTINGS_TRIGGER_SIZE, SETTINGS_TRIGGER_SIZE);
        let trigger = gtk::Button::new();
        trigger.add_css_class("system-monitor-settings-trigger");
        trigger.set_tooltip_text(Some("Configure system monitor"));
        let trigger_icon = gtk::Label::new(Some(ICON_SETTINGS));
        trigger_icon.add_css_class("system-monitor-settings-trigger-icon");
        trigger.set_child(Some(&trigger_icon));
        trigger_window.set_child(Some(&trigger));

        let handle_window = desktop_window(
            application,
            monitor,
            HANDLE_NAMESPACE,
            "system-monitor-handle-window",
            true,
        );
        handle_window.set_default_size(DRAG_HANDLE_WIDTH, geometry.panel_height);
        let handle = gtk::Overlay::new();
        handle.add_css_class("system-monitor-drag-handle");
        handle.set_tooltip_text(Some("Drag the right edge to move the monitor"));
        handle.set_cursor_from_name(Some("grab"));
        let width_resize_zone = gtk::Box::new(gtk::Orientation::Vertical, 0);
        width_resize_zone.add_css_class("system-monitor-width-resize-zone");
        width_resize_zone.set_halign(gtk::Align::Fill);
        width_resize_zone.set_valign(gtk::Align::Fill);
        width_resize_zone.set_margin_bottom(RESIZE_CORNER_SIZE);
        width_resize_zone.set_cursor_from_name(Some("ew-resize"));
        width_resize_zone.set_tooltip_text(Some("Right-drag to change only the monitor width"));
        handle.add_overlay(&width_resize_zone);
        let resize_corner = gtk::Box::new(gtk::Orientation::Vertical, 0);
        resize_corner.add_css_class("system-monitor-resize-corner");
        resize_corner.set_size_request(RESIZE_CORNER_SIZE, RESIZE_CORNER_SIZE);
        resize_corner.set_halign(gtk::Align::End);
        resize_corner.set_valign(gtk::Align::End);
        resize_corner.set_cursor_from_name(Some("nwse-resize"));
        resize_corner.set_tooltip_text(Some("Right-drag the corner to scale the whole monitor"));
        handle.add_overlay(&resize_corner);
        handle_window.set_child(Some(&handle));

        let height_handle_window = desktop_window(
            application,
            monitor,
            HEIGHT_HANDLE_NAMESPACE,
            "system-monitor-height-handle-window",
            true,
        );
        height_handle_window.set_default_size(geometry.panel_width, HEIGHT_HANDLE_SIZE);
        let height_handle = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        height_handle.add_css_class("system-monitor-height-handle");
        height_handle.set_cursor_from_name(Some("ns-resize"));
        height_handle.set_tooltip_text(Some(
            "Right-drag to change the monitor height without changing its width",
        ));
        height_handle_window.set_child(Some(&height_handle));

        let settings_window = gtk::ApplicationWindow::builder()
            .application(application)
            .decorated(false)
            .resizable(false)
            .build();
        settings_window.add_css_class("system-monitor-settings-window");
        settings_window.init_layer_shell();
        settings_window.set_namespace(Some(SETTINGS_NAMESPACE));
        settings_window.set_layer(Layer::Top);
        settings_window.set_keyboard_mode(KeyboardMode::OnDemand);
        settings_window.set_monitor(Some(monitor));
        settings_window.set_anchor(Edge::Top, true);
        settings_window.set_anchor(Edge::Left, true);
        settings_window.set_anchor(Edge::Right, false);
        settings_window.set_anchor(Edge::Bottom, false);
        settings_window.set_exclusive_zone(-1);
        settings_window.set_hide_on_close(true);
        settings_window.set_default_size(SETTINGS_WINDOW_WIDTH, SETTINGS_WINDOW_HEIGHT);

        let settings_panel = MonitorSettingsPanel::new(controller);
        let settings_surface = gtk::Box::new(gtk::Orientation::Vertical, 0);
        settings_surface.add_css_class("system-monitor-settings-surface");
        settings_surface.append(&settings_panel.root);
        let settings_reveal = PopupReveal::masked(settings_surface.upcast::<gtk::Widget>());
        let settings_root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        settings_root.add_css_class("widget-popup-root");
        settings_root.set_focusable(true);
        settings_root.append(settings_reveal.widget());
        settings_window.set_child(Some(&settings_root));

        let settings_focus_armed = Rc::new(Cell::new(false));
        let settings_trigger_pressed = Rc::new(Cell::new(false));

        {
            let pressed = Rc::clone(&settings_trigger_pressed);
            let click = gtk::GestureClick::new();
            click.set_button(gdk::BUTTON_PRIMARY);
            click.set_propagation_phase(gtk::PropagationPhase::Capture);
            click.connect_pressed(move |_, _, _, _| {
                pressed.set(true);
                let pressed = Rc::clone(&pressed);
                glib::timeout_add_local_once(Duration::from_millis(150), move || {
                    pressed.set(false);
                });
            });
            trigger.add_controller(click);
        }

        apply_monitor_size(
            &placement,
            &layout,
            &content_window,
            &trigger_window,
            &handle_window,
            &height_handle_window,
        );
        apply_monitor_placement(
            &placement,
            &content_window,
            &trigger_window,
            &handle_window,
            &height_handle_window,
        );

        let move_state = Rc::new(PointerDragState::default());
        let width_state = Rc::new(PointerDragState::default());
        let height_state = Rc::new(PointerDragState::default());
        let resize_state = Rc::new(PointerDragState::default());

        {
            let placement = Rc::clone(&placement);
            let move_state = Rc::clone(&move_state);
            let width_state = Rc::clone(&width_state);
            let height_state = Rc::clone(&height_state);
            let resize_state = Rc::clone(&resize_state);
            let weak_layout = Rc::downgrade(&layout);
            let weak_content = content_window.downgrade();
            let weak_trigger = trigger_window.downgrade();
            let weak_handle = handle_window.downgrade();
            let weak_height_handle = height_handle_window.downgrade();
            controller.subscribe_snapshot(move |snapshot| {
                let (
                    Some(layout),
                    Some(content),
                    Some(trigger),
                    Some(handle),
                    Some(height_handle),
                ) = (
                    weak_layout.upgrade(),
                    weak_content.upgrade(),
                    weak_trigger.upgrade(),
                    weak_handle.upgrade(),
                    weak_height_handle.upgrade(),
                ) else {
                    return false;
                };
                layout.update_snapshot(snapshot);
                if move_state.active.get()
                    || width_state.active.get()
                    || height_state.active.get()
                    || resize_state.active.get()
                {
                    apply_monitor_content_size(&placement, &layout, &content, &trigger);
                    apply_monitor_content_placement(&placement, &content, &trigger);
                } else {
                    apply_monitor_size(
                        &placement,
                        &layout,
                        &content,
                        &trigger,
                        &handle,
                        &height_handle,
                    );
                    apply_monitor_placement(
                        &placement,
                        &content,
                        &trigger,
                        &handle,
                        &height_handle,
                    );
                }
                true
            });
        }
        {
            let weak_layout = Rc::downgrade(&layout);
            controller.subscribe_settings(move |settings| {
                let Some(layout) = weak_layout.upgrade() else {
                    return false;
                };
                layout.apply_settings(settings);
                true
            });
        }
        {
            let placement = Rc::clone(&placement);
            let weak_layout = Rc::downgrade(&layout);
            let weak_content = content_window.downgrade();
            let weak_trigger = trigger_window.downgrade();
            let weak_handle = handle_window.downgrade();
            let weak_height_handle = height_handle_window.downgrade();
            controller.subscribe_settings(move |settings| {
                let (
                    Some(layout),
                    Some(content),
                    Some(trigger),
                    Some(handle),
                    Some(height_handle),
                ) = (
                    weak_layout.upgrade(),
                    weak_content.upgrade(),
                    weak_trigger.upgrade(),
                    weak_handle.upgrade(),
                    weak_height_handle.upgrade(),
                ) else {
                    return false;
                };
                placement.apply_settings(settings);
                apply_monitor_size(
                    &placement,
                    &layout,
                    &content,
                    &trigger,
                    &handle,
                    &height_handle,
                );
                apply_monitor_placement(
                    &placement,
                    &content,
                    &trigger,
                    &handle,
                    &height_handle,
                );
                true
            });
        }

        {
            let placement = Rc::clone(&placement);
            let weak_settings = settings_window.downgrade();
            let weak_settings_root = settings_root.downgrade();
            let settings_panel = Rc::downgrade(&settings_panel);
            let focus_armed = Rc::clone(&settings_focus_armed);
            let reveal = settings_reveal.clone();
            trigger.connect_clicked(move |_| {
                let Some(window) = weak_settings.upgrade() else {
                    return;
                };
                if reveal.is_revealed() {
                    focus_armed.set(false);
                    reveal.hide(&window);
                    return;
                }
                if let Some(panel) = settings_panel.upgrade() {
                    panel.rebuild();
                }
                let (left, top) = settings_popup_position(&placement);
                window.set_margin(Edge::Left, left);
                window.set_margin(Edge::Top, top);
                reveal.sync_top_anchor(&window);
                focus_armed.set(false);
                let generation = reveal.show(&window);
                let weak_window = window.downgrade();
                let weak_settings_root = weak_settings_root.clone();
                let reveal = reveal.clone();
                glib::idle_add_local_once(move || {
                    if reveal.is_current(generation)
                        && weak_window
                            .upgrade()
                            .is_some_and(|window| window.is_visible())
                        && let Some(root) = weak_settings_root.upgrade()
                    {
                        root.grab_focus();
                    }
                });
            });
        }

        {
            let focus_armed = Rc::clone(&settings_focus_armed);
            let reveal = settings_reveal.clone();
            settings_window.connect_visible_notify(move |window| {
                if !window.is_visible() {
                    focus_armed.set(false);
                    reveal.reset_hidden();
                }
            });
        }

        {
            let weak_window = settings_window.downgrade();
            let focus_armed = Rc::clone(&settings_focus_armed);
            let reveal = settings_reveal.clone();
            let key = gtk::EventControllerKey::new();
            key.connect_key_pressed(move |_, key, _, _| {
                if key == gdk::Key::Escape
                    && let Some(window) = weak_window.upgrade()
                {
                    focus_armed.set(false);
                    reveal.hide(&window);
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
            settings_window.add_controller(key);
        }

        {
            let focus_armed = Rc::clone(&settings_focus_armed);
            let trigger_pressed = Rc::clone(&settings_trigger_pressed);
            let reveal = settings_reveal.clone();
            settings_window.connect_is_active_notify(move |window| {
                if window.is_active() {
                    if window.is_visible() && reveal.is_revealed() {
                        focus_armed.set(true);
                    }
                    return;
                }
                if !window.is_visible()
                    || !reveal.is_revealed()
                    || !focus_armed.get()
                    || trigger_pressed.get()
                {
                    return;
                }
                let weak_window = window.downgrade();
                let focus_armed = Rc::clone(&focus_armed);
                let trigger_pressed = Rc::clone(&trigger_pressed);
                let reveal = reveal.clone();
                glib::timeout_add_local_once(Duration::from_millis(60), move || {
                    if let Some(window) = weak_window.upgrade()
                        && window.is_visible()
                        && !window.is_active()
                        && reveal.is_revealed()
                        && focus_armed.get()
                        && !trigger_pressed.get()
                    {
                        focus_armed.set(false);
                        reveal.hide(&window);
                    }
                });
            });
        }

        let drag = gtk::GestureDrag::new();
        drag.set_button(gdk::BUTTON_PRIMARY);
        {
            let placement = Rc::clone(&placement);
            let move_state = Rc::clone(&move_state);
            let weak_settings = settings_window.downgrade();
            let reveal = settings_reveal.clone();
            let weak_handle = handle.downgrade();
            let weak_content = content_window.downgrade();
            let weak_trigger = trigger_window.downgrade();
            drag.connect_drag_begin(move |_, _, _| {
                placement.drag_start_x.set(placement.x.get());
                placement.drag_start_y.set(placement.y.get());
                move_state.begin();
                if let Some(handle) = weak_handle.upgrade() {
                    handle.add_css_class("system-monitor-dragging");
                    handle.set_cursor_from_name(Some("grabbing"));

                    let move_state = Rc::clone(&move_state);
                    let placement = Rc::clone(&placement);
                    let weak_content = weak_content.clone();
                    let weak_trigger = weak_trigger.clone();
                    handle.add_tick_callback(move |_, _| {
                        if !move_state.active.get() {
                            return glib::ControlFlow::Break;
                        }
                        let (Some(content), Some(trigger)) =
                            (weak_content.upgrade(), weak_trigger.upgrade())
                        else {
                            return glib::ControlFlow::Break;
                        };
                        placement.set(
                            placement.drag_start_x.get() + move_state.offset_x.get().round() as i32,
                            placement.drag_start_y.get() + move_state.offset_y.get().round() as i32,
                        );
                        apply_monitor_content_placement(&placement, &content, &trigger);
                        glib::ControlFlow::Continue
                    });
                }
                if let Some(window) = weak_settings.upgrade() {
                    reveal.hide(&window);
                }
            });
        }
        {
            let move_state = Rc::clone(&move_state);
            drag.connect_drag_update(move |_, offset_x, offset_y| {
                move_state.update(offset_x, offset_y);
            });
        }
        {
            let placement = Rc::clone(&placement);
            let move_state = Rc::clone(&move_state);
            let controller = Rc::clone(controller);
            let weak_content = content_window.downgrade();
            let weak_trigger = trigger_window.downgrade();
            let weak_handle_window = handle_window.downgrade();
            let weak_height_handle_window = height_handle_window.downgrade();
            let weak_handle = handle.downgrade();
            drag.connect_drag_end(move |_, offset_x, offset_y| {
                move_state.end(offset_x, offset_y);
                placement.set(
                    placement.drag_start_x.get() + offset_x.round() as i32,
                    placement.drag_start_y.get() + offset_y.round() as i32,
                );
                if let (Some(content), Some(trigger)) =
                    (weak_content.upgrade(), weak_trigger.upgrade())
                {
                    apply_monitor_content_placement(&placement, &content, &trigger);
                }
                if let Some(handle) = weak_handle.upgrade() {
                    handle.remove_css_class("system-monitor-dragging");
                    handle.set_cursor_from_name(Some("grab"));
                }
                if !controller.set_position(placement.x.get(), placement.y.get()) {
                    placement.apply_settings(&controller.settings());
                }
                if let (Some(content), Some(trigger), Some(handle), Some(height_handle)) = (
                    weak_content.upgrade(),
                    weak_trigger.upgrade(),
                    weak_handle_window.upgrade(),
                    weak_height_handle_window.upgrade(),
                ) {
                    apply_monitor_placement(
                        &placement,
                        &content,
                        &trigger,
                        &handle,
                        &height_handle,
                    );
                }
            });
        }
        handle.add_controller(drag);

        let width_resize = gtk::GestureDrag::new();
        width_resize.set_button(gdk::BUTTON_SECONDARY);
        {
            let placement = Rc::clone(&placement);
            let width_state = Rc::clone(&width_state);
            let weak_layout = Rc::downgrade(&layout);
            let weak_settings = settings_window.downgrade();
            let reveal = settings_reveal.clone();
            let weak_handle = handle.downgrade();
            let weak_zone = width_resize_zone.downgrade();
            let weak_content = content_window.downgrade();
            let weak_trigger = trigger_window.downgrade();
            width_resize.connect_drag_begin(move |_, _, _| {
                let Some(layout) = weak_layout.upgrade() else {
                    return;
                };
                placement.begin_width_resize(layout.root.width().max(placement.width.get()));
                width_state.begin();
                if let Some(handle) = weak_handle.upgrade() {
                    handle.add_css_class("system-monitor-width-resizing");
                }
                if let Some(zone) = weak_zone.upgrade() {
                    let width_state = Rc::clone(&width_state);
                    let placement = Rc::clone(&placement);
                    let layout = Rc::clone(&layout);
                    let weak_content = weak_content.clone();
                    let weak_trigger = weak_trigger.clone();
                    zone.add_tick_callback(move |_, _| {
                        if !width_state.active.get() {
                            return glib::ControlFlow::Break;
                        }
                        let (Some(content), Some(trigger)) =
                            (weak_content.upgrade(), weak_trigger.upgrade())
                        else {
                            return glib::ControlFlow::Break;
                        };
                        placement.resize_width(width_state.offset_x.get());
                        apply_monitor_content_size(&placement, &layout, &content, &trigger);
                        apply_monitor_content_placement(&placement, &content, &trigger);
                        glib::ControlFlow::Continue
                    });
                }
                if let Some(window) = weak_settings.upgrade() {
                    reveal.hide(&window);
                }
            });
        }
        {
            let width_state = Rc::clone(&width_state);
            width_resize.connect_drag_update(move |_, offset_x, offset_y| {
                width_state.update(offset_x, offset_y);
            });
        }
        {
            let placement = Rc::clone(&placement);
            let width_state = Rc::clone(&width_state);
            let controller = Rc::clone(controller);
            let weak_layout = Rc::downgrade(&layout);
            let weak_content = content_window.downgrade();
            let weak_trigger = trigger_window.downgrade();
            let weak_handle_window = handle_window.downgrade();
            let weak_height_handle_window = height_handle_window.downgrade();
            let weak_handle = handle.downgrade();
            width_resize.connect_drag_end(move |_, offset_x, offset_y| {
                width_state.end(offset_x, offset_y);
                placement.resize_width(offset_x);
                if let (Some(layout), Some(content), Some(trigger)) = (
                    weak_layout.upgrade(),
                    weak_content.upgrade(),
                    weak_trigger.upgrade(),
                ) {
                    apply_monitor_content_size(&placement, &layout, &content, &trigger);
                    apply_monitor_content_placement(&placement, &content, &trigger);
                }
                if let Some(handle) = weak_handle.upgrade() {
                    handle.remove_css_class("system-monitor-width-resizing");
                }
                if !controller.set_dimensions(placement.scale_milli.get(), placement.width.get()) {
                    placement.apply_settings(&controller.settings());
                }
                if let (
                    Some(layout),
                    Some(content),
                    Some(trigger),
                    Some(handle),
                    Some(height_handle),
                ) = (
                    weak_layout.upgrade(),
                    weak_content.upgrade(),
                    weak_trigger.upgrade(),
                    weak_handle_window.upgrade(),
                    weak_height_handle_window.upgrade(),
                ) {
                    apply_monitor_size(
                        &placement,
                        &layout,
                        &content,
                        &trigger,
                        &handle,
                        &height_handle,
                    );
                    apply_monitor_placement(
                        &placement,
                        &content,
                        &trigger,
                        &handle,
                        &height_handle,
                    );
                }
            });
        }
        width_resize_zone.add_controller(width_resize);

        let height_resize = gtk::GestureDrag::new();
        height_resize.set_button(gdk::BUTTON_SECONDARY);
        {
            let placement = Rc::clone(&placement);
            let height_state = Rc::clone(&height_state);
            let weak_layout = Rc::downgrade(&layout);
            let weak_settings = settings_window.downgrade();
            let reveal = settings_reveal.clone();
            let weak_height_handle = height_handle.downgrade();
            let weak_handle = handle.downgrade();
            let weak_content = content_window.downgrade();
            let weak_trigger = trigger_window.downgrade();
            height_resize.connect_drag_begin(move |_, _, _| {
                let Some(layout) = weak_layout.upgrade() else {
                    return;
                };
                let measured_width = layout.root.width().max(placement.width.get());
                let measured_height = layout
                    .root
                    .height()
                    .max(layout.natural_height(measured_width));
                placement.begin_resize(measured_width, measured_height);
                height_state.begin();
                if let Some(handle) = weak_handle.upgrade() {
                    handle.add_css_class("system-monitor-height-resizing");
                }
                if let Some(height_handle) = weak_height_handle.upgrade() {
                    height_handle.add_css_class("system-monitor-height-resizing");

                    let height_state = Rc::clone(&height_state);
                    let placement = Rc::clone(&placement);
                    let layout = Rc::clone(&layout);
                    let weak_content = weak_content.clone();
                    let weak_trigger = weak_trigger.clone();
                    height_handle.add_tick_callback(move |_, _| {
                        if !height_state.active.get() {
                            return glib::ControlFlow::Break;
                        }
                        let (Some(content), Some(trigger)) =
                            (weak_content.upgrade(), weak_trigger.upgrade())
                        else {
                            return glib::ControlFlow::Break;
                        };
                        placement.resize_height(height_state.offset_y.get());
                        apply_monitor_content_size(&placement, &layout, &content, &trigger);
                        apply_monitor_content_placement(&placement, &content, &trigger);
                        glib::ControlFlow::Continue
                    });
                }
                if let Some(window) = weak_settings.upgrade() {
                    reveal.hide(&window);
                }
            });
        }
        {
            let height_state = Rc::clone(&height_state);
            height_resize.connect_drag_update(move |_, offset_x, offset_y| {
                height_state.update(offset_x, offset_y);
            });
        }
        {
            let placement = Rc::clone(&placement);
            let height_state = Rc::clone(&height_state);
            let controller = Rc::clone(controller);
            let weak_layout = Rc::downgrade(&layout);
            let weak_content = content_window.downgrade();
            let weak_trigger = trigger_window.downgrade();
            let weak_handle_window = handle_window.downgrade();
            let weak_height_handle_window = height_handle_window.downgrade();
            let weak_height_handle = height_handle.downgrade();
            let weak_handle = handle.downgrade();
            height_resize.connect_drag_end(move |_, offset_x, offset_y| {
                height_state.end(offset_x, offset_y);
                placement.resize_height(offset_y);
                if let (Some(layout), Some(content), Some(trigger)) = (
                    weak_layout.upgrade(),
                    weak_content.upgrade(),
                    weak_trigger.upgrade(),
                ) {
                    apply_monitor_content_size(&placement, &layout, &content, &trigger);
                    apply_monitor_content_placement(&placement, &content, &trigger);
                }
                if let Some(height_handle) = weak_height_handle.upgrade() {
                    height_handle.remove_css_class("system-monitor-height-resizing");
                }
                if let Some(handle) = weak_handle.upgrade() {
                    handle.remove_css_class("system-monitor-height-resizing");
                }
                if !controller.set_dimensions(placement.scale_milli.get(), placement.width.get()) {
                    placement.apply_settings(&controller.settings());
                }
                if let (
                    Some(layout),
                    Some(content),
                    Some(trigger),
                    Some(handle),
                    Some(height_handle),
                ) = (
                    weak_layout.upgrade(),
                    weak_content.upgrade(),
                    weak_trigger.upgrade(),
                    weak_handle_window.upgrade(),
                    weak_height_handle_window.upgrade(),
                ) {
                    apply_monitor_size(
                        &placement,
                        &layout,
                        &content,
                        &trigger,
                        &handle,
                        &height_handle,
                    );
                    apply_monitor_placement(
                        &placement,
                        &content,
                        &trigger,
                        &handle,
                        &height_handle,
                    );
                }
            });
        }
        height_handle.add_controller(height_resize);

        let resize = gtk::GestureDrag::new();
        resize.set_button(gdk::BUTTON_SECONDARY);
        {
            let placement = Rc::clone(&placement);
            let resize_state = Rc::clone(&resize_state);
            let weak_layout = Rc::downgrade(&layout);
            let weak_settings = settings_window.downgrade();
            let reveal = settings_reveal.clone();
            let weak_handle = handle.downgrade();
            let weak_content = content_window.downgrade();
            let weak_trigger = trigger_window.downgrade();
            resize.connect_drag_begin(move |_, _, _| {
                let Some(layout) = weak_layout.upgrade() else {
                    return;
                };
                let measured_width = layout.root.width().max(placement.width.get());
                let measured_height = layout
                    .root
                    .height()
                    .max(layout.natural_height(measured_width));
                placement.begin_resize(measured_width, measured_height);
                resize_state.begin();
                if let Some(handle) = weak_handle.upgrade() {
                    handle.add_css_class("system-monitor-resizing");

                    let resize_state = Rc::clone(&resize_state);
                    let placement = Rc::clone(&placement);
                    let layout = Rc::clone(&layout);
                    let weak_content = weak_content.clone();
                    let weak_trigger = weak_trigger.clone();
                    handle.add_tick_callback(move |_, _| {
                        if !resize_state.active.get() {
                            return glib::ControlFlow::Break;
                        }
                        let (Some(content), Some(trigger)) =
                            (weak_content.upgrade(), weak_trigger.upgrade())
                        else {
                            return glib::ControlFlow::Break;
                        };
                        placement.resize(resize_state.offset_x.get(), resize_state.offset_y.get());
                        apply_monitor_content_size(&placement, &layout, &content, &trigger);
                        apply_monitor_content_placement(&placement, &content, &trigger);
                        glib::ControlFlow::Continue
                    });
                }
                if let Some(window) = weak_settings.upgrade() {
                    reveal.hide(&window);
                }
            });
        }
        {
            let resize_state = Rc::clone(&resize_state);
            resize.connect_drag_update(move |_, offset_x, offset_y| {
                resize_state.update(offset_x, offset_y);
            });
        }
        {
            let placement = Rc::clone(&placement);
            let resize_state = Rc::clone(&resize_state);
            let controller = Rc::clone(controller);
            let weak_layout = Rc::downgrade(&layout);
            let weak_content = content_window.downgrade();
            let weak_trigger = trigger_window.downgrade();
            let weak_handle_window = handle_window.downgrade();
            let weak_height_handle_window = height_handle_window.downgrade();
            let weak_handle = handle.downgrade();
            resize.connect_drag_end(move |_, offset_x, offset_y| {
                resize_state.end(offset_x, offset_y);
                placement.resize(offset_x, offset_y);
                if let (Some(layout), Some(content), Some(trigger)) = (
                    weak_layout.upgrade(),
                    weak_content.upgrade(),
                    weak_trigger.upgrade(),
                ) {
                    apply_monitor_content_size(&placement, &layout, &content, &trigger);
                    apply_monitor_content_placement(&placement, &content, &trigger);
                }
                if let Some(handle) = weak_handle.upgrade() {
                    handle.remove_css_class("system-monitor-resizing");
                    handle.set_cursor_from_name(Some("grab"));
                }
                if !controller.set_dimensions(placement.scale_milli.get(), placement.width.get()) {
                    placement.apply_settings(&controller.settings());
                }
                if let (
                    Some(layout),
                    Some(content),
                    Some(trigger),
                    Some(handle),
                    Some(height_handle),
                ) = (
                    weak_layout.upgrade(),
                    weak_content.upgrade(),
                    weak_trigger.upgrade(),
                    weak_handle_window.upgrade(),
                    weak_height_handle_window.upgrade(),
                ) {
                    apply_monitor_size(
                        &placement,
                        &layout,
                        &content,
                        &trigger,
                        &handle,
                        &height_handle,
                    );
                    apply_monitor_placement(
                        &placement,
                        &content,
                        &trigger,
                        &handle,
                        &height_handle,
                    );
                }
            });
        }
        resize_corner.add_controller(resize);

        content_window.present();
        trigger_window.present();
        height_handle_window.present();
        handle_window.present();

        Self {
            monitor: monitor.clone(),
            content_window,
            trigger_window,
            handle_window,
            height_handle_window,
            settings_window,
            _layout: layout,
            _settings_panel: settings_panel,
        }
    }

    pub fn monitor(&self) -> &gdk::Monitor {
        &self.monitor
    }
}

impl Drop for SystemMonitorView {
    fn drop(&mut self) {
        close_monitor_window(&self.settings_window);
        close_monitor_window(&self.handle_window);
        close_monitor_window(&self.height_handle_window);
        close_monitor_window(&self.trigger_window);
        close_monitor_window(&self.content_window);
    }
}

fn close_monitor_window(window: &gtk::ApplicationWindow) {
    window.set_hide_on_close(false);
    window.close();
}

fn desktop_window(
    application: &gtk::Application,
    monitor: &gdk::Monitor,
    namespace: &str,
    css_class: &str,
    targetable: bool,
) -> gtk::ApplicationWindow {
    let window = gtk::ApplicationWindow::builder()
        .application(application)
        .decorated(false)
        .build();
    window.add_css_class(css_class);
    window.set_focusable(false);
    window.set_can_target(targetable);
    window.init_layer_shell();
    window.set_namespace(Some(namespace));
    window.set_layer(Layer::Bottom);
    window.set_keyboard_mode(KeyboardMode::None);
    window.set_monitor(Some(monitor));
    window.set_anchor(Edge::Top, true);
    window.set_anchor(Edge::Left, true);
    window.set_anchor(Edge::Right, false);
    window.set_anchor(Edge::Bottom, false);
    window.set_exclusive_zone(-1);
    window
}

fn apply_monitor_placement(
    placement: &PlacementRuntime,
    content: &gtk::ApplicationWindow,
    trigger: &gtk::ApplicationWindow,
    handle: &gtk::ApplicationWindow,
    height_handle: &gtk::ApplicationWindow,
) {
    apply_monitor_content_placement(placement, content, trigger);
    apply_monitor_handle_placement(placement, handle, height_handle);
}

fn apply_monitor_content_placement(
    placement: &PlacementRuntime,
    content: &gtk::ApplicationWindow,
    trigger: &gtk::ApplicationWindow,
) {
    let x = placement.x.get();
    let y = placement.y.get();
    content.set_margin(Edge::Left, x);
    content.set_margin(Edge::Top, y);
    trigger.set_margin(Edge::Left, x);
    trigger.set_margin(Edge::Top, y);
}

fn apply_monitor_handle_placement(
    placement: &PlacementRuntime,
    handle: &gtk::ApplicationWindow,
    height_handle: &gtk::ApplicationWindow,
) {
    let x = placement.x.get();
    let y = placement.y.get();
    let scale_milli = placement.scale_milli.get();
    handle.set_margin(
        Edge::Left,
        x + placement.width.get() - scaled_pixels(DRAG_HANDLE_WIDTH, scale_milli),
    );
    handle.set_margin(Edge::Top, y);
    height_handle.set_margin(Edge::Left, x);
    height_handle.set_margin(
        Edge::Top,
        y + placement.height.get() - scaled_pixels(HEIGHT_HANDLE_SIZE, scale_milli),
    );
}

fn apply_monitor_size(
    placement: &PlacementRuntime,
    layout: &MonitorLayout,
    content: &gtk::ApplicationWindow,
    trigger: &gtk::ApplicationWindow,
    handle: &gtk::ApplicationWindow,
    height_handle: &gtk::ApplicationWindow,
) {
    apply_monitor_content_size(placement, layout, content, trigger);
    apply_monitor_handle_size(placement, handle, height_handle);
}

fn apply_monitor_content_size(
    placement: &PlacementRuntime,
    layout: &MonitorLayout,
    content: &gtk::ApplicationWindow,
    trigger: &gtk::ApplicationWindow,
) {
    let width = placement.width.get();
    layout.apply_scale(placement.scale_milli.get());
    layout.root.set_size_request(width, -1);
    content.set_default_size(width, -1);
    placement.set_measured_height(layout.natural_height(width));
    placement.set(placement.x.get(), placement.y.get());
    trigger.set_default_size(
        scaled_pixels(SETTINGS_TRIGGER_SIZE, placement.scale_milli.get()),
        scaled_pixels(SETTINGS_TRIGGER_SIZE, placement.scale_milli.get()),
    );
}

fn apply_monitor_handle_size(
    placement: &PlacementRuntime,
    handle: &gtk::ApplicationWindow,
    height_handle: &gtk::ApplicationWindow,
) {
    let scale_milli = placement.scale_milli.get();
    let handle_width = scaled_pixels(DRAG_HANDLE_WIDTH, scale_milli);
    handle.set_default_size(handle_width, placement.height.get());
    height_handle.set_default_size(
        placement.width.get().saturating_sub(handle_width).max(1),
        scaled_pixels(HEIGHT_HANDLE_SIZE, scale_milli),
    );
}

fn settings_popup_position(placement: &PlacementRuntime) -> (i32, i32) {
    let trigger_size = scaled_pixels(SETTINGS_TRIGGER_SIZE, placement.scale_milli.get());
    let left = (placement.x.get() + trigger_size + 6).clamp(
        PANEL_EDGE_MARGIN,
        (placement.geometry.screen_width - SETTINGS_WINDOW_WIDTH - PANEL_EDGE_MARGIN)
            .max(PANEL_EDGE_MARGIN),
    );
    let top = placement.y.get().clamp(
        PANEL_EDGE_MARGIN,
        (placement.geometry.screen_height - SETTINGS_WINDOW_HEIGHT - PANEL_EDGE_MARGIN)
            .max(PANEL_EDGE_MARGIN),
    );
    (left, top)
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
    let values = line
        .split_whitespace()
        .skip(1)
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    if values.len() < 4 {
        return None;
    }
    let total = values.iter().copied().sum();
    let idle = values[3].saturating_add(values.get(4).copied().unwrap_or_default());
    Some(CpuTimes { total, idle })
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
        let value = if raw.abs() > 500.0 { raw / 1000.0 } else { raw };
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
    fn panel_position_is_clamped_to_the_monitor() {
        let geometry = PanelGeometry {
            screen_width: 1280,
            screen_height: 720,
            panel_width: 230,
            panel_height: 480,
        };
        assert_eq!(
            geometry.clamp(-100, 900, 230, 480),
            (PANEL_EDGE_MARGIN, 232)
        );
    }

    #[test]
    fn corner_drag_scales_width_and_height_together() {
        let geometry = PanelGeometry {
            screen_width: 1920,
            screen_height: 1080,
            panel_width: 230,
            panel_height: 400,
        };
        let placement = PlacementRuntime::new(geometry, &MonitorSettings::default());
        placement.begin_resize(230, 400);
        placement.resize(115.0, 200.0);

        assert_eq!(placement.scale_milli.get(), 1_500);
        assert_eq!(placement.width.get(), 345);
        assert_eq!(placement.height.get(), 600);
    }

    #[test]
    fn right_edge_drag_changes_only_width() {
        let geometry = PanelGeometry {
            screen_width: 1920,
            screen_height: 1080,
            panel_width: 230,
            panel_height: 400,
        };
        let placement = PlacementRuntime::new(geometry, &MonitorSettings::default());
        let original_height = placement.height.get();
        let original_scale = placement.scale_milli.get();
        placement.begin_width_resize(230);
        placement.resize_width(150.0);

        assert_eq!(placement.width.get(), 380);
        assert_eq!(placement.height.get(), original_height);
        assert_eq!(placement.scale_milli.get(), original_scale);
    }

    #[test]
    fn bottom_edge_drag_changes_scale_without_changing_width() {
        let geometry = PanelGeometry {
            screen_width: 1920,
            screen_height: 1080,
            panel_width: 230,
            panel_height: 400,
        };
        let placement = PlacementRuntime::new(geometry, &MonitorSettings::default());
        placement.begin_resize(380, 400);
        placement.resize_height(200.0);

        assert_eq!(placement.width.get(), 380);
        assert_eq!(placement.height.get(), 600);
        assert_eq!(placement.scale_milli.get(), 1_500);
    }

    #[test]
    fn independent_width_is_restored_from_settings() {
        let geometry = PanelGeometry {
            screen_width: 1920,
            screen_height: 1080,
            panel_width: 230,
            panel_height: 400,
        };
        let settings = MonitorSettings {
            panel_width: Some(520),
            ..MonitorSettings::default()
        };

        assert_eq!(PlacementRuntime::new(geometry, &settings).width.get(), 520);
    }

    #[test]
    fn saved_width_is_migrated_to_a_proportional_scale() {
        let geometry = PanelGeometry {
            screen_width: 1920,
            screen_height: 1080,
            panel_width: 230,
            panel_height: 400,
        };
        let settings = MonitorSettings {
            legacy_width: Some(460),
            ..MonitorSettings::default()
        };

        assert_eq!(geometry.resolve_scale(&settings), 2_000);
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
