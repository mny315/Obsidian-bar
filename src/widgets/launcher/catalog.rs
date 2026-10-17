//! Application discovery, desktop entry metadata and catalog refreshes.
use super::{
    LaunchableApp, LauncherCategory, LauncherController, LauncherUserState, normalize_fields,
};
use crate::widgets::run_background;
use gtk::{gio, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    rc::{Rc, Weak},
};

mod watches;
use watches::{DesktopMonitors, desktop_watches};

#[derive(Default)]
pub(super) struct LauncherCatalogData {
    pub(super) apps: Vec<Rc<LaunchableApp>>,
    pub(super) valid_keys: HashSet<String>,
}

struct LauncherCatalogBuild {
    pending: VecDeque<gio::AppInfo>,
    metadata: HashMap<String, (Vec<String>, String)>,
    data: LauncherCatalogData,
}

pub struct LauncherCatalog {
    pub(super) data: RefCell<LauncherCatalogData>,
    subscribers: RefCell<Vec<Weak<LauncherController>>>,
    refresh_scheduled: Cell<bool>,
    refresh_running: Cell<bool>,
    refresh_again: Cell<bool>,
    pub(super) initialized: Cell<bool>,
    app_monitor: gio::AppInfoMonitor,
    app_monitor_handler: RefCell<Option<glib::SignalHandlerId>>,
    desktop_monitors: RefCell<DesktopMonitors>,
    monitor_rebuild_scheduled: Cell<bool>,
}

impl LauncherCatalog {
    pub fn new() -> Rc<Self> {
        let app_monitor = gio::AppInfoMonitor::get();
        Rc::new(Self {
            data: RefCell::new(LauncherCatalogData::default()),
            subscribers: RefCell::new(Vec::new()),
            refresh_scheduled: Cell::new(false),
            refresh_running: Cell::new(false),
            refresh_again: Cell::new(false),
            initialized: Cell::new(false),
            app_monitor,
            app_monitor_handler: RefCell::new(None),
            desktop_monitors: RefCell::new(DesktopMonitors::default()),
            monitor_rebuild_scheduled: Cell::new(false),
        })
    }

    pub fn start(self: &Rc<Self>) {
        if self.app_monitor_handler.borrow().is_some() {
            return;
        }
        let weak = Rc::downgrade(self);
        let handler = self.app_monitor.connect_changed(move |_| {
            if let Some(catalog) = weak.upgrade() {
                catalog.schedule_refresh();
                catalog.schedule_monitor_rebuild();
            }
        });
        self.app_monitor_handler.replace(Some(handler));
        self.rebuild_desktop_monitors();
        self.schedule_refresh();
    }

    pub(super) fn subscribe(&self, controller: &Rc<LauncherController>) {
        self.subscribers
            .borrow_mut()
            .push(Rc::downgrade(controller));
    }

    pub(super) fn unsubscribe(&self, controller: &Rc<LauncherController>) {
        self.subscribers.borrow_mut().retain(|subscriber| {
            subscriber
                .upgrade()
                .is_some_and(|candidate| !Rc::ptr_eq(&candidate, controller))
        });
    }

    pub(super) fn refresh_if_stale(self: &Rc<Self>) {
        if self.refresh_scheduled.get() {
            return;
        }

        let stale = !self.initialized.get() || {
            let current_keys = current_visible_app_keys();
            let catalog = self.data.borrow();
            !current_keys.eq(&catalog.valid_keys)
        };
        if stale {
            self.schedule_refresh();
        }
    }

    fn rebuild_desktop_monitors(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.desktop_monitors.borrow_mut().refresh(
            desktop_watches(),
            move |rebuild_after_change| {
                let Some(catalog) = weak.upgrade() else {
                    return;
                };
                catalog.schedule_refresh();
                if rebuild_after_change {
                    catalog.schedule_monitor_rebuild();
                }
            },
        );
    }

