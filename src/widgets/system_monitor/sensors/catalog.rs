//! Sensor descriptors discovered independently from changing readings.
use super::*;

#[derive(Default)]
pub(super) struct SensorCatalog {
    pub(super) complete: bool,
    temperatures: Vec<SensorDescriptor>,
    power: Vec<SensorDescriptor>,
    fans: Vec<SensorDescriptor>,
}

struct SensorQuery {
    path: PathBuf,
    section: MonitorSection,
    divisor: f64,
    range: std::ops::RangeInclusive<f64>,
}

struct SensorDescriptor {
    id: String,
    label: String,
    query: SensorQuery,
}

impl SensorDescriptor {
    fn read(&self, settings: &MonitorSettings, discover: bool) -> SensorReading {
        let (value, state) = sensor_value(
            &self.query.path,
            settings,
            self.query.section,
            &self.id,
            discover,
            self.query.divisor,
            self.query.range.clone(),
        );
        SensorReading {
            id: self.id.clone(),
            label: self.label.clone(),
            value,
            state,
        }
    }
}

impl SensorCatalog {
    pub(super) fn sample(&self, settings: &MonitorSettings, discover: bool) -> HwmonSnapshot {
        let read = |items: &[SensorDescriptor]| {
            items
                .iter()
                .map(|sensor| sensor.read(settings, discover))
                .collect()
        };
        HwmonSnapshot {
            temperatures: read(&self.temperatures),
            power: read(&self.power),
            fans: read(&self.fans),
        }
    }
}

pub(super) fn discover(hwmon: &[SourceStamp], thermal: &[SourceStamp]) -> SensorCatalog {
    let mut complete = true;
    let mut temperatures = Vec::new();
    let mut power = Vec::new();
    let mut fans = Vec::new();

    let devices = hwmon
        .iter()
        .filter_map(|source| {
            let directory = source.path.clone();
            let chip = read_trimmed(directory.join("name")).unwrap_or_else(|| {
                complete = false;
                "sensor".to_owned()
            });
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
        if files.is_empty() {
            complete = false;
        }
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
                let query = SensorQuery {
                    path: path.clone(),
                    section: MonitorSection::Temperatures,
                    divisor: 1000.0,
                    range: -40.0..=200.0,
                };
                let label = sensor_display_label(
                    &device_label,
                    &chip,
                    read_trimmed(directory.join(format!("temp{index}_label"))).as_deref(),
                    SensorKind::Temperature,
                    &index,
                );
                temperatures.push(SensorDescriptor { id, label, query });
            }
            if let Some(index) = sensor_index(name, "fan", "_input") {
                let id = format!("{device_id}:fan{index}");
                let query = SensorQuery {
                    path: path.clone(),
                    section: MonitorSection::Cooling,
                    divisor: 1.0,
                    range: 0.0..=100_000.0,
                };
                let label = sensor_display_label(
                    &device_label,
                    &chip,
                    read_trimmed(directory.join(format!("fan{index}_label"))).as_deref(),
                    SensorKind::Fan,
                    &index,
                );
                fans.push(SensorDescriptor { id, label, query });
            }
            if let Some(index) = sensor_index(name, "power", "_average")
                && !is_gpu
            {
                let id = format!("{device_id}:power{index}");
                let query = SensorQuery {
                    path: path.clone(),
                    section: MonitorSection::Power,
                    divisor: 1_000_000.0,
                    range: 0.0..=10_000.0,
                };
                power_indices.insert(index.clone());
                power.push(SensorDescriptor {
                    id,
                    label: sensor_display_label(
                        &device_label,
                        &chip,
                        read_trimmed(directory.join(format!("power{index}_label"))).as_deref(),
                        SensorKind::Power,
                        &index,
                    ),
                    query,
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
                let query = SensorQuery {
                    path: path.clone(),
                    section: MonitorSection::Power,
                    divisor: 1_000_000.0,
                    range: 0.0..=10_000.0,
                };
                power.push(SensorDescriptor {
                    id,
                    label: sensor_display_label(
                        &device_label,
                        &chip,
                        read_trimmed(directory.join(format!("power{index}_label"))).as_deref(),
                        SensorKind::Power,
                        &index,
                    ),
                    query,
                });
            }
        }
    }

    append_thermal_zone_temperatures(&mut temperatures, thermal, &mut complete);
    temperatures.sort_by_key(|sensor| temperature_priority(&sensor.label));
    power.sort_by_key(|sensor| sensor_priority(&sensor.label));
    fans.sort_by_key(|sensor| sensor_priority(&sensor.label));
    uniquify_sensor_labels(&mut temperatures);
    uniquify_sensor_labels(&mut power);
    uniquify_sensor_labels(&mut fans);

    SensorCatalog {
        complete,
        temperatures,
        power,
        fans,
    }
}

fn append_thermal_zone_temperatures(
    temperatures: &mut Vec<SensorDescriptor>,
    thermal: &[SourceStamp],
    complete: &mut bool,
) {
    for source in thermal {
        let directory = &source.path;
        let Some(name) = directory.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let kind = read_trimmed(directory.join("type")).unwrap_or_else(|| {
            *complete = false;
            name.to_owned()
        });
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
        let id = format!("{}:temp", directory.display());
        let query = SensorQuery {
            path: directory.join("temp"),
            section: MonitorSection::Temperatures,
            divisor: 1000.0,
            range: -40.0..=200.0,
        };
        let mut label = thermal_zone_device_label(&kind);
        if !label.to_ascii_lowercase().contains("temperature") {
            label.push_str(" Temperature");
        }
        temperatures.push(SensorDescriptor { id, label, query });
    }
}

fn uniquify_sensor_labels(readings: &mut [SensorDescriptor]) {
    let mut counts = HashMap::<String, usize>::new();
    for reading in readings {
        let count = counts.entry(reading.label.clone()).or_default();
        *count += 1;
        if *count > 1 {
            reading.label = format!("{} {}", reading.label, count);
        }
    }
}
