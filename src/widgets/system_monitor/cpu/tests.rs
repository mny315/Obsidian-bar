use super::*;
use std::cell::Cell;

#[derive(Default)]
struct FixtureCpuReader {
    calls: [Cell<u32>; 5],
    frequency_missing: Cell<bool>,
    name_missing: Cell<bool>,
}

impl FixtureCpuReader {
    fn read(&self, index: usize) -> u32 {
        let count = self.calls[index].get() + 1;
        self.calls[index].set(count);
        count
    }

    fn counts(&self) -> [u32; 5] {
        self.calls.each_ref().map(Cell::get)
    }
}

impl CpuReader for FixtureCpuReader {
    fn name(&self) -> Option<Arc<str>> {
        self.read(4);
        (!self.name_missing.get()).then(|| Arc::from("Fixture CPU"))
    }

    fn times(&self) -> Option<CpuTimes> {
        let reading = u64::from(self.read(0));
        Some(CpuTimes {
            total: reading * 100,
            idle: reading * 50,
        })
    }
    fn frequency_mhz(&self) -> Option<f64> {
        let reading = 3500.0 + f64::from(self.read(1));
        (!self.frequency_missing.get()).then_some(reading)
    }
    fn load(&self) -> (Option<f64>, Option<u64>, Option<u64>) {
        let reading = self.read(2);
        (
            Some(f64::from(reading)),
            Some(u64::from(reading)),
            Some(100),
        )
    }
    fn uptime_seconds(&self) -> Option<u64> {
        Some(1000 + u64::from(self.read(3)))
    }
}

#[test]
fn cpu_name_uses_the_first_model_and_supports_arm_descriptions() {
    for (content, expected) in [
        (
            "processor: 0\nmodel name: \tAMD Ryzen 9 9950X 16-Core Processor  \n\nprocessor: 1\nmodel name: Other CPU\n",
            Some("AMD Ryzen 9 9950X"),
        ),
        (
            "processor: 0\nmodel name: Intel(R) Core(TM) i7-12700K\n",
            Some("Intel(R) Core(TM) i7-12700K"),
        ),
        (
            "Hardware: Example board\nProcessor: ARMv7 Processor rev 4 (v7l)\n",
            Some("ARMv7 Processor rev 4 (v7l)"),
        ),
        ("Hardware: Example SoC\n", Some("Example SoC")),
        ("processor: 0\nmodel name: \nHardware: \n", None),
        ("", None),
    ] {
        assert_eq!(read_cpu_name(content.as_bytes()).as_deref(), expected);
    }
}

#[test]
fn cpu_model_hides_core_count_without_damaging_model_numbers_or_intel_core_names() {
    for (name, expected) in [
        ("AMD Ryzen 9 9950X 16-Core Processor", "AMD Ryzen 9 9950X"),
        ("AMD EPYC 9654 96-Core Processor", "AMD EPYC 9654"),
        ("AMD Ryzen 5 3600 6-Core", "AMD Ryzen 5 3600"),
        ("Intel(R) Core(TM) i7-12700K", "Intel(R) Core(TM) i7-12700K"),
        ("ARMv7 Processor rev 4 (v7l)", "ARMv7 Processor rev 4 (v7l)"),
        ("Example CPU -Core Processor", "Example CPU -Core Processor"),
    ] {
        assert_eq!(compact_cpu_name(name), expected);
    }
}

#[test]
fn unavailable_cpu_name_is_not_read_again_on_every_sample() {
    let reader = FixtureCpuReader::default();
    reader.name_missing.set(true);
    let mut sampler = CpuSampler::default();
    let mut snapshot = SystemSnapshot::default();
    for discover in [true, false, false] {
        sampler.sample_with(
            &mut snapshot,
            &MonitorSettings::default(),
            discover,
            &reader,
        );
        assert!(snapshot.cpu_name.is_none());
    }
    assert_eq!(reader.counts()[4], 1);
}

