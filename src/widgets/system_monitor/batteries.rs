//! Peripheral batteries. Slow receiver queries run away from the monitor sampler.
use super::sampler::{battery_capacity, read_trimmed, sorted_directory_paths};
use gio::glib::{
    self,
    variant::{FromVariant, ObjectPath, ToVariant},
};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Mutex, mpsc},
    time::{Duration, Instant},
};

mod hid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PowerOffTarget {
    SteamController { id: String },
}

impl PowerOffTarget {
    pub(super) fn turn_off(&self) -> Result<(), String> {
        let result = match self {
            Self::SteamController { id } => hid::power_off_steam_controller(id),
        };
        result.map_err(|error| {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                "Access needed: apply the receiver udev rules and reconnect it".into()
            } else {
                error.to_string()
            }
        })
    }
}

#[derive(Clone, Debug)]
pub(super) struct DeviceBattery {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) percent: Option<f64>,
    pub(super) detail: String,
    pub(super) power_off: Option<PowerOffTarget>,
}

impl DeviceBattery {
    pub(super) fn display_value(&self) -> String {
        match self.percent {
            Some(percent) if self.detail == "Charging" => format!("{percent:.0}% · Charging"),
            Some(percent) => format!("{percent:.0}%"),
            None => self.detail.clone(),
        }
    }
}

#[derive(Default)]
pub(super) struct BatterySampler {
    cached: Vec<DeviceBattery>,
    pending: Option<Mutex<mpsc::Receiver<Vec<DeviceBattery>>>>,
    requested: Option<Instant>,
}

