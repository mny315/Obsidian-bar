use super::*;

mod intel;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum DataState {
    #[default]
    Ready,
    Sleeping,
    PermissionDenied,
    DriverMissing,
    Unsupported,
    Unavailable,
    Paused,
    Waiting,
}

impl DataState {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Ready => "Available",
            Self::Sleeping => "Sleeping",
            Self::PermissionDenied => "No permission",
            Self::DriverMissing => "Driver unavailable",
            Self::Unsupported => "Not reported by driver",
            Self::Unavailable => "Device unavailable",
            Self::Paused => "Updates paused",
            Self::Waiting => "Waiting for next sample",
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct GpuDevice {
    pub id: String,
    pub name: String,
    pub state: DataState,
    pub data: GpuSnapshot,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct GpuRequest {
    pub load: bool,
    pub memory: bool,
    pub frequency: bool,
    pub temperature: bool,
    pub power: bool,
}

impl GpuRequest {
    fn new(settings: &MonitorSettings, id: &str) -> Self {
        Self {
            load: settings.wants(MonitorSection::Gpu, &metric_id(id, "load")),
            memory: ["memory-used", "memory-total"]
                .into_iter()
                .any(|key| settings.wants(MonitorSection::Gpu, &metric_id(id, key))),
            frequency: settings.wants(MonitorSection::Gpu, &metric_id(id, "frequency")),
            temperature: settings
                .wants(MonitorSection::Temperatures, &metric_id(id, "temperature")),
            power: settings.wants(MonitorSection::Power, &metric_id(id, "power")),
        }
    }

    pub(super) fn any(self) -> bool {
        self.load || self.memory || self.frequency || self.temperature || self.power
    }
}

pub(super) fn metric_id(device: &str, metric: &str) -> String {
    format!("gpu@{device}:{metric}")
}

pub(super) fn legacy_metric_id(section: MonitorSection, id: &str) -> Option<&'static str> {
    if !id.starts_with("gpu@") {
        return None;
    }
    match (section, id.rsplit(':').next()?) {
        (MonitorSection::Gpu, "load") => Some("gpu"),
        (MonitorSection::Gpu, "memory-used") => Some("gpu-memory-used"),
        (MonitorSection::Gpu, "memory-total") => Some("gpu-memory-total"),
        (MonitorSection::Temperatures, "temperature") => Some("gpu-runtime-temperature"),
        (MonitorSection::Power, "power") => Some("gpu-power"),
        _ => None,
    }
}

#[derive(Default)]
pub(super) struct GpuSampler {
    previous: HashMap<String, GpuDevice>,
    intel: intel::IntelSampler,
}

impl GpuSampler {
    pub(super) fn sample(
        &mut self,
        settings: &MonitorSettings,
        visible: bool,
        enabled: &AtomicBool,
    ) -> Vec<GpuDevice> {
        let mut devices = Vec::new();
        let mut seen = HashSet::new();
        let mut paths = sorted_directory_paths(Path::new("/sys/class/drm"))
            .into_iter()
            .filter(|card| {
                card.file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| name.strip_prefix("card"))
                    .is_some_and(|index| {
                        !index.is_empty() && index.chars().all(|c| c.is_ascii_digit())
                    })
            })
            .filter_map(|card| {
                fs::canonicalize(card.join("device"))
                    .ok()
                    .map(|device| (card, device))
            })
            .collect::<Vec<_>>();
        // Unbound devices have no DRM card. Enumerating PCI metadata lets us
        // explain a missing driver without opening or waking the GPU.
        paths.extend(
            sorted_directory_paths(Path::new("/sys/bus/pci/devices"))
                .into_iter()
                .filter(|device| is_gpu_device(device))
                .map(|device| (device.clone(), device)),
        );
        for (card, device) in paths {
            let id = device
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            if !seen.insert(id.clone()) {
                continue;
            }
            let vendor = read_trimmed(device.join("vendor")).unwrap_or_default();
            let driver = fs::read_link(device.join("driver")).ok().and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            });
            let name = self
                .previous
                .get(&id)
                .map(|gpu| gpu.name.clone())
                .unwrap_or_else(|| device_name(&device, &id, &vendor));
            let request = GpuRequest::new(settings, &id);
            let state = if driver.is_none() {
                DataState::DriverMissing
            } else if device_runtime_suspended(&device) {
                DataState::Sleeping
            } else if !enabled.load(Ordering::Acquire)
                || !request.any()
                || (settings.economy && !visible)
            {
                DataState::Paused
            } else {
                DataState::Ready
            };
            let result = if state != DataState::Ready {
                self.intel.pause(&id);
                Err(state)
            } else if vendor == "0x10de" && driver.as_deref() == Some("nvidia") {
                nvidia::read_snapshot(&device, enabled, request)
            } else if matches!(driver.as_deref(), Some("i915" | "xe")) {
                Ok(self.intel.sample(&card, &device, &id, request))
            } else {
                Ok(read_sysfs(&device, request))
            };
            let (mut data, state) = match result {
                Ok(data) => (data, DataState::Ready),
                Err(state) => (GpuSnapshot::default(), state),
            };
            data.client_memory = matches!(driver.as_deref(), Some("i915" | "xe"));
            let name = data.name.clone().unwrap_or(name);
            devices.push(GpuDevice {
                id,
                name,
                state,
                data,
            });
        }
        self.previous = devices
            .iter()
            .cloned()
            .map(|device| (device.id.clone(), device))
            .collect();
        devices
    }
}

