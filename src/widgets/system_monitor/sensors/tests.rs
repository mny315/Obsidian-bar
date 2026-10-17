use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "obsidian-sensors-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("hwmon")).unwrap();
        fs::create_dir_all(root.join("thermal")).unwrap();
        Self(root)
    }

    fn write(&self, path: &str, value: &str) {
        let path = self.0.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, value).unwrap();
    }

    fn sampler(&self) -> HwmonSampler {
        HwmonSampler {
            hwmon_root: self.0.join("hwmon"),
            thermal_root: self.0.join("thermal"),
            ..HwmonSampler::default()
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn cached_metadata_keeps_values_fresh_and_respects_metric_selection() {
    let fixture = Fixture::new();
    fixture.write("hwmon/hwmon0/name", "coretemp");
    fixture.write("hwmon/hwmon0/temp1_label", "Package id 0");
    fixture.write("hwmon/hwmon0/temp1_input", "41000");
    let mut sampler = fixture.sampler();
    let mut settings = MonitorSettings::default();
    let now = Instant::now();
    let first = sampler.sample_at(&settings, true, now);
    let id = &first.temperatures[0].id;
    assert_eq!(first.temperatures[0].label, "CPU Package Temperature");
    assert_eq!(first.temperatures[0].value, 41.0);

    fixture.write("hwmon/hwmon0/temp1_input", "52000");
    fixture.write("hwmon/hwmon0/temp1_label", "Core 0");
    let fresh = sampler.sample_at(&settings, false, now + Duration::from_secs(1));
    assert_eq!(fresh.temperatures[0].value, 52.0);
    assert_eq!(fresh.temperatures[0].label, first.temperatures[0].label);
    assert_eq!(sampler.discovered_at, Some(now));

    settings.set_metric_visible(MonitorSection::Temperatures, id, false);
    fixture.write("hwmon/hwmon0/temp1_input", "invalid");
    let hidden = sampler.sample_at(&settings, false, now + Duration::from_secs(2));
    assert_eq!(hidden.temperatures[0].state, gpu::DataState::Paused);
    assert_eq!(sampler.discovered_at, Some(now));
    settings.set_metric_visible(MonitorSection::Temperatures, id, true);
    let invalid = sampler.sample_at(&settings, false, now + Duration::from_secs(3));
    assert_eq!(invalid.temperatures[0].state, gpu::DataState::Unsupported);
    fixture.write("hwmon/hwmon0/temp1_input", "53000");
    let renamed = sampler.sample_at(&settings, false, now + METADATA_REFRESH);
    assert_eq!(renamed.temperatures[0].id, *id);
    assert_eq!(renamed.temperatures[0].label, "CPU Core 0 Temperature");
    assert_eq!(renamed.temperatures[0].value, 53.0);
}

#[test]
fn hotplug_suspend_resume_and_replacement_invalidate_descriptors_immediately() {
    let fixture = Fixture::new();
    fixture.write("hwmon/hwmon0/name", "coretemp");
    fixture.write("hwmon/hwmon0/temp1_input", "40000");
    fixture.write("hwmon/hwmon0/power/runtime_status", "active");
    fixture.write("thermal/thermal_zone0/type", "x86_pkg_temp");
    fixture.write("thermal/thermal_zone0/temp", "45000");
    let mut sampler = fixture.sampler();
    let settings = MonitorSettings::default();
    let now = Instant::now();
    let first = sampler.sample_at(&settings, false, now);
    assert_eq!(
        first.temperatures.len(),
        1,
        "do not duplicate CPU thermal zones"
    );

    fixture.write("hwmon/hwmon0/power/runtime_status", "suspended");
    let asleep = sampler.sample_at(&settings, false, now);
    assert_eq!(asleep.temperatures.len(), 1);
    assert!(asleep.temperatures[0].id.contains("thermal_zone0"));
    fixture.write("hwmon/hwmon0/power/runtime_status", "active");
    let awake = sampler.sample_at(&settings, false, now);
    assert_eq!(awake.temperatures[0].id, first.temperatures[0].id);

    fixture.write("hwmon/hwmon1/name", "jc42");
    fixture.write("hwmon/hwmon1/temp1_input", "32000");
    let plugged = sampler.sample_at(&settings, false, now);
    assert_eq!(plugged.temperatures.len(), 2);
    fs::rename(fixture.0.join("hwmon/hwmon1"), fixture.0.join("old-device")).unwrap();
    fixture.write("hwmon/hwmon1/name", "nvme");
    fixture.write("hwmon/hwmon1/device/serial", "replacement");
    fixture.write("hwmon/hwmon1/temp1_input", "35000");
    let replaced = sampler.sample_at(&settings, false, now);
    assert!(
        replaced
            .temperatures
            .iter()
            .any(|sensor| sensor.id == "nvme:replacement:temp1")
    );
    fs::rename(
        fixture.0.join("hwmon/hwmon1"),
        fixture.0.join("unplugged-device"),
    )
    .unwrap();
    assert_eq!(
        sampler.sample_at(&settings, false, now).temperatures.len(),
        1
    );
}

#[test]
fn discovery_preserves_sensor_priorities_and_deduplicates_special_devices() {
    let fixture = Fixture::new();
    fixture.write("hwmon/hwmon0/name", "nvme");
    fixture.write("hwmon/hwmon0/temp1_input", "99000");
    fixture.write("hwmon/hwmon0/temp1_label", "Sensor 1");
    fixture.write("hwmon/hwmon0/temp2_input", "35000");
    fixture.write("hwmon/hwmon0/temp2_label", "Composite");
    fixture.write("hwmon/hwmon0/power1_input", "99000000");
    fixture.write("hwmon/hwmon0/power1_average", "12000000");
    fixture.write("hwmon/hwmon0/power2_input", "3000000");
    fixture.write("hwmon/hwmon1/name", "nvidia");
    fixture.write("hwmon/hwmon1/temp1_input", "50000");
    fixture.write("hwmon/hwmon2/name", "coretemp");
    fixture.write("hwmon/hwmon2/temp1_input", "45000");
    fixture.write("hwmon/hwmon2/fan1_input", "1100");
    fixture.write("hwmon/hwmon3/name", "coretemp");
    fixture.write("hwmon/hwmon3/temp1_input", "46000");
    fixture.write("thermal/thermal_zone0/type", "gpu-thermal");
    fixture.write("thermal/thermal_zone0/temp", "51000");
    let snapshot = fixture.sampler().sample(&MonitorSettings::default(), false);
    assert_eq!(snapshot.temperatures.len(), 3);
    assert!(snapshot.temperatures[0].label.starts_with("CPU 1"));
    assert!(snapshot.temperatures[1].label.starts_with("CPU 2"));
    assert_eq!(snapshot.temperatures[2].value, 35.0);
    assert_eq!(
        snapshot
            .power
            .iter()
            .map(|sensor| sensor.value)
            .collect::<Vec<_>>(),
        [12.0, 3.0]
    );
    assert_eq!(snapshot.fans[0].value, 1100.0);
}
