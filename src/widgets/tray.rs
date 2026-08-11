use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    rc::{Rc, Weak},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use super::{
    dbus::{unbox_variant, variant_value},
    tooltip::{BarTooltipExt, BarTooltipSuppression, hide_all_tooltips_immediately},
};
use gtk::{gdk, gio, glib, prelude::*};
use tracing::{debug, warn};

const DBUS_SERVICE: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const DBUS_INTERFACE: &str = "org.freedesktop.DBus";
const DBUS_PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";
const WATCHER_SERVICE: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const WATCHER_INTERFACE: &str = "org.kde.StatusNotifierWatcher";
const ITEM_INTERFACE: &str = "org.kde.StatusNotifierItem";
const ITEM_DEFAULT_PATH: &str = "/StatusNotifierItem";
const DBUSMENU_INTERFACE: &str = "com.canonical.dbusmenu";
const DBUS_TIMEOUT_MS: i32 = 2_000;
const DBUS_RETRY_BASE_DELAY: Duration = Duration::from_secs(1);
const DBUS_RETRY_MAX_DELAY: Duration = Duration::from_secs(30);
const CONTEXT_MENU_VERTICAL_OFFSET: i32 = 10;
const CONTEXT_MENU_REVEAL_DURATION: Duration = Duration::from_millis(220);
const CONTEXT_MENU_REVEAL_CLASS: &str = "tray-menu-opening";
const ICON_SIZE: i32 = 18;
const REQUEST_NAME_PRIMARY_OWNER: u32 = 1;
const REQUEST_NAME_ALREADY_OWNER: u32 = 4;
const REQUEST_NAME_DO_NOT_QUEUE: u32 = 4;
const ICON_SCAN_MAX_DEPTH: usize = 3;
const ICON_SCAN_MAX_ENTRIES: usize = 512;
const ICON_NEGATIVE_CACHE_TTL: Duration = Duration::from_secs(30);
const MENU_MAX_DEPTH: usize = 8;
const MENU_MAX_NODES: usize = 256;
const MENU_MAX_CHILDREN: usize = 64;

const WATCHER_XML: &str = r#"
<node>
  <interface name="org.kde.StatusNotifierWatcher">
    <method name="RegisterStatusNotifierItem">
      <arg type="s" name="service" direction="in"/>
    </method>
    <method name="RegisterStatusNotifierHost">
      <arg type="s" name="service" direction="in"/>
    </method>
    <property name="RegisteredStatusNotifierItems" type="as" access="read"/>
    <property name="IsStatusNotifierHostRegistered" type="b" access="read"/>
    <property name="ProtocolVersion" type="i" access="read"/>
    <signal name="StatusNotifierItemRegistered">
      <arg type="s" name="service"/>
    </signal>
    <signal name="StatusNotifierItemUnregistered">
      <arg type="s" name="service"/>
    </signal>
    <signal name="StatusNotifierHostRegistered"/>
  </interface>
</node>
"#;

type PropertyMap = HashMap<String, glib::Variant>;
type IconPixmaps = Vec<(i32, i32, Vec<u8>)>;
type IconFileCache = HashMap<(PathBuf, String), (Instant, Option<PathBuf>)>;

std::thread_local! {
    static ICON_FILE_CACHE: RefCell<IconFileCache> = RefCell::new(HashMap::new());
}

#[derive(Default)]
struct WatcherMirror {
    items: Vec<String>,
    host_registered: bool,
}

impl WatcherMirror {
    fn insert(&mut self, item: String) -> bool {
        if self.items.iter().any(|current| current == &item) {
            return false;
        }
        self.items.push(item);
        true
    }

    fn remove_service(&mut self, service: &str) -> Vec<String> {
        let mut removed = Vec::new();
        self.items.retain(|item| {
            let keep = split_item_id(item)
                .map(|(item_service, _)| item_service != service)
                .unwrap_or(true);
            if !keep {
                removed.push(item.clone());
            }
            keep
        });
        removed
    }

    fn remove_item(&mut self, item: &str) -> bool {
        let before = self.items.len();
        self.items.retain(|current| current != item);
        before != self.items.len()
    }
}

enum TrayEvent {
    Registered(String),
    ExternalRegistered {
        owner: String,
        item: String,
    },
    ExternalUnregistered {
        owner: String,
        item: String,
    },
    ItemChanged(String),
    RefreshRequested(String),
    NameOwnerChanged {
        name: String,
        new_owner: String,
    },
    ManagerReady(gio::DBusProxy),
    WatcherNameResult(bool),
    ExternalWatcherReady {
        proxy: Option<gio::DBusProxy>,
        error: Option<String>,
    },
    ExternalSnapshot {
        owner: String,
        items: Vec<String>,
    },
    ItemReady {
        request_id: u64,
        canonical: String,
        service: String,
        proxy: Option<gio::DBusProxy>,
        error: Option<String>,
    },
}

fn enqueue_tray_event(sender: &async_channel::Sender<TrayEvent>, event: TrayEvent) {
    match sender.try_send(event) {
        Ok(()) => {}
        Err(async_channel::TrySendError::Full(event)) => {
            let sender = sender.clone();
            glib::MainContext::default().spawn_local(async move {
                let _ = sender.send(event).await;
            });
        }
        Err(async_channel::TrySendError::Closed(_)) => {}
    }
}

pub struct TrayController {
    state: Rc<RefCell<TrayState>>,
}

impl TrayController {
    pub fn new() -> Self {
        let mirror = Arc::new(Mutex::new(WatcherMirror::default()));
        let (events_tx, events_rx) = async_channel::bounded::<TrayEvent>(256);
        let state = Rc::new(RefCell::new(TrayState::new(mirror, events_tx.clone())));

        let weak_state = Rc::downgrade(&state);
        glib::MainContext::default().spawn_local(async move {
            while let Ok(event) = events_rx.recv().await {
                let Some(state) = weak_state.upgrade() else {
                    break;
                };
                state.borrow_mut().handle_event(event);
            }
        });

        TrayState::initialize(&state);
        Self { state }
    }
}

pub struct TrayIndicator {
    view: Rc<TrayView>,
}

impl TrayIndicator {
    pub fn new(controller: &TrayController) -> Self {
        let view = Rc::new(TrayView::new());
        controller.state.borrow_mut().attach_view(&view);
        Self { view }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.view.root
    }

    pub fn dismiss(&self) {
        self.view.dismiss();
    }
}

struct TrayState {
    connection: Option<gio::DBusConnection>,
    manager: Option<gio::DBusProxy>,
    external_watcher: Option<gio::DBusProxy>,
    watcher_registration: Option<gio::RegistrationId>,
    owns_watcher: bool,
    mirror: Arc<Mutex<WatcherMirror>>,
    items: Vec<Rc<TrayItem>>,
    pending_items: HashMap<String, u64>,
    next_item_request_id: u64,
    views: Vec<Weak<TrayView>>,
    events: async_channel::Sender<TrayEvent>,
    bus_get_pending: bool,
    bus_retry_pending: bool,
    bus_retry_attempt: u32,
}

