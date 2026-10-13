//! Battery-only receiver protocols; no pairing, input mappings or device settings.
use super::{DeviceBattery, read_trimmed, sorted_directory_paths};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::Path,
    sync::atomic::{AtomicU8, Ordering},
    time::{Duration, Instant},
};

fn open(path: &Path, writable: bool) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(writable)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
}

fn receive(file: &mut File, deadline: Instant) -> Option<Vec<u8>> {
    loop {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        let mut poll = libc::pollfd {
            fd: file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll points to one initialized descriptor, valid for this call.
        let ready =
            unsafe { libc::poll(&mut poll, 1, remaining.as_millis().clamp(1, 1000) as i32) };
        if ready < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if ready == 0 {
            continue;
        }
        if ready < 0 || poll.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return None;
        }
        let mut data = [0_u8; 64];
        match file.read(&mut data) {
            Ok(0) => return None,
            Ok(count) => return Some(data[..count].to_vec()),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(_) => return None,
        }
    }
}

fn unavailable(id: String, name: &str, detail: &str) -> DeviceBattery {
    DeviceBattery {
        id,
        name: name.into(),
        percent: None,
        detail: detail.into(),
    }
}

pub(super) fn read_devices() -> Vec<DeviceBattery> {
    let mut batteries = Vec::new();
    let mut steam = Vec::new();
    for path in sorted_directory_paths(Path::new("/sys/class/hidraw")) {
        let Some(event) = read_trimmed(path.join("device/uevent")) else {
            continue;
        };
        let field = |key| event.lines().find_map(|line| line.strip_prefix(key));
        let Some(id) = field("HID_ID=") else {
            continue;
        };
        let phys = field("HID_PHYS=").unwrap_or("");
        let serial = field("HID_UNIQ=").filter(|s| !s.is_empty()).unwrap_or(phys);
        let Some(filename) = path.file_name() else {
            continue;
        };
        let node = Path::new("/dev").join(filename);
        if id == "0003:0000046D:0000C548" && phys.ends_with("/input2") {
            let key = format!("bolt:{serial}");
            match open(&node, true) {
                Ok(mut file) => batteries.extend(read_bolt(&mut file, &key)),
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                    batteries.push(unavailable(key, "Logitech Bolt", "Access needed"))
                }
                Err(_) => {}
            }
        } else if matches!(id, "0003:000003F0:000006BE" | "0003:000003F0:000002CC") {
            let key = format!("hyperx:{serial}");
            match open(&node, true) {
                Ok(mut file) => {
                    if let Some(battery) = read_hyperx(&mut file, key) {
                        batteries.push(battery);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => batteries.push(
                    unavailable(key, "HyperX Cloud III S Wireless", "Access needed"),
                ),
                Err(_) => {}
            }
        } else if matches!(id, "0003:000028DE:00001304" | "0003:000028DE:00001305")
            && let Some(slot) = phys
                .rsplit_once("/input")
                .and_then(|(_, n)| n.parse::<u8>().ok())
                .filter(|n| (2..=5).contains(n))
        {
            // Each Puck interface is a distinct controller slot. Opening hidraw
            // read-only gives us our own event queue without touching Steam's.
            if let Ok(file) = open(&node, false) {
                steam.push(SteamSlot {
                    file,
                    id: format!("steam:{serial}:{}", slot - 1),
                    slot: slot - 1,
                    battery: None,
                });
            }
        }
    }
    batteries.extend(read_steam(&mut steam));
    batteries
}

// HID++ 2.0 feature discovery / battery queries, as documented by Solaar:
// https://github.com/pwr-Solaar/Solaar/blob/master/lib/logitech_receiver/hidpp20.py
// Receiver pairing information is read from register 0xB5, never modified.
fn hidpp_function(device: u8, function: u8) -> u8 {
    // A sleeping device may reply after a timeout. Rotate IDs 2..=6 to keep
    // consecutive queries distinct without borrowing the kernel's ID 1,
    // OpenRGB's ID 7, or Solaar's IDs 8..=15 (older versions rotate them).
    static REQUEST_ID: AtomicU8 = AtomicU8::new(2);
    if device == 0xff {
        function
    } else {
        let id = REQUEST_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| {
                Some(if id == 6 { 2 } else { id + 1 })
            })
            .expect("software ID rotation always succeeds");
        (function & 0xf0) | id
    }
}

