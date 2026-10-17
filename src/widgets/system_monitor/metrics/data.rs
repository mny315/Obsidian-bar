//! Metric descriptions, selected readings and display formatting.
use super::super::{
    METRIC_METER, MonitorSection, MonitorSettings, SystemSnapshot, batteries::PowerOffTarget, gpu,
};
use super::METRIC_GRAPH;

#[derive(Clone)]
pub(in super::super) struct DisplayMetric {
    pub(in super::super) id: String,
    pub(in super::super) label: String,
    pub(in super::super) value: String,
    pub(in super::super) group: Option<MetricGroup>,
    pub(in super::super) fraction: Option<f64>,
    pub(in super::super) power_off: Option<PowerOffTarget>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in super::super) struct MetricGroup {
    pub(in super::super) id: String,
    pub(in super::super) label: String,
}

impl DisplayMetric {
    fn lazy<V: Into<String>>(
        values: bool,
        id: impl Into<String>,
        label: impl Into<String>,
        value: impl FnOnce() -> V,
    ) -> Self {
        Self::new(
            id,
            label,
            if values {
                value().into()
            } else {
                String::new()
            },
        )
    }

    pub(in super::super) fn new(
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
            power_off: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in super::super) struct MetricOption {
    pub(in super::super) id: String,
    pub(in super::super) label: String,
    pub(in super::super) group: Option<MetricGroup>,
}

pub(in super::super) fn metric_options(
    section: MonitorSection,
    snapshot: &SystemSnapshot,
) -> Vec<MetricOption> {
    let (metrics, fraction) = collect_metrics(section, snapshot, false);
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

pub(in super::super) fn selected_metrics(
    section: MonitorSection,
    snapshot: &SystemSnapshot,
    settings: &MonitorSettings,
) -> (Vec<DisplayMetric>, Option<f64>, bool) {
    let (mut metrics, fraction) = display_metrics(section, snapshot);
    metrics.retain(|metric| settings.metric_visible(section, &metric.id));
    for metric in &mut metrics {
        let name = settings.metric_name(section, &metric.id, &metric.label);
        if name != metric.label {
            metric.label = name.to_owned();
        }
    }
    let fraction = fraction.filter(|_| settings.metric_visible(section, METRIC_METER));
    let graph = section == MonitorSection::Network
        && snapshot.network_available
        && settings.metric_visible(section, METRIC_GRAPH);
    (metrics, fraction, graph)
}

pub(in super::super) fn display_metrics(
    section: MonitorSection,
    snapshot: &SystemSnapshot,
) -> (Vec<DisplayMetric>, Option<f64>) {
    collect_metrics(section, snapshot, true)
}

fn collect_metrics(
    section: MonitorSection,
    snapshot: &SystemSnapshot,
    values: bool,
) -> (Vec<DisplayMetric>, Option<f64>) {
    let mut metrics = Vec::new();
    let fraction = match section {
        MonitorSection::Cpu => {
            if !snapshot.ready {
                metrics.push(DisplayMetric::lazy(
                    values,
                    "loading",
                    "System Monitor",
                    || "Loading…",
                ));
                return (metrics, None);
            }
            if let Some(cpu) = snapshot.cpu_percent {
                metrics.push(DisplayMetric::lazy(values, "cpu", "CPU", || {
                    format_percent(cpu)
                }));
            }
            if let Some(frequency) = snapshot.cpu_frequency_mhz {
                metrics.push(DisplayMetric::lazy(
                    values,
                    "cpu-frequency",
                    "CPU Frequency",
                    || {
                        if frequency >= 1000.0 {
                            format!("{:.2} GHz", frequency / 1000.0)
                        } else {
                            format!("{frequency:.0} MHz")
                        }
                    },
                ));
            }
            if let Some(load) = snapshot.load_average {
                metrics.push(DisplayMetric::lazy(
                    values,
                    "load-average",
                    "Load (1 min)",
                    || format!("{load:.2}"),
                ));
            }
            if let (Some(running), Some(total)) =
                (snapshot.running_processes, snapshot.total_processes)
            {
                metrics.push(DisplayMetric::lazy(
                    values,
                    "processes",
                    "Processes",
                    || format!("{running} / {total}"),
                ));
            }
            if let Some(seconds) = snapshot.uptime_seconds {
                metrics.push(DisplayMetric::lazy(values, "uptime", "Uptime", || {
                    format_duration(seconds)
                }));
            }
            if let Some(name) = &snapshot.cpu_name {
                for metric in &mut metrics {
                    metric.group = Some(MetricGroup {
                        id: "cpu".into(),
                        label: name.to_string(),
                    });
                }
            }
            None
        }
        MonitorSection::Gpu => {
            metrics.extend(gpu::display(section, &snapshot.gpus, values));
            None
        }
        MonitorSection::Memory => {
            if let Some(used) = snapshot.memory_used {
                metrics.push(DisplayMetric::lazy(
                    values,
                    "memory-used",
                    "Memory Used",
                    || format_bytes(used),
                ));
            }
            if let Some(total) = snapshot.memory_total {
                metrics.push(DisplayMetric::lazy(
                    values,
                    "memory-total",
                    "Memory Total",
                    || format_bytes(total),
                ));
            }
            if let (Some(used), Some(total)) = (snapshot.memory_used, snapshot.memory_total) {
                metrics.push(DisplayMetric::lazy(
                    values,
                    "memory-available",
                    "Available",
                    || format_bytes(total.saturating_sub(used)),
                ));
            }
            if let Some(used) = snapshot.swap_used {
                metrics.push(DisplayMetric::lazy(
                    values,
                    "swap-used",
                    "Swap Used",
                    || format_bytes(used),
                ));
            }
            if let Some(total) = snapshot.swap_total {
                metrics.push(DisplayMetric::lazy(
                    values,
                    "swap-total",
                    "Swap Total",
                    || format_bytes(total),
                ));
            }
            ratio(snapshot.memory_used, snapshot.memory_total)
        }
        MonitorSection::Network => {
            if snapshot.network_available {
                if !snapshot.network_interfaces.is_empty() {
                    metrics.push(DisplayMetric::lazy(
                        values,
                        "network-interfaces",
                        "Interfaces",
                        || snapshot.network_interfaces.join(", "),
                    ));
                }
                metrics.push(DisplayMetric::lazy(
                    values,
                    "download",
                    "Download Rate",
                    || format_rate(snapshot.download_bytes_per_second),
                ));
                metrics.push(DisplayMetric::lazy(values, "upload", "Upload Rate", || {
                    format_rate(snapshot.upload_bytes_per_second)
                }));
                metrics.push(DisplayMetric::lazy(
                    values,
                    "network-received",
                    "Received",
                    || format_bytes(snapshot.network_received_bytes),
                ));
                metrics.push(DisplayMetric::lazy(values, "network-sent", "Sent", || {
                    format_bytes(snapshot.network_transmitted_bytes)
                }));
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
                        DisplayMetric::lazy(values, format!("{}:{id}", disk.id), label, || {
                            format_bytes(bytes)
                        });
                    metric.group = Some(group.clone());
                    metrics.push(metric);
                }
                if let Some(fraction) = ratio(Some(disk.used), Some(disk.total)) {
                    let mut metric = DisplayMetric::lazy(
                        values,
                        format!("{}:{METRIC_METER}", disk.id),
                        "Usage bar",
                        || "",
                    );
                    metric.group = Some(group);
                    metric.fraction = Some(fraction);
                    metrics.push(metric);
                }
            }
            None
        }
        MonitorSection::Temperatures => {
            for sensor in &snapshot.temperatures {
                metrics.push(DisplayMetric::lazy(
                    values,
                    &sensor.id,
                    &sensor.label,
                    || sensor.formatted("°C", 1),
                ));
            }
            metrics.extend(gpu::display(section, &snapshot.gpus, values));
            None
        }
        MonitorSection::Power => {
            for sensor in &snapshot.power {
                metrics.push(DisplayMetric::lazy(
                    values,
                    &sensor.id,
                    &sensor.label,
                    || sensor.formatted("W", 1),
                ));
            }
            metrics.extend(gpu::display(section, &snapshot.gpus, values));
            None
        }
        MonitorSection::Cooling => {
            for sensor in &snapshot.fans {
                metrics.push(DisplayMetric::lazy(
                    values,
                    &sensor.id,
                    &sensor.label,
                    || sensor.formatted("RPM", 0),
                ));
            }
            None
        }
        MonitorSection::Battery => {
            for battery in &snapshot.device_batteries {
                let mut metric = DisplayMetric::lazy(values, &battery.id, &battery.name, || {
                    battery.display_value()
                });
                if values {
                    metric.power_off.clone_from(&battery.power_off);
                }
                metrics.push(metric);
            }
            if let Some(battery) = snapshot.battery.as_ref() {
                metrics.push(DisplayMetric::lazy(values, "battery", "Battery", || {
                    format!("{:.0}%", battery.percent)
                }));
                metrics.push(DisplayMetric::lazy(
                    values,
                    "battery-status",
                    "Status",
                    || &battery.status,
                ));
                if let Some(power) = battery.power_watts {
                    let label = if battery.status.eq_ignore_ascii_case("charging") {
                        "Charging Rate"
                    } else {
                        "Power Draw"
                    };
                    metrics.push(DisplayMetric::lazy(values, "battery-power", label, || {
                        format!("{power:.1} W")
                    }));
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

pub(in super::super) fn format_percent(value: f64) -> String {
    if value >= 10.0 {
        format!("{value:.0}%")
    } else {
        format!("{value:.1}%")
    }
}

pub(in super::super) fn format_bytes(bytes: u64) -> String {
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