impl TrayState {
    fn new(mirror: Arc<Mutex<WatcherMirror>>, events: async_channel::Sender<TrayEvent>) -> Self {
        Self {
            connection: None,
            manager: None,
            external_watcher: None,
            watcher_registration: None,
            owns_watcher: false,
            mirror,
            items: Vec::new(),
            pending_items: HashMap::new(),
            next_item_request_id: 0,
            views: Vec::new(),
            events,
            bus_get_pending: false,
            bus_retry_pending: false,
            bus_retry_attempt: 0,
        }
    }

    fn initialize(this: &Rc<RefCell<Self>>) {
        {
            let mut state = this.borrow_mut();
            if state.connection.is_some() || state.bus_get_pending {
                return;
            }
            state.bus_get_pending = true;
        }

        let weak = Rc::downgrade(this);
        gio::bus_get(
            gio::BusType::Session,
            None::<&gio::Cancellable>,
            move |result| {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                this.borrow_mut().bus_get_pending = false;
                let connection = match result {
                    Ok(connection) => connection,
                    Err(error) => {
                        warn!(%error, "session D-Bus is unavailable; retrying tray");
                        Self::schedule_initialize_retry(&this);
                        return;
                    }
                };

                let mut state = this.borrow_mut();
                state.bus_retry_attempt = 0;
                let events = state.events.clone();
                state.connection = Some(connection.clone());
                state.install_name_owner_listener(&events);
                let mirror = state.mirror.clone();
                state.register_watcher_object(&connection, &events, mirror);
                request_name(&connection, WATCHER_SERVICE, &events);
            },
        );
    }

    fn schedule_initialize_retry(this: &Rc<RefCell<Self>>) {
        let delay = {
            let mut state = this.borrow_mut();
            if state.connection.is_some() || state.bus_retry_pending {
                return;
            }
            state.bus_retry_pending = true;
            let multiplier = 1_u32 << state.bus_retry_attempt.min(5);
            state.bus_retry_attempt = state.bus_retry_attempt.saturating_add(1);
            DBUS_RETRY_BASE_DELAY
                .saturating_mul(multiplier)
                .min(DBUS_RETRY_MAX_DELAY)
        };

        let weak = Rc::downgrade(this);
        glib::timeout_add_local_once(delay, move || {
            let Some(this) = weak.upgrade() else {
                return;
            };
            this.borrow_mut().bus_retry_pending = false;
            Self::initialize(&this);
        });
    }

    fn activate_owned_watcher(&mut self) {
        if let Ok(mut mirror) = self.mirror.lock() {
            mirror.host_registered = true;
            for item in &self.items {
                mirror.insert(item.id.clone());
            }
        }
        self.external_watcher = None;
        debug!("StatusNotifierWatcher acquired");
    }

    fn install_name_owner_listener(&mut self, events: &async_channel::Sender<TrayEvent>) {
        let events = events.clone();
        gio::DBusProxy::for_bus(
            gio::BusType::Session,
            gio::DBusProxyFlags::DO_NOT_LOAD_PROPERTIES,
            None,
            DBUS_SERVICE,
            DBUS_PATH,
            DBUS_INTERFACE,
            None::<&gio::Cancellable>,
            move |result| match result {
                Ok(manager) => enqueue_tray_event(&events, TrayEvent::ManagerReady(manager)),
                Err(error) => warn!(%error, "failed to watch session D-Bus names"),
            },
        );
    }

    fn finish_name_owner_listener(&mut self, manager: gio::DBusProxy) {
        let signal_events = self.events.clone();
        manager.connect_g_signal(Some("NameOwnerChanged"), move |_, _, _, parameters| {
            let Some((name, _old_owner, new_owner)) = parameters.get::<(String, String, String)>()
            else {
                return;
            };
            enqueue_tray_event(
                &signal_events,
                TrayEvent::NameOwnerChanged { name, new_owner },
            );
        });
        self.manager = Some(manager);
    }

    fn register_watcher_object(
        &mut self,
        connection: &gio::DBusConnection,
        events: &async_channel::Sender<TrayEvent>,
        mirror: Arc<Mutex<WatcherMirror>>,
    ) {
        let node = match gio::DBusNodeInfo::for_xml(WATCHER_XML) {
            Ok(node) => node,
            Err(error) => {
                warn!(%error, "failed to parse StatusNotifierWatcher interface");
                return;
            }
        };
        let Some(interface) = node.lookup_interface(WATCHER_INTERFACE) else {
            warn!("StatusNotifierWatcher interface is missing");
            return;
        };

        let method_events = events.clone();
        let method_mirror = mirror.clone();
        let property_mirror = mirror;
        let registration = connection
            .register_object(WATCHER_PATH, &interface)
            .method_call(
                move |connection,
                      sender,
                      _object_path,
                      _interface_name,
                      method_name,
                      parameters,
                      invocation| {
                    match method_name {
                        "RegisterStatusNotifierItem" => {
                            let Some((raw,)) = parameters.get::<(String,)>() else {
                                invocation.return_dbus_error(
                                    "org.freedesktop.DBus.Error.InvalidArgs",
                                    "Expected one service or object path",
                                );
                                return;
                            };
                            let Some(item) = normalize_item_id(&raw, sender) else {
                                invocation.return_dbus_error(
                                    "org.freedesktop.DBus.Error.InvalidArgs",
                                    "Invalid StatusNotifierItem identifier",
                                );
                                return;
                            };

                            let inserted = method_mirror
                                .lock()
                                .map(|mut mirror| mirror.insert(item.clone()))
                                .unwrap_or(false);
                            if inserted {
                                let signal = (item.clone(),).to_variant();
                                let _ = connection.emit_signal(
                                    None::<&str>,
                                    WATCHER_PATH,
                                    WATCHER_INTERFACE,
                                    "StatusNotifierItemRegistered",
                                    Some(&signal),
                                );
                                enqueue_tray_event(&method_events, TrayEvent::Registered(item));
                            }
                            invocation.return_value(None);
                        }
                        "RegisterStatusNotifierHost" => {
                            let changed = method_mirror
                                .lock()
                                .map(|mut mirror| {
                                    let changed = !mirror.host_registered;
                                    mirror.host_registered = true;
                                    changed
                                })
                                .unwrap_or(false);
                            if changed {
                                let _ = connection.emit_signal(
                                    None::<&str>,
                                    WATCHER_PATH,
                                    WATCHER_INTERFACE,
                                    "StatusNotifierHostRegistered",
                                    None,
                                );
                            }
                            invocation.return_value(None);
                        }
                        _ => invocation.return_dbus_error(
                            "org.freedesktop.DBus.Error.UnknownMethod",
                            "Unknown StatusNotifierWatcher method",
                        ),
                    }
                },
            )
            .property(move |_, _, _, _, property_name| {
                let mirror = property_mirror.lock().ok();
                match property_name {
                    "RegisteredStatusNotifierItems" => mirror
                        .as_ref()
                        .map(|mirror| mirror.items.clone())
                        .unwrap_or_default()
                        .to_variant(),
                    "IsStatusNotifierHostRegistered" => mirror
                        .as_ref()
                        .map(|mirror| mirror.host_registered)
                        .unwrap_or(false)
                        .to_variant(),
                    "ProtocolVersion" => 0i32.to_variant(),
                    _ => ().to_variant(),
                }
            })
            .build();

        match registration {
            Ok(registration) => self.watcher_registration = Some(registration),
            Err(error) => warn!(%error, "failed to export StatusNotifierWatcher"),
        }
    }

