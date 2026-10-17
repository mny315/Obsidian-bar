//! PCI class metadata is immutable for one sysfs device object.
use std::{
    collections::HashMap,
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

pub(super) type DeviceIdentity = (u64, u64);

pub(super) fn device_identity(path: &Path) -> Option<DeviceIdentity> {
    fs::metadata(path)
        .ok()
        .map(|metadata| (metadata.dev(), metadata.ino()))
}

#[derive(Default)]
pub(super) struct PciGpuDiscovery {
    classes: HashMap<PathBuf, (DeviceIdentity, bool)>,
}

impl PciGpuDiscovery {
    pub(super) fn paths(&mut self, root: &Path) -> Vec<PathBuf> {
        self.paths_with(root, super::is_gpu_device)
    }

    fn paths_with(&mut self, root: &Path, mut is_gpu: impl FnMut(&Path) -> bool) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(root) else {
            self.classes.clear();
            return Vec::new();
        };
        let mut classes = HashMap::with_capacity(self.classes.len());
        let mut gpus = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let identity = device_identity(&path);
            let graphics = match (identity, self.classes.get(&path)) {
                (Some(identity), Some((cached_identity, graphics)))
                    if identity == *cached_identity =>
                {
                    *graphics
                }
                _ => is_gpu(&path),
            };
            if let Some(identity) = identity {
                classes.insert(path.clone(), (identity, graphics));
            }
            if graphics {
                gpus.push(path);
            }
        }
        self.classes = classes;
        gpus.sort();
        gpus
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_devices_skip_class_reads_but_hotplug_and_replacements_are_observed() {
        let root =
            std::env::temp_dir().join(format!("obsidian-pci-discovery-{}", std::process::id()));
        let pci = root.join("pci");
        let first = pci.join("0000:01:00.0");
        let second = pci.join("0000:02:00.0");
        for (path, class) in [(&first, "0x030000"), (&second, "0x020000")] {
            fs::create_dir_all(path).unwrap();
            fs::write(path.join("class"), class).unwrap();
        }
        let mut discovery = PciGpuDiscovery::default();
        assert_eq!(
            discovery.paths(&pci).as_slice(),
            std::slice::from_ref(&first)
        );
        assert_eq!(
            discovery
                .paths_with(&pci, |_| panic!("unchanged device metadata was read again"))
                .as_slice(),
            std::slice::from_ref(&first)
        );
        assert_eq!(
            discovery.classes.len(),
            2,
            "negative classifications must also be cached"
        );

        // Hold the old inode alive to model replacing a device at the same PCI
        // address between samples, without relying on directory timestamps.
        fs::rename(&second, root.join("removed-network-device")).unwrap();
        fs::create_dir(&second).unwrap();
        fs::write(second.join("class"), "0x030200").unwrap();
        let mut reclassified = Vec::new();
        assert_eq!(
            discovery.paths_with(&pci, |path| {
                reclassified.push(path.to_path_buf());
                super::super::is_gpu_device(path)
            }),
            [first.clone(), second.clone()]
        );
        assert_eq!(reclassified.as_slice(), std::slice::from_ref(&second));
        fs::rename(&first, root.join("removed-gpu")).unwrap();
        assert_eq!(discovery.paths(&pci), [second]);
        assert_eq!(discovery.classes.len(), 1);
        assert!(discovery.paths(&root.join("missing")).is_empty());
        assert!(discovery.classes.is_empty());
        fs::remove_dir_all(root).unwrap();
    }
}