fn device_name(device: &Path, id: &str, vendor: &str) -> String {
    let properties = fs::read_to_string(format!("/run/udev/data/+pci:{id}")).unwrap_or_default();
    properties
        .lines()
        .find_map(|line| line.strip_prefix("E:ID_MODEL_FROM_DATABASE="))
        .map(str::to_owned)
        .or_else(|| read_trimmed(device.join("label")))
        .unwrap_or_else(|| {
            format!(
                "{} GPU ({id})",
                match vendor {
                    "0x10de" => "NVIDIA",
                    "0x1002" => "AMD",
                    "0x8086" => "Intel",
                    _ => "PCI",
                }
            )
        })
}

pub(super) fn is_gpu_device(path: &Path) -> bool {
    [path.join("class"), path.join("device/class")]
        .into_iter()
        .filter_map(read_trimmed)
        .any(|class| class.starts_with("0x03"))
}

fn read_value(path: &Path) -> Result<f64, DataState> {
    fs::read_to_string(path)
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::PermissionDenied => DataState::PermissionDenied,
            _ => DataState::Unsupported,
        })?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
        .ok_or(DataState::Unavailable)
}

fn read_sysfs(device: &Path, request: GpuRequest) -> GpuSnapshot {
    let mut data = GpuSnapshot::default();
    for (key, requested, target, path, divisor) in [
        (
            "load",
            request.load,
            &mut data.utilization_percent,
            "gpu_busy_percent",
            1.0,
        ),
        (
            "frequency",
            request.frequency,
            &mut data.frequency_mhz,
            "pp_dpm_sclk",
            1.0,
        ),
    ] {
        if !requested {
            data.states.insert(key, DataState::Paused);
            continue;
        }
        let value = if key == "frequency" {
            fs::read_to_string(device.join(path))
                .ok()
                .and_then(|content| {
                    content
                        .lines()
                        .find(|line| line.contains('*'))
                        .and_then(|line| line.split_whitespace().nth(1))
                        .and_then(|value| {
                            value
                                .trim_end_matches("Mhz")
                                .trim_end_matches("MHz")
                                .parse()
                                .ok()
                        })
                })
                .ok_or(DataState::Unsupported)
        } else {
            read_value(&device.join(path))
        };
        match value {
            Ok(value) => *target = Some(value / divisor),
            Err(state) => {
                data.states.insert(key, state);
            }
        }
    }
    if request.memory {
        for (key, field, file) in [
            ("memory-used", &mut data.memory_used, "mem_info_vram_used"),
            (
                "memory-total",
                &mut data.memory_total,
                "mem_info_vram_total",
            ),
        ] {
            match read_value(&device.join(file)) {
                Ok(value) if value >= 0.0 => *field = Some(value as u64),
                Err(state) => {
                    data.states.insert(key, state);
                }
                _ => {}
            }
        }
    } else {
        for key in ["memory-used", "memory-total"] {
            data.states.insert(key, DataState::Paused);
        }
    }
    read_hwmon(device, request, &mut data);
    data
}

fn read_hwmon(device: &Path, request: GpuRequest, data: &mut GpuSnapshot) {
    for (key, requested, field, files, divisor) in [
        (
            "temperature",
            request.temperature,
            &mut data.temperature_celsius,
            ["temp1_input", "temp2_input"],
            1000.0,
        ),
        (
            "power",
            request.power,
            &mut data.power_watts,
            ["power1_average", "power1_input"],
            1_000_000.0,
        ),
    ] {
        if !requested {
            data.states.insert(key, DataState::Paused);
            continue;
        }
        let mut state = DataState::Unsupported;
        for directory in sorted_directory_paths(&device.join("hwmon")) {
            for file in files {
                match read_value(&directory.join(file)) {
                    Ok(value) => {
                        *field = Some(value / divisor);
                        break;
                    }
                    Err(DataState::PermissionDenied) => state = DataState::PermissionDenied,
                    _ => {}
                }
            }
            if field.is_some() {
                break;
            }
        }
        if field.is_none() {
            data.states.insert(key, state);
        }
    }
}