    fn attach_external_watcher(&mut self, events: &async_channel::Sender<TrayEvent>) {
        let events = events.clone();
        gio::DBusProxy::for_bus(
            gio::BusType::Session,
            gio::DBusProxyFlags::DO_NOT_AUTO_START,
            None,
            WATCHER_SERVICE,
            WATCHER_PATH,
            WATCHER_INTERFACE,
            None::<&gio::Cancellable>,
            move |result| {
                let (proxy, error) = match result {
                    Ok(proxy) if proxy.name_owner().is_some() => (Some(proxy), None),
                    Ok(_) => (None, None),
                    Err(error) => (None, Some(error.to_string())),
                };
                enqueue_tray_event(&events, TrayEvent::ExternalWatcherReady { proxy, error });
            },
        );
    }

    fn finish_external_watcher(&mut self, watcher: gio::DBusProxy) {
        if self.owns_watcher {
            return;
        }
        let Some(owner) = watcher.name_owner().map(|owner| owner.as_str().to_owned()) else {
            return;
        };
        let signal_owner = owner.clone();
        let signal_events = self.events.clone();
        watcher.connect_g_signal(None::<&str>, move |_, _, signal_name, parameters| {
            match signal_name {
                "StatusNotifierItemRegistered" => {
                    if let Some((item,)) = parameters.get::<(String,)>() {
                        enqueue_tray_event(
                            &signal_events,
                            TrayEvent::ExternalRegistered {
                                owner: signal_owner.clone(),
                                item,
                            },
                        );
                    }
                }
                "StatusNotifierItemUnregistered" => {
                    if let Some((item,)) = parameters.get::<(String,)>() {
                        enqueue_tray_event(
                            &signal_events,
                            TrayEvent::ExternalUnregistered {
                                owner: signal_owner.clone(),
                                item,
                            },
                        );
                    }
                }
                _ => {}
            }
        });

        if let Some(items) =
            cached_property::<Vec<String>>(&watcher, "RegisteredStatusNotifierItems")
        {
            enqueue_tray_event(&self.events, TrayEvent::ExternalSnapshot { owner, items });
        }

        if let Some(connection) = self.connection.as_ref()
            && let Some(unique_name) = connection.unique_name()
        {
            let parameters = (unique_name.as_str(),).to_variant();
            watcher.call(
                "RegisterStatusNotifierHost",
                Some(&parameters),
                gio::DBusCallFlags::NONE,
                DBUS_TIMEOUT_MS,
                None::<&gio::Cancellable>,
                |_| {},
            );
        }

        self.external_watcher = Some(watcher);
    }

    fn handle_event(&mut self, event: TrayEvent) {
        match event {
            TrayEvent::Registered(item) => self.register_item(&item),
            TrayEvent::ExternalRegistered { owner, item } => {
                if self.has_external_owner(&owner) {
                    self.register_item(&item);
                }
            }
            TrayEvent::ExternalUnregistered { owner, item } => {
                if self.has_external_owner(&owner) {
                    self.unregister_item(&item);
                }
            }
            TrayEvent::ItemChanged(item_id) => {
                if let Some(item) = self.items.iter().find(|item| item.id == item_id).cloned() {
                    self.update_views(&item);
                }
            }
            TrayEvent::RefreshRequested(item_id) => {
                if let Some(item) = self.items.iter().find(|item| item.id == item_id).cloned() {
                    item.schedule_refresh();
                }
            }
            TrayEvent::ManagerReady(manager) => self.finish_name_owner_listener(manager),
            TrayEvent::WatcherNameResult(owns_watcher) => {
                self.owns_watcher = owns_watcher;
                if owns_watcher {
                    self.activate_owned_watcher();
                } else {
                    let events = self.events.clone();
                    self.attach_external_watcher(&events);
                }
            }
            TrayEvent::ExternalWatcherReady { proxy, error } => {
                if let Some(proxy) = proxy {
                    self.finish_external_watcher(proxy);
                } else if let Some(error) = error {
                    debug!(%error, "no external StatusNotifierWatcher available");
                }
            }
            TrayEvent::NameOwnerChanged { name, new_owner } => {
                if new_owner.is_empty() {
                    self.remove_service(&name);
                }

                if name == WATCHER_SERVICE {
                    let our_unique_name = self
                        .connection
                        .as_ref()
                        .and_then(gio::DBusConnection::unique_name)
                        .map(|name| name.as_str().to_owned());
                    let still_owned_by_us = our_unique_name
                        .as_deref()
                        .is_some_and(|our_name| our_name == new_owner);

                    if self.owns_watcher && !still_owned_by_us {
                        self.owns_watcher = false;
                    }
                    if !self.owns_watcher {
                        self.external_watcher = None;
                        if new_owner.is_empty() {
                            if let Some(connection) = self.connection.as_ref() {
                                request_name(connection, WATCHER_SERVICE, &self.events);
                            }
                        } else if !still_owned_by_us {
                            let events = self.events.clone();
                            self.attach_external_watcher(&events);
                        }
                    }
                }
            }
            TrayEvent::ExternalSnapshot { owner, items } => {
                if self.has_external_owner(&owner) {
                    self.apply_external_snapshot(items);
                }
            }
            TrayEvent::ItemReady {
                request_id,
                canonical,
                service,
                proxy,
                error,
            } => self.finish_item_registration(request_id, canonical, service, proxy, error),
        }
    }

    fn has_external_owner(&self, owner: &str) -> bool {
        !self.owns_watcher
            && self
                .external_watcher
                .as_ref()
                .and_then(gio::DBusProxy::name_owner)
                .is_some_and(|current| current.as_str() == owner)
    }

    fn register_item(&mut self, item_id: &str) {
        self.insert_item(item_id);
    }

    fn insert_item(&mut self, item_id: &str) -> bool {
        let Some(canonical) = normalize_item_id(item_id, None) else {
            return false;
        };
        let Some((service, path)) = split_item_id(&canonical) else {
            return false;
        };
        if self.items.iter().any(|item| item.id == canonical)
            || self.pending_items.contains_key(&canonical)
        {
            return false;
        }
        self.next_item_request_id = self.next_item_request_id.wrapping_add(1).max(1);
        let request_id = self.next_item_request_id;
        self.pending_items.insert(canonical.clone(), request_id);
        if self.owns_watcher
            && let Ok(mut mirror) = self.mirror.lock()
        {
            mirror.insert(canonical.clone());
        }

        let events = self.events.clone();
        let service_owned = service.to_owned();
        let canonical_for_callback = canonical.clone();
        gio::DBusProxy::for_bus(
            gio::BusType::Session,
            gio::DBusProxyFlags::DO_NOT_AUTO_START,
            None,
            service,
            path,
            ITEM_INTERFACE,
            None::<&gio::Cancellable>,
            move |result| {
                let (proxy, error) = match result {
                    Ok(proxy) if proxy.name_owner().is_some() => (Some(proxy), None),
                    Ok(_) => (
                        None,
                        Some("StatusNotifierItem has no D-Bus owner".to_owned()),
                    ),
                    Err(error) => (None, Some(error.to_string())),
                };
                enqueue_tray_event(
                    &events,
                    TrayEvent::ItemReady {
                        request_id,
                        canonical: canonical_for_callback,
                        service: service_owned,
                        proxy,
                        error,
                    },
                );
            },
        );
        true
    }