fn hidpp(file: &mut File, device: u8, feature: u8, function: u8, params: &[u8]) -> Option<Vec<u8>> {
    let function = hidpp_function(device, function);
    let mut packet = [0_u8; 20];
    let short = params.len() <= 3;
    packet[..4].copy_from_slice(&[if short { 0x10 } else { 0x11 }, device, feature, function]);
    packet[4..4 + params.len().min(16)].copy_from_slice(&params[..params.len().min(16)]);
    file.write_all(&packet[..if short { 7 } else { 20 }]).ok()?;
    let deadline = Instant::now() + Duration::from_millis(1500);
    while let Some(reply) = receive(file, deadline) {
        if let Some(result) = hidpp_reply(&reply, device, feature, function, params) {
            return result;
        }
    }
    None
}

fn hidpp_reply(
    data: &[u8],
    device: u8,
    feature: u8,
    function: u8,
    params: &[u8],
) -> Option<Option<Vec<u8>>> {
    if data.len() < 7 || !matches!(data[0], 0x10 | 0x11) || data[1] != device {
        return None;
    }
    if matches!(data[2], 0x8f | 0xff) && data[3] == feature && data[4] == function {
        return Some(None);
    }
    if data[2] != feature || data[3] != function {
        return None;
    }
    // Receiver register replies echo the sub-register (and name page).
    if device == 0xff && feature == 0x83 && params.first() != data.get(4) {
        return None;
    }
    if device == 0xff && feature == 0x83 && params.len() > 1 && params.get(1) != data.get(5) {
        return None;
    }
    Some(Some(data[4..].to_vec()))
}

fn feature(file: &mut File, slot: u8, id: u16) -> Option<u8> {
    let reply = hidpp(file, slot, 0, 0x0a, &id.to_be_bytes())?;
    reply.first().copied().filter(|id| *id != 0)
}

fn device_name(file: &mut File, slot: u8) -> Option<String> {
    let index = feature(file, slot, 0x0005)?;
    let length = usize::from(*hidpp(file, slot, index, 0x0a, &[])?.first()?);
    let mut name = Vec::new();
    while name.len() < length {
        let fragment = hidpp(file, slot, index, 0x1a, &[name.len() as u8])?;
        if fragment.is_empty() {
            return None;
        }
        name.extend_from_slice(&fragment[..fragment.len().min(length - name.len())]);
    }
    String::from_utf8(name)
        .ok()
        .filter(|name| !name.trim().is_empty() && !name.chars().any(char::is_control))
}

fn read_bolt(file: &mut File, receiver: &str) -> Vec<DeviceBattery> {
    let mut batteries = Vec::new();
    for slot in 1..=6 {
        let Some(info) = hidpp(file, 0xff, 0x83, 0xb5, &[0x50 + slot]) else {
            continue;
        };
        if info.len() < 8 {
            continue;
        }
        let serial = info[4..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let id = if serial == "00000000" {
            format!("{receiver}:{slot}")
        } else {
            format!("bolt:{serial}")
        };
        let name = hidpp(file, 0xff, 0x83, 0xb5, &[0x60 + slot, 1])
            .and_then(|reply| {
                let len = usize::from(*reply.get(2)?).min(14);
                let len = len.min(reply.len().saturating_sub(3));
                Some(String::from_utf8_lossy(reply.get(3..3 + len)?).into_owned())
            })
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| format!("Logitech device {slot}"));
        let mut battery = unavailable(id, &name, "Asleep / unavailable");
        if let Some(index) = feature(file, slot, 0x1004) {
            let capabilities = hidpp(file, slot, index, 0x0a, &[]);
            let exact = capabilities
                .as_ref()
                .and_then(|r| r.get(1))
                .is_some_and(|flags| flags & 2 != 0);
            if let Some(reply) = hidpp(file, slot, index, 0x1a, &[])
                && let Some((percent, detail)) = unified_battery(&reply, exact)
            {
                battery.percent = percent;
                battery.detail = detail.into();
            }
        } else if let Some(index) = feature(file, slot, 0x1000)
            && let Some(reply) = hidpp(file, slot, index, 0x0a, &[])
            && reply.len() >= 3
            && reply[0] <= 100
        {
            // 0 means unknown for the older 0x1000 feature.
            battery.percent = (reply[0] != 0).then_some(f64::from(reply[0]));
            battery.detail = logitech_status(reply[2]).into();
        }
        if let Some(name) = device_name(file, slot) {
            battery.name = name;
        }
        batteries.push(battery);
    }
    batteries
}