impl BatterySampler {
    pub(super) fn sample(&mut self) -> Vec<DeviceBattery> {
        if let Some(receiver) = &self.pending {
            let result = receiver
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .try_recv();
            match result {
                Ok(mut batteries) => {
                    for battery in &mut batteries {
                        if battery.percent.is_none()
                            && let Some(previous) =
                                self.cached.iter().find(|old| old.id == battery.id)
                        {
                            // Keep the full name when a sleeping Bolt device
                            // only exposes its shortened receiver codename.
                            battery.name.clone_from(&previous.name);
                        }
                    }
                    self.cached = batteries;
                    self.pending = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => self.pending = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if self.pending.is_none()
            && self
                .requested
                .is_none_or(|at| at.elapsed() >= Duration::from_secs(15))
        {
            let (sender, receiver) = mpsc::channel();
            self.pending = Some(Mutex::new(receiver));
            self.requested = Some(Instant::now());
            std::thread::spawn(move || {
                let _ = sender.send(read_devices());
            });
        }
        self.cached.clone()
    }
}

fn read_devices() -> Vec<DeviceBattery> {
    let mut batteries = read_sysfs(Path::new("/sys/class/power_supply"));
    read_upower(&mut batteries);
    batteries.extend(hid::read_devices());
    unique_devices(batteries)
}

fn unique_devices(batteries: Vec<DeviceBattery>) -> Vec<DeviceBattery> {
    // The same paired device can be visible through multiple receivers. Names
    // may differ (a sleeping receiver only knows a codename), so deduplication
    // must precede the name sort and prefer an actual reading over a placeholder.
    let quality = |battery: &DeviceBattery| {
        if battery.percent.is_some() {
            2
        } else if matches!(
            battery.detail.as_str(),
            "Critical" | "Low" | "Normal" | "High" | "Full" | "Charging"
        ) {
            1
        } else {
            0
        }
    };
    let mut unique: HashMap<String, DeviceBattery> = HashMap::new();
    for battery in batteries {
        match unique.entry(battery.id.clone()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(battery);
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                if quality(&battery) > quality(entry.get()) {
                    entry.insert(battery);
                }
            }
        }
    }
    let mut batteries = unique.into_values().collect::<Vec<_>>();
    batteries.sort_by_cached_key(|battery| (battery.name.to_lowercase(), battery.id.clone()));
    batteries
}

fn read_sysfs(root: &Path) -> Vec<DeviceBattery> {
    sorted_directory_paths(root)
        .into_iter()
        .filter_map(|path| {
            if read_trimmed(path.join("type")).as_deref() != Some("Battery")
                || read_trimmed(path.join("scope")).as_deref() != Some("Device")
                || read_trimmed(path.join("present")).as_deref() == Some("0")
                || read_trimmed(path.join("online")).as_deref() == Some("0")
            {
                return None;
            }
            let native = path.file_name()?.to_string_lossy().into_owned();
            let name = read_trimmed(path.join("model_name"))
                .or_else(|| read_trimmed(path.join("device/name")))
                .unwrap_or_else(|| native.clone());
            let percent = battery_capacity(&path).map(|capacity| capacity.0);
            let detail = if percent.is_some() {
                read_trimmed(path.join("status"))
            } else {
                read_trimmed(path.join("capacity_level"))
            }
            .unwrap_or_else(|| "Unavailable".into());
            Some(DeviceBattery {
                id: format!("device:{native}"),
                name,
                percent,
                detail,
                power_off: None,
            })
        })
        .collect()
}

type Properties = HashMap<String, glib::Variant>;
fn property<T: FromVariant>(props: &Properties, key: &str) -> Option<T> {
    crate::widgets::dbus::variant_value(props.get(key)?)
}

// UPower covers Bluetooth Battery1 as well as userspace battery providers.
// https://upower.freedesktop.org/docs/Device.html
fn read_upower(batteries: &mut Vec<DeviceBattery>) {
    let Ok(bus) = gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE) else {
        return;
    };
    let call = |path: &str, interface: &str, method: &str, args: Option<&glib::Variant>| {
        bus.call_sync(
            Some("org.freedesktop.UPower"),
            path,
            interface,
            method,
            args,
            None,
            gio::DBusCallFlags::NO_AUTO_START,
            500,
            gio::Cancellable::NONE,
        )
        .ok()
    };
    let Some(reply) = call(
        "/org/freedesktop/UPower",
        "org.freedesktop.UPower",
        "EnumerateDevices",
        None,
    ) else {
        return;
    };
    let Some((paths,)) = reply.get::<(Vec<ObjectPath>,)>() else {
        return;
    };
    for path in paths.into_iter().take(64) {
        let args = ("org.freedesktop.UPower.Device",).to_variant();
        let Some(reply) = call(
            path.as_str(),
            "org.freedesktop.DBus.Properties",
            "GetAll",
            Some(&args),
        ) else {
            continue;
        };
        let Some((props,)) = reply.get::<(Properties,)>() else {
            continue;
        };
        let Some(battery) = from_upower(path.as_str(), &props) else {
            continue;
        };
        if let Some(existing) = batteries
            .iter_mut()
            .find(|existing| existing.id == battery.id)
        {
            *existing = battery;
        } else {
            batteries.push(battery);
        }
    }
}

fn from_upower(path: &str, props: &Properties) -> Option<DeviceBattery> {
    if path.ends_with("/DisplayDevice")
        || property::<bool>(props, "PowerSupply").unwrap_or(false)
        || !property::<bool>(props, "IsPresent").unwrap_or(false)
        || property::<u32>(props, "Type").is_none_or(|kind| kind <= 1)
    {
        return None;
    }
    let native = property::<String>(props, "NativePath")
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| path.into());
    let native = Path::new(&native).file_name()?.to_string_lossy();
    // UPower's None (1) means that Percentage is authoritative. Unknown (0)
    // is a coarse device with no reading, not an empty battery. Older providers
    // without BatteryLevel still expose Percentage directly.
    let level = property::<u32>(props, "BatteryLevel").unwrap_or(1);
    let percent = (level == 1)
        .then(|| property::<f64>(props, "Percentage"))
        .flatten()
        .filter(|p| (0.0..=100.0).contains(p));
    let detail = if percent.is_none() {
        match level {
            3 => "Low",
            4 => "Critical",
            6 => "Normal",
            7 => "High",
            8 => "Full",
            _ => "Unavailable",
        }
    } else {
        match property::<u32>(props, "State") {
            Some(1) => "Charging",
            Some(4) => "Full",
            _ => "Discharging",
        }
    }
    .into();
    let name = property::<String>(props, "Model")
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "Wireless device".into());
    Some(DeviceBattery {
        id: format!("device:{native}"),
        name,
        percent,
        detail,
        power_off: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_receivers_keep_the_best_reading_even_when_names_differ() {
        let battery = |id: &str, name: &str, percent, detail: &str| DeviceBattery {
            id: id.into(),
            name: name.into(),
            percent,
            detail: detail.into(),
            power_off: None,
        };
        for reverse in [false, true] {
            let mut readings = vec![
                battery("mouse", "Z sleeping mouse", None, "Asleep / unavailable"),
                battery("keyboard", "M keyboard", Some(80.0), "Discharging"),
                battery("mouse", "A awake mouse", Some(55.0), "Discharging"),
                battery("coarse", "C sleeping", None, "Asleep / unavailable"),
                battery("coarse", "C awake", None, "Normal"),
            ];
            if reverse {
                readings.reverse();
            }
            let readings = unique_devices(readings);
            assert_eq!(readings.len(), 3);
            assert_eq!(readings[0].name, "A awake mouse");
            assert_eq!(readings[0].percent, Some(55.0));
            assert_eq!(readings[1].detail, "Normal");
        }
    }

    #[test]
    fn upower_keeps_coarse_levels_and_excludes_system_batteries() {
        let mut props = Properties::from([
            ("Type".into(), 5_u32.to_variant()),
            ("IsPresent".into(), true.to_variant()),
            ("PowerSupply".into(), false.to_variant()),
            ("Percentage".into(), 55_f64.to_variant()),
            ("BatteryLevel".into(), 6_u32.to_variant()),
            ("NativePath".into(), "mouse".to_variant()),
        ]);
        let battery = from_upower("/mouse", &props).unwrap();
        assert_eq!(battery.percent, None);
        assert_eq!(battery.display_value(), "Normal");
        props.insert("BatteryLevel".into(), 1_u32.to_variant());
        assert_eq!(from_upower("/mouse", &props).unwrap().percent, Some(55.0));
        props.remove("BatteryLevel");
        assert_eq!(from_upower("/mouse", &props).unwrap().percent, Some(55.0));
        for (level, label) in [(0, "Unavailable"), (3, "Low"), (4, "Critical")] {
            props.insert("BatteryLevel".into(), (level as u32).to_variant());
            let battery = from_upower("/mouse", &props).unwrap();
            assert_eq!(battery.percent, None);
            assert_eq!(battery.display_value(), label);
        }
        props.insert("BatteryLevel".into(), 1_u32.to_variant());
        props.insert("Percentage".into(), f64::NAN.to_variant());
        assert_eq!(from_upower("/mouse", &props).unwrap().percent, None);
        props.insert("PowerSupply".into(), true.to_variant());
        assert!(from_upower("/mouse", &props).is_none());
    }

    #[test]
    fn sysfs_reads_peripherals_without_averaging_them() {
        let root =
            std::env::temp_dir().join(format!("obsidian-peripherals-{}", std::process::id()));
        for (name, fields) in [
            (
                "mouse",
                vec![
                    ("scope", "Device"),
                    ("capacity", "73"),
                    ("model_name", "Test Mouse"),
                ],
            ),
            (
                "controller",
                vec![("scope", "Device"), ("capacity_level", "Low")],
            ),
            (
                "absent",
                vec![("scope", "Device"), ("present", "0"), ("capacity", "80")],
            ),
            (
                "offline",
                vec![("scope", "Device"), ("online", "0"), ("capacity", "80")],
            ),
            ("BAT0", vec![("scope", "System"), ("capacity", "95")]),
        ] {
            let path = root.join(name);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("type"), "Battery").unwrap();
            for (field, value) in fields {
                std::fs::write(path.join(field), value).unwrap();
            }
        }
        let devices = read_sysfs(&root);
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].display_value(), "Low");
        assert_eq!(devices[1].display_value(), "73%");
        assert_eq!(devices[1].name, "Test Mouse");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "queries connected peripheral receivers and the system bus"]
    fn live_device_batteries() {
        for battery in read_devices() {
            println!(
                "{}: {} [{}]",
                battery.name,
                battery.display_value(),
                battery.id
            );
        }
    }
}