    fn finish_item_registration(
        &mut self,
        request_id: u64,
        canonical: String,
        service: String,
        proxy: Option<gio::DBusProxy>,
        error: Option<String>,
    ) {
        if self.pending_items.get(&canonical).copied() != Some(request_id) {
            return;
        }
        self.pending_items.remove(&canonical);
        let Some(proxy) = proxy else {
            debug!(
                error = error.as_deref().unwrap_or("unknown error"),
                item = %canonical,
                "failed to connect tray item"
            );
            if let Ok(mut mirror) = self.mirror.lock() {
                mirror.remove_item(&canonical);
            }
            return;
        };
        if self.items.iter().any(|item| item.id == canonical) {
            return;
        }

        self.items.push(TrayItem::from_proxy(
            canonical,
            service,
            proxy,
            &self.events,
        ));
        self.sync_views();
    }

    fn apply_external_snapshot(&mut self, items: Vec<String>) {
        let canonical_items = items
            .into_iter()
            .filter_map(|item| normalize_item_id(&item, None))
            .collect::<Vec<_>>();
        let before = self.items.len();
        {
            let live_items = canonical_items
                .iter()
                .map(String::as_str)
                .collect::<HashSet<_>>();
            self.items
                .retain(|item| live_items.contains(item.id.as_str()));
            self.pending_items
                .retain(|item, _| live_items.contains(item.as_str()));
        }

        let changed = before != self.items.len();
        for item in canonical_items {
            self.insert_item(&item);
        }
        if changed {
            self.sync_views();
        }
    }

    fn unregister_item(&mut self, item_id: &str) {
        let Some(canonical) = normalize_item_id(item_id, None) else {
            return;
        };
        let before = self.items.len();
        self.pending_items.remove(&canonical);
        self.items.retain(|item| item.id != canonical);
        if let Ok(mut mirror) = self.mirror.lock() {
            mirror.remove_item(&canonical);
        }
        if before != self.items.len() {
            self.sync_views();
        }
    }

    fn remove_service(&mut self, service: &str) {
        let before = self.items.len();
        self.pending_items.retain(|item, _| {
            split_item_id(item).is_none_or(|(item_service, _)| item_service != service)
        });
        self.items.retain(|item| item.service != service);
        let items_changed = before != self.items.len();
        let mirror_removed = self
            .mirror
            .lock()
            .map(|mut mirror| mirror.remove_service(service))
            .unwrap_or_default();

        if self.owns_watcher
            && let Some(connection) = self.connection.as_ref()
        {
            for item in mirror_removed {
                let parameters = (item,).to_variant();
                let _ = connection.emit_signal(
                    None::<&str>,
                    WATCHER_PATH,
                    WATCHER_INTERFACE,
                    "StatusNotifierItemUnregistered",
                    Some(&parameters),
                );
            }
        }
        if items_changed {
            self.sync_views();
        }
    }

    fn attach_view(&mut self, view: &Rc<TrayView>) {
        self.views.push(Rc::downgrade(view));
        view.sync(&self.items);
    }

    fn update_views(&mut self, item: &Rc<TrayItem>) {
        self.views.retain(|view| {
            if let Some(view) = view.upgrade() {
                view.update_item(item);
                true
            } else {
                false
            }
        });
    }

    fn sync_views(&mut self) {
        self.views.retain(|view| {
            if let Some(view) = view.upgrade() {
                view.sync(&self.items);
                true
            } else {
                false
            }
        });
    }
}

impl Drop for TrayState {
    fn drop(&mut self) {
        if let (Some(connection), Some(registration)) =
            (self.connection.as_ref(), self.watcher_registration.take())
        {
            let _ = connection.unregister_object(registration);
        }
    }
}

struct TrayView {
    root: gtk::Box,
    buttons: RefCell<HashMap<String, TrayButton>>,
}

impl TrayView {
    fn new() -> Self {
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        root.add_css_class("section");
        root.add_css_class("tray-capsule");
        root.set_halign(gtk::Align::End);
        root.set_valign(gtk::Align::Center);
        root.set_visible(false);
        Self {
            root,
            buttons: RefCell::new(HashMap::new()),
        }
    }

    fn sync(&self, items: &[Rc<TrayItem>]) {
        let live_items = items
            .iter()
            .map(|item| item.id.as_str())
            .collect::<HashSet<_>>();

        self.buttons.borrow_mut().retain(|item_id, button| {
            if live_items.contains(item_id.as_str()) {
                true
            } else {
                self.root.remove(button.widget());
                false
            }
        });

        for item in items {
            self.update_item(item);
        }
        self.sync_visibility();
    }

    fn update_item(&self, item: &Rc<TrayItem>) {
        let Some(snapshot) = item.snapshot() else {
            if let Some(button) = self.buttons.borrow_mut().remove(&item.id) {
                self.root.remove(button.widget());
            }
            self.sync_visibility();
            return;
        };

        if let Some(button) = self.buttons.borrow().get(&item.id) {
            button.update(snapshot);
            return;
        }

        let button = item.build_button(snapshot);
        self.root.append(button.widget());
        self.buttons.borrow_mut().insert(item.id.clone(), button);
        self.sync_visibility();
    }

    fn sync_visibility(&self) {
        self.root.set_visible(!self.buttons.borrow().is_empty());
    }

    fn dismiss(&self) {
        for button in self.buttons.borrow().values() {
            if let Some(popover) = button.root.popover() {
                popover.popdown();
            }
        }
    }
}

struct TrayButton {
    root: gtk::MenuButton,
    image: gtk::Image,
    tooltip: RefCell<String>,
    icon_signature: RefCell<TrayIconSignature>,
}

impl TrayButton {
    fn widget(&self) -> &gtk::MenuButton {
        &self.root
    }

    fn update(&self, snapshot: TraySnapshot) {
        let icon_signature = snapshot.icon_signature();
        if *self.tooltip.borrow() != snapshot.tooltip {
            self.root.set_bar_tooltip_text(Some(&snapshot.tooltip));
        }
        if *self.icon_signature.borrow() != icon_signature {
            apply_icon(&self.image, &snapshot);
        }
        *self.tooltip.borrow_mut() = snapshot.tooltip;
        *self.icon_signature.borrow_mut() = icon_signature;
    }
}

struct TrayItem {
    id: String,
    service: String,
    proxy: gio::DBusProxy,
    menu_proxy: RefCell<Option<(String, gio::DBusProxy)>>,
    events: async_channel::Sender<TrayEvent>,
    refresh_pending: Cell<bool>,
}

