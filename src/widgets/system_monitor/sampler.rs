use super::batteries::read_battery_snapshot;
use super::cpu::CpuSampler;
use super::gpu;
use super::memory::read_memory_usage;
use super::metrics::metric_options;
use super::network::{NetworkCounters, network_rates, read_network_snapshot};
use super::sensors::HwmonSampler;
use super::storage::read_disk_snapshots;
use super::{MonitorSection, MonitorSettings, SystemSnapshot};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub(super) struct SystemSampler {
    pub(super) cpu: CpuSampler,
    pub(super) previous_network: Option<NetworkCounters>,
    pub(super) gpu_sampling_enabled: Arc<AtomicBool>,
    pub(super) cached: Arc<SystemSnapshot>,
    pub(super) gpus: gpu::GpuSampler,
    pub(super) batteries: super::batteries::BatterySampler,
    pub(super) hwmon: HwmonSampler,
}

impl Default for SystemSampler {
    fn default() -> Self {
        Self {
            cpu: CpuSampler::default(),
            previous_network: None,
            gpu_sampling_enabled: Arc::new(AtomicBool::new(true)),
            cached: Arc::default(),
            gpus: gpu::GpuSampler::default(),
            batteries: super::batteries::BatterySampler::default(),
            hwmon: HwmonSampler::default(),
        }
    }
}

impl SystemSampler {
    #[cfg(test)]
    pub(super) fn sample(&mut self) -> Arc<SystemSnapshot> {
        self.sample_with(&MonitorSettings::default(), true)
    }

    pub(super) fn sample_with(
        &mut self,
        settings: &MonitorSettings,
        visible: bool,
    ) -> Arc<SystemSnapshot> {
        let first = !self.cached.ready;
        let wants = |section| {
            if first {
                return true;
            }
            if !settings
                .sections
                .iter()
                .any(|item| item.section == section && item.visible)
            {
                return false;
            }
            if settings.hidden_metrics.is_empty() {
                return true;
            }
            let options = metric_options(section, &self.cached);
            options.is_empty()
                || options
                    .iter()
                    .any(|metric| settings.metric_visible(section, &metric.id))
        };
        let [cpu, memory, storage, battery, network] = [
            MonitorSection::Cpu,
            MonitorSection::Memory,
            MonitorSection::Storage,
            MonitorSection::Battery,
            MonitorSection::Network,
        ]
        .map(wants);
        // Keep scalar readings for paused sections, and copy collections only
        // when they will be retained. Cloning the whole previous snapshot would
        // also duplicate all sensor and GPU data immediately replaced below.
        let mut next = self.cached.for_sample(storage, battery);
        if cpu {
            self.cpu.sample(&mut next, settings, first);
        } else {
            next.section_states
                .insert(MonitorSection::Cpu, gpu::DataState::Paused);
        }
        if memory {
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
        if storage {
            next.disks = read_disk_snapshots();
        } else {
            next.section_states
                .insert(MonitorSection::Storage, gpu::DataState::Paused);
        }
        if battery {
            next.battery = read_battery_snapshot();
            next.device_batteries = self.batteries.sample();
        } else {
            next.section_states
                .insert(MonitorSection::Battery, gpu::DataState::Paused);
        }
        let sample_network = network;
        let (choices, counters) =
            read_network_snapshot(settings.network_interface.as_deref(), sample_network);
        next.network_choices = choices;
        if sample_network {
            if let Some(current) = counters {
                let rates = self
                    .previous_network
                    .as_ref()
                    .and_then(|previous| network_rates(previous, &current))
                    .unwrap_or_default();
                next.network_available = true;
                next.network_interfaces.clone_from(&current.interfaces);
                next.download_bytes_per_second = rates.0;
                next.upload_bytes_per_second = rates.1;
                next.network_received_bytes = current.received;
                next.network_transmitted_bytes = current.transmitted;
                self.previous_network = Some(current);
            } else {
                self.previous_network = None;
                next.network_available = false;
                next.network_interfaces
                    .clone_from(&self.cached.network_interfaces);
                next.section_states
                    .insert(MonitorSection::Network, gpu::DataState::Unavailable);
            }
        } else {
            next.network_interfaces
                .clone_from(&self.cached.network_interfaces);
            next.section_states
                .insert(MonitorSection::Network, gpu::DataState::Paused);
        }
        let hwmon = self.hwmon.sample(settings, first);
        next.temperatures = hwmon.temperatures;
        next.power = hwmon.power;
        next.fans = hwmon.fans;
        next.gpus = self
            .gpus
            .sample(settings, visible, &self.gpu_sampling_enabled);
        self.cached = Arc::new(next);
        Arc::clone(&self.cached)
    }
}

impl SystemSnapshot {
    pub(super) fn for_sample(&self, storage: bool, battery: bool) -> Self {
        Self {
            ready: true,
            cpu_name: self.cpu_name.clone(),
            cpu_percent: self.cpu_percent,
            cpu_frequency_mhz: self.cpu_frequency_mhz,
            load_average: self.load_average,
            running_processes: self.running_processes,
            total_processes: self.total_processes,
            uptime_seconds: self.uptime_seconds,
            memory_used: self.memory_used,
            memory_total: self.memory_total,
            swap_used: self.swap_used,
            swap_total: self.swap_total,
            network_available: self.network_available,
            download_bytes_per_second: self.download_bytes_per_second,
            upload_bytes_per_second: self.upload_bytes_per_second,
            network_received_bytes: self.network_received_bytes,
            network_transmitted_bytes: self.network_transmitted_bytes,
            disks: if storage {
                Vec::new()
            } else {
                self.disks.clone()
            },
            battery: if battery { None } else { self.battery.clone() },
            device_batteries: if battery {
                Vec::new()
            } else {
                self.device_batteries.clone()
            },
            ..Self::default()
        }
    }
}