    fn schedule_monitor_rebuild(self: &Rc<Self>) {
        if self.monitor_rebuild_scheduled.replace(true) {
            return;
        }

        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            let Some(catalog) = weak.upgrade() else {
                return;
            };
            catalog.monitor_rebuild_scheduled.set(false);
            catalog.rebuild_desktop_monitors();
        });
    }

    fn schedule_refresh(self: &Rc<Self>) {
        if self.refresh_scheduled.replace(true) {
            // Events queued before the scan starts are covered by that scan.
            // Only changes during an actual build require another pass.
            if self.refresh_running.get() {
                self.refresh_again.set(true);
            }
            return;
        }

        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            let Some(catalog) = weak.upgrade() else {
                return;
            };
            catalog.begin_refresh();
        });
    }

    fn begin_refresh(self: &Rc<Self>) {
        self.refresh_running.set(true);
        let pending = gio::AppInfo::all().into_iter().collect::<VecDeque<_>>();
        let ids = pending
            .iter()
            .filter(|info| info.should_show())
            .filter_map(|info| info.id())
            .filter(|id| !id.is_empty())
            .map(|id| id.to_string())
            .collect::<HashSet<_>>();
        let weak = Rc::downgrade(self);
        run_background(
            move || {
                ids.into_iter()
                    .map(|id| {
                        let metadata = desktop_metadata(&id);
                        (id, metadata)
                    })
                    .collect()
            },
            move |metadata| {
                if let Some(catalog) = weak.upgrade() {
                    catalog.build_rows(pending, metadata);
                }
            },
        );
    }

    fn build_rows(
        self: &Rc<Self>,
        pending: VecDeque<gio::AppInfo>,
        metadata: HashMap<String, (Vec<String>, String)>,
    ) {
        const APPS_PER_IDLE: usize = 12;

        let build = Rc::new(RefCell::new(LauncherCatalogBuild {
            pending,
            metadata,
            data: LauncherCatalogData::default(),
        }));
        let weak = Rc::downgrade(self);
        glib::idle_add_local(move || {
            let Some(catalog) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };

            let finished = {
                let mut build = build.borrow_mut();
                for _ in 0..APPS_PER_IDLE {
                    let Some(info) = build.pending.pop_front() else {
                        break;
                    };
                    let LauncherCatalogBuild { data, metadata, .. } = &mut *build;
                    let LauncherCatalogData { apps, valid_keys } = data;
                    if let Some(app) = build_catalog_app(info, valid_keys, |id| {
                        metadata.remove(id).unwrap_or_default()
                    }) {
                        apps.push(Rc::new(app));
                    }
                }
                build.pending.is_empty()
            };

            if !finished {
                return glib::ControlFlow::Continue;
            }

            let mut data = std::mem::take(&mut build.borrow_mut().data);
            data.apps.sort_by_cached_key(|app| app.name.to_lowercase());
            catalog.apply_refresh(data);
            glib::ControlFlow::Break
        });
    }

    fn apply_refresh(self: &Rc<Self>, data: LauncherCatalogData) {
        self.data.replace(data);
        self.initialized.set(true);
        self.refresh_running.set(false);
        self.refresh_scheduled.set(false);

        for controller in self.live_controllers() {
            controller.catalog_changed();
        }

        if self.refresh_again.replace(false) {
            self.schedule_refresh();
        }
    }

    pub(super) fn notify_user_state_changed(&self, state: &LauncherUserState) {
        for controller in self.live_controllers() {
            controller.user_state_changed(state);
        }
    }

    fn live_controllers(&self) -> Vec<Rc<LauncherController>> {
        let mut subscribers = self.subscribers.borrow_mut();
        let mut controllers = Vec::with_capacity(subscribers.len());
        subscribers.retain(|subscriber| {
            if let Some(controller) = subscriber.upgrade() {
                controllers.push(controller);
                true
            } else {
                false
            }
        });
        controllers
    }
}

