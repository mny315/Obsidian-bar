//! Shared, cancellable scans for wallpaper files and folder navigation.
use super::{WallpaperError, is_supported_wallpaper};
use gio::prelude::*;
use std::{
    cell::RefCell,
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
    rc::Rc,
};

#[derive(Clone, Default)]
pub(super) struct ScanRequests(Rc<ScanState>);

#[derive(Default)]
struct ScanState {
    current: RefCell<Option<gio::Cancellable>>,
}

impl ScanRequests {
    pub(super) fn begin(&self) -> gio::Cancellable {
        let request = gio::Cancellable::new();
        let previous = self.0.current.replace(Some(request.clone()));
        // Cancellation handlers run synchronously and may start another scan.
        // Install this request first so that a newer reentrant request wins.
        if let Some(previous) = previous {
            previous.cancel();
        }
        request
    }

    pub(super) fn cancel(&self) {
        let request = self.0.current.borrow_mut().take();
        if let Some(request) = request {
            request.cancel();
        }
    }
}

impl Drop for ScanState {
    fn drop(&mut self) {
        if let Some(request) = self.current.get_mut().take() {
            request.cancel();
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum EntryKind {
    Directory,
    Wallpaper,
}

pub(super) fn list_entries(
    directory: &Path,
    kind: EntryKind,
    request: Option<&gio::Cancellable>,
) -> Result<Vec<PathBuf>, WallpaperError> {
    scan_directory(directory, kind, || {
        request.is_some_and(|request| request.is_cancelled())
    })
}

fn scan_directory(
    directory: &Path,
    kind: EntryKind,
    cancelled: impl Fn() -> bool,
) -> Result<Vec<PathBuf>, WallpaperError> {
    let check = || {
        if cancelled() {
            Err(WallpaperError::Io(io::Error::new(
                io::ErrorKind::Interrupted,
                "directory scan cancelled",
            )))
        } else {
            Ok(())
        }
    };
    check()?;
    let entries = fs::read_dir(directory).map_err(WallpaperError::Io)?;
    let mut paths = Vec::new();
    for entry in entries {
        check()?;
        let entry = entry.map_err(WallpaperError::Io)?;
        let path = entry.path();
        if matches!(kind, EntryKind::Wallpaper) && !is_supported_wallpaper(&path) {
            continue;
        }
        let follows = || match kind {
            EntryKind::Directory => path.is_dir(),
            EntryKind::Wallpaper => path.is_file(),
        };
        let matches = entry.file_type().map_or_else(
            |_| follows(),
            |file_type| {
                (match kind {
                    EntryKind::Directory => file_type.is_dir(),
                    EntryKind::Wallpaper => file_type.is_file(),
                }) || file_type.is_symlink() && follows()
            },
        );
        if matches {
            paths.push(path);
        }
    }
    check()?;
    paths.sort_by_cached_key(|path| {
        path.file_name()
            .and_then(OsStr::to_str)
            .unwrap_or_default()
            .to_ascii_lowercase()
    });
    check()?;
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn newer_requests_cancel_older_scans_and_last_owner_cancels_the_current_one() {
        let scans = ScanRequests::default();
        let first = scans.begin();
        let second = scans.begin();
        assert!(first.is_cancelled());
        assert!(!second.is_cancelled());
        let other_owner = scans.clone();
        drop(scans);
        assert!(!second.is_cancelled());
        other_owner.cancel();
        assert!(second.is_cancelled());
        let third = other_owner.begin();
        assert!(!third.is_cancelled());
        drop(other_owner);
        assert!(third.is_cancelled());
    }

    #[test]
    fn cancellation_handlers_can_start_a_newer_scan_without_borrowing_or_losing_it() {
        let scans = ScanRequests::default();
        let first = scans.begin();
        let nested = Rc::new(RefCell::new(None::<gio::Cancellable>));
        let requests = scans.clone();
        let next = Rc::clone(&nested);
        first.connect_local("cancelled", false, move |_| {
            next.replace(Some(requests.begin()));
            None
        });
        let superseded = scans.begin();
        assert!(first.is_cancelled());
        assert!(superseded.is_cancelled());
        let current = nested.borrow_mut().take().unwrap();
        assert!(!current.is_cancelled());

        let requests = scans.clone();
        let next = Rc::clone(&nested);
        current.connect_local("cancelled", false, move |_| {
            next.replace(Some(requests.begin()));
            None
        });
        scans.cancel();
        assert!(current.is_cancelled());
        let newer = nested.borrow_mut().take().unwrap();
        assert!(!newer.is_cancelled());
        scans.cancel();
        assert!(newer.is_cancelled());
    }

    #[test]
    fn cancelled_scans_stop_before_opening_or_finishing_a_directory() {
        let request = gio::Cancellable::new();
        request.cancel();
        let missing = std::env::temp_dir().join("obsidian-cancelled-scan-nonexistent");
        let Err(WallpaperError::Io(error)) =
            list_entries(&missing, EntryKind::Wallpaper, Some(&request))
        else {
            panic!("cancelled scan must return an interruption");
        };
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);

        let root =
            std::env::temp_dir().join(format!("obsidian-interrupted-scan-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        for index in 0..32 {
            fs::write(root.join(format!("{index}.png")), b"fixture").unwrap();
        }
        let checks = Cell::new(0);
        let result = scan_directory(&root, EntryKind::Wallpaper, || {
            checks.set(checks.get() + 1);
            checks.get() == 5
        });
        let Err(WallpaperError::Io(error)) = result else {
            panic!("cancelled scan must stop between entries");
        };
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(checks.get(), 5);
        fs::remove_dir_all(root).unwrap();
    }
}