fn logitech_status(status: u8) -> &'static str {
    match status {
        0 => "Discharging",
        1 | 2 => "Charging",
        3 => "Full",
        _ => "Unavailable",
    }
}

fn unified_battery(reply: &[u8], exact: bool) -> Option<(Option<f64>, &'static str)> {
    if reply.len() < 3 || reply[0] > 100 {
        return None;
    }
    let percent = exact.then_some(f64::from(reply[0]));
    let detail = if exact {
        logitech_status(reply[2])
    } else {
        match reply[1] {
            1 => "Critical",
            2 => "Low",
            4 => "Normal",
            8 => "Full",
            _ => "Unavailable",
        }
    };
    Some((percent, detail))
}

// Cloud III S query-only feature reports (0x06 = battery, 0x48 = charging).
// https://github.com/LennardKittner/HyperHeadset/blob/main/src/devices/cloud_iii_s_wireless.rs
fn hyperx_query(file: &mut File, command: u8) -> Option<u8> {
    let mut packet = [0_u8; 64];
    packet[..6].copy_from_slice(&[0x0c, 2, 3, 1, 0, command]);
    // Linux HIDIOCSFEATURE(len): _IOC(READ|WRITE, 'H', 0x06, len).
    let request = (3_u32 << 30) | (64 << 16) | (u32::from(b'H') << 8) | 6;
    // SAFETY: the request encodes the exact size of the live 64-byte buffer.
    if unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            request as libc::c_ulong,
            packet.as_mut_ptr(),
        )
    } < 0
    {
        return None;
    }
    let deadline = Instant::now() + Duration::from_millis(800);
    while let Some(reply) = receive(file, deadline) {
        if let Some(value) = hyperx_reply(&reply, command) {
            return Some(value);
        }
    }
    None
}

fn hyperx_reply(reply: &[u8], command: u8) -> Option<u8> {
    (reply.len() >= 7 && reply[..6] == [0x0c, 2, 3, 1, 0, command] && reply[6] != 0xff)
        .then(|| reply[6])
}

fn read_hyperx(file: &mut File, id: String) -> Option<DeviceBattery> {
    // A receiver stays attached even when the headset is off.
    if hyperx_query(file, 0x02)? != 2 {
        return None;
    }
    let percent = hyperx_query(file, 0x06).filter(|p| *p <= 100)?;
    let charging = hyperx_query(file, 0x48);
    Some(DeviceBattery {
        id,
        name: "HyperX Cloud III S Wireless".into(),
        percent: Some(f64::from(percent)),
        detail: if charging == Some(1) {
            "Charging"
        } else {
            "Discharging"
        }
        .into(),
    })
}

struct SteamSlot {
    file: File,
    id: String,
    slot: u8,
    battery: Option<DeviceBattery>,
}

// Steam Triton battery input reports, also used by SDL's Steam driver:
// https://github.com/libsdl-org/SDL/blob/main/src/joystick/hidapi/steam/controller_structs.h
fn steam_report(report: &[u8]) -> Option<(f64, &'static str)> {
    if report.len() < 15 || report[0] != 0x43 || report[2] > 100 {
        return None;
    }
    let state = match report[1] {
        1 => "Discharging",
        2 => "Charging",
        4 => "Full",
        _ => "Unavailable",
    };
    Some((f64::from(report[2]), state))
}

