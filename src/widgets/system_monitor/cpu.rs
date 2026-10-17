//! CPU readings grouped by their underlying source, with hidden metrics paused.
use super::files::{read_number, sorted_directory_paths};
use super::{MonitorSection, MonitorSettings, SystemSnapshot};
use std::{
    cell::OnceCell,
    fs,
    io::{BufRead, BufReader},
    path::Path,
    sync::Arc,
};

#[derive(Clone, Copy)]
pub(super) struct CpuTimes {
    pub(super) total: u64,
    pub(super) idle: u64,
}

#[derive(Default)]
pub(super) struct CpuSampler {
    previous: Option<CpuTimes>,
    name: OnceCell<Option<Arc<str>>>,
}

trait CpuReader {
    fn name(&self) -> Option<Arc<str>>;
    fn times(&self) -> Option<CpuTimes>;
    fn frequency_mhz(&self) -> Option<f64>;
    fn load(&self) -> (Option<f64>, Option<u64>, Option<u64>);
    fn uptime_seconds(&self) -> Option<u64>;
}

impl CpuSampler {
    pub(super) fn sample(
        &mut self,
        snapshot: &mut SystemSnapshot,
        settings: &MonitorSettings,
        discover: bool,
    ) {
        self.sample_with(snapshot, settings, discover, &ProcCpuReader);
    }

    fn sample_with(
        &mut self,
        snapshot: &mut SystemSnapshot,
        settings: &MonitorSettings,
        discover: bool,
        reader: &impl CpuReader,
    ) {
        snapshot.cpu_name = self.name.get_or_init(|| reader.name()).clone();
        let wants = |id| discover || settings.metric_visible(MonitorSection::Cpu, id);
        // Keep rate counters warm while the section is active. Reopening the
        // usage row must show the current interval, not an average of the whole
        // time it was hidden.
        snapshot.cpu_percent = reader.times().map(|current| {
            let percent = self
                .previous
                .and_then(|previous| cpu_usage(previous, current))
                .unwrap_or(0.0);
            self.previous = Some(current);
            percent
        });
        // Missing readings still retry so a newly available source can appear
        // in settings even if that metric was hidden on a previous machine.
        if wants("cpu-frequency") || snapshot.cpu_frequency_mhz.is_none() {
            snapshot.cpu_frequency_mhz = reader.frequency_mhz();
        }
        // Load average and process counts share the same /proc/loadavg read.
        if wants("load-average")
            || wants("processes")
            || snapshot.load_average.is_none()
            || snapshot.running_processes.is_none()
            || snapshot.total_processes.is_none()
        {
            (
                snapshot.load_average,
                snapshot.running_processes,
                snapshot.total_processes,
            ) = reader.load();
        }
        if wants("uptime") || snapshot.uptime_seconds.is_none() {
            snapshot.uptime_seconds = reader.uptime_seconds();
        }
    }
}

struct ProcCpuReader;

impl CpuReader for ProcCpuReader {
    fn name(&self) -> Option<Arc<str>> {
        read_cpu_name(BufReader::new(fs::File::open("/proc/cpuinfo").ok()?))
    }

    fn times(&self) -> Option<CpuTimes> {
        let file = fs::File::open("/proc/stat").ok()?;
        let mut reader = BufReader::with_capacity(256, file);
        let mut line = String::new();
        // The aggregate CPU row precedes per-core and interrupt counters.
        while reader.read_line(&mut line).ok()? != 0 {
            if line.starts_with("cpu ") {
                return parse_cpu_times(&line);
            }
            line.clear();
        }
        None
    }

    fn frequency_mhz(&self) -> Option<f64> {
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
        (!frequencies.is_empty())
            .then(|| frequencies.iter().sum::<f64>() / frequencies.len() as f64)
    }

    fn load(&self) -> (Option<f64>, Option<u64>, Option<u64>) {
        let Ok(content) = fs::read_to_string("/proc/loadavg") else {
            return (None, None, None);
        };
        let mut fields = content.split_whitespace();
        let load = fields.next().and_then(|value| value.parse().ok());
        let processes = fields.nth(2).and_then(|value| value.split_once('/'));
        let running = processes.and_then(|(running, _)| running.parse().ok());
        let total = processes.and_then(|(_, total)| total.parse().ok());
        (load, running, total)
    }

    fn uptime_seconds(&self) -> Option<u64> {
        fs::read_to_string("/proc/uptime")
            .ok()?
            .split_whitespace()
            .next()?
            .parse::<f64>()
            .ok()
            .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
            .map(|seconds| seconds as u64)
    }
}

fn read_cpu_name(reader: impl BufRead) -> Option<Arc<str>> {
    let mut processor = None;
    let mut hardware = None;
    for line in reader.lines().map_while(Result::ok) {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            // Stop after the first model: subsequent cores repeat its name.
            "model name" => return Some(Arc::from(compact_cpu_name(value))),
            "Processor" if processor.is_none() => processor = Some(Arc::from(value)),
            "Hardware" if hardware.is_none() => hardware = Some(Arc::from(value)),
            _ => {}
        }
    }
    processor.or(hardware)
}

fn compact_cpu_name(name: &str) -> &str {
    let Some((model, cores)) = name
        .strip_suffix(" Processor")
        .unwrap_or(name)
        .rsplit_once(' ')
    else {
        return name;
    };
    match cores.strip_suffix("-Core") {
        Some(count) if !count.is_empty() && count.bytes().all(|byte| byte.is_ascii_digit()) => {
            model.trim_end()
        }
        _ => name,
    }
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

#[cfg(test)]
mod tests;
