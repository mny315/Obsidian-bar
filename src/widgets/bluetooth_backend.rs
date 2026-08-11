use std::{
    cell::RefCell,
    collections::HashMap,
    fmt,
    rc::{Rc, Weak},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use gio::glib;
use glib::variant::{ObjectPath, ToVariant};

use super::dbus::{object_path, variant_value};

const BLUEZ_SERVICE: &str = "org.bluez";
const BLUEZ_ROOT_PATH: &str = "/";
const BLUEZ_AGENT_MANAGER_PATH: &str = "/org/bluez";
const BLUEZ_OBJECT_MANAGER_INTERFACE: &str = "org.freedesktop.DBus.ObjectManager";
const BLUEZ_AGENT_MANAGER_INTERFACE: &str = "org.bluez.AgentManager1";
const BLUEZ_ADAPTER_INTERFACE: &str = "org.bluez.Adapter1";
const BLUEZ_DEVICE_INTERFACE: &str = "org.bluez.Device1";
const BLUEZ_BATTERY_INTERFACE: &str = "org.bluez.Battery1";
const DBUS_SERVICE: &str = "org.freedesktop.DBus";
const DBUS_INTERFACE: &str = "org.freedesktop.DBus";
const DBUS_PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";

const AGENT_PATH: &str = "/dev/obsidian_bar/BluetoothAgent";
const AGENT_CAPABILITY: &str = "KeyboardDisplay";
struct SnapshotCacheEntry {
    revision: u64,
    loaded_at: Instant,
    snapshot: Arc<BluetoothSnapshot>,
}
type AgentEventHandler = Rc<dyn Fn(AgentEvent)>;

static SNAPSHOT_REVISION: AtomicU64 = AtomicU64::new(0);
static SNAPSHOT_CACHE: OnceLock<Mutex<Option<SnapshotCacheEntry>>> = OnceLock::new();
static BLUETOOTH_SNAPSHOT_LOCK: Mutex<()> = Mutex::new(());
static BLUETOOTH_WRITE_LOCK: Mutex<()> = Mutex::new(());
const DBUS_TIMEOUT_MS: i32 = 8_000;
const SNAPSHOT_CACHE_TTL: Duration = Duration::from_secs(5);
const PAIR_TIMEOUT_MS: i32 = 120_000;
const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(700);
const WRITE_CALL_FLAGS: gio::DBusCallFlags = gio::DBusCallFlags::ALLOW_INTERACTIVE_AUTHORIZATION;

const AGENT_XML: &str = r#"
<node>
  <interface name="org.bluez.Agent1">
    <method name="Release" />
    <method name="RequestPinCode">
      <arg type="o" direction="in" name="device" />
      <arg type="s" direction="out" name="pincode" />
    </method>
    <method name="DisplayPinCode">
      <arg type="o" direction="in" name="device" />
      <arg type="s" direction="in" name="pincode" />
    </method>
    <method name="RequestPasskey">
      <arg type="o" direction="in" name="device" />
      <arg type="u" direction="out" name="passkey" />
    </method>
    <method name="DisplayPasskey">
      <arg type="o" direction="in" name="device" />
      <arg type="u" direction="in" name="passkey" />
      <arg type="q" direction="in" name="entered" />
    </method>
    <method name="RequestConfirmation">
      <arg type="o" direction="in" name="device" />
      <arg type="u" direction="in" name="passkey" />
    </method>
    <method name="RequestAuthorization">
      <arg type="o" direction="in" name="device" />
    </method>
    <method name="AuthorizeService">
      <arg type="o" direction="in" name="device" />
      <arg type="s" direction="in" name="uuid" />
    </method>
    <method name="Cancel" />
  </interface>
</node>
"#;

type PropertyMap = HashMap<String, glib::Variant>;
type InterfaceMap = HashMap<String, PropertyMap>;
type ManagedObjects = HashMap<ObjectPath, InterfaceMap>;

#[derive(Debug)]
struct DbusCallError {
    message: String,
    remote_name: Option<String>,
}

impl DbusCallError {
    fn from_glib(error: glib::Error) -> Self {
        let remote_name = gio::DBusError::remote_error(&error).map(|name| name.to_string());
        Self {
            message: error.to_string(),
            remote_name,
        }
    }
}

impl fmt::Display for DbusCallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AgentPromptKind {
    PinCode,
    Passkey,
    Confirmation { passkey: u32 },
    Authorization,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum AgentEvent {
    Prompt {
        device_path: String,
        kind: AgentPromptKind,
    },
    DisplayPinCode {
        device_path: String,
        pincode: String,
    },
    DisplayPasskey {
        device_path: String,
        passkey: u32,
        entered: u16,
    },
    Cancel,
    Release,
}

struct PendingAgentRequest {
    kind: AgentPromptKind,
    invocation: gio::DBusMethodInvocation,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct BluetoothSnapshot {
    pub adapter: Option<BluetoothAdapter>,
    pub devices: Vec<BluetoothDevice>,
}

impl BluetoothSnapshot {
    pub fn available(&self) -> bool {
        self.adapter.is_some()
    }

    pub fn powered(&self) -> bool {
        self.adapter.as_ref().is_some_and(|adapter| adapter.powered)
    }

    pub fn discovering(&self) -> bool {
        self.adapter
            .as_ref()
            .is_some_and(|adapter| adapter.discovering)
    }

    pub fn connected(&self) -> bool {
        self.devices.iter().any(|device| device.connected)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct BluetoothAdapter {
    pub path: String,
    pub alias: String,
    pub powered: bool,
    pub discovering: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct BluetoothDevice {
    pub path: String,
    pub name: String,
    pub icon: String,
    pub paired: bool,
    pub trusted: bool,
    pub connected: bool,
    pub battery: Option<u8>,
}

impl BluetoothDevice {
    pub fn known(&self) -> bool {
        self.paired || self.trusted || self.connected
    }
}

#[derive(Clone, Copy)]
pub(super) struct BluetoothBackend;

impl BluetoothBackend {
    pub fn subscribe_changes(
        &self,
        connection: &gio::DBusConnection,
        handler: Rc<dyn Fn()>,
    ) -> Vec<gio::SignalSubscription> {
        let object_manager_handler = Rc::clone(&handler);
        let object_manager = connection.subscribe_to_signal(
            Some(BLUEZ_SERVICE),
            Some(BLUEZ_OBJECT_MANAGER_INTERFACE),
            None,
            Some(BLUEZ_ROOT_PATH),
            None,
            gio::DBusSignalFlags::NONE,
            move |signal| {
                if bluez_signal_affects_snapshot(
                    signal.interface_name,
                    signal.signal_name,
                    signal.parameters,
                ) {
                    invalidate_snapshot_cache();
                    object_manager_handler();
                }
            },
        );

        let properties_handler = Rc::clone(&handler);
        let properties = connection.subscribe_to_signal(
            Some(BLUEZ_SERVICE),
            Some(DBUS_PROPERTIES_INTERFACE),
            Some("PropertiesChanged"),
            None,
            None,
            gio::DBusSignalFlags::NONE,
            move |signal| {
                if bluez_signal_affects_snapshot(
                    signal.interface_name,
                    signal.signal_name,
                    signal.parameters,
                ) {
                    invalidate_snapshot_cache();
                    properties_handler();
                }
            },
        );

        let owner = connection.subscribe_to_signal(
            Some(DBUS_SERVICE),
            Some(DBUS_INTERFACE),
            Some("NameOwnerChanged"),
            None,
            Some(BLUEZ_SERVICE),
            gio::DBusSignalFlags::NONE,
            move |_| {
                invalidate_snapshot_cache();
                handler();
            },
        );

        vec![object_manager, properties, owner]
    }

    pub fn snapshot(&self) -> Result<Arc<BluetoothSnapshot>, String> {
        if let Some(snapshot) = cached_snapshot() {
            return Ok(snapshot);
        }
        let _snapshot_guard = BLUETOOTH_SNAPSHOT_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(snapshot) = cached_snapshot() {
            return Ok(snapshot);
        }
        let mut last_snapshot = None;
        for _ in 0..2 {
            let revision = SNAPSHOT_REVISION.load(Ordering::Acquire);
            let preferred_adapter = {
                let cache = snapshot_cache()
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(entry) = cache.as_ref()
                    && entry.revision == revision
                    && entry.loaded_at.elapsed() < SNAPSHOT_CACHE_TTL
                {
                    return Ok(Arc::clone(&entry.snapshot));
                }
                cache
                    .as_ref()
                    .and_then(|entry| entry.snapshot.adapter.as_ref())
                    .map(|adapter| adapter.path.clone())
            };

            let snapshot = Arc::new(self.read_snapshot(preferred_adapter.as_deref())?);
            let latest_revision = SNAPSHOT_REVISION.load(Ordering::Acquire);
            if latest_revision == revision {
                let mut cache = snapshot_cache()
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                *cache = Some(SnapshotCacheEntry {
                    revision,
                    loaded_at: Instant::now(),
                    snapshot: Arc::clone(&snapshot),
                });
                return Ok(snapshot);
            }
            last_snapshot = Some(snapshot);
        }

        last_snapshot.ok_or_else(|| "failed to read Bluetooth state".to_owned())
    }

    fn read_snapshot(&self, preferred_adapter: Option<&str>) -> Result<BluetoothSnapshot, String> {
        let reply = match call_dbus(
            BLUEZ_ROOT_PATH,
            BLUEZ_OBJECT_MANAGER_INTERFACE,
            "GetManagedObjects",
            None,
            gio::DBusCallFlags::NONE,
            DBUS_TIMEOUT_MS,
        ) {
            Ok(reply) => reply,
            Err(error) => {
                if is_bluez_unavailable(&error) {
                    return Ok(BluetoothSnapshot::default());
                }
                return Err(format!("failed to read BlueZ objects: {error}"));
            }
        };
        let (objects,) = reply
            .get::<(ManagedObjects,)>()
            .ok_or_else(|| "BlueZ returned an invalid object tree".to_owned())?;

        let adapters = objects.iter().filter_map(|(path, interfaces)| {
            let properties = interfaces.get(BLUEZ_ADAPTER_INTERFACE)?;
            Some(BluetoothAdapter {
                path: path.as_str().to_owned(),
                alias: property::<String>(properties, "Alias")
                    .filter(|alias| !alias.trim().is_empty())
                    .unwrap_or_else(|| "Bluetooth".to_owned()),
                powered: property::<bool>(properties, "Powered").unwrap_or(false),
                discovering: property::<bool>(properties, "Discovering").unwrap_or(false),
            })
        });
        let Some(adapter) = select_adapter(adapters, preferred_adapter) else {
            return Ok(BluetoothSnapshot::default());
        };

        let mut devices = objects
            .iter()
            .filter_map(|(path, interfaces)| {
                let properties = interfaces.get(BLUEZ_DEVICE_INTERFACE)?;
                let adapter_path = property::<ObjectPath>(properties, "Adapter")?;
                if adapter_path.as_str() != adapter.path {
                    return None;
                }

                let address = property::<String>(properties, "Address").unwrap_or_default();
                let alias = property::<String>(properties, "Alias").unwrap_or_default();
                let name = if alias.trim().is_empty() {
                    property::<String>(properties, "Name")
                        .filter(|name| !name.trim().is_empty())
                        .unwrap_or_else(|| {
                            if address.is_empty() {
                                "Unknown device".to_owned()
                            } else {
                                address
                            }
                        })
                } else {
                    alias
                };
                let battery = interfaces
                    .get(BLUEZ_BATTERY_INTERFACE)
                    .and_then(|battery| property::<u8>(battery, "Percentage"));

                Some(BluetoothDevice {
                    path: path.as_str().to_owned(),
                    name,
                    icon: property::<String>(properties, "Icon").unwrap_or_default(),
                    paired: property::<bool>(properties, "Paired").unwrap_or(false),
                    trusted: property::<bool>(properties, "Trusted").unwrap_or(false),
                    connected: property::<bool>(properties, "Connected").unwrap_or(false),
                    battery,
                })
            })
            .collect::<Vec<_>>();
        devices.sort_by_cached_key(|device| {
            let rank = if device.connected {
                0
            } else if device.known() {
                1
            } else {
                2
            };
            (rank, device.name.to_lowercase(), device.path.clone())
        });

        Ok(BluetoothSnapshot {
            adapter: Some(adapter),
            devices,
        })
    }

    pub fn set_powered(&self, adapter_path: &str, powered: bool) -> Result<(), String> {
        let _io_guard = bluetooth_write_guard()?;
        let _mutation = SnapshotMutation::begin();
        set_property(
            adapter_path,
            BLUEZ_ADAPTER_INTERFACE,
            "Powered",
            powered.to_variant(),
        )
        .map_err(|error| {
            let action = if powered { "enable" } else { "disable" };
            format!("failed to {action} Bluetooth: {error}")
        })
    }

    pub fn start_discovery(&self, adapter_path: &str) -> Result<(), String> {
        let _io_guard = bluetooth_write_guard()?;
        let _mutation = SnapshotMutation::begin();
        call_method(
            adapter_path,
            BLUEZ_ADAPTER_INTERFACE,
            "StartDiscovery",
            None,
            DBUS_TIMEOUT_MS,
        )
        .map(|_| ())
        .or_else(|error| {
            if is_remote_error(&error, "org.bluez.Error.InProgress") {
                Ok(())
            } else {
                Err(format!("failed to start Bluetooth scan: {error}"))
            }
        })
    }

    pub fn stop_discovery(&self, adapter_path: &str) -> Result<(), String> {
        let _io_guard = bluetooth_write_guard()?;
        let _mutation = SnapshotMutation::begin();
        call_method(
            adapter_path,
            BLUEZ_ADAPTER_INTERFACE,
            "StopDiscovery",
            None,
            DBUS_TIMEOUT_MS,
        )
        .map(|_| ())
        .or_else(|error| {
            if is_remote_error(&error, "org.bluez.Error.NotReady") {
                Ok(())
            } else {
                Err(format!("failed to stop Bluetooth scan: {error}"))
            }
        })
    }

    pub fn set_connected(
        &self,
        device_path: &str,
        device_name: &str,
        paired: bool,
        trusted: bool,
        connected: bool,
    ) -> Result<(), String> {
        let _io_guard = bluetooth_write_guard()?;
        let _mutation = SnapshotMutation::begin();
        if !connected {
            return call_method(
                device_path,
                BLUEZ_DEVICE_INTERFACE,
                "Disconnect",
                None,
                DBUS_TIMEOUT_MS,
            )
            .map(|_| ())
            .or_else(|error| {
                if is_remote_error(&error, "org.bluez.Error.NotConnected") {
                    Ok(())
                } else {
                    Err(format!("failed to disconnect {device_name}: {error}"))
                }
            });
        }

        if !paired {
            register_agent_with_bluez()?;
            if let Err(error) = call_method(
                device_path,
                BLUEZ_DEVICE_INTERFACE,
                "Pair",
                None,
                PAIR_TIMEOUT_MS,
            ) && !is_remote_error(&error, "org.bluez.Error.AlreadyExists")
            {
                return Err(format!("failed to pair {device_name}: {error}"));
            }
        }

        if !trusted {
            set_property(
                device_path,
                BLUEZ_DEVICE_INTERFACE,
                "Trusted",
                true.to_variant(),
            )
            .map_err(|error| format!("failed to trust {device_name}: {error}"))?;
        }

        let mut last_error = None;
        for attempt in 0..3 {
            match call_method(
                device_path,
                BLUEZ_DEVICE_INTERFACE,
                "Connect",
                None,
                DBUS_TIMEOUT_MS,
            ) {
                Ok(_) => return Ok(()),
                Err(error) if is_remote_error(&error, "org.bluez.Error.AlreadyConnected") => {
                    return Ok(());
                }
                Err(error) => {
                    let transient = is_remote_error(&error, "org.bluez.Error.InProgress")
                        || is_remote_error(&error, "org.bluez.Error.NotReady")
                        || is_remote_error(&error, "org.bluez.Error.Failed");
                    last_error = Some(error);
                    if !transient || attempt == 2 {
                        break;
                    }
                    thread::sleep(CONNECT_RETRY_DELAY);
                }
            }
        }

        Err(format!(
            "failed to connect {device_name}: {}",
            last_error.map_or_else(
                || "unknown BlueZ error".to_owned(),
                |error| error.to_string(),
            )
        ))
    }

    pub fn remove_device(&self, adapter_path: &str, device_path: &str) -> Result<(), String> {
        let _io_guard = bluetooth_write_guard()?;
        let _mutation = SnapshotMutation::begin();
        let device = object_path(device_path)?;
        let parameters = (device,).to_variant();
        call_method(
            adapter_path,
            BLUEZ_ADAPTER_INTERFACE,
            "RemoveDevice",
            Some(&parameters),
            DBUS_TIMEOUT_MS,
        )
        .map(|_| ())
        .map_err(|error| format!("failed to remove Bluetooth device: {error}"))
    }

    pub fn cancel_pairing(&self, device_path: &str) -> Result<(), String> {
        // Must remain callable while Pair() is blocked in another worker.
        let _mutation = SnapshotMutation::begin();
        call_method(
            device_path,
            BLUEZ_DEVICE_INTERFACE,
            "CancelPairing",
            None,
            DBUS_TIMEOUT_MS,
        )
        .map(|_| ())
        .or_else(|error| {
            if is_remote_error(&error, "org.bluez.Error.DoesNotExist") {
                Ok(())
            } else {
                Err(format!("failed to cancel Bluetooth pairing: {error}"))
            }
        })
    }
}

fn select_adapter(
    adapters: impl Iterator<Item = BluetoothAdapter>,
    preferred_adapter: Option<&str>,
) -> Option<BluetoothAdapter> {
    adapters.min_by(|left, right| {
        let rank = |adapter: &BluetoothAdapter| {
            (
                !adapter.powered,
                preferred_adapter != Some(adapter.path.as_str()),
            )
        };
        rank(left)
            .cmp(&rank(right))
            .then_with(|| left.path.cmp(&right.path))
    })
}

fn bluetooth_write_guard() -> Result<std::sync::MutexGuard<'static, ()>, String> {
    match BLUETOOTH_WRITE_LOCK.try_lock() {
        Ok(guard) => Ok(guard),
        Err(std::sync::TryLockError::Poisoned(poisoned)) => Ok(poisoned.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => {
            Err("another Bluetooth action is already in progress".to_owned())
        }
    }
}

fn snapshot_cache() -> &'static Mutex<Option<SnapshotCacheEntry>> {
    SNAPSHOT_CACHE.get_or_init(|| Mutex::new(None))
}

fn cached_snapshot() -> Option<Arc<BluetoothSnapshot>> {
    let revision = SNAPSHOT_REVISION.load(Ordering::Acquire);
    snapshot_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .filter(|entry| {
            entry.revision == revision && entry.loaded_at.elapsed() < SNAPSHOT_CACHE_TTL
        })
        .map(|entry| Arc::clone(&entry.snapshot))
}

fn invalidate_snapshot_cache() {
    SNAPSHOT_REVISION.fetch_add(1, Ordering::AcqRel);
}

struct SnapshotMutation;

impl SnapshotMutation {
    fn begin() -> Self {
        invalidate_snapshot_cache();
        Self
    }
}

impl Drop for SnapshotMutation {
    fn drop(&mut self) {
        invalidate_snapshot_cache();
    }
}

struct AgentRegistration {
    connection: gio::DBusConnection,
    registration_id: Option<gio::RegistrationId>,
}

impl Drop for AgentRegistration {
    fn drop(&mut self) {
        if let Some(registration_id) = self.registration_id.take() {
            let _ = self.connection.unregister_object(registration_id);
        }
    }
}

struct AgentSession {
    id: u64,
    device_path: String,
    handler: AgentEventHandler,
}

#[derive(Default)]
struct AgentRuntime {
    registration: Option<AgentRegistration>,
    session: Option<AgentSession>,
    pending: Option<PendingAgentRequest>,
    next_session_id: u64,
}

impl AgentRuntime {
    fn next_session_id(&mut self) -> u64 {
        self.next_session_id = self.next_session_id.wrapping_add(1);
        if self.next_session_id == 0 {
            self.next_session_id = 1;
        }
        self.next_session_id
    }
}

impl Drop for AgentRuntime {
    fn drop(&mut self) {
        return_agent_error(
            self.pending.take(),
            "org.bluez.Error.Canceled",
            "Bluetooth agent is shutting down",
        );
    }
}

#[derive(Clone, Default)]
pub(crate) struct BluetoothAgent {
    runtime: Rc<RefCell<AgentRuntime>>,
}

impl BluetoothAgent {
    pub(super) fn begin_session(
        &self,
        device_path: &str,
        handler: AgentEventHandler,
    ) -> Option<AgentSessionGuard> {
        let (session_id, stale_request) = {
            let mut runtime = self.runtime.borrow_mut();
            if runtime.session.is_some() {
                return None;
            }

            let stale_request = runtime.pending.take();
            let session_id = runtime.next_session_id();
            runtime.session = Some(AgentSession {
                id: session_id,
                device_path: device_path.to_owned(),
                handler,
            });
            (session_id, stale_request)
        };

        return_agent_error(
            stale_request,
            "org.bluez.Error.Canceled",
            "Previous Bluetooth pairing session ended",
        );
        Some(AgentSessionGuard {
            runtime: Rc::downgrade(&self.runtime),
            session_id,
        })
    }

    pub(super) fn submit_input(&self, value: &str) -> Result<(), String> {
        let value = value.trim();
        if value.is_empty() {
            return Err("PIN or passkey is required".to_owned());
        }

        let (request, parameters) = {
            let mut runtime = self.runtime.borrow_mut();
            let request = runtime
                .pending
                .as_ref()
                .ok_or_else(|| "Bluetooth is not waiting for a PIN or passkey".to_owned())?;

            let parameters = match request.kind {
                AgentPromptKind::PinCode => {
                    if value.chars().count() > 16 {
                        return Err("PIN must be between 1 and 16 characters".to_owned());
                    }
                    (value.to_owned(),).to_variant()
                }
                AgentPromptKind::Passkey => {
                    if value.len() > 6 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                        return Err("Passkey must be a number from 000000 to 999999".to_owned());
                    }
                    let passkey = value
                        .parse::<u32>()
                        .map_err(|_| "Invalid Bluetooth passkey".to_owned())?;
                    if passkey > 999_999 {
                        return Err("Passkey must be a number from 000000 to 999999".to_owned());
                    }
                    (passkey,).to_variant()
                }
                _ => return Err("Bluetooth is not waiting for text input".to_owned()),
            };

            let request = runtime
                .pending
                .take()
                .ok_or_else(|| "Bluetooth pairing request disappeared".to_owned())?;
            (request, parameters)
        };

        request.invocation.return_value(Some(&parameters));
        Ok(())
    }

    pub(super) fn confirm_request(&self) -> Result<(), String> {
        let request = {
            let mut runtime = self.runtime.borrow_mut();
            let request = runtime
                .pending
                .as_ref()
                .ok_or_else(|| "Bluetooth is not waiting for confirmation".to_owned())?;
            if !matches!(
                request.kind,
                AgentPromptKind::Confirmation { .. } | AgentPromptKind::Authorization
            ) {
                return Err("Bluetooth is not waiting for confirmation".to_owned());
            }

            runtime
                .pending
                .take()
                .ok_or_else(|| "Bluetooth pairing request disappeared".to_owned())?
        };

        request.invocation.return_value(None);
        Ok(())
    }

    pub(super) fn reject_request(&self) {
        reject_agent_request_with(
            self.runtime.as_ref(),
            "org.bluez.Error.Rejected",
            "Bluetooth pairing rejected",
        );
    }

    pub(super) fn ensure_registered(&self) -> Result<(), String> {
        if self.runtime.borrow().registration.is_some() {
            return Ok(());
        }

        let node = gio::DBusNodeInfo::for_xml(AGENT_XML)
            .map_err(|error| format!("failed to parse Bluetooth agent interface: {error}"))?;
        let interface = node
            .lookup_interface("org.bluez.Agent1")
            .ok_or_else(|| "Bluetooth agent interface is missing".to_owned())?;
        let connection =
            system_bus().map_err(|error| format!("system D-Bus is unavailable: {error}"))?;
        let weak_runtime = Rc::downgrade(&self.runtime);

        let registration_id = connection
            .register_object(AGENT_PATH, &interface)
            .method_call(
                move |_connection,
                      _sender,
                      _object_path,
                      _interface_name,
                      method_name,
                      parameters,
                      invocation| {
                    let Some(runtime) = weak_runtime.upgrade() else {
                        invocation.return_dbus_error(
                            "org.bluez.Error.Canceled",
                            "Bluetooth agent is shutting down",
                        );
                        return;
                    };
                    handle_agent_method_call(
                        runtime.as_ref(),
                        method_name,
                        &parameters,
                        invocation,
                    );
                },
            )
            .build()
            .map_err(|error| format!("failed to export Bluetooth agent: {error}"))?;

        self.runtime.borrow_mut().registration = Some(AgentRegistration {
            connection,
            registration_id: Some(registration_id),
        });
        Ok(())
    }
}

pub(super) struct AgentSessionGuard {
    runtime: Weak<RefCell<AgentRuntime>>,
    session_id: u64,
}

impl Drop for AgentSessionGuard {
    fn drop(&mut self) {
        let Some(runtime) = self.runtime.upgrade() else {
            return;
        };
        let pending = {
            let mut runtime = runtime.borrow_mut();
            if runtime.session.as_ref().map(|session| session.id) != Some(self.session_id) {
                return;
            }

            drop(runtime.session.take());
            runtime.pending.take()
        };
        return_agent_error(
            pending,
            "org.bluez.Error.Canceled",
            "Bluetooth pairing session ended",
        );
    }
}

fn reject_agent_request_with(runtime: &RefCell<AgentRuntime>, error_name: &str, message: &str) {
    let request = runtime.borrow_mut().pending.take();
    return_agent_error(request, error_name, message);
}

fn return_agent_error(request: Option<PendingAgentRequest>, error_name: &str, message: &str) {
    if let Some(request) = request {
        request.invocation.return_dbus_error(error_name, message);
    }
}

fn emit_agent_event(runtime: &RefCell<AgentRuntime>, event: AgentEvent) {
    let handler = runtime
        .borrow()
        .session
        .as_ref()
        .map(|session| Rc::clone(&session.handler));
    if let Some(handler) = handler {
        handler(event);
    }
}

fn agent_session_matches(runtime: &RefCell<AgentRuntime>, device_path: &str) -> bool {
    runtime
        .borrow()
        .session
        .as_ref()
        .is_some_and(|session| session.device_path == device_path)
}

fn reject_mismatched_agent_device(
    runtime: &RefCell<AgentRuntime>,
    device_path: &str,
    invocation: &gio::DBusMethodInvocation,
) -> bool {
    if agent_session_matches(runtime, device_path) {
        return false;
    }
    invocation.clone().return_dbus_error(
        "org.bluez.Error.Rejected",
        "Bluetooth request does not match the active pairing session",
    );
    true
}

fn install_pending_agent_request(
    runtime: &RefCell<AgentRuntime>,
    device_path: String,
    kind: AgentPromptKind,
    invocation: gio::DBusMethodInvocation,
) {
    if !agent_session_matches(runtime, &device_path) {
        invocation.return_dbus_error(
            "org.bluez.Error.Rejected",
            "Bluetooth request does not match the active pairing session",
        );
        return;
    }

    let previous = runtime
        .borrow_mut()
        .pending
        .replace(PendingAgentRequest { kind, invocation });
    return_agent_error(
        previous,
        "org.bluez.Error.Canceled",
        "Superseded by another Bluetooth pairing request",
    );
    emit_agent_event(runtime, AgentEvent::Prompt { device_path, kind });
}

fn handle_agent_method_call(
    runtime: &RefCell<AgentRuntime>,
    method_name: &str,
    parameters: &glib::Variant,
    invocation: gio::DBusMethodInvocation,
) {
    match method_name {
        "RequestPinCode" => {
            let Some((device,)) = parameters.get::<(ObjectPath,)>() else {
                invocation.return_dbus_error(
                    "org.bluez.Error.Rejected",
                    "BlueZ sent an invalid PIN request",
                );
                return;
            };
            install_pending_agent_request(
                runtime,
                device.as_str().to_owned(),
                AgentPromptKind::PinCode,
                invocation,
            );
        }
        "DisplayPinCode" => {
            let Some((device, pincode)) = parameters.get::<(ObjectPath, String)>() else {
                invocation.return_dbus_error(
                    "org.bluez.Error.Rejected",
                    "BlueZ sent an invalid PIN display request",
                );
                return;
            };
            if reject_mismatched_agent_device(runtime, device.as_str(), &invocation) {
                return;
            }
            emit_agent_event(
                runtime,
                AgentEvent::DisplayPinCode {
                    device_path: device.as_str().to_owned(),
                    pincode,
                },
            );
            invocation.return_value(None);
        }
        "RequestPasskey" => {
            let Some((device,)) = parameters.get::<(ObjectPath,)>() else {
                invocation.return_dbus_error(
                    "org.bluez.Error.Rejected",
                    "BlueZ sent an invalid passkey request",
                );
                return;
            };
            install_pending_agent_request(
                runtime,
                device.as_str().to_owned(),
                AgentPromptKind::Passkey,
                invocation,
            );
        }
        "DisplayPasskey" => {
            let Some((device, passkey, entered)) = parameters.get::<(ObjectPath, u32, u16)>()
            else {
                invocation.return_dbus_error(
                    "org.bluez.Error.Rejected",
                    "BlueZ sent an invalid passkey display request",
                );
                return;
            };
            if reject_mismatched_agent_device(runtime, device.as_str(), &invocation) {
                return;
            }
            emit_agent_event(
                runtime,
                AgentEvent::DisplayPasskey {
                    device_path: device.as_str().to_owned(),
                    passkey,
                    entered,
                },
            );
            invocation.return_value(None);
        }
        "RequestConfirmation" => {
            let Some((device, passkey)) = parameters.get::<(ObjectPath, u32)>() else {
                invocation.return_dbus_error(
                    "org.bluez.Error.Rejected",
                    "BlueZ sent an invalid confirmation request",
                );
                return;
            };
            install_pending_agent_request(
                runtime,
                device.as_str().to_owned(),
                AgentPromptKind::Confirmation { passkey },
                invocation,
            );
        }
        "RequestAuthorization" => {
            let Some((device,)) = parameters.get::<(ObjectPath,)>() else {
                invocation.return_dbus_error(
                    "org.bluez.Error.Rejected",
                    "BlueZ sent an invalid authorization request",
                );
                return;
            };
            install_pending_agent_request(
                runtime,
                device.as_str().to_owned(),
                AgentPromptKind::Authorization,
                invocation,
            );
        }
        "AuthorizeService" => {
            let Some((device, _uuid)) = parameters.get::<(ObjectPath, String)>() else {
                invocation.return_dbus_error(
                    "org.bluez.Error.Rejected",
                    "BlueZ sent an invalid service authorization request",
                );
                return;
            };
            install_pending_agent_request(
                runtime,
                device.as_str().to_owned(),
                AgentPromptKind::Authorization,
                invocation,
            );
        }
        "Cancel" => {
            reject_agent_request_with(
                runtime,
                "org.bluez.Error.Canceled",
                "Bluetooth pairing canceled by BlueZ",
            );
            emit_agent_event(runtime, AgentEvent::Cancel);
            invocation.return_value(None);
        }
        "Release" => {
            reject_agent_request_with(
                runtime,
                "org.bluez.Error.Canceled",
                "Bluetooth pairing agent released by BlueZ",
            );
            emit_agent_event(runtime, AgentEvent::Release);
            invocation.return_value(None);
        }
        _ => invocation.return_dbus_error(
            "org.freedesktop.DBus.Error.UnknownMethod",
            "Unknown Bluetooth agent method",
        ),
    }
}

fn register_agent_with_bluez() -> Result<(), String> {
    let agent_path = object_path(AGENT_PATH)?;
    let parameters = (agent_path, AGENT_CAPABILITY).to_variant();

    if let Err(error) = call_method(
        BLUEZ_AGENT_MANAGER_PATH,
        BLUEZ_AGENT_MANAGER_INTERFACE,
        "RegisterAgent",
        Some(&parameters),
        DBUS_TIMEOUT_MS,
    ) && !is_remote_error(&error, "org.bluez.Error.AlreadyExists")
    {
        return Err(format!("failed to register Bluetooth agent: {error}"));
    }

    Ok(())
}

fn bluez_signal_affects_snapshot(
    interface_name: &str,
    signal_name: &str,
    parameters: &glib::Variant,
) -> bool {
    match (interface_name, signal_name) {
        (BLUEZ_OBJECT_MANAGER_INTERFACE, "InterfacesAdded") => parameters
            .get::<(ObjectPath, InterfaceMap)>()
            .is_none_or(|(_, interfaces)| interfaces.keys().any(|name| is_tracked_interface(name))),
        (BLUEZ_OBJECT_MANAGER_INTERFACE, "InterfacesRemoved") => parameters
            .get::<(ObjectPath, Vec<String>)>()
            .is_none_or(|(_, interfaces)| interfaces.iter().any(|name| is_tracked_interface(name))),
        (DBUS_PROPERTIES_INTERFACE, "PropertiesChanged") => parameters
            .get::<(String, PropertyMap, Vec<String>)>()
            .is_none_or(|(interface, changed, invalidated)| {
                let tracked = tracked_properties(&interface);
                !tracked.is_empty()
                    && (changed.keys().any(|name| tracked.contains(&name.as_str()))
                        || invalidated
                            .iter()
                            .any(|name| tracked.contains(&name.as_str())))
            }),
        _ => false,
    }
}

fn is_tracked_interface(interface: &str) -> bool {
    matches!(
        interface,
        BLUEZ_ADAPTER_INTERFACE | BLUEZ_DEVICE_INTERFACE | BLUEZ_BATTERY_INTERFACE
    )
}

fn tracked_properties(interface: &str) -> &'static [&'static str] {
    match interface {
        BLUEZ_ADAPTER_INTERFACE => &["Alias", "Powered", "Discovering"],
        BLUEZ_DEVICE_INTERFACE => &[
            "Adapter",
            "Address",
            "Alias",
            "Name",
            "Icon",
            "Paired",
            "Trusted",
            "Connected",
        ],
        BLUEZ_BATTERY_INTERFACE => &["Percentage"],
        _ => &[],
    }
}

fn is_bluez_unavailable(error: &DbusCallError) -> bool {
    matches!(
        error.remote_name.as_deref(),
        Some("org.freedesktop.DBus.Error.ServiceUnknown")
            | Some("org.freedesktop.DBus.Error.NameHasNoOwner")
    )
}

fn system_bus() -> Result<gio::DBusConnection, DbusCallError> {
    gio::bus_get_sync(gio::BusType::System, None::<&gio::Cancellable>)
        .map_err(DbusCallError::from_glib)
}

fn call_dbus(
    path: &str,
    interface: &str,
    method: &str,
    parameters: Option<&glib::Variant>,
    flags: gio::DBusCallFlags,
    timeout_ms: i32,
) -> Result<glib::Variant, DbusCallError> {
    system_bus()?
        .call_sync(
            Some(BLUEZ_SERVICE),
            path,
            interface,
            method,
            parameters,
            None,
            flags,
            timeout_ms,
            None::<&gio::Cancellable>,
        )
        .map_err(DbusCallError::from_glib)
}

fn call_method(
    path: &str,
    interface: &str,
    method: &str,
    parameters: Option<&glib::Variant>,
    timeout_ms: i32,
) -> Result<glib::Variant, DbusCallError> {
    call_dbus(
        path,
        interface,
        method,
        parameters,
        WRITE_CALL_FLAGS,
        timeout_ms,
    )
}

fn set_property(
    path: &str,
    interface: &str,
    property_name: &str,
    value: glib::Variant,
) -> Result<(), DbusCallError> {
    let parameters = (interface, property_name, value).to_variant();
    call_method(
        path,
        DBUS_PROPERTIES_INTERFACE,
        "Set",
        Some(&parameters),
        DBUS_TIMEOUT_MS,
    )
    .map(|_| ())
}

fn property<T: glib::variant::FromVariant>(properties: &PropertyMap, name: &str) -> Option<T> {
    properties.get(name).and_then(variant_value)
}

fn is_remote_error(error: &DbusCallError, name: &str) -> bool {
    error.remote_name.as_deref() == Some(name)
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, rc::Rc};

    use gio::glib::{self, variant::ToVariant};

    use super::{
        AgentEvent, BLUEZ_DEVICE_INTERFACE, BLUEZ_OBJECT_MANAGER_INTERFACE, BluetoothAdapter,
        BluetoothAgent, DBUS_PROPERTIES_INTERFACE, DbusCallError, InterfaceMap, PropertyMap,
        agent_session_matches, bluez_signal_affects_snapshot, is_bluez_unavailable,
        is_remote_error, select_adapter,
    };

    #[test]
    fn agent_session_is_scoped_to_the_requested_device() {
        let agent = BluetoothAgent::default();
        let _session = agent
            .begin_session("/org/bluez/hci0/dev_00", Rc::new(|_| {}))
            .expect("the session should start");

        assert!(agent_session_matches(
            agent.runtime.as_ref(),
            "/org/bluez/hci0/dev_00",
        ));
        assert!(!agent_session_matches(
            agent.runtime.as_ref(),
            "/org/bluez/hci0/dev_11",
        ));
    }

    #[test]
    fn agent_session_guard_releases_handler_and_exclusivity() {
        let agent = BluetoothAgent::default();
        let handler: Rc<dyn Fn(AgentEvent)> = Rc::new(|_| {});

        let first = agent
            .begin_session("/org/bluez/hci0/dev_00", Rc::clone(&handler))
            .expect("the first session should start");
        assert_eq!(Rc::strong_count(&handler), 2);
        assert!(
            agent
                .begin_session("/org/bluez/hci0/dev_00", Rc::new(|_| {}))
                .is_none()
        );

        drop(first);
        assert_eq!(Rc::strong_count(&handler), 1);
        assert!(
            agent
                .begin_session("/org/bluez/hci0/dev_00", Rc::new(|_| {}))
                .is_some()
        );
    }

    #[test]
    fn remote_bluez_error_matching_is_exact() {
        let error = DbusCallError::from_glib(gio::DBusError::new_for_dbus_error(
            "org.bluez.Error.InProgress",
            "Operation already in progress",
        ));

        assert!(is_remote_error(&error, "org.bluez.Error.InProgress"));
        assert!(!is_remote_error(&error, "org.bluez.Error.Failed"));
    }

    #[test]
    fn bluez_unavailable_errors_are_distinguished_from_action_errors() {
        let unavailable = DbusCallError::from_glib(gio::DBusError::new_for_dbus_error(
            "org.freedesktop.DBus.Error.ServiceUnknown",
            "BlueZ is not running",
        ));
        let failed = DbusCallError::from_glib(gio::DBusError::new_for_dbus_error(
            "org.bluez.Error.Failed",
            "Operation failed",
        ));

        assert!(is_bluez_unavailable(&unavailable));
        assert!(!is_bluez_unavailable(&failed));
    }

    #[test]
    fn signal_filter_ignores_noisy_device_properties() {
        let rssi = HashMap::from([("RSSI".to_owned(), (-42_i16).to_variant())]);
        let rssi_change = (
            BLUEZ_DEVICE_INTERFACE.to_owned(),
            rssi,
            Vec::<String>::new(),
        )
            .to_variant();
        assert!(!bluez_signal_affects_snapshot(
            DBUS_PROPERTIES_INTERFACE,
            "PropertiesChanged",
            &rssi_change,
        ));

        let connected = HashMap::from([("Connected".to_owned(), true.to_variant())]);
        let connected_change = (
            BLUEZ_DEVICE_INTERFACE.to_owned(),
            connected,
            Vec::<String>::new(),
        )
            .to_variant();
        assert!(bluez_signal_affects_snapshot(
            DBUS_PROPERTIES_INTERFACE,
            "PropertiesChanged",
            &connected_change,
        ));
    }

    #[test]
    fn signal_filter_tracks_relevant_interface_lifecycle() {
        let path = glib::variant::ObjectPath::try_from("/org/bluez/hci0/dev_00_11_22_33_44_55")
            .expect("valid object path");
        let irrelevant: InterfaceMap =
            HashMap::from([("org.bluez.MediaControl1".to_owned(), PropertyMap::new())]);
        let irrelevant_added = (path.clone(), irrelevant).to_variant();
        assert!(!bluez_signal_affects_snapshot(
            BLUEZ_OBJECT_MANAGER_INTERFACE,
            "InterfacesAdded",
            &irrelevant_added,
        ));

        let relevant: InterfaceMap =
            HashMap::from([(BLUEZ_DEVICE_INTERFACE.to_owned(), PropertyMap::new())]);
        let relevant_added = (path, relevant).to_variant();
        assert!(bluez_signal_affects_snapshot(
            BLUEZ_OBJECT_MANAGER_INTERFACE,
            "InterfacesAdded",
            &relevant_added,
        ));
    }

    #[test]
    fn adapter_selection_keeps_previous_adapter_when_all_are_off() {
        let adapters = vec![
            BluetoothAdapter {
                path: "/org/bluez/hci0".to_owned(),
                alias: "hci0".to_owned(),
                powered: false,
                discovering: false,
            },
            BluetoothAdapter {
                path: "/org/bluez/hci1".to_owned(),
                alias: "hci1".to_owned(),
                powered: false,
                discovering: false,
            },
        ];

        let selected = select_adapter(adapters.into_iter(), Some("/org/bluez/hci1"))
            .expect("an adapter should be selected");
        assert_eq!(selected.path, "/org/bluez/hci1");
    }

    #[test]
    fn adapter_selection_prefers_powered_adapter_over_inactive_previous_one() {
        let adapters = vec![
            BluetoothAdapter {
                path: "/org/bluez/hci0".to_owned(),
                alias: "hci0".to_owned(),
                powered: true,
                discovering: false,
            },
            BluetoothAdapter {
                path: "/org/bluez/hci1".to_owned(),
                alias: "hci1".to_owned(),
                powered: false,
                discovering: false,
            },
        ];

        let selected = select_adapter(adapters.into_iter(), Some("/org/bluez/hci1"))
            .expect("an adapter should be selected");
        assert_eq!(selected.path, "/org/bluez/hci0");
    }
}