fn read_steam(slots: &mut [SteamSlot]) -> Vec<DeviceBattery> {
    if slots.is_empty() {
        return Vec::new();
    }
    // Reports arrive every ~2.5 s. Poll all slots together so unused receiver
    // slots do not multiply the wait. We do not send any controller commands.
    let deadline = Instant::now() + Duration::from_millis(3200);
    let mut polls = slots
        .iter()
        .map(|slot| libc::pollfd {
            fd: slot.file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        })
        .collect::<Vec<_>>();
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        // SAFETY: polls contains polls.len() initialized descriptors.
        let ready = unsafe {
            libc::poll(
                polls.as_mut_ptr(),
                polls.len() as libc::nfds_t,
                remaining.as_millis().clamp(1, 3200) as i32,
            )
        };
        if ready <= 0 {
            break;
        }
        for (slot, poll) in slots.iter_mut().zip(&mut polls) {
            if poll.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                slot.battery = None;
                poll.fd = -1;
                continue;
            }
            if poll.revents & libc::POLLIN == 0 {
                continue;
            }
            let mut report = [0_u8; 64];
            let Ok(count) = slot.file.read(&mut report) else {
                continue;
            };
            let report = &report[..count];
            if let Some((percent, detail)) = steam_report(report) {
                let name = if slot.slot == 1 {
                    "Steam Controller".into()
                } else {
                    format!("Steam Controller {}", slot.slot)
                };
                slot.battery = Some(DeviceBattery {
                    id: slot.id.clone(),
                    name,
                    percent: Some(percent),
                    detail: detail.into(),
                });
            } else if report.len() >= 2 && matches!(report[0], 0x46 | 0x79) && report[1] == 1 {
                slot.battery = None;
            }
        }
        if polls.iter().all(|poll| poll.fd < 0) {
            break;
        }
    }
    slots
        .iter_mut()
        .filter_map(|slot| slot.battery.take())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hidpp_client_ids_do_not_collide_with_kernel_or_common_clients() {
        let mut previous = None;
        for _ in 0..512 {
            let function = hidpp_function(1, 0x1a);
            assert_eq!(function & 0xf0, 0x10);
            assert!((2..=6).contains(&(function & 0x0f)));
            assert_ne!(previous, Some(function));
            previous = Some(function);
        }
        assert_eq!(hidpp_function(0xff, 0xb5), 0xb5);
    }

    #[test]
    fn steam_battery_validates_report_and_charge_state() {
        let mut report = [0_u8; 15];
        report[..3].copy_from_slice(&[0x43, 1, 62]);
        assert_eq!(steam_report(&report), Some((62.0, "Discharging")));
        report[1] = 2;
        assert_eq!(steam_report(&report), Some((62.0, "Charging")));
        assert!(steam_report(&report[..3]).is_none());
        report[2] = 255;
        assert!(steam_report(&report).is_none());
        report[0] = 0x42;
        report[2] = 62;
        assert!(steam_report(&report).is_none());
    }
    #[test]
    fn logitech_does_not_invent_percentages_for_coarse_levels() {
        assert_eq!(unified_battery(&[0, 4, 0], false), Some((None, "Normal")));
        assert_eq!(
            unified_battery(&[0, 1, 0], true),
            Some((Some(0.0), "Discharging"))
        );
        assert_eq!(
            unified_battery(&[73, 4, 1], true),
            Some((Some(73.0), "Charging"))
        );
        assert!(unified_battery(&[255, 4, 1], true).is_none());
    }
    #[test]
    fn hidpp_ignores_notifications_other_clients_and_slots() {
        let reply = [0x11, 1, 8, 0x1a, 75, 4, 0];
        assert_eq!(
            hidpp_reply(&reply, 1, 8, 0x1a, &[]),
            Some(Some(vec![75, 4, 0]))
        );
        assert_eq!(hidpp_reply(&reply, 2, 8, 0x1a, &[]), None);
        assert_eq!(hidpp_reply(&reply, 1, 8, 0x10, &[]), None);
        let high_feature = [0x11, 1, 0x83, 0x12, 75, 4, 0];
        assert_eq!(
            hidpp_reply(&high_feature, 1, 0x83, 0x12, &[]),
            Some(Some(vec![75, 4, 0]))
        );
        assert_eq!(
            hidpp_reply(&[0x10, 1, 0xff, 8, 0x1a, 2, 0], 1, 8, 0x1a, &[]),
            Some(None)
        );
        let name = [0x11, 0xff, 0x83, 0xb5, 0x61, 1, 3, b'M', b'X', b'3'];
        assert_eq!(
            hidpp_reply(&name, 0xff, 0x83, 0xb5, &[0x61, 1]),
            Some(Some(name[4..].to_vec()))
        );
        assert_eq!(hidpp_reply(&name, 0xff, 0x83, 0xb5, &[0x62, 1]), None);
        assert_eq!(hidpp_reply(&name, 0xff, 0x83, 0xb5, &[0x61, 2]), None);
    }
    #[test]
    fn hyperx_checks_header_command_and_unavailable_sentinel() {
        assert_eq!(hyperx_reply(&[0x0c, 2, 3, 1, 0, 6, 95], 6), Some(95));
        assert_eq!(hyperx_reply(&[0x0c, 2, 3, 1, 0, 0x48, 1], 6), None);
        assert_eq!(hyperx_reply(&[0x0c, 2, 3, 1, 0, 6, 255], 6), None);
        assert_eq!(hyperx_reply(&[0x0c, 2, 3], 6), None);
    }
}