impl TrayItem {
    fn from_proxy(
        id: String,
        service: String,
        proxy: gio::DBusProxy,
        events: &async_channel::Sender<TrayEvent>,
    ) -> Rc<Self> {
        let item = Rc::new(Self {
            id,
            service,
            proxy,
            menu_proxy: RefCell::new(None),
            events: events.clone(),
            refresh_pending: Cell::new(false),
        });
        item.connect_signals();
        item
    }

    fn connect_signals(self: &Rc<Self>) {
        let changed_id = self.id.clone();
        let changed_events = self.events.clone();
        self.proxy.connect_g_properties_changed(move |_, _, _| {
            enqueue_tray_event(&changed_events, TrayEvent::ItemChanged(changed_id.clone()));
        });

        let refresh_id = self.id.clone();
        let refresh_events = self.events.clone();
        self.proxy
            .connect_g_signal(None::<&str>, move |_, _, signal_name, _| {
                if matches!(
                    signal_name,
                    "NewTitle"
                        | "NewIcon"
                        | "NewAttentionIcon"
                        | "NewOverlayIcon"
                        | "NewToolTip"
                        | "NewStatus"
                ) {
                    enqueue_tray_event(
                        &refresh_events,
                        TrayEvent::RefreshRequested(refresh_id.clone()),
                    );
                }
            });
    }

    fn schedule_refresh(self: &Rc<Self>) {
        if self.refresh_pending.replace(true) {
            return;
        }

        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            let Some(item) = weak.upgrade() else {
                return;
            };
            item.refresh_pending.set(false);
            refresh_item_properties(&item.proxy, item.id.clone(), item.events.clone());
        });
    }

    fn snapshot(&self) -> Option<TraySnapshot> {
        let status = cached_property::<String>(&self.proxy, "Status").unwrap_or_default();
        let title = cached_property::<String>(&self.proxy, "Title")
            .filter(|title| !title.is_empty())
            .or_else(|| cached_property::<String>(&self.proxy, "Id"))
            .unwrap_or_else(|| self.service.clone());
        let tooltip = tooltip_text(&self.proxy).unwrap_or_else(|| title.clone());
        let attention = status == "NeedsAttention";
        let icon_name_property = if attention {
            "AttentionIconName"
        } else {
            "IconName"
        };
        let icon_pixmap_property = if attention {
            "AttentionIconPixmap"
        } else {
            "IconPixmap"
        };
        let icon_name = cached_property::<String>(&self.proxy, icon_name_property)
            .filter(|name| !name.is_empty());
        let icon_pixmaps = if icon_name.is_some() {
            None
        } else {
            cached_property::<IconPixmaps>(&self.proxy, icon_pixmap_property)
                .filter(|pixmaps| !pixmaps.is_empty())
        };
        let icon_theme_path =
            cached_property::<String>(&self.proxy, "IconThemePath").filter(|path| !path.is_empty());
        if icon_name.is_none() && icon_pixmaps.is_none() {
            return None;
        }

        Some(TraySnapshot {
            tooltip,
            icon_name,
            icon_pixmaps,
            icon_theme_path,
        })
    }

    fn build_button(self: &Rc<Self>, snapshot: TraySnapshot) -> TrayButton {
        let button = gtk::MenuButton::new();
        button.add_css_class("tray-item");
        button.set_has_frame(false);
        button.set_always_show_arrow(false);
        button.set_bar_tooltip_text(Some(&snapshot.tooltip));
        button.set_focusable(false);

        let image = gtk::Image::new();
        image.set_pixel_size(ICON_SIZE);
        image.set_halign(gtk::Align::Center);
        image.set_valign(gtk::Align::Center);
        apply_icon(&image, &snapshot);
        button.set_child(Some(&image));

        button.set_popover(Some(&gtk::Popover::new()));

        let primary = gtk::GestureClick::new();
        primary.set_button(gdk::BUTTON_PRIMARY);
        primary.set_propagation_phase(gtk::PropagationPhase::Capture);
        let item = self.clone();
        primary.connect_pressed(move |gesture, _, _, _| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            item.activate();
        });
        button.add_controller(primary);

        let secondary = gtk::GestureClick::new();
        secondary.set_button(gdk::BUTTON_SECONDARY);
        secondary.set_propagation_phase(gtk::PropagationPhase::Capture);
        let item = self.clone();
        let weak_button = button.downgrade();
        secondary.connect_pressed(move |gesture, _, _, _| {
            // MenuButton has its own pointer handling and this tray gesture claims
            // the secondary-button sequence. Hide the layer-shell tooltip here
            // explicitly instead of relying only on the generic tooltip gesture.
            let tooltip_suppression = BarTooltipSuppression::begin();
            gesture.set_state(gtk::EventSequenceState::Claimed);
            if let Some(button) = weak_button.upgrade() {
                item.open_context_menu(&button, tooltip_suppression);
            }
        });
        button.add_controller(secondary);
        let icon_signature = snapshot.icon_signature();
        TrayButton {
            root: button,
            image,
            tooltip: RefCell::new(snapshot.tooltip),
            icon_signature: RefCell::new(icon_signature),
        }
    }

    fn activate(&self) {
        call_item_method(&self.proxy, "Activate", Some(&(0i32, 0i32).to_variant()));
    }

    fn open_context_menu(
        self: &Rc<Self>,
        anchor: &gtk::MenuButton,
        tooltip_suppression: Rc<BarTooltipSuppression>,
    ) {
        let menu_path = cached_property::<glib::variant::ObjectPath>(&self.proxy, "Menu")
            .map(|path| path.as_str().to_owned())
            .filter(|path| path != "/");
        let Some(menu_path) = menu_path else {
            open_item_context_menu(&self.proxy, tooltip_suppression);
            return;
        };

        let cached = self.menu_proxy.borrow().as_ref().and_then(|(path, proxy)| {
            (path == &menu_path && proxy.name_owner().is_some()).then(|| proxy.clone())
        });
        if let Some(proxy) = cached {
            self.load_context_menu(anchor, proxy, tooltip_suppression);
            return;
        }

        let weak_self = Rc::downgrade(self);
        let weak_anchor = anchor.downgrade();
        let cached_menu_path = menu_path.clone();
        gio::DBusProxy::for_bus(
            gio::BusType::Session,
            gio::DBusProxyFlags::DO_NOT_AUTO_START,
            None,
            &self.service,
            &menu_path,
            DBUSMENU_INTERFACE,
            None::<&gio::Cancellable>,
            move |result| {
                let (Some(this), Some(anchor)) = (weak_self.upgrade(), weak_anchor.upgrade())
                else {
                    return;
                };
                match result {
                    Ok(proxy) => {
                        this.menu_proxy
                            .replace(Some((cached_menu_path.clone(), proxy.clone())));
                        this.load_context_menu(&anchor, proxy, tooltip_suppression);
                    }
                    Err(error) => {
                        debug!(%error, item = %this.id, "failed to connect DBusMenu");
                        open_item_context_menu(&this.proxy, tooltip_suppression);
                    }
                }
            },
        );
    }

    fn load_context_menu(
        self: &Rc<Self>,
        anchor: &gtk::MenuButton,
        menu_proxy: gio::DBusProxy,
        tooltip_suppression: Rc<BarTooltipSuppression>,
    ) {
        let about_to_show = (0i32,).to_variant();
        let weak_anchor = anchor.downgrade();
        let fallback_proxy = self.proxy.clone();
        let menu_proxy_for_layout = menu_proxy.clone();
        menu_proxy.call(
            "AboutToShow",
            Some(&about_to_show),
            gio::DBusCallFlags::NONE,
            DBUS_TIMEOUT_MS,
            None::<&gio::Cancellable>,
            move |_| {
                let parameters = (0i32, -1i32, Vec::<String>::new()).to_variant();
                let menu_proxy_for_reply = menu_proxy_for_layout.clone();
                let fallback_proxy_for_reply = fallback_proxy.clone();
                let weak_anchor_for_reply = weak_anchor.clone();
                menu_proxy_for_layout.call(
                    "GetLayout",
                    Some(&parameters),
                    gio::DBusCallFlags::NONE,
                    DBUS_TIMEOUT_MS,
                    None::<&gio::Cancellable>,
                    move |result| {
                        let Some(anchor) = weak_anchor_for_reply.upgrade() else {
                            return;
                        };
                        let root = result.ok().and_then(|reply| parse_menu_layout(&reply));
                        if let Some(root) =
                            root.filter(|root| has_visible_menu_item(&root.children))
                        {
                            show_menu_popover(
                                &anchor,
                                &menu_proxy_for_reply,
                                &root.children,
                                tooltip_suppression,
                            );
                        } else {
                            open_item_context_menu(&fallback_proxy_for_reply, tooltip_suppression);
                        }
                    },
                );
            },
        );
    }
}

