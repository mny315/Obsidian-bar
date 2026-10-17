//! Mounted block volumes, usage counters and storage display names.
use super::{DiskSnapshot, files::read_trimmed};
use std::{
    borrow::Cow,
    collections::HashSet,
    ffi::CString,
    fs,
    path::{Path, PathBuf},
};

fn decode_mount_field(value: &str) -> Cow<'_, str> {
    let Some(first_escape) = value.find('\\') else {
        return Cow::Borrowed(value);
    };
    let mut decoded = String::with_capacity(value.len());
    decoded.push_str(&value[..first_escape]);
    let mut remaining = &value[first_escape..];
    while let Some(index) = remaining.find('\\') {
        decoded.push_str(&remaining[..index]);
        remaining = &remaining[index..];
        let character = match remaining.get(..4) {
            Some("\\040") => Some(' '),
            Some("\\011") => Some('\t'),
            Some("\\012") => Some('\n'),
            Some("\\134") => Some('\\'),
            _ => None,
        };
        if let Some(character) = character {
            decoded.push(character);
            remaining = &remaining[4..];
        } else {
            decoded.push('\\');
            remaining = &remaining[1..];
        }
    }
    decoded.push_str(remaining);
    Cow::Owned(decoded)
}

pub(super) fn disk_mounts(mountinfo: &str) -> Vec<(String, PathBuf)> {
    let mut mounts = mountinfo
        .lines()
        .filter_map(|line| {
            let (mount, filesystem) = line.split_once(" - ")?;
            let mut fields = mount.split_whitespace();
            let mut filesystem = filesystem.split_whitespace();
            let kind = filesystem.next()?;
            let source = filesystem.next()?;
            // Include mounted block filesystems, not tmpfs, network mounts or
            // loop-backed application images. statvfs stays local to the machine.
            if !source.starts_with("/dev/")
                || ["/dev/loop", "/dev/ram", "/dev/zram"]
                    .iter()
                    .any(|prefix| source.starts_with(prefix))
            {
                return None;
            }
            let device = fields.nth(2)?;
            let mount_point = PathBuf::from(decode_mount_field(fields.nth(1)?).into_owned());
            // Btrfs subvolumes may have distinct device numbers but share space on
            // the same source. Ordinary bind mounts share the device number.
            let id = if kind == "btrfs" {
                decode_mount_field(source).into_owned()
            } else {
                device.to_owned()
            };
            Some((id, mount_point))
        })
        .collect::<Vec<_>>();
    mounts.sort_by(|a, b| {
        a.1.components()
            .count()
            .cmp(&b.1.components().count())
            .then_with(|| a.1.cmp(&b.1))
    });
    let mut seen = HashSet::new();
    mounts.retain(|(id, _)| seen.insert(id.clone()));
    mounts
}

pub(super) fn read_disk_snapshots() -> Vec<DiskSnapshot> {
    let Ok(mountinfo) = fs::read_to_string("/proc/self/mountinfo") else {
        return Vec::new();
    };
    disk_mounts(&mountinfo)
        .into_iter()
        .filter_map(|(device, mount_point)| {
            let (used, total, available) = read_disk_usage(&mount_point)?;
            (total > 0).then_some(DiskSnapshot {
                // Device numbers can change after reboot; the mount identifies
                // the volume whose metrics the user configured.
                id: format!("mount:{}", mount_point.display()),
                name: disk_display_name(&device, &mount_point),
                used,
                total,
                available,
            })
        })
        .collect()
}

fn disk_display_name(device: &str, mount: &Path) -> String {
    use std::os::unix::fs::MetadataExt;
    let device = if device.starts_with("/dev/") {
        fs::metadata(device)
            .ok()
            .map(|metadata| {
                format!(
                    "{}:{}",
                    libc::major(metadata.rdev()),
                    libc::minor(metadata.rdev())
                )
            })
            .unwrap_or_default()
    } else {
        device.to_owned()
    };
    let properties = fs::read_to_string(format!("/run/udev/data/b{device}")).unwrap_or_default();
    let property = |name: &str| properties.lines().find_map(|line| line.strip_prefix(name));
    let sysfs = PathBuf::from(format!("/sys/dev/block/{device}"));
    let model = property("E:ID_MODEL=")
        .map(str::to_owned)
        .or_else(|| read_trimmed(sysfs.join("device/model")))
        .or_else(|| read_trimmed(sysfs.join("../device/model")))
        .and_then(|model| compact_storage_model(&model));
    storage_display_name(model.as_deref(), property("E:ID_FS_LABEL="), mount)
}

pub(super) fn storage_display_name(
    model: Option<&str>,
    label: Option<&str>,
    mount: &Path,
) -> String {
    let partition = label
        .filter(|label| !label.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| match mount.to_str() {
            Some("/") => "System".into(),
            Some("/boot" | "/boot/efi" | "/efi") => "Boot".into(),
            _ => mount
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Volume".into()),
        });
    match model {
        Some(model) => format!("{model} · {partition}"),
        None => partition,
    }
}

fn read_disk_usage(path: &Path) -> Option<(u64, u64, u64)> {
    let path = CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return None;
    }
    let stats = unsafe { stats.assume_init() };
    let block_size = if stats.f_frsize > 0 {
        stats.f_frsize
    } else {
        stats.f_bsize
    } as u128;
    let total = u128::from(stats.f_blocks).saturating_mul(block_size);
    let available = u128::from(stats.f_bavail).saturating_mul(block_size);
    let free = u128::from(stats.f_bfree).saturating_mul(block_size);
    let used = total.saturating_sub(free);
    Some((
        used.min(u128::from(u64::MAX)) as u64,
        total.min(u128::from(u64::MAX)) as u64,
        available.min(total).min(u128::from(u64::MAX)) as u64,
    ))
}

pub(super) fn compact_storage_model(model: &str) -> Option<String> {
    let words = model
        .split_whitespace()
        .filter(|word| {
            let normalized = word
                .trim_matches(|character: char| !character.is_ascii_alphanumeric())
                .to_ascii_lowercase();
            !matches!(
                normalized.as_str(),
                "ssd" | "nvme" | "with" | "heatsink" | "solid" | "state" | "drive"
            ) && !is_storage_capacity(word)
        })
        .map(|word| word.replace('_', " "))
        .collect::<Vec<_>>();
    (!words.is_empty()).then(|| words.join(" "))
}

fn is_storage_capacity(word: &str) -> bool {
    let normalized = word
        .trim_matches(|character: char| !character.is_ascii_alphanumeric() && character != '.')
        .to_ascii_lowercase();
    ["kib", "mib", "gib", "tib", "kb", "mb", "gb", "tb"]
        .into_iter()
        .find_map(|suffix| normalized.strip_suffix(suffix))
        .is_some_and(|number| {
            !number.is_empty()
                && number
                    .chars()
                    .all(|character| character.is_ascii_digit() || character == '.')
        })
}