#[test]
fn missing_hidden_sources_are_retried_until_the_metric_can_be_discovered() {
    let reader = FixtureCpuReader::default();
    reader.frequency_missing.set(true);
    let mut settings = MonitorSettings::default();
    settings.set_metric_visible(MonitorSection::Cpu, "cpu-frequency", false);
    let mut sampler = CpuSampler::default();
    let mut snapshot = SystemSnapshot::default();
    sampler.sample_with(&mut snapshot, &settings, true, &reader);
    assert!(snapshot.cpu_frequency_mhz.is_none());
    sampler.sample_with(&mut snapshot, &settings, false, &reader);
    assert_eq!(reader.counts()[1], 2);
    assert!(snapshot.cpu_frequency_mhz.is_none());
    reader.frequency_missing.set(false);
    sampler.sample_with(&mut snapshot, &settings, false, &reader);
    assert_eq!(snapshot.cpu_frequency_mhz, Some(3503.0));
    sampler.sample_with(&mut snapshot, &settings, false, &reader);
    assert_eq!(
        reader.counts()[1],
        3,
        "a discovered hidden source must stop polling"
    );
}

#[test]
fn hidden_cpu_metrics_skip_their_sources_and_keep_readings_available_for_reopening() {
    let reader = FixtureCpuReader::default();
    let mut sampler = CpuSampler::default();
    let mut snapshot = SystemSnapshot {
        ready: true,
        ..SystemSnapshot::default()
    };
    let mut settings = MonitorSettings::default();
    settings.set_metric_visible(MonitorSection::Cpu, "cpu-frequency", false);
    sampler.sample_with(&mut snapshot, &settings, true, &reader);
    assert_eq!(
        reader.counts(),
        [1, 1, 1, 1, 1],
        "initial discovery must read every source"
    );
    assert_eq!(snapshot.cpu_frequency_mhz, Some(3501.0));
    let name = snapshot.cpu_name.clone().unwrap();
    assert_eq!(&*name, "Fixture CPU");

    sampler.sample_with(&mut snapshot, &settings, false, &reader);
    assert_eq!(reader.counts(), [2, 1, 2, 2, 1]);
    assert!(Arc::ptr_eq(&name, snapshot.cpu_name.as_ref().unwrap()));
    assert_eq!(snapshot.cpu_percent, Some(50.0));
    assert_eq!(snapshot.cpu_frequency_mhz, Some(3501.0));
    for id in ["load-average", "processes", "uptime"] {
        settings.set_metric_visible(MonitorSection::Cpu, id, false);
    }
    sampler.sample_with(&mut snapshot, &settings, false, &reader);
    assert_eq!(reader.counts(), [3, 1, 2, 2, 1]);
    assert_eq!(snapshot.cpu_percent, Some(50.0));
    assert_eq!(snapshot.load_average, Some(2.0));
    assert_eq!(snapshot.uptime_seconds, Some(1002));

    settings.set_metric_visible(MonitorSection::Cpu, "processes", true);
    settings.set_metric_visible(MonitorSection::Cpu, "cpu", false);
    sampler.sample_with(&mut snapshot, &settings, false, &reader);
    assert_eq!(
        reader.counts(),
        [4, 1, 3, 2, 1],
        "load and process counts must share one read"
    );
    assert_eq!(snapshot.running_processes, Some(3));
    settings.set_metric_visible(MonitorSection::Cpu, "cpu-frequency", true);
    sampler.sample_with(&mut snapshot, &settings, false, &reader);
    assert_eq!(reader.counts(), [5, 2, 4, 2, 1]);
    assert_eq!(snapshot.cpu_frequency_mhz, Some(3502.0));
    settings.set_metric_visible(MonitorSection::Cpu, "cpu", true);
    sampler.sample_with(&mut snapshot, &settings, false, &reader);
    assert_eq!(reader.counts()[0], 6);
    assert_eq!(
        snapshot.cpu_percent,
        Some(50.0),
        "reopening must retain the current rate interval"
    );
}