struct TraySnapshot {
    tooltip: String,
    icon_name: Option<String>,
    icon_pixmaps: Option<IconPixmaps>,
    icon_theme_path: Option<String>,
}

impl TraySnapshot {
    fn icon_signature(&self) -> TrayIconSignature {
        let pixmap_hash = self.icon_pixmaps.as_ref().map(|pixmaps| {
            let mut hasher = DefaultHasher::new();
            pixmaps.hash(&mut hasher);
            hasher.finish()
        });
        TrayIconSignature {
            icon_name: self.icon_name.clone(),
            icon_theme_path: self.icon_theme_path.clone(),
            pixmap_hash,
        }
    }
}

#[derive(PartialEq, Eq)]
struct TrayIconSignature {
    icon_name: Option<String>,
    icon_theme_path: Option<String>,
    pixmap_hash: Option<u64>,
}

#[derive(Default)]
struct MenuNode {
    id: i32,
    label: String,
    item_type: String,
    enabled: bool,
    visible: bool,
    toggle_type: String,
    toggle_state: i32,
    icon_name: Option<String>,
    children: Vec<MenuNode>,
}

fn has_visible_menu_item(nodes: &[MenuNode]) -> bool {
    nodes
        .iter()
        .any(|node| node.visible && node.item_type != "separator")
}

fn show_menu_popover(
    anchor: &gtk::MenuButton,
    proxy: &gio::DBusProxy,
    nodes: &[MenuNode],
    tooltip_suppression: Rc<BarTooltipSuppression>,
) {
    // The popover can synthesize a fresh pointer-enter on its anchor. Merely
    // hiding the current tooltip is therefore insufficient: after the normal
    // 420 ms delay it can be scheduled again over the open menu. Keep the bar
    // tooltip system suspended until this root context menu actually closes.
    hide_all_tooltips_immediately();

    let popover = gtk::Popover::new();
    popover.add_css_class("tray-menu-popover-window");
    popover.set_has_arrow(false);
    popover.set_position(gtk::PositionType::Bottom);
    popover.set_offset(0, CONTEXT_MENU_VERTICAL_OFFSET);
    popover.set_autohide(true);

    let suppression_holder = Rc::new(RefCell::new(Some(tooltip_suppression)));
    let suppression_on_closed = Rc::clone(&suppression_holder);
    popover.connect_closed(move |_| {
        suppression_on_closed.borrow_mut().take();
    });
    let suppression_on_visibility = Rc::clone(&suppression_holder);
    popover.connect_visible_notify(move |popover| {
        if !popover.is_visible() {
            suppression_on_visibility.borrow_mut().take();
        }
    });

    let content = build_menu_box(proxy, nodes, &popover);
    install_menu_reveal(&popover);
    popover.set_child(Some(&content));
    anchor.set_popover(Some(&popover));
    anchor.popup();
}

fn build_menu_box(
    proxy: &gio::DBusProxy,
    nodes: &[MenuNode],
    root_popover: &gtk::Popover,
) -> gtk::Box {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    for node in nodes.iter().filter(|node| node.visible) {
        if node.item_type == "separator" {
            content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
            continue;
        }

        if !node.children.is_empty() {
            let menu_button = gtk::MenuButton::new();
            menu_button.add_css_class("tray-menu-row");
            menu_button.set_sensitive(node.enabled);
            menu_button.set_child(Some(&menu_row_content(node, true)));

            let submenu = gtk::Popover::new();
            submenu.add_css_class("tray-menu-popover-window");
            submenu.add_css_class("tray-submenu-popover-window");
            submenu.set_has_arrow(false);
            submenu.set_position(gtk::PositionType::Right);
            let submenu_content = build_menu_box(proxy, &node.children, root_popover);
            install_menu_reveal(&submenu);
            submenu.set_child(Some(&submenu_content));
            menu_button.set_popover(Some(&submenu));
            content.append(&menu_button);
            continue;
        }

        let button = gtk::Button::new();
        button.add_css_class("tray-menu-row");
        button.set_sensitive(node.enabled);
        button.set_child(Some(&menu_row_content(node, false)));
        let id = node.id;
        let proxy = proxy.clone();
        let weak_root_popover = root_popover.downgrade();
        button.connect_clicked(move |_| {
            send_menu_event(&proxy, id);
            if let Some(root_popover) = weak_root_popover.upgrade() {
                root_popover.popdown();
            }
        });
        content.append(&button);
    }
    content
}

fn install_menu_reveal(popover: &gtk::Popover) {
    popover.set_opacity(0.0);
    let reveal_generation = Rc::new(Cell::new(0_u64));
    let reveal_generation_for_notify = Rc::clone(&reveal_generation);
    popover.connect_visible_notify(move |popover| {
        let generation = reveal_generation_for_notify.get().wrapping_add(1);
        reveal_generation_for_notify.set(generation);
        popover.remove_css_class(CONTEXT_MENU_REVEAL_CLASS);

        if !popover.is_visible() {
            popover.set_opacity(0.0);
            return;
        }

        popover.set_opacity(0.0);
        let weak_popover = popover.downgrade();
        let reveal_generation = Rc::clone(&reveal_generation_for_notify);
        glib::idle_add_local_once(move || {
            let Some(popover) = weak_popover.upgrade() else {
                return;
            };
            if reveal_generation.get() != generation || !popover.is_visible() {
                return;
            }

            popover.add_css_class(CONTEXT_MENU_REVEAL_CLASS);
            popover.set_opacity(1.0);
            let weak_popover = popover.downgrade();
            let reveal_generation = Rc::clone(&reveal_generation);
            glib::timeout_add_local_once(CONTEXT_MENU_REVEAL_DURATION, move || {
                if reveal_generation.get() == generation
                    && let Some(popover) = weak_popover.upgrade()
                {
                    popover.remove_css_class(CONTEXT_MENU_REVEAL_CLASS);
                }
            });
        });
    });
}