impl Drop for LauncherCatalog {
    fn drop(&mut self) {
        if let Some(handler) = self.app_monitor_handler.get_mut().take() {
            self.app_monitor.disconnect(handler);
        }
    }
}

fn launchable_app_identity(info: &gio::AppInfo) -> Option<(String, String, String, String)> {
    if !info.should_show() {
        return None;
    }

    let name = info.display_name().trim().to_owned();
    if name.is_empty() {
        return None;
    }

    let id = info.id().map(|value| value.to_string()).unwrap_or_default();
    let executable = info.executable().to_string_lossy().trim().to_owned();
    let key = if id.is_empty() {
        format!("fallback:{executable}::{name}")
    } else {
        format!("id:{id}")
    };

    Some((key, name, id, executable))
}

fn current_visible_app_keys() -> HashSet<String> {
    gio::AppInfo::all()
        .into_iter()
        .filter_map(|info| launchable_app_identity(&info).map(|(key, _, _, _)| key))
        .collect()
}

#[cfg(test)]
pub(super) fn catalog_app(
    info: gio::AppInfo,
    valid_keys: &mut HashSet<String>,
) -> Option<LaunchableApp> {
    build_catalog_app(info, valid_keys, desktop_metadata)
}

fn build_catalog_app(
    info: gio::AppInfo,
    valid_keys: &mut HashSet<String>,
    metadata: impl FnOnce(&str) -> (Vec<String>, String),
) -> Option<LaunchableApp> {
    let (key, name, id, executable) = launchable_app_identity(&info)?;
    if !valid_keys.insert(key.clone()) {
        return None;
    }

    let description = info
        .description()
        .map(|value| value.trim().to_owned())
        .unwrap_or_default();
    let (categories, keywords) = metadata(&id);
    let primary_category = classify_category(&categories);
    let search_blob = normalize_fields(
        [
            name.as_str(),
            description.as_str(),
            executable.as_str(),
            keywords.as_str(),
            id.as_str(),
        ]
        .into_iter()
        .chain(categories.iter().map(String::as_str)),
    );
    let meta = if !description.is_empty() {
        description
    } else if !executable.is_empty() {
        executable
    } else if !id.is_empty() {
        id
    } else {
        "Desktop application".to_owned()
    };

    Some(LaunchableApp {
        key,
        name,
        meta,
        primary_category,
        search_blob,
        info,
    })
}

pub(super) fn classify_category(categories: &[String]) -> Option<LauncherCategory> {
    LauncherCategory::PRIORITY.into_iter().find(|category| {
        category.matches().iter().any(|candidate| {
            categories
                .iter()
                .any(|actual| actual.eq_ignore_ascii_case(candidate))
        })
    })
}

fn desktop_metadata(id: &str) -> (Vec<String>, String) {
    if id.is_empty() {
        return (Vec::new(), String::new());
    }

    let key_file = glib::KeyFile::new();
    let desktop_file = PathBuf::from("applications").join(id);
    if key_file
        .load_from_data_dirs(&desktop_file, glib::KeyFileFlags::KEEP_TRANSLATIONS)
        .is_err()
    {
        return (Vec::new(), String::new());
    }

    let categories = key_file
        .string("Desktop Entry", "Categories")
        .ok()
        .map(|raw| desktop_values(&raw).map(ToOwned::to_owned).collect())
        .unwrap_or_default();

    let keywords = key_file
        .locale_string("Desktop Entry", "Keywords", None)
        .or_else(|_| key_file.string("Desktop Entry", "Keywords"))
        .map(|raw| desktop_values(&raw).collect::<Vec<_>>().join(" "))
        .unwrap_or_default();

    (categories, keywords)
}

fn desktop_values(raw: &str) -> impl Iterator<Item = &str> {
    raw.split(';')
        .map(str::trim)
        .filter(|value| !value.is_empty())
}
