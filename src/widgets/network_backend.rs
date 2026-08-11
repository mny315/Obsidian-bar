use std::{
    cmp::Reverse,
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use gio::{glib, prelude::*};
use glib::variant::{ObjectPath, ToVariant};

use super::dbus::{object_path, variant_value};

const NM_SERVICE: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const NM_INTERFACE: &str = "org.freedesktop.NetworkManager";
const NM_DEVICE_INTERFACE: &str = "org.freedesktop.NetworkManager.Device";
const NM_WIRELESS_INTERFACE: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const NM_ACCESS_POINT_INTERFACE: &str = "org.freedesktop.NetworkManager.AccessPoint";
const NM_SETTINGS_PATH: &str = "/org/freedesktop/NetworkManager/Settings";
const NM_SETTINGS_INTERFACE: &str = "org.freedesktop.NetworkManager.Settings";
const NM_SETTINGS_CONNECTION_INTERFACE: &str = "org.freedesktop.NetworkManager.Settings.Connection";
const DBUS_PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";

const SYSTEMD_SERVICE: &str = "org.freedesktop.systemd1";
const SYSTEMD_PATH: &str = "/org/freedesktop/systemd1";
const SYSTEMD_MANAGER_INTERFACE: &str = "org.freedesktop.systemd1.Manager";
const SYSTEMD_UNIT_INTERFACE: &str = "org.freedesktop.systemd1.Unit";
const VLESS_UNIT: &str = "sing-box.service";

const NM_DEVICE_TYPE_WIFI: u32 = 2;
const NM_AP_FLAGS_PRIVACY: u32 = 0x1;
const NM_AP_SEC_KEY_MGMT_PSK: u32 = 0x0000_0100;
const NM_AP_SEC_KEY_MGMT_802_1X: u32 = 0x0000_0200;
const NM_AP_SEC_KEY_MGMT_SAE: u32 = 0x0000_0400;
const NM_AP_SEC_KEY_MGMT_OWE: u32 = 0x0000_0800;
const NM_AP_SEC_KEY_MGMT_OWE_TM: u32 = 0x0000_1000;
const NM_AP_SEC_KEY_MGMT_EAP_SUITE_B_192: u32 = 0x0000_2000;
const ROOT_OBJECT_PATH: &str = "/";
const DBUS_TIMEOUT_MS: i32 = 5_000;
const NETWORK_CACHE_TTL: Duration = Duration::from_millis(500);
static NETWORK_SNAPSHOT_LOCK: Mutex<()> = Mutex::new(());
static NETWORK_WRITE_LOCK: Mutex<()> = Mutex::new(());
static NETWORK_SNAPSHOT_CACHE: OnceLock<Mutex<Option<(Instant, WifiSnapshot)>>> = OnceLock::new();
static SAVED_WIFI_CACHE: OnceLock<Arc<Mutex<SavedWifiCache>>> = OnceLock::new();
const WRITE_CALL_FLAGS: gio::DBusCallFlags = gio::DBusCallFlags::ALLOW_INTERACTIVE_AUTHORIZATION;

type SettingsMap = HashMap<String, HashMap<String, glib::Variant>>;
type SavedWifiMap = HashMap<String, Arc<[String]>>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct WifiSnapshot {
    pub available: bool,
    pub enabled: bool,
    pub last_scan: i64,
    pub networks: Vec<WifiNetwork>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct VlessState {
    pub available: bool,
    pub active: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum WifiSecurity {
    #[default]
    Open,
    Wep,
    Personal,
    Sae,
    Owe,
    Enterprise,
}

impl WifiSecurity {
    pub fn secured(self) -> bool {
        self != Self::Open
    }

    pub fn requires_password(self) -> bool {
        matches!(self, Self::Wep | Self::Personal | Self::Sae)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Enterprise => "enterprise",
            _ => "secured",
        }
    }

    pub fn supports_new_profile(self) -> bool {
        self != Self::Enterprise
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct WifiNetwork {
    pub ssid: String,
    pub signal: u8,
    pub security: WifiSecurity,
    pub saved_paths: Arc<[String]>,
    pub active: bool,
    pub ap_path: String,
    pub device_path: String,
}

impl WifiNetwork {
    pub fn saved(&self) -> bool {
        !self.saved_paths.is_empty()
    }
}

#[derive(Debug)]
struct AccessPointCandidate {
    signal: u8,
    security: WifiSecurity,
    active: bool,
    ap_path: String,
}

#[derive(Debug, Default)]
struct SavedWifiCache {
    version: Option<u64>,
    profiles: Arc<SavedWifiMap>,
}

#[derive(Clone, Debug)]
pub(super) struct NetworkBackend {
    saved_wifi_cache: Arc<Mutex<SavedWifiCache>>,
}

impl Default for NetworkBackend {
    fn default() -> Self {
        Self {
            saved_wifi_cache: Arc::clone(
                SAVED_WIFI_CACHE.get_or_init(|| Arc::new(Mutex::new(SavedWifiCache::default()))),
            ),
        }
    }
}

impl NetworkBackend {
    pub fn snapshot(&self) -> Result<WifiSnapshot, String> {
        if let Some(snapshot) = cached_network_snapshot() {
            return Ok(snapshot);
        }
        let _snapshot_guard = NETWORK_SNAPSHOT_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(snapshot) = cached_network_snapshot() {
            return Ok(snapshot);
        }

        let snapshot = self.read_snapshot()?;
        *network_snapshot_cache()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some((Instant::now(), snapshot.clone()));
        Ok(snapshot)
    }

    fn read_snapshot(&self) -> Result<WifiSnapshot, String> {
        let manager = manager_proxy()?;
        let enabled = property::<bool>(&manager, "WirelessEnabled").unwrap_or(false);

        let Some(device_path) = find_wifi_device(&manager)? else {
            return Ok(WifiSnapshot {
                available: false,
                enabled,
                last_scan: -1,
                networks: Vec::new(),
            });
        };

        if !enabled {
            return Ok(WifiSnapshot {
                available: true,
                enabled: false,
                last_scan: -1,
                networks: Vec::new(),
            });
        }

        let wireless = proxy(&device_path, NM_WIRELESS_INTERFACE)?;
        let active_path = property::<ObjectPath>(&wireless, "ActiveAccessPoint")
            .map(|path| path.as_str().to_owned())
            .unwrap_or_else(|| ROOT_OBJECT_PATH.to_owned());
        let last_scan = property::<i64>(&wireless, "LastScan").unwrap_or(-1);

        let paths = access_point_paths(&wireless)?;

        let mut by_ssid = HashMap::<String, AccessPointCandidate>::with_capacity(paths.len());

        for path in paths {
            let ap_path = path.as_str().to_owned();
            let access_point = match proxy(&ap_path, NM_ACCESS_POINT_INTERFACE) {
                Ok(proxy) => proxy,
                Err(_) => continue,
            };

            let Some(ssid_bytes) = property::<Vec<u8>>(&access_point, "Ssid") else {
                continue;
            };
            let Some(ssid) = ssid_from_bytes(&ssid_bytes) else {
                continue;
            };

            let signal = property::<u8>(&access_point, "Strength").unwrap_or(0);
            let flags = property::<u32>(&access_point, "Flags").unwrap_or(0);
            let wpa_flags = property::<u32>(&access_point, "WpaFlags").unwrap_or(0);
            let rsn_flags = property::<u32>(&access_point, "RsnFlags").unwrap_or(0);
            let security = wifi_security(flags, wpa_flags, rsn_flags);
            let candidate = AccessPointCandidate {
                signal,
                security,
                active: active_path != ROOT_OBJECT_PATH && active_path == ap_path,
                ap_path,
            };

            match by_ssid.get_mut(&ssid) {
                Some(existing) => merge_access_point(existing, candidate),
                None => {
                    by_ssid.insert(ssid, candidate);
                }
            }
        }

        let saved = if by_ssid.is_empty() {
            None
        } else {
            Some(self.saved_wifi_connections()?)
        };
        let mut networks = by_ssid
            .into_iter()
            .map(|(ssid, access_point)| WifiNetwork {
                saved_paths: saved
                    .as_ref()
                    .and_then(|saved| saved.get(&ssid))
                    .cloned()
                    .unwrap_or_default(),
                ssid,
                signal: access_point.signal,
                security: access_point.security,
                active: access_point.active,
                ap_path: access_point.ap_path,
                device_path: device_path.clone(),
            })
            .collect::<Vec<_>>();
        networks.sort_by_cached_key(|network| {
            (
                !network.active,
                Reverse(network.signal),
                network.ssid.to_lowercase(),
            )
        });

        Ok(WifiSnapshot {
            available: true,
            enabled,
            last_scan,
            networks,
        })
    }

    pub fn vless_state(&self) -> Result<VlessState, String> {
        vless_state()
    }

    pub fn set_enabled(&self, enabled: bool) -> Result<(), String> {
        let _io_guard = network_write_guard()?;
        let properties = proxy(NM_PATH, DBUS_PROPERTIES_INTERFACE)?;
        let value = enabled.to_variant();
        let parameters = (NM_INTERFACE, "WirelessEnabled", value).to_variant();

        properties
            .call_sync(
                "Set",
                Some(&parameters),
                WRITE_CALL_FLAGS,
                DBUS_TIMEOUT_MS,
                None::<&gio::Cancellable>,
            )
            .map_err(|error| format!("failed to set Wi-Fi radio state: {error}"))?;
        invalidate_network_snapshot_cache();
        Ok(())
    }

    pub fn request_scan(&self) -> Result<i64, String> {
        let _io_guard = network_write_guard()?;
        let manager = manager_proxy()?;
        let device_path = find_wifi_device(&manager)?
            .ok_or_else(|| "no Wi-Fi device is managed by NetworkManager".to_owned())?;
        let wireless = proxy(&device_path, NM_WIRELESS_INTERFACE)?;
        let previous_last_scan = property::<i64>(&wireless, "LastScan").unwrap_or(-1);
        let options = HashMap::<String, glib::Variant>::new();
        let parameters = (options,).to_variant();

        wireless
            .call_sync(
                "RequestScan",
                Some(&parameters),
                WRITE_CALL_FLAGS,
                DBUS_TIMEOUT_MS,
                None::<&gio::Cancellable>,
            )
            .map_err(|error| format!("failed to request Wi-Fi scan: {error}"))?;
        invalidate_network_snapshot_cache();
        Ok(previous_last_scan)
    }

    pub fn connect(&self, network: &WifiNetwork, password: Option<&str>) -> Result<(), String> {
        let _io_guard = network_write_guard()?;
        let manager = manager_proxy()?;
        let device = object_path(&network.device_path)?;
        let access_point = object_path(&network.ap_path)?;

        if network.saved() {
            let connection = object_path(ROOT_OBJECT_PATH)?;
            let parameters = (connection, device, access_point).to_variant();
            manager
                .call_sync(
                    "ActivateConnection",
                    Some(&parameters),
                    WRITE_CALL_FLAGS,
                    DBUS_TIMEOUT_MS,
                    None::<&gio::Cancellable>,
                )
                .map_err(|error| format!("failed to activate {}: {error}", network.ssid))?;
            invalidate_network_snapshot_cache();
            return Ok(());
        }

        let mut settings = SettingsMap::new();

        let mut connection = HashMap::new();
        connection.insert("id".to_owned(), network.ssid.to_variant());
        connection.insert("type".to_owned(), "802-11-wireless".to_variant());
        settings.insert("connection".to_owned(), connection);

        let mut wireless = HashMap::new();
        wireless.insert(
            "ssid".to_owned(),
            network.ssid.as_bytes().to_vec().to_variant(),
        );
        if network.security.secured() {
            wireless.insert(
                "security".to_owned(),
                "802-11-wireless-security".to_variant(),
            );
        }
        settings.insert("802-11-wireless".to_owned(), wireless);

        let password = || required_password(password);
        let mut security = HashMap::new();
        match network.security {
            WifiSecurity::Open => {}
            WifiSecurity::Personal => {
                security.insert("key-mgmt".to_owned(), "wpa-psk".to_variant());
                security.insert("psk".to_owned(), password()?.to_variant());
            }
            WifiSecurity::Sae => {
                security.insert("key-mgmt".to_owned(), "sae".to_variant());
                security.insert("psk".to_owned(), password()?.to_variant());
            }
            WifiSecurity::Owe => {
                security.insert("key-mgmt".to_owned(), "owe".to_variant());
            }
            WifiSecurity::Wep => {
                let password = password()?;
                security.insert("key-mgmt".to_owned(), "none".to_variant());
                security.insert("wep-key0".to_owned(), password.to_variant());
                security.insert(
                    "wep-key-type".to_owned(),
                    wep_key_type(password).to_variant(),
                );
            }
            WifiSecurity::Enterprise => {
                return Err(
                    "enterprise Wi-Fi needs a saved NetworkManager profile with 802.1X credentials"
                        .to_owned(),
                );
            }
        }
        if !security.is_empty() {
            settings.insert("802-11-wireless-security".to_owned(), security);
        }

        let parameters = (settings, device, access_point).to_variant();
        manager
            .call_sync(
                "AddAndActivateConnection",
                Some(&parameters),
                WRITE_CALL_FLAGS,
                DBUS_TIMEOUT_MS,
                None::<&gio::Cancellable>,
            )
            .map_err(|error| format!("failed to connect to {}: {error}", network.ssid))?;
        self.invalidate_saved_wifi_cache();
        invalidate_network_snapshot_cache();
        Ok(())
    }

    pub fn set_vless_active(&self, active: bool) -> Result<(), String> {
        let _io_guard = network_write_guard()?;
        let state = vless_state()?;
        if !state.available {
            return Err(format!("{VLESS_UNIT} is not installed"));
        }

        let manager = proxy_for(SYSTEMD_SERVICE, SYSTEMD_PATH, SYSTEMD_MANAGER_INTERFACE)?;
        let method = if active { "StartUnit" } else { "StopUnit" };
        let parameters = (VLESS_UNIT, "replace").to_variant();
        manager
            .call_sync(
                method,
                Some(&parameters),
                WRITE_CALL_FLAGS,
                DBUS_TIMEOUT_MS,
                None::<&gio::Cancellable>,
            )
            .map_err(|error| {
                let action = if active { "start" } else { "stop" };
                format!("failed to {action} {VLESS_UNIT}: {error}")
            })?;
        Ok(())
    }

    pub fn forget(&self, network: &WifiNetwork) -> Result<(), String> {
        let _io_guard = network_write_guard()?;
        let mut failures = Vec::with_capacity(network.saved_paths.len());

        for path in network.saved_paths.iter() {
            let result = proxy(path, NM_SETTINGS_CONNECTION_INTERFACE).and_then(|connection| {
                connection
                    .call_sync(
                        "Delete",
                        None,
                        WRITE_CALL_FLAGS,
                        DBUS_TIMEOUT_MS,
                        None::<&gio::Cancellable>,
                    )
                    .map(|_| ())
                    .map_err(|error| format!("{path}: {error}"))
            });

            if let Err(error) = result {
                failures.push(error);
            }
        }

        if !network.saved_paths.is_empty() {
            self.invalidate_saved_wifi_cache();
            invalidate_network_snapshot_cache();
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "failed to forget {}: {}",
                network.ssid,
                failures.join("; ")
            ))
        }
    }
}

fn network_write_guard() -> Result<std::sync::MutexGuard<'static, ()>, String> {
    match NETWORK_WRITE_LOCK.try_lock() {
        Ok(guard) => Ok(guard),
        Err(std::sync::TryLockError::Poisoned(poisoned)) => Ok(poisoned.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => {
            Err("another network action is already in progress".to_owned())
        }
    }
}

fn network_snapshot_cache() -> &'static Mutex<Option<(Instant, WifiSnapshot)>> {
    NETWORK_SNAPSHOT_CACHE.get_or_init(|| Mutex::new(None))
}

fn cached_network_snapshot() -> Option<WifiSnapshot> {
    network_snapshot_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .filter(|(loaded_at, _)| loaded_at.elapsed() < NETWORK_CACHE_TTL)
        .map(|(_, snapshot)| snapshot.clone())
}

fn invalidate_network_snapshot_cache() {
    *network_snapshot_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

fn manager_proxy() -> Result<gio::DBusProxy, String> {
    proxy(NM_PATH, NM_INTERFACE)
}

fn proxy(path: &str, interface: &str) -> Result<gio::DBusProxy, String> {
    proxy_for(NM_SERVICE, path, interface)
}

fn proxy_for(service: &str, path: &str, interface: &str) -> Result<gio::DBusProxy, String> {
    gio::DBusProxy::for_bus_sync(
        gio::BusType::System,
        gio::DBusProxyFlags::DO_NOT_CONNECT_SIGNALS,
        None,
        service,
        path,
        interface,
        None::<&gio::Cancellable>,
    )
    .map_err(|error| format!("D-Bus proxy {service} {interface} at {path} failed: {error}"))
}

fn vless_state() -> Result<VlessState, String> {
    let manager = proxy_for(SYSTEMD_SERVICE, SYSTEMD_PATH, SYSTEMD_MANAGER_INTERFACE)?;
    let parameters = (VLESS_UNIT,).to_variant();
    let reply = match manager.call_sync(
        "LoadUnit",
        Some(&parameters),
        gio::DBusCallFlags::NONE,
        DBUS_TIMEOUT_MS,
        None::<&gio::Cancellable>,
    ) {
        Ok(reply) => reply,
        Err(_) => return Ok(VlessState::default()),
    };
    let (path,) = reply
        .get::<(ObjectPath,)>()
        .ok_or_else(|| "systemd returned an invalid unit object path".to_owned())?;
    let unit = proxy_for(SYSTEMD_SERVICE, path.as_str(), SYSTEMD_UNIT_INTERFACE)?;
    let load_state = property::<String>(&unit, "LoadState").unwrap_or_default();
    let active_state = property::<String>(&unit, "ActiveState").unwrap_or_default();
    let available = !load_state.is_empty()
        && load_state != "not-found"
        && load_state != "error"
        && load_state != "masked";

    Ok(VlessState {
        available,
        active: available && active_state == "active",
    })
}

fn property<T: glib::variant::FromVariant>(proxy: &gio::DBusProxy, name: &str) -> Option<T> {
    proxy
        .cached_property(name)
        .and_then(|value| variant_value(&value))
}

fn find_wifi_device(manager: &gio::DBusProxy) -> Result<Option<String>, String> {
    let paths = manager_device_paths(manager)?;
    let mut fallback = None;

    for path in paths {
        let path = path.as_str().to_owned();
        let device = match proxy(&path, NM_DEVICE_INTERFACE) {
            Ok(proxy) => proxy,
            Err(_) => continue,
        };
        if property::<u32>(&device, "DeviceType") != Some(NM_DEVICE_TYPE_WIFI) {
            continue;
        }

        if fallback.is_none() {
            fallback = Some(path.clone());
        }

        let active = property::<ObjectPath>(&device, "ActiveConnection")
            .is_some_and(|path| path.as_str() != ROOT_OBJECT_PATH);
        if active {
            return Ok(Some(path));
        }
    }

    Ok(fallback)
}

fn object_paths(
    proxy: &gio::DBusProxy,
    property_name: &str,
    method_name: &str,
    operation: &str,
    invalid_reply: &str,
) -> Result<Vec<ObjectPath>, String> {
    if let Some(paths) = property::<Vec<ObjectPath>>(proxy, property_name) {
        return Ok(paths);
    }

    let reply = proxy
        .call_sync(
            method_name,
            None,
            gio::DBusCallFlags::NONE,
            DBUS_TIMEOUT_MS,
            None::<&gio::Cancellable>,
        )
        .map_err(|error| format!("{operation}: {error}"))?;
    reply
        .get::<(Vec<ObjectPath>,)>()
        .map(|(paths,)| paths)
        .ok_or_else(|| invalid_reply.to_owned())
}

fn manager_device_paths(manager: &gio::DBusProxy) -> Result<Vec<ObjectPath>, String> {
    object_paths(
        manager,
        "Devices",
        "GetDevices",
        "failed to enumerate NetworkManager devices",
        "NetworkManager returned an invalid device list",
    )
}

fn access_point_paths(wireless: &gio::DBusProxy) -> Result<Vec<ObjectPath>, String> {
    object_paths(
        wireless,
        "AccessPoints",
        "GetAllAccessPoints",
        "failed to list Wi-Fi access points",
        "NetworkManager returned an invalid access-point list",
    )
}

impl NetworkBackend {
    fn invalidate_saved_wifi_cache(&self) {
        let mut cache = self
            .saved_wifi_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cache.version = None;
        cache.profiles = Arc::default();
    }

    fn saved_wifi_connections(&self) -> Result<Arc<SavedWifiMap>, String> {
        let settings = proxy(NM_SETTINGS_PATH, NM_SETTINGS_INTERFACE)?;
        let version = property::<u64>(&settings, "VersionId");

        if let Some(version) = version {
            let cache = self
                .saved_wifi_cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if cache.version == Some(version) {
                return Ok(Arc::clone(&cache.profiles));
            }
        }

        let paths = saved_connection_paths(&settings)?;
        let mut saved = HashMap::<String, Vec<String>>::with_capacity(paths.len());

        for path in paths {
            let path_string = path.as_str().to_owned();
            let connection = match proxy(&path_string, NM_SETTINGS_CONNECTION_INTERFACE) {
                Ok(proxy) => proxy,
                Err(_) => continue,
            };
            let reply = match connection.call_sync(
                "GetSettings",
                None,
                gio::DBusCallFlags::NONE,
                DBUS_TIMEOUT_MS,
                None::<&gio::Cancellable>,
            ) {
                Ok(reply) => reply,
                Err(_) => continue,
            };
            let Some((settings,)) = reply.get::<(SettingsMap,)>() else {
                continue;
            };

            let connection_type = settings
                .get("connection")
                .and_then(|section| section.get("type"))
                .and_then(variant_value::<String>);
            if connection_type.as_deref() != Some("802-11-wireless") {
                continue;
            }

            let Some(ssid) = settings
                .get("802-11-wireless")
                .and_then(|section| section.get("ssid"))
                .and_then(variant_value::<Vec<u8>>)
                .and_then(|bytes| ssid_from_bytes(&bytes))
            else {
                continue;
            };

            saved.entry(ssid).or_default().push(path_string);
        }

        let saved = Arc::new(
            saved
                .into_iter()
                .map(|(ssid, paths)| (ssid, Arc::<[String]>::from(paths)))
                .collect::<SavedWifiMap>(),
        );
        if let Some(version) = version {
            let mut cache = self
                .saved_wifi_cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            cache.version = Some(version);
            cache.profiles = Arc::clone(&saved);
        }
        Ok(saved)
    }
}

fn saved_connection_paths(settings: &gio::DBusProxy) -> Result<Vec<ObjectPath>, String> {
    object_paths(
        settings,
        "Connections",
        "ListConnections",
        "failed to list saved NetworkManager profiles",
        "NetworkManager returned an invalid saved-profile list",
    )
}

fn ssid_from_bytes(bytes: &[u8]) -> Option<String> {
    let ssid = String::from_utf8_lossy(bytes).into_owned();
    (!ssid.is_empty()).then_some(ssid)
}

fn required_password(password: Option<&str>) -> Result<&str, String> {
    password
        .filter(|password| !password.is_empty())
        .ok_or_else(|| "a password is required for this network".to_owned())
}

fn wifi_security(flags: u32, wpa_flags: u32, rsn_flags: u32) -> WifiSecurity {
    let security = wpa_flags | rsn_flags;
    if security & NM_AP_SEC_KEY_MGMT_PSK != 0 {
        WifiSecurity::Personal
    } else if security & NM_AP_SEC_KEY_MGMT_SAE != 0 {
        WifiSecurity::Sae
    } else if security & (NM_AP_SEC_KEY_MGMT_OWE | NM_AP_SEC_KEY_MGMT_OWE_TM) != 0 {
        WifiSecurity::Owe
    } else if security & (NM_AP_SEC_KEY_MGMT_802_1X | NM_AP_SEC_KEY_MGMT_EAP_SUITE_B_192) != 0 {
        WifiSecurity::Enterprise
    } else if flags & NM_AP_FLAGS_PRIVACY != 0 {
        WifiSecurity::Wep
    } else {
        WifiSecurity::Open
    }
}

fn wep_key_type(secret: &str) -> u32 {
    let bytes = secret.as_bytes();
    let exact_ascii_key = matches!(bytes.len(), 5 | 13) && bytes.iter().all(u8::is_ascii);
    let exact_hex_key = matches!(bytes.len(), 10 | 26) && bytes.iter().all(u8::is_ascii_hexdigit);
    if exact_ascii_key || exact_hex_key {
        1
    } else {
        2
    }
}

fn merge_access_point(existing: &mut AccessPointCandidate, candidate: AccessPointCandidate) {
    let candidate_is_better = candidate.active && !existing.active
        || candidate.active == existing.active && candidate.signal > existing.signal;

    if candidate_is_better {
        *existing = candidate;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AccessPointCandidate, WifiSecurity, merge_access_point, required_password, ssid_from_bytes,
        wep_key_type, wifi_security,
    };

    fn access_point(signal: u8, active: bool) -> AccessPointCandidate {
        AccessPointCandidate {
            signal,
            security: WifiSecurity::Personal,
            active,
            ap_path: format!("/ap/{signal}"),
        }
    }

    #[test]
    fn active_access_point_wins_over_stronger_duplicate() {
        let mut existing = access_point(90, false);
        merge_access_point(&mut existing, access_point(45, true));
        assert!(existing.active);
        assert_eq!(existing.signal, 45);
    }

    #[test]
    fn stronger_access_point_wins_when_activity_matches() {
        let mut existing = access_point(30, false);
        merge_access_point(&mut existing, access_point(75, false));
        assert_eq!(existing.signal, 75);
    }

    #[test]
    fn security_flags_prefer_personal_transition_mode() {
        assert_eq!(wifi_security(1, 0, 0x100 | 0x400), WifiSecurity::Personal);
        assert_eq!(wifi_security(1, 0, 0x400), WifiSecurity::Sae);
        assert_eq!(wifi_security(1, 0, 0x800), WifiSecurity::Owe);
        assert_eq!(wifi_security(1, 0, 0x200), WifiSecurity::Enterprise);
        assert_eq!(wifi_security(1, 0, 0), WifiSecurity::Wep);
        assert_eq!(wifi_security(0, 0, 0), WifiSecurity::Open);
    }

    #[test]
    fn ssid_decoding_preserves_significant_spaces() {
        assert_eq!(
            ssid_from_bytes(b"  spaced network  ").as_deref(),
            Some("  spaced network  ")
        );
        assert_eq!(ssid_from_bytes(b""), None);
    }

    #[test]
    fn password_validation_preserves_significant_spaces() {
        assert_eq!(required_password(Some("  secret  ")).unwrap(), "  secret  ");
        assert!(required_password(Some("")).is_err());
        assert!(required_password(None).is_err());
    }

    #[test]
    fn wep_key_shape_selects_raw_key_or_passphrase() {
        assert_eq!(wep_key_type("abcde"), 1);
        assert_eq!(wep_key_type("0011223344"), 1);
        assert_eq!(wep_key_type("ordinary passphrase"), 2);
    }
}