fn menu_row_content(node: &MenuNode, submenu: bool) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.set_hexpand(true);

    let indicator = gtk::Label::new(None);
    indicator.add_css_class("tray-menu-indicator");
    indicator.set_width_chars(1);
    indicator.set_label(if node.toggle_state > 0 {
        if node.toggle_type == "radio" {
            "●"
        } else {
            "✓"
        }
    } else {
        ""
    });
    row.append(&indicator);

    if let Some(icon_name) = node.icon_name.as_deref() {
        let icon = gtk::Image::from_icon_name(icon_name);
        icon.set_pixel_size(16);
        row.append(&icon);
    }

    let label = gtk::Label::new(Some(&normalize_menu_label(&node.label)));
    label.add_css_class("tray-menu-label");
    label.set_xalign(0.0);
    label.set_hexpand(true);
    row.append(&label);

    if submenu {
        let arrow = gtk::Label::new(Some("›"));
        arrow.add_css_class("tray-menu-arrow");
        row.append(&arrow);
    }
    row
}

fn parse_menu_layout(reply: &glib::Variant) -> Option<MenuNode> {
    if reply.n_children() < 2 {
        return None;
    }
    let mut remaining = MENU_MAX_NODES;
    parse_menu_node(&reply.child_value(1), 0, &mut remaining)
}

fn parse_menu_node(value: &glib::Variant, depth: usize, remaining: &mut usize) -> Option<MenuNode> {
    if depth > MENU_MAX_DEPTH || *remaining == 0 {
        return None;
    }
    *remaining -= 1;
    let value = unbox_variant(value);
    if value.n_children() < 3 {
        return None;
    }

    let id = value.child_value(0).get::<i32>()?;
    let properties = value
        .child_value(1)
        .get::<PropertyMap>()
        .unwrap_or_default();
    let children_value = value.child_value(2);
    let mut children = Vec::new();
    let child_count = children_value.n_children().min(MENU_MAX_CHILDREN);
    for index in 0..child_count {
        if let Some(child) =
            parse_menu_node(&children_value.child_value(index), depth + 1, remaining)
        {
            children.push(child);
        }
        if *remaining == 0 {
            break;
        }
    }

    Some(MenuNode {
        id,
        label: map_property::<String>(&properties, "label").unwrap_or_default(),
        item_type: map_property::<String>(&properties, "type")
            .unwrap_or_else(|| "standard".to_owned()),
        enabled: map_property::<bool>(&properties, "enabled").unwrap_or(true),
        visible: map_property::<bool>(&properties, "visible").unwrap_or(true),
        toggle_type: map_property::<String>(&properties, "toggle-type").unwrap_or_default(),
        toggle_state: map_property::<i32>(&properties, "toggle-state").unwrap_or(0),
        icon_name: map_property::<String>(&properties, "icon-name").filter(|name| !name.is_empty()),
        children,
    })
}

fn send_menu_event(proxy: &gio::DBusProxy, id: i32) {
    let data = 0i32.to_variant();
    let parameters = (id, "clicked", data, 0u32).to_variant();
    proxy.call(
        "Event",
        Some(&parameters),
        gio::DBusCallFlags::NONE,
        DBUS_TIMEOUT_MS,
        None::<&gio::Cancellable>,
        move |result| {
            if let Err(error) = result {
                debug!(%error, id, "DBusMenu event failed");
            }
        },
    );
}

fn apply_icon(image: &gtk::Image, snapshot: &TraySnapshot) {
    image.clear();

    if let Some(icon_name) = snapshot.icon_name.as_deref() {
        apply_icon_name(image, icon_name, snapshot.icon_theme_path.as_deref());
        return;
    }

    if let Some(pixbuf) = snapshot.icon_pixmaps.as_ref().and_then(pixbuf_from_pixmaps) {
        image.set_from_gicon(&pixbuf);
        return;
    }

    image.set_icon_name(Some("image-missing-symbolic"));
}

fn apply_icon_name(image: &gtk::Image, icon_name: &str, icon_theme_path: Option<&str>) {
    let direct_path = Path::new(icon_name);
    if direct_path.is_file() {
        set_file_icon(image, direct_path);
        return;
    }

    if let Some(theme_path) = icon_theme_path {
        let theme_root = Path::new(theme_path);
        if let Some(path) = find_icon_file(theme_root, icon_name) {
            set_file_icon(image, &path);
            return;
        }
    }

    let icon = gio::ThemedIcon::new(icon_name);
    image.set_from_gicon(&icon);
}

fn set_file_icon(image: &gtk::Image, path: &Path) {
    let file = gio::File::for_path(path);
    let icon = gio::FileIcon::new(&file);
    image.set_from_gicon(&icon);
}

fn find_icon_file(icon_root: &Path, icon_name: &str) -> Option<PathBuf> {
    let key = (icon_root.to_path_buf(), icon_name.to_owned());
    ICON_FILE_CACHE.with(|cache| {
        match cache.borrow().get(&key).cloned() {
            Some((_, Some(path))) if path.is_file() => return Some(path),
            Some((loaded_at, None)) if loaded_at.elapsed() < ICON_NEGATIVE_CACHE_TTL => {
                return None;
            }
            Some(_) => {
                cache.borrow_mut().remove(&key);
            }
            None => {}
        }

        let resolved = scan_icon_file(icon_root, icon_name);
        cache
            .borrow_mut()
            .insert(key, (Instant::now(), resolved.clone()));
        resolved
    })
}

