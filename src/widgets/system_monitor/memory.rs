//! Physical memory and swap readings from procfs.
use std::fs;

#[derive(Clone, Copy)]
pub(super) struct MemoryUsage {
    pub(super) used: u64,
    pub(super) total: u64,
    pub(super) swap_used: u64,
    pub(super) swap_total: u64,
}

pub(super) fn read_memory_usage() -> Option<MemoryUsage> {
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
