//! Hardware and thermal-zone sensors, stable identities and display labels.
use super::files::{read_trimmed, sorted_directory_paths};
use super::storage::compact_storage_model;
use super::{MonitorSection, MonitorSettings, SensorReading, gpu};
use std::{
    collections::{HashMap, HashSet},
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

mod catalog;
use catalog::SensorCatalog;

// Labels and device descriptions rarely change in place. A bounded refresh also
// recovers attributes that appeared after a driver created its hwmon directory.
const METADATA_REFRESH: Duration = Duration::from_secs(30);

#[derive(PartialEq, Eq)]
struct SourceStamp {
    path: PathBuf,
    identity: Option<(u64, u64, Option<SystemTime>)>,
}

pub(super) struct HwmonSampler {
    hwmon_root: PathBuf,
    thermal_root: PathBuf,
    sources: Option<(Vec<SourceStamp>, Vec<SourceStamp>)>,
    catalog: SensorCatalog,
    discovered_at: Option<Instant>,
}

impl Default for HwmonSampler {
    fn default() -> Self {
        Self {
            hwmon_root: "/sys/class/hwmon".into(),
            thermal_root: "/sys/class/thermal".into(),
            sources: None,
            catalog: SensorCatalog::default(),
            discovered_at: None,
        }
    }
}

impl HwmonSampler {
    pub(super) fn sample(&mut self, settings: &MonitorSettings, discover: bool) -> HwmonSnapshot {
        self.sample_at(settings, discover, Instant::now())
    }

    fn sample_at(
        &mut self,
        settings: &MonitorSettings,
        discover: bool,
        now: Instant,
    ) -> HwmonSnapshot {
        let sources = (
            active_sources(&self.hwmon_root, false),
            active_sources(&self.thermal_root, true),
        );
        if self.sources.as_ref() != Some(&sources)
            || !self.catalog.complete
            || self
                .discovered_at
                .is_none_or(|at| now.saturating_duration_since(at) >= METADATA_REFRESH)
        {
            self.catalog = catalog::discover(&sources.0, &sources.1);
            self.sources = Some(sources);
            self.discovered_at = Some(now);
        }
        self.catalog.sample(settings, discover)
    }
}

#[cfg(test)]
mod tests;

fn active_sources(root: &Path, thermal: bool) -> Vec<SourceStamp> {
    sorted_directory_paths(root)
        .into_iter()
        .filter(|directory| {
            (!thermal
                || directory
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("thermal_zone")))
                && !device_is_nvidia(directory)
                && !device_runtime_suspended(directory)
        })
        .map(|path| {
            let identity = fs::metadata(&path)
                .ok()
                .map(|metadata| (metadata.dev(), metadata.ino(), metadata.modified().ok()));
            SourceStamp { path, identity }
        })
        .collect()
}

pub(super) struct HwmonSnapshot {
    pub(super) temperatures: Vec<SensorReading>,
    pub(super) power: Vec<SensorReading>,
    pub(super) fans: Vec<SensorReading>,
}

pub(super) fn hwmon_device_key(directory: &Path, chip: &str) -> String {
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

pub(super) fn sensor_value(
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

pub(super) fn thermal_zone_device_label(kind: &str) -> String {
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
pub(super) enum SensorKind {
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

pub(super) fn sensor_display_label(
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

fn is_gpu_label(label: &str) -> bool {
    let label = label.to_ascii_lowercase();
    label.contains("gpu")
        || label.contains("amdgpu")
        || label.contains("nvidia")
        || label.contains("nouveau")
}

pub(super) fn device_is_nvidia(path: &Path) -> bool {
    [path.join("vendor"), path.join("device/vendor")]
        .into_iter()
        .filter_map(read_trimmed)
        .any(|vendor| vendor.eq_ignore_ascii_case("0x10de"))
}

pub(super) fn device_runtime_suspended(path: &Path) -> bool {
    [
        path.join("power/runtime_status"),
        path.join("device/power/runtime_status"),
    ]
    .into_iter()
    .filter_map(read_trimmed)
    .any(|status| matches!(status.as_str(), "suspended" | "suspending"))
}