fn scan_icon_file(icon_root: &Path, icon_name: &str) -> Option<PathBuf> {
    if !icon_root.is_dir() {
        return None;
    }

    let direct_path = icon_root.join(icon_name);
    if direct_path.is_file() {
        return Some(direct_path);
    }

    let wanted = Path::new(icon_name)
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or(icon_name);
    let mut stack = vec![(icon_root.to_path_buf(), 0usize)];
    let mut inspected = 0usize;

    while let Some((directory, depth)) = stack.pop() {
        if depth > ICON_SCAN_MAX_DEPTH || inspected >= ICON_SCAN_MAX_ENTRIES {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            inspected += 1;
            if inspected > ICON_SCAN_MAX_ENTRIES {
                break;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if file_type.is_dir() && depth < ICON_SCAN_MAX_DEPTH {
                stack.push((path, depth + 1));
            } else if file_type.is_file()
                && path.file_stem().and_then(|name| name.to_str()) == Some(wanted)
            {
                return Some(path);
            }
        }
    }

    None
}

fn pixbuf_from_pixmaps(pixmaps: &IconPixmaps) -> Option<gdk_pixbuf::Pixbuf> {
    let (width, height, argb, pixel_count) = pixmaps
        .iter()
        .filter_map(|(width, height, bytes)| {
            if *width <= 0 || *height <= 0 {
                return None;
            }
            let width_usize = usize::try_from(*width).ok()?;
            let height_usize = usize::try_from(*height).ok()?;
            let pixel_count = width_usize.checked_mul(height_usize)?;
            let byte_count = pixel_count.checked_mul(4)?;
            (bytes.len() >= byte_count).then_some((width, height, bytes, pixel_count))
        })
        .min_by_key(|entry| {
            let edge = (*entry.0).max(*entry.1);
            if edge >= ICON_SIZE {
                (0, edge - ICON_SIZE)
            } else {
                (1, ICON_SIZE - edge)
            }
        })?;

    let mut rgba = Vec::with_capacity(pixel_count.checked_mul(4)?);
    for pixel in argb.chunks_exact(4).take(pixel_count) {
        rgba.extend_from_slice(&[pixel[1], pixel[2], pixel[3], pixel[0]]);
    }

    let rowstride = (*width).checked_mul(4)?;
    let bytes = glib::Bytes::from_owned(rgba);
    Some(gdk_pixbuf::Pixbuf::from_bytes(
        &bytes,
        gdk_pixbuf::Colorspace::Rgb,
        true,
        8,
        *width,
        *height,
        rowstride,
    ))
}

fn refresh_item_properties(
    proxy: &gio::DBusProxy,
    item_id: String,
    events: async_channel::Sender<TrayEvent>,
) {
    let Some(destination) = proxy.name() else {
        enqueue_tray_event(&events, TrayEvent::ItemChanged(item_id));
        return;
    };
    let object_path = proxy.object_path();
    let parameters = (ITEM_INTERFACE,).to_variant();
    let cache = proxy.clone();

    proxy.connection().call(
        Some(destination.as_str()),
        object_path.as_str(),
        DBUS_PROPERTIES_INTERFACE,
        "GetAll",
        Some(&parameters),
        None::<&glib::VariantTy>,
        gio::DBusCallFlags::NONE,
        DBUS_TIMEOUT_MS,
        None::<&gio::Cancellable>,
        move |result| {
            if let Ok(reply) = result
                && let Some((properties,)) = reply.get::<(PropertyMap,)>()
            {
                for (name, value) in properties {
                    let value = unbox_variant(&value);
                    cache.set_cached_property(&name, Some(&value));
                }
            }
            enqueue_tray_event(&events, TrayEvent::ItemChanged(item_id));
        },
    );
}

fn tooltip_text(proxy: &gio::DBusProxy) -> Option<String> {
    let tooltip = proxy.cached_property("ToolTip")?;
    let tooltip = unbox_variant(&tooltip);
    if tooltip.n_children() < 4 {
        return None;
    }

    let title = tooltip.child_value(2).get::<String>().unwrap_or_default();
    let subtitle = tooltip.child_value(3).get::<String>().unwrap_or_default();
    match (title.is_empty(), subtitle.is_empty()) {
        (false, false) => Some(format!("{title}\n{subtitle}")),
        (false, true) => Some(title),
        (true, false) => Some(subtitle),
        (true, true) => None,
    }
}

fn open_item_context_menu(proxy: &gio::DBusProxy, tooltip_suppression: Rc<BarTooltipSuppression>) {
    // Native SNI menus live outside this process, so there is no GTK popover
    // close signal to observe. Keep suppression through the popup transition;
    // after that, the native menu owns the pointer and normal leave handling is
    // enough to prevent the old tray tooltip from returning.
    hide_all_tooltips_immediately();
    call_item_method(proxy, "ContextMenu", Some(&(0i32, 0i32).to_variant()));
    glib::timeout_add_local_once(Duration::from_millis(900), move || {
        drop(tooltip_suppression);
    });
}

fn call_item_method(
    proxy: &gio::DBusProxy,
    method: &'static str,
    parameters: Option<&glib::Variant>,
) {
    proxy.call(
        method,
        parameters,
        gio::DBusCallFlags::NONE,
        DBUS_TIMEOUT_MS,
        None::<&gio::Cancellable>,
        move |result| {
            if let Err(error) = result {
                debug!(%error, %method, "StatusNotifierItem method failed");
            }
        },
    );
}

fn request_name(
    connection: &gio::DBusConnection,
    name: &str,
    events: &async_channel::Sender<TrayEvent>,
) {
    let parameters = (name, REQUEST_NAME_DO_NOT_QUEUE).to_variant();
    let events = events.clone();
    connection.call(
        Some(DBUS_SERVICE),
        DBUS_PATH,
        DBUS_INTERFACE,
        "RequestName",
        Some(&parameters),
        None::<&glib::VariantTy>,
        gio::DBusCallFlags::NONE,
        DBUS_TIMEOUT_MS,
        None::<&gio::Cancellable>,
        move |result| {
            let owns = result
                .ok()
                .and_then(|reply| reply.get::<(u32,)>())
                .is_some_and(|(result,)| {
                    matches!(
                        result,
                        REQUEST_NAME_PRIMARY_OWNER | REQUEST_NAME_ALREADY_OWNER
                    )
                });
            enqueue_tray_event(&events, TrayEvent::WatcherNameResult(owns));
        },
    );
}

fn normalize_item_id(raw: &str, sender: Option<&str>) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if raw.starts_with('/') {
        return sender.map(|sender| format!("{sender}{raw}"));
    }
    if raw.contains('/') {
        return split_item_id(raw).map(|(service, path)| format!("{service}{path}"));
    }
    Some(format!("{raw}{ITEM_DEFAULT_PATH}"))
}

fn split_item_id(item: &str) -> Option<(&str, &str)> {
    let slash = item.find('/')?;
    let (service, path) = item.split_at(slash);
    if service.is_empty() || path.is_empty() {
        None
    } else {
        Some((service, path))
    }
}

fn normalize_menu_label(label: &str) -> String {
    let mut normalized = String::with_capacity(label.len());
    let mut characters = label.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '_' {
            normalized.push(character);
        } else if characters.next_if_eq(&'_').is_some() {
            normalized.push('_');
        }
    }
    normalized
}

fn cached_property<T: glib::variant::FromVariant>(proxy: &gio::DBusProxy, name: &str) -> Option<T> {
    proxy
        .cached_property(name)
        .and_then(|value| variant_value(&value))
}

fn map_property<T: glib::variant::FromVariant>(properties: &PropertyMap, name: &str) -> Option<T> {
    properties.get(name).and_then(variant_value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_status_notifier_item_ids() {
        assert_eq!(
            normalize_item_id("/StatusNotifierItem", Some(":1.42")),
            Some(":1.42/StatusNotifierItem".to_owned())
        );
        assert_eq!(
            normalize_item_id("org.example.Tray", None),
            Some("org.example.Tray/StatusNotifierItem".to_owned())
        );
        assert_eq!(
            normalize_item_id("org.example.Tray/Custom", None),
            Some("org.example.Tray/Custom".to_owned())
        );
        assert_eq!(normalize_item_id("   ", None), None);
    }

    #[test]
    fn strips_menu_mnemonics_but_preserves_escaped_underscores() {
        assert_eq!(normalize_menu_label("_Open"), "Open");
        assert_eq!(normalize_menu_label("Save __As"), "Save _As");
        assert_eq!(normalize_menu_label("A___B"), "A_B");
    }
}
