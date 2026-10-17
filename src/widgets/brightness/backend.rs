//! Backlight and DDC/CI discovery, reads and writes.
use crate::widgets::{command, snapshot_cache::SnapshotCache};
use std::{
    cmp::Ordering,
    fs,
    path::Path,
    sync::Mutex,
    time::{Duration, Instant},
};
use tracing::debug;

pub(super) const BRIGHTNESS_MIN: f64 = 0.05;
const BRIGHTNESS_CACHE_TTL: Duration = Duration::from_millis(500);
static BRIGHTNESS_CACHE: SnapshotCache<BrightnessState> = SnapshotCache::new(BRIGHTNESS_CACHE_TTL);
static BRIGHTNESS_WRITE_LOCK: Mutex<()> = Mutex::new(());
pub(super) const BACKLIGHT_CLASS_PATH: &str = "/sys/class/backlight";
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum BrightnessBackend {
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
pub(super) struct BrightnessState {
    pub(super) backend: BrightnessBackend,
    pub(super) value: f64,
}

impl Default for BrightnessState {
    fn default() -> Self {
        Self {
            backend: BrightnessBackend::Unknown,
            value: BRIGHTNESS_MIN,
        }
    }
}

pub(super) fn read_brightness() -> Result<BrightnessState, String> {
    BRIGHTNESS_CACHE.get_or_try_update(read_brightness_uncached)
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

pub(super) fn invalidate_brightness_cache() {
    BRIGHTNESS_CACHE.invalidate();
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

pub(super) fn write_brightness(
    backend: BrightnessBackend,
    value: f64,
) -> Result<BrightnessBackend, String> {
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
        BRIGHTNESS_CACHE.store(BrightnessState {
            backend: *resolved_backend,
            value: clamp_brightness(value),
        });
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

pub(super) fn percentage(value: f64) -> i32 {
    (clamp_brightness(value) * 100.0).round() as i32
}

pub(super) fn clamp_brightness(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(BRIGHTNESS_MIN, 1.0)
    } else {
        BRIGHTNESS_MIN
    }
}

#[cfg(test)]
mod tests;
