//! Desktop-entry watch discovery and reuse across application catalog changes.
use gtk::{gio, prelude::*};
use std::{
    collections::{HashMap, HashSet},
    env, fs,
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct DesktopWatch {
    directory: PathBuf,
    exact_path: Option<PathBuf>,
}

#[derive(Default)]
pub(super) struct DesktopMonitors(HashMap<DesktopWatch, MonitoredDirectory>);

struct MonitoredDirectory {
    identity: Option<(u64, u64)>,
    monitor: gio::FileMonitor,
}

impl DesktopMonitors {
    pub(super) fn refresh(
        &mut self,
        watches: Vec<DesktopWatch>,
        on_change: impl Fn(bool) + Clone + 'static,
    ) {
        let mut current = HashMap::with_capacity(watches.len());
        for watch in watches {
            let identity = fs::metadata(&watch.directory)
                .ok()
                .map(|metadata| (metadata.dev(), metadata.ino()));
            if let Some(previous) = self.0.remove(&watch) {
                // A replaced directory or symlink can keep the same path while
                // its old monitor still observes the previous inode.
                if identity.is_some()
                    && previous.identity == identity
                    && !previous.monitor.is_cancelled()
                {
                    current.insert(watch, previous);
                    continue;
                }
                previous.monitor.cancel();
            }
            let file = gio::File::for_path(&watch.directory);
            let Ok(monitor) =
                file.monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
            else {
                continue;
            };
            let exact_path = watch.exact_path.clone();
            let on_change = on_change.clone();
            monitor.connect_changed(move |_, file, other_file, _| {
                if desktop_watch_matches(exact_path.as_deref(), file, other_file) {
                    on_change(exact_path.is_some());
                }
            });
            current.insert(watch, MonitoredDirectory { identity, monitor });
        }
        for obsolete in self.0.values() {
            obsolete.monitor.cancel();
        }
        self.0 = current;
    }
}

impl Drop for DesktopMonitors {
    fn drop(&mut self) {
        for monitor in self.0.values() {
            monitor.monitor.cancel();
        }
    }
}

fn desktop_application_dirs() -> Vec<PathBuf> {
    let mut data_dirs = Vec::new();

    if let Some(data_home) = env::var_os("XDG_DATA_HOME") {
        data_dirs.push(PathBuf::from(data_home));
    } else if let Some(home) = env::var_os("HOME") {
        data_dirs.push(PathBuf::from(home).join(".local/share"));
    }

    if let Some(raw_dirs) = env::var_os("XDG_DATA_DIRS") {
        data_dirs.extend(env::split_paths(&raw_dirs));
    } else {
        data_dirs.extend([
            PathBuf::from("/usr/local/share"),
            PathBuf::from("/usr/share"),
        ]);
    }

    if let Some(home) = env::var_os("HOME") {
        let home = PathBuf::from(home);
        data_dirs.push(home.join(".nix-profile/share"));
        data_dirs.push(home.join(".local/share/flatpak/exports/share"));
    }
    if let Some(user) = env::var_os("USER") {
        data_dirs.push(
            PathBuf::from("/etc/profiles/per-user")
                .join(user)
                .join("share"),
        );
    }
    data_dirs.push(PathBuf::from("/run/current-system/sw/share"));
    data_dirs.push(PathBuf::from("/var/lib/flatpak/exports/share"));

    let mut seen = HashSet::new();
    data_dirs
        .into_iter()
        .map(|directory| normalize_path(&directory.join("applications")))
        .filter(|directory| seen.insert(directory.clone()))
        .collect()
}

pub(super) fn desktop_watches() -> Vec<DesktopWatch> {
    let mut watches = HashSet::new();

    for applications in desktop_application_dirs() {
        if applications.is_dir() {
            watches.insert(DesktopWatch {
                directory: applications.clone(),
                exact_path: None,
            });
        }

        if let Some(watch) = nearest_missing_component_watch(&applications) {
            watches.insert(watch);
        } else if let Some(parent) = applications.parent().filter(|parent| parent.is_dir()) {
            watches.insert(DesktopWatch {
                directory: parent.to_owned(),
                exact_path: Some(applications.clone()),
            });
        }

        let (resolved, symlinks) = resolve_path_and_symlinks(&applications, 0);
        for symlink in symlinks {
            if let Some(parent) = symlink.parent().filter(|parent| parent.is_dir()) {
                watches.insert(DesktopWatch {
                    directory: parent.to_owned(),
                    exact_path: Some(symlink),
                });
            }
        }

        if resolved != applications {
            if resolved.is_dir() {
                watches.insert(DesktopWatch {
                    directory: resolved.clone(),
                    exact_path: None,
                });
            }
            if let Some(watch) = nearest_missing_component_watch(&resolved) {
                watches.insert(watch);
            } else if let Some(parent) = resolved.parent().filter(|parent| parent.is_dir()) {
                watches.insert(DesktopWatch {
                    directory: parent.to_owned(),
                    exact_path: Some(resolved),
                });
            }
        }
    }

    watches.into_iter().collect()
}

fn nearest_missing_component_watch(path: &Path) -> Option<DesktopWatch> {
    let mut missing = normalize_path(path);
    while !missing.exists() {
        let parent = missing.parent()?;
        if parent.is_dir() {
            return Some(DesktopWatch {
                directory: parent.to_owned(),
                exact_path: Some(missing),
            });
        }
        missing = parent.to_owned();
    }
    None
}

fn resolve_path_and_symlinks(path: &Path, depth: u8) -> (PathBuf, Vec<PathBuf>) {
    if depth >= 32 {
        return (normalize_path(path), Vec::new());
    }

    let mut resolved = PathBuf::new();
    let mut components = path.components();

    while let Some(component) = components.next() {
        resolved.push(component.as_os_str());
        let resolved_normalized = normalize_path(&resolved);
        let is_symlink = fs::symlink_metadata(&resolved_normalized)
            .is_ok_and(|metadata| metadata.file_type().is_symlink());
        if !is_symlink {
            resolved = resolved_normalized;
            continue;
        }

        let Ok(target) = fs::read_link(&resolved_normalized) else {
            return (resolved_normalized.clone(), vec![resolved_normalized]);
        };
        let mut next = if target.is_absolute() {
            target
        } else {
            resolved_normalized
                .parent()
                .unwrap_or_else(|| Path::new("/"))
                .join(target)
        };
        for remaining in components {
            next.push(remaining.as_os_str());
        }

        let (final_path, mut nested_symlinks) =
            resolve_path_and_symlinks(&normalize_path(&next), depth + 1);
        nested_symlinks.insert(0, resolved_normalized);
        return (final_path, nested_symlinks);
    }

    (normalize_path(&resolved), Vec::new())
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

fn desktop_watch_matches(
    exact_path: Option<&Path>,
    file: &gio::File,
    other_file: Option<&gio::File>,
) -> bool {
    let Some(exact_path) = exact_path else {
        return true;
    };
    let exact_path = normalize_path(exact_path);
    let paths = [file.path(), other_file.and_then(|file| file.path())];

    let mut had_path = false;
    for path in paths.into_iter().flatten() {
        had_path = true;
        if normalize_path(&path) == exact_path {
            return true;
        }
    }

    !had_path
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::Cell,
        rc::Rc,
        time::{Duration, Instant},
    };

    #[test]
    fn unchanged_watches_keep_delivering_events_and_removed_watches_stop() {
        let context = gtk::glib::MainContext::new();
        context
            .with_thread_default(|| {
                let root = env::temp_dir().join(format!(
                    "obsidian-desktop-watch-reuse-{}",
                    std::process::id()
                ));
                let watch = |name| DesktopWatch {
                    directory: root.join(name),
                    exact_path: None,
                };
                let a = watch("a");
                let b = watch("b");
                let c = watch("c");
                for watch in [&a, &b, &c] {
                    fs::create_dir_all(&watch.directory).unwrap();
                }
                let changes = Rc::new(Cell::new(0));
                let callback = {
                    let changes = Rc::clone(&changes);
                    move |rebuild: bool| {
                        assert!(!rebuild);
                        changes.set(changes.get() + 1);
                    }
                };
                let mut monitors = DesktopMonitors::default();
                monitors.refresh(vec![a.clone(), b.clone()], callback.clone());
                let original_a = monitors.0[&a].monitor.clone();
                let removed_b = monitors.0[&b].monitor.clone();
                monitors.refresh(vec![a.clone(), c.clone()], callback.clone());
                assert_eq!(monitors.0[&a].monitor, original_a);
                assert!(!original_a.is_cancelled());
                assert!(removed_b.is_cancelled());
                let added_c = monitors.0[&c].monitor.clone();
                assert!(!added_c.is_cancelled());

                fs::write(a.directory.join("fixture.desktop"), b"fixture").unwrap();
                let deadline = Instant::now() + Duration::from_secs(3);
                while changes.get() == 0 {
                    assert!(
                        Instant::now() < deadline,
                        "retained monitor lost its callback"
                    );
                    context.iteration(false);
                    std::thread::sleep(Duration::from_millis(1));
                }
                fs::rename(&a.directory, root.join("old-a")).unwrap();
                fs::create_dir(&a.directory).unwrap();
                monitors.refresh(vec![a.clone(), c.clone()], callback.clone());
                assert_ne!(monitors.0[&a].monitor, original_a);
                assert!(original_a.is_cancelled());
                assert_eq!(monitors.0[&c].monitor, added_c);
                let replacement_a = monitors.0[&a].monitor.clone();
                assert!(!replacement_a.is_cancelled());
                monitors.refresh(Vec::new(), callback.clone());
                assert!(replacement_a.is_cancelled());
                assert!(added_c.is_cancelled());
                assert!(monitors.0.is_empty());
                monitors.refresh(vec![a.clone()], callback);
                let last = monitors.0[&a].monitor.clone();
                drop(monitors);
                assert!(last.is_cancelled());
                fs::remove_dir_all(root).unwrap();
            })
            .unwrap();
    }
}
