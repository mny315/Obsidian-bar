//! One-shot NetworkManager reads without a proxy per access point or profile.
use super::{DBUS_PROPERTIES_INTERFACE, DBUS_TIMEOUT_MS};
use gio::{glib, prelude::*};
use glib::variant::ToVariant;
use std::collections::HashMap;

pub(super) struct NetworkReader {
    connection: gio::DBusConnection,
    owner: glib::GString,
}

impl NetworkReader {
    pub(super) fn from_proxy(proxy: &gio::DBusProxy) -> Result<Self, String> {
        Ok(Self {
            connection: proxy.connection(),
            owner: proxy
                .name_owner()
                .ok_or_else(|| "NetworkManager is unavailable".to_owned())?,
        })
    }

    pub(super) fn call(
        &self,
        path: &str,
        interface: &str,
        method: &str,
        parameters: Option<&glib::Variant>,
    ) -> Result<glib::Variant, String> {
        self.connection
            .call_sync(
                // Keep these object reads on their originating proxy's owner
                // if NetworkManager restarts in the middle of a scan.
                Some(&self.owner),
                path,
                interface,
                method,
                parameters,
                None,
                gio::DBusCallFlags::NONE,
                DBUS_TIMEOUT_MS,
                gio::Cancellable::NONE,
            )
            .map_err(|error| format!("D-Bus {interface}.{method} at {path} failed: {error}"))
    }

    pub(super) fn properties(
        &self,
        path: &str,
        interface: &str,
    ) -> Result<HashMap<String, glib::Variant>, String> {
        self.call(
            path,
            DBUS_PROPERTIES_INTERFACE,
            "GetAll",
            Some(&(interface,).to_variant()),
        )?
        .get::<(HashMap<String, glib::Variant>,)>()
        .map(|(properties,)| properties)
        .ok_or_else(|| format!("invalid {interface} properties at {path}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widgets::{dbus::variant_value, network_backend::SettingsMap};
    use std::{
        sync::mpsc,
        time::{Duration, Instant},
    };

    #[test]
    #[ignore = "requires session D-Bus; only reads an isolated fixture service"]
    fn reads_access_point_properties_and_profile_settings_without_object_proxies() {
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).unwrap();
                let info = gio::DBusNodeInfo::for_xml(
                    "<node><interface name='dev.obsidian.AccessPointFixture'>
                <property name='Ssid' type='ay' access='read'/>
                <property name='Strength' type='y' access='read'/>
                <method name='GetSettings'><arg type='a{sa{sv}}' direction='out'/></method>
                </interface></node>",
                )
                .unwrap();
                let registration = bus
                    .register_object("/ObsidianNetworkTest", &info.interfaces()[0])
                    .property(|_, _, _, _, property| match property {
                        "Ssid" => b" \xff fixture ".to_vec().to_variant(),
                        "Strength" => 73_u8.to_variant(),
                        _ => unreachable!("unexpected fixture property"),
                    })
                    .method_call(|_, _, _, _, method, _, invocation| {
                        assert_eq!(method, "GetSettings");
                        let settings = SettingsMap::from([(
                            "802-11-wireless".into(),
                            HashMap::from([(
                                "ssid".into(),
                                b" \xff fixture ".to_vec().to_variant(),
                            )]),
                        )]);
                        invocation.return_value(Some(&(settings,).to_variant()));
                    })
                    .build()
                    .unwrap();
                let reader = NetworkReader {
                    connection: bus.clone(),
                    owner: bus.unique_name().unwrap(),
                };
                let (finished, result) = mpsc::channel();
                let worker = std::thread::spawn(move || {
                    let properties = reader
                        .properties("/ObsidianNetworkTest", "dev.obsidian.AccessPointFixture")
                        .unwrap();
                    assert_eq!(
                        variant_value::<Vec<u8>>(&properties["Ssid"]).unwrap(),
                        b" \xff fixture "
                    );
                    assert_eq!(variant_value::<u8>(&properties["Strength"]), Some(73));
                    let profile = reader
                        .call(
                            "/ObsidianNetworkTest",
                            "dev.obsidian.AccessPointFixture",
                            "GetSettings",
                            None,
                        )
                        .unwrap();
                    assert_eq!(
                        profile.get::<(SettingsMap,)>().unwrap().0["802-11-wireless"]["ssid"],
                        properties["Ssid"]
                    );
                    assert!(
                        reader
                            .properties("/MissingNetworkFixture", "dev.obsidian.AccessPointFixture")
                            .is_err()
                    );
                    finished.send(()).unwrap();
                });
                let deadline = Instant::now() + Duration::from_secs(8);
                loop {
                    context.iteration(false);
                    match result.try_recv() {
                        Ok(()) => break,
                        Err(mpsc::TryRecvError::Disconnected) => {
                            worker.join().unwrap();
                            panic!("fixture worker stopped");
                        }
                        Err(mpsc::TryRecvError::Empty) => {}
                    }
                    assert!(Instant::now() < deadline, "fixture D-Bus read stalled");
                    std::thread::sleep(Duration::from_millis(1));
                }
                worker.join().unwrap();
                bus.unregister_object(registration).unwrap();
            })
            .unwrap();
    }
}
