use super::gpu;
use super::{
    ICON_MONITOR, METRIC_METER, MonitorSection, MonitorSettings, SCALE_MILLI_DEFAULT,
    SystemSnapshot, scaled_pixels,
};
use crate::widgets::clear_box;
use gtk::{gdk, prelude::*};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

const NETWORK_HISTORY_LENGTH: usize = 64;
pub(super) const METRIC_GRAPH: &str = "traffic-graph";

#[derive(Clone)]
pub(super) struct DisplayMetric {
    pub(super) id: String,
    pub(super) label: String,
    pub(super) value: String,
    pub(super) group: Option<MetricGroup>,
    pub(super) fraction: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct MetricGroup {
    pub(super) id: String,
    pub(super) label: String,
}

impl DisplayMetric {
    pub(super) fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
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
pub(super) struct MetricOption {
    pub(super) id: String,
    pub(super) label: String,
    pub(super) group: Option<MetricGroup>,
}

pub(super) fn metric_options(
    section: MonitorSection,
    snapshot: &SystemSnapshot,
) -> Vec<MetricOption> {
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

pub(super) fn selected_metrics(
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
pub(super) struct NetworkGraphState {
    pub(super) download: VecDeque<f64>,
    pub(super) upload: VecDeque<f64>,
}

impl NetworkGraphState {
    pub(super) fn push(&mut self, download: f64, upload: f64) {
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

pub(super) struct MeterView {
    pub(super) area: gtk::DrawingArea,
    pub(super) fraction: Rc<Cell<f64>>,
}

impl MeterView {
    pub(super) fn new() -> Self {
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

    pub(super) fn set_fraction(&self, fraction: Option<f64>) {
        self.area.set_visible(fraction.is_some());
        self.fraction
            .set(fraction.unwrap_or_default().clamp(0.0, 1.0));
        self.area.queue_draw();
    }
}

pub(super) struct SectionView {
    pub(super) root: gtk::Box,
    pub(super) rows: gtk::Box,
    pub(super) signature: RefCell<Vec<(String, String)>>,
    pub(super) metric_groups: RefCell<Vec<Option<MetricGroup>>>,
    pub(super) metric_views: RefCell<Vec<MetricView>>,
    pub(super) meter: Option<MeterView>,
    pub(super) graph: Option<gtk::DrawingArea>,
    pub(super) graph_state: Option<Rc<RefCell<NetworkGraphState>>>,
    pub(super) available: Cell<bool>,
    pub(super) scale_milli: Cell<i32>,
    pub(super) minimum_width: Cell<i32>,
}

pub(super) enum MetricView {
    Value(gtk::Label),
    Meter(MeterView),
}

impl SectionView {
    pub(super) fn new(section: MonitorSection) -> Rc<Self> {
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

    pub(super) fn update(
        &self,
        metrics: Vec<DisplayMetric>,
        fraction: Option<f64>,
        graph_visible: bool,
    ) {
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

    pub(super) fn apply_scale(&self, scale_milli: i32) {
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

    pub(super) fn push_network_sample(&self, download: f64, upload: f64) {
        let Some(state) = self.graph_state.as_ref() else {
            return;
        };
        state.borrow_mut().push(download, upload);
        if let Some(graph) = self.graph.as_ref() {
            graph.queue_draw();
        }
    }
}

pub(super) struct MonitorLayout {
    pub(super) root: gtk::Box,
    pub(super) sections: HashMap<MonitorSection, Rc<SectionView>>,
    pub(super) empty_state: gtk::Box,
    pub(super) empty_title: gtk::Label,
    pub(super) empty_hint: gtk::Label,
    pub(super) applied_scale: Cell<i32>,
    pub(super) scale_provider: gtk::CssProvider,
    pub(super) display: gdk::Display,
    pub(super) settings: RefCell<MonitorSettings>,
    pub(super) snapshot: RefCell<SystemSnapshot>,
}

impl MonitorLayout {
    pub(super) fn new(
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

    pub(super) fn apply_settings(&self, settings: &MonitorSettings) {
        self.settings.replace(settings.clone());
        if self.applied_scale.replace(settings.scale_milli) != settings.scale_milli {
            self.scale_provider.load_from_data(&format!(
                ".system-monitor-body, .system-monitor-settings-surface {{ font-size: {:.2}px; }}",
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

    pub(super) fn minimum_width(&self) -> i32 {
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

    pub(super) fn natural_height(&self, width: i32) -> i32 {
        let (_, natural, _, _) = self.root.measure(gtk::Orientation::Vertical, width.max(1));
        natural.max(1)
    }

    pub(super) fn update_snapshot(&self, snapshot: &SystemSnapshot) {
        self.snapshot.replace(snapshot.clone());
        if let Some(view) = self.sections.get(&MonitorSection::Network) {
            view.push_network_sample(
                snapshot.download_bytes_per_second,
                snapshot.upload_bytes_per_second,
            );
        }
        self.update_metrics(snapshot);
    }

    pub(super) fn update_metrics(&self, snapshot: &SystemSnapshot) {
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

    pub(super) fn refresh_visibility(&self) {
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

pub(super) fn display_metrics(
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

pub(super) fn format_percent(value: f64) -> String {
    if value >= 10.0 {
        format!("{value:.0}%")
    } else {
        format!("{value:.1}%")
    }
}

pub(super) fn format_bytes(bytes: u64) -> String {
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