pub(super) fn display(section: MonitorSection, devices: &[GpuDevice]) -> Vec<DisplayMetric> {
    let mut rows = Vec::new();
    for device in devices {
        let data = &device.data;
        let fields = match section {
            MonitorSection::Gpu => vec![
                (
                    "load",
                    if data.client_memory {
                        "Application GPU Load"
                    } else {
                        "GPU Load"
                    },
                    data.utilization_percent.map(format_percent),
                ),
                (
                    "memory-used",
                    if data.client_memory {
                        "Application GPU Memory"
                    } else {
                        "GPU Memory Used"
                    },
                    data.memory_used.map(format_bytes),
                ),
                (
                    "memory-total",
                    "GPU Memory Total",
                    data.memory_total.map(format_bytes),
                ),
                (
                    "frequency",
                    "GPU Frequency",
                    data.frequency_mhz.map(|value| format!("{value:.0} MHz")),
                ),
            ],
            MonitorSection::Temperatures => vec![(
                "temperature",
                "GPU Temperature",
                data.temperature_celsius
                    .map(|value| format!("{value:.1} °C")),
            )],
            MonitorSection::Power => vec![(
                "power",
                "GPU Power",
                data.power_watts.map(|value| format!("{value:.1} W")),
            )],
            _ => Vec::new(),
        };
        for (key, label, value) in fields {
            let state = if device.state == DataState::Ready {
                data.states
                    .get(key)
                    .copied()
                    .unwrap_or(DataState::Unsupported)
            } else {
                device.state
            };
            let mut metric = DisplayMetric::new(
                metric_id(&device.id, key),
                label,
                value.unwrap_or_else(|| state.label().to_owned()),
            );
            metric.group = Some(MetricGroup {
                id: device.id.clone(),
                label: if devices
                    .iter()
                    .filter(|other| other.name == device.name)
                    .count()
                    > 1
                {
                    format!("{} · {}", device.name, device.id)
                } else {
                    device.name.clone()
                },
            });
            rows.push(metric);
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devices_keep_separate_values_names_and_switches() {
        let devices = [
            GpuDevice {
                id: "0000:01:00.0".into(),
                name: "Same GPU".into(),
                state: DataState::Ready,
                data: GpuSnapshot {
                    utilization_percent: Some(25.0),
                    ..GpuSnapshot::default()
                },
            },
            GpuDevice {
                id: "0000:02:00.0".into(),
                name: "Same GPU".into(),
                state: DataState::Sleeping,
                data: GpuSnapshot::default(),
            },
        ];
        let rows = display(MonitorSection::Gpu, &devices);
        assert_eq!(rows.len(), 8);
        assert_eq!(rows[0].value, format_percent(25.0));
        assert_eq!(rows[4].value, "Sleeping");
        assert_ne!(
            rows[0].group.as_ref().unwrap().label,
            rows[4].group.as_ref().unwrap().label
        );
        let mut settings = MonitorSettings::default();
        settings.set_metric_visible(MonitorSection::Gpu, &rows[0].id, false);
        settings
            .metric_names
            .insert(format!("gpu:{}", rows[4].id), "Second GPU".into());
        assert!(!GpuRequest::new(&settings, &devices[0].id).load);
        assert!(GpuRequest::new(&settings, &devices[1].id).load);
        assert_eq!(
            settings.metric_name(MonitorSection::Gpu, &rows[4].id, "GPU Load"),
            "Second GPU"
        );
        assert_eq!(
            settings.metric_name(MonitorSection::Gpu, &rows[0].id, "GPU Load"),
            "GPU Load"
        );
        settings.hidden_metrics.insert("gpu:gpu-memory-used".into());
        assert!(!settings.metric_visible(MonitorSection::Gpu, &rows[1].id));
        assert!(!settings.metric_visible(MonitorSection::Gpu, &rows[5].id));
        assert!(settings.metric_visible(MonitorSection::Gpu, &rows[2].id));
    }

    #[test]
    fn temperature_and_power_requests_follow_their_own_groups() {
        let mut settings = MonitorSettings::default();
        for preference in &mut settings.sections {
            preference.visible = preference.section == MonitorSection::Power;
        }
        let request = GpuRequest::new(&settings, "0000:01:00.0");
        assert!(request.power);
        assert!(!request.load && !request.memory && !request.frequency && !request.temperature);
        settings.set_metric_visible(
            MonitorSection::Power,
            &metric_id("0000:01:00.0", "power"),
            false,
        );
        assert!(!GpuRequest::new(&settings, "0000:01:00.0").any());
    }
}
