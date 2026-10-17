//! System battery capacity and aggregation, separate from peripheral receivers.
use super::super::{
    BatterySnapshot,
    files::{read_number, read_trimmed, sorted_directory_paths},
};
use std::path::Path;

pub(in super::super) fn read_battery_snapshot() -> Option<BatterySnapshot> {
    read_battery_snapshot_at(Path::new("/sys/class/power_supply"))
}

pub(in super::super) fn battery_capacity(supply: &Path) -> Option<(f64, Option<f64>)> {
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

pub(in super::super) fn read_battery_snapshot_at(power_supplies: &Path) -> Option<BatterySnapshot> {
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
