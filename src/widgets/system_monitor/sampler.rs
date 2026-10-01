use super::gpu;
use super::metrics::metric_options;
use super::{
    BatterySnapshot, DiskSnapshot, MonitorSection, MonitorSettings, SensorReading, SystemSnapshot,
};
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

#[derive(Clone, Copy)]
pub(super) struct CpuTimes {
    pub(super) total: u64,
    pub(super) idle: u64,
}

#[derive(Clone)]
pub(super) struct NetworkCounters {
    pub(super) read_at: Instant,
    pub(super) received: u64,
    pub(super) transmitted: u64,
    pub(super) interfaces: Vec<String>,
}

pub(super) struct SystemSampler {
    pub(super) previous_cpu: Option<CpuTimes>,
    pub(super) previous_network: Option<NetworkCounters>,
    pub(super) gpu_sampling_enabled: Arc<AtomicBool>,
    pub(super) cached: SystemSnapshot,
    pub(super) gpus: gpu::GpuSampler,
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
    pub(super) fn sample(&mut self) -> SystemSnapshot {
        self.sample_with(&MonitorSettings::default(), true)
    }

    pub(super) fn sample_with(
        &mut self,
        settings: &MonitorSettings,
        visible: bool,
    ) -> SystemSnapshot {
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

pub(super) fn parse_cpu_times(line: &str) -> Option<CpuTimes> {
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

pub(super) fn cpu_usage(previous: CpuTimes, current: CpuTimes) -> Option<f64> {
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
pub(super) struct MemoryUsage {
    pub(super) used: u64,
    pub(super) total: u64,
    pub(super) swap_used: u64,
    pub(super) swap_total: u64,
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

pub(super) fn disk_mounts(mountinfo: &str) -> Vec<(String, PathBuf)> {
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

pub(super) fn storage_display_name(
    model: Option<&str>,
    label: Option<&str>,
    mount: &Path,
) -> String {
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

pub(super) fn parse_network_counters(
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

pub(super) fn parse_default_interfaces(ipv4: &str, ipv6: &str) -> HashSet<String> {
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

pub(super) fn network_rates(
    previous: &NetworkCounters,
    current: &NetworkCounters,
) -> Option<(f64, f64)> {
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

pub(super) fn compact_storage_model(model: &str) -> Option<String> {
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

fn read_battery_snapshot() -> Option<BatterySnapshot> {
    read_battery_snapshot_at(Path::new("/sys/class/power_supply"))
}

pub(super) fn battery_capacity(supply: &Path) -> Option<(f64, Option<f64>)> {
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

pub(super) fn read_battery_snapshot_at(power_supplies: &Path) -> Option<BatterySnapshot> {
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

pub(super) fn sorted_directory_paths(path: &Path) -> Vec<PathBuf> {
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

pub(super) fn read_trimmed(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn read_number(path: impl AsRef<Path>) -> Option<f64> {
    read_trimmed(path)?.parse().ok()
}

pub(super) fn read_u64(path: impl AsRef<Path>) -> Option<u64> {
    read_trimmed(path)?.parse().ok()
}
