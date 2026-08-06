use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    rc::Rc,
    time::Duration,
};

use gtk::{gdk, glib, prelude::*};
use tracing::{debug, warn};

use super::tooltip::BarTooltipExt;
use super::{
    BAR_POPUP_WIDTH, Generation, PopupReveal, RefreshGate, attach_popup_escape_handler,
    attach_popup_lifecycle, build_bar_popup, build_quick_toggle_button, build_refresh_button,
    clear_box, detach_application_window, empty_state_label,
    network_backend::{NetworkBackend, VlessState, WifiNetwork, WifiSnapshot},
    reset_hidden_popup_state, run_background, run_when_popup_visible, set_optional_label,
    set_spinner_active,
};

const SCAN_COMPLETION_TIMEOUT: Duration = Duration::from_secs(10);
const SIGNAL_SUBSCRIPTION_RETRY_BASE_DELAY: Duration = Duration::from_secs(1);
const SIGNAL_SUBSCRIPTION_RETRY_MAX_DELAY: Duration = Duration::from_secs(30);
const VLESS_ACTION_REFRESH_DELAY: Duration = Duration::from_millis(650);
const NETWORK_LIST_MIN_HEIGHT: i32 = 96;
const NETWORK_LIST_MAX_HEIGHT: i32 = 220;
const NETWORK_POPUP_NAMESPACE: &str = "obsidian-bar-network";
const NETWORK_MANAGER_BUS_NAME: &str = "org.freedesktop.NetworkManager";
const NM_INTERFACE: &str = "org.freedesktop.NetworkManager";
const NM_DEVICE_INTERFACE: &str = "org.freedesktop.NetworkManager.Device";
const NM_WIRELESS_INTERFACE: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const NM_ACCESS_POINT_INTERFACE: &str = "org.freedesktop.NetworkManager.AccessPoint";
const NM_SETTINGS_INTERFACE: &str = "org.freedesktop.NetworkManager.Settings";
const NM_SETTINGS_CONNECTION_INTERFACE: &str = "org.freedesktop.NetworkManager.Settings.Connection";
const DBUS_PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";
const DBUS_BUS_NAME: &str = "org.freedesktop.DBus";
const DBUS_BUS_PATH: &str = "/org/freedesktop/DBus";
const DBUS_BUS_INTERFACE: &str = "org.freedesktop.DBus";

const ICON_WIFI_HIGH: &str = "󰤨";
const ICON_WIFI_GOOD: &str = "󰤥";
const ICON_WIFI_MID: &str = "󰤢";
const ICON_WIFI_LOW: &str = "󰤟";
const ICON_WIFI_NONE: &str = "󰤮";
const ICON_WIFI_OFF: &str = "󰖪";
const ICON_REFRESH: &str = "󰑐";
const ICON_CHECK: &str = "󰄬";
const ICON_LOCK: &str = "󰌾";
const ICON_FORGET: &str = "󰅖";
const ICON_VLESS_ACTIVE: &str = "󰌾";
const ICON_VLESS_INACTIVE: &str = "󰌿";

#[derive(Default)]
struct NetworkState {
    wifi: WifiSnapshot,
    vless: VlessState,
    password_target: Option<WifiNetwork>,
}

struct NetworkRow {
    row: gtk::Box,
    main: gtk::Button,
    model: Rc<RefCell<WifiNetwork>>,
    icon: gtk::Label,
    meta: gtk::Label,
    status: gtk::Label,
    forget_button: gtk::Button,
}

impl NetworkRow {
    fn set_action_enabled(&self, enabled: bool) {
        self.main.set_sensitive(enabled);
        self.forget_button.set_sensitive(enabled);
    }

    fn update(&self, network: &WifiNetwork) {
        let current = self.model.borrow();
        if *current == *network {
            return;
        }

        if wifi_signal_icon(current.signal) != wifi_signal_icon(network.signal) {
            self.icon.set_text(wifi_signal_icon(network.signal));
        }

        let current_saved = current.saved();
        let next_saved = network.saved();
        if current.signal != network.signal
            || current.security != network.security
            || current_saved != next_saved
        {
            self.meta.set_text(&network_meta(network));
        }

        if current.active != network.active
            || current.security != network.security
            || current_saved != next_saved
        {
            self.status.set_text(network_status_icon(network));
        }
        if current_saved != next_saved {
            self.forget_button.set_visible(next_saved);
        }

        drop(current);
        self.model.replace(network.clone());
    }
}

#[derive(Clone, Copy)]
enum RefreshTarget {
    Wifi,
    Vless,
}

struct NetworkController {
    trigger: gtk::Button,
    trigger_label: gtk::Label,

    popup: gtk::ApplicationWindow,
    popup_root: gtk::Box,
    popup_title: gtk::Label,
    popup_status: gtk::Label,
    wifi_switch: gtk::Switch,
    header_actions: gtk::Box,
    vless_button: gtk::Button,
    vless_icon: gtk::Label,
    rescan_button: gtk::Button,
    rescan_icon: gtk::Label,
    rescan_spinner: gtk::Spinner,
    list: gtk::Box,
    network_rows: RefCell<HashMap<String, NetworkRow>>,
    header_initialized: Cell<bool>,
    list_initialized: Cell<bool>,
    list_dirty: Cell<bool>,
    password_box: gtk::Box,
    password_title: gtk::Label,
    password_entry: gtk::Entry,
    password_connect_button: gtk::Button,
    notice: gtk::Label,

    backend: NetworkBackend,
    state: RefCell<NetworkState>,
    wifi_read: RefreshGate,
    wifi_revision: Generation,
    vless_read: RefreshGate,
    vless_revision: Generation,
    action_busy: Cell<bool>,
    syncing_switch: Cell<bool>,
    popup_reveal: PopupReveal,
    focus_armed: Rc<Cell<bool>>,
    signal_refresh_pending: Cell<bool>,
    signal_subscription_pending: Cell<bool>,
    signal_subscription_retry_pending: Cell<bool>,
    signal_subscription_retry_attempt: Cell<u32>,
    signal_subscriptions: RefCell<Vec<gio::SignalSubscription>>,
    rescan_busy: Cell<bool>,
    rescan_baseline: Cell<Option<i64>>,
    rescan_generation: Generation,
}

pub struct NetworkIndicator {
    root: gtk::Box,
    _controller: Rc<NetworkController>,
}

impl Drop for NetworkIndicator {
    fn drop(&mut self) {
        detach_application_window(&self._controller.popup);
    }
}

impl NetworkIndicator {
    pub fn new(
        application: &gtk::Application,
        bar_window: &gtk::ApplicationWindow,
        monitor: &gdk::Monitor,
    ) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        root.add_css_class("quick-control");
        root.set_valign(gtk::Align::Center);

        let (trigger, trigger_label) =
            build_quick_toggle_button(ICON_WIFI_OFF, "network-trigger", &["network-trigger-icon"]);
        trigger.set_bar_tooltip_text(Some("Wi-Fi"));
        root.append(&trigger);

        let popup = build_bar_popup(
            application,
            monitor,
            NETWORK_POPUP_NAMESPACE,
            "network-popup-window",
        );

        let popup_root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        popup_root.add_css_class("widget-popup-root");
        popup_root.set_focusable(true);

        let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
        frame.add_css_class("widget-popup-frame");
        frame.add_css_class("network-popover-window");
        frame.set_overflow(gtk::Overflow::Hidden);
        frame.set_size_request(BAR_POPUP_WIDTH, -1);

        let popup_content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        popup_content.add_css_class("network-popover");

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        header.add_css_class("network-header");
        header.set_valign(gtk::Align::Center);

        let header_text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        header_text.set_hexpand(true);
        header_text.set_valign(gtk::Align::Center);

        let popup_title = gtk::Label::new(Some("Wi-Fi"));
        popup_title.add_css_class("network-header-title");
        popup_title.set_xalign(0.0);
        popup_title.set_ellipsize(gtk::pango::EllipsizeMode::End);

        let popup_status = gtk::Label::new(Some("Loading…"));
        popup_status.add_css_class("network-header-meta");
        popup_status.set_xalign(0.0);
        popup_status.set_ellipsize(gtk::pango::EllipsizeMode::End);

        header_text.append(&popup_title);
        header_text.append(&popup_status);

        let vless_icon = gtk::Label::new(Some(ICON_VLESS_INACTIVE));
        vless_icon.add_css_class("network-action-icon");
        let vless_button = gtk::Button::new();
        vless_button.add_css_class("network-icon-button");
        vless_button.add_css_class("network-vless-button");
        vless_button.set_valign(gtk::Align::Center);
        vless_button.set_child(Some(&vless_icon));
        vless_button.set_visible(false);

        let (rescan_button, rescan_icon, rescan_spinner) = build_refresh_button(ICON_REFRESH);

        let header_actions = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        header_actions.add_css_class("network-header-action-capsule");
        header_actions.set_valign(gtk::Align::Center);
        header_actions.set_visible(false);
        header_actions.append(&vless_button);
        header_actions.append(&rescan_button);

        let wifi_switch = gtk::Switch::new();
        wifi_switch.add_css_class("network-wifi-switch");
        wifi_switch.set_valign(gtk::Align::Center);

        header.append(&header_text);
        header.append(&header_actions);
        header.append(&wifi_switch);

        let password_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
        password_box.add_css_class("network-password-box");
        password_box.set_visible(false);

        let password_title = gtk::Label::new(Some("Connect"));
        password_title.add_css_class("network-password-title");
        password_title.set_xalign(0.0);
        password_title.set_ellipsize(gtk::pango::EllipsizeMode::End);

        let password_entry = gtk::Entry::new();
        password_entry.add_css_class("network-password-entry");
        password_entry.set_visibility(false);
        password_entry.set_input_purpose(gtk::InputPurpose::Password);
        password_entry.set_placeholder_text(Some("Password"));
        password_entry.set_hexpand(true);

        let password_actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        password_actions.set_halign(gtk::Align::End);

        let cancel_button = gtk::Button::with_label("Cancel");
        cancel_button.add_css_class("network-password-action");
        let connect_button = gtk::Button::with_label("Connect");
        connect_button.add_css_class("network-password-action");

        password_actions.append(&cancel_button);
        password_actions.append(&connect_button);
        password_box.append(&password_title);
        password_box.append(&password_entry);
        password_box.append(&password_actions);

        let notice = gtk::Label::new(None);
        notice.add_css_class("network-notice");
        notice.set_xalign(0.0);
        notice.set_wrap(true);
        notice.set_visible(false);

        let section_title = gtk::Label::new(Some("Networks"));
        section_title.add_css_class("network-section-title");
        section_title.set_xalign(0.0);

        let list_capsule = gtk::Box::new(gtk::Orientation::Vertical, 0);
        list_capsule.add_css_class("network-list-capsule");

        let scroller = gtk::ScrolledWindow::new();
        scroller.add_css_class("network-list-scroller");
        scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Never);
        scroller.set_kinetic_scrolling(true);
        scroller.set_propagate_natural_height(true);
        scroller.set_min_content_height(NETWORK_LIST_MIN_HEIGHT);
        scroller.set_max_content_height(NETWORK_LIST_MAX_HEIGHT);

        let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
        list.add_css_class("network-list-inner");
        scroller.set_child(Some(&list));
        list_capsule.append(&scroller);

        popup_content.append(&header);
        popup_content.append(&password_box);
        popup_content.append(&section_title);
        popup_content.append(&list_capsule);
        popup_content.append(&notice);

        frame.append(&popup_content);
        let popup_reveal = PopupReveal::masked(frame.clone().upcast::<gtk::Widget>());
        popup_root.append(popup_reveal.widget());
        popup.set_child(Some(&popup_root));

        let controller = Rc::new(NetworkController {
            trigger,
            trigger_label,
            popup,
            popup_root,
            popup_title,
            popup_status,
            wifi_switch,
            header_actions,
            vless_button,
            vless_icon,
            rescan_button,
            rescan_icon,
            rescan_spinner,
            list,
            network_rows: RefCell::new(HashMap::new()),
            header_initialized: Cell::new(false),
            list_initialized: Cell::new(false),
            list_dirty: Cell::new(false),
            password_box,
            password_title,
            password_entry,
            password_connect_button: connect_button.clone(),
            notice,
            backend: NetworkBackend::default(),
            state: RefCell::new(NetworkState::default()),
            wifi_read: RefreshGate::default(),
            wifi_revision: Generation::default(),
            vless_read: RefreshGate::default(),
            vless_revision: Generation::default(),
            action_busy: Cell::new(false),
            syncing_switch: Cell::new(false),
            popup_reveal,
            focus_armed: Rc::new(Cell::new(false)),
            signal_refresh_pending: Cell::new(false),
            signal_subscription_pending: Cell::new(false),
            signal_subscription_retry_pending: Cell::new(false),
            signal_subscription_retry_attempt: Cell::new(0),
            signal_subscriptions: RefCell::new(Vec::new()),
            rescan_busy: Cell::new(false),
            rescan_baseline: Cell::new(None),
            rescan_generation: Generation::default(),
        });

        NetworkController::connect(&controller, bar_window, &cancel_button, &connect_button);
        NetworkController::subscribe_to_network_manager(&controller);
        controller.refresh_snapshot();
        controller.refresh_vless();

        Self {
            root,
            _controller: controller,
        }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    pub fn dismiss(&self) {
        self._controller.close_popup();
    }
}

impl NetworkController {
    fn connect(
        this: &Rc<Self>,
        bar_window: &gtk::ApplicationWindow,
        cancel_button: &gtk::Button,
        connect_button: &gtk::Button,
    ) {
        let weak = Rc::downgrade(this);
        this.trigger.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.toggle_popup();
            }
        });

        let weak = Rc::downgrade(this);
        this.vless_button.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.toggle_vless();
            }
        });

        let weak = Rc::downgrade(this);
        this.rescan_button.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.rescan();
            }
        });

        let weak = Rc::downgrade(this);
        this.wifi_switch.connect_active_notify(move |switch| {
            let Some(this) = weak.upgrade() else {
                return;
            };
            if this.syncing_switch.get() {
                return;
            }
            this.set_wifi_enabled(switch.is_active());
        });

        let weak = Rc::downgrade(this);
        cancel_button.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.hide_password_prompt();
            }
        });

        let weak = Rc::downgrade(this);
        connect_button.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.connect_password_target();
            }
        });

        let weak = Rc::downgrade(this);
        this.password_entry.connect_activate(move |_| {
            if let Some(this) = weak.upgrade() {
                this.connect_password_target();
            }
        });

        attach_popup_escape_handler(&this.popup, Rc::downgrade(this), |this| {
            if this.password_box.is_visible() {
                this.hide_password_prompt();
            } else if this.popup.is_visible() {
                this.close_popup();
            } else {
                return false;
            }
            true
        });

        attach_popup_lifecycle(
            bar_window,
            &this.trigger,
            &this.popup,
            &this.focus_armed,
            Rc::downgrade(this),
            |this| this.close_popup(),
            |this| {
                reset_hidden_popup_state(
                    &this.popup_reveal,
                    &this.focus_armed,
                    &this.trigger,
                    "network-popup-open",
                );
                this.hide_password_prompt();
            },
        );
    }

    fn subscribe_to_network_manager(this: &Rc<Self>) {
        if !this.signal_subscriptions.borrow().is_empty()
            || this.signal_subscription_pending.replace(true)
        {
            return;
        }
        let weak = Rc::downgrade(this);
        gio::bus_get(
            gio::BusType::System,
            None::<&gio::Cancellable>,
            move |result| {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                this.signal_subscription_pending.set(false);
                match result {
                    Ok(connection) => {
                        this.signal_subscription_retry_attempt.set(0);
                        Self::install_network_manager_subscriptions(&this, &connection);
                    }
                    Err(error) => {
                        debug!(
                            %error,
                            "system D-Bus is unavailable; retrying NetworkManager signals"
                        );
                        this.schedule_signal_subscription_retry();
                    }
                }
            },
        );
    }

    fn schedule_signal_subscription_retry(self: &Rc<Self>) {
        if self.signal_subscription_retry_pending.replace(true) {
            return;
        }

        let attempt = self.signal_subscription_retry_attempt.get();
        self.signal_subscription_retry_attempt
            .set(attempt.saturating_add(1));
        let multiplier = 1_u32 << attempt.min(5);
        let delay = SIGNAL_SUBSCRIPTION_RETRY_BASE_DELAY
            .saturating_mul(multiplier)
            .min(SIGNAL_SUBSCRIPTION_RETRY_MAX_DELAY);

        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(delay, move || {
            let Some(this) = weak.upgrade() else {
                return;
            };
            this.signal_subscription_retry_pending.set(false);
            Self::subscribe_to_network_manager(&this);
        });
    }

    fn install_network_manager_subscriptions(this: &Rc<Self>, connection: &gio::DBusConnection) {
        let filters = [
            (Some(NM_INTERFACE), None, None),
            (Some(NM_DEVICE_INTERFACE), None, None),
            (Some(NM_WIRELESS_INTERFACE), None, None),
            (Some(NM_ACCESS_POINT_INTERFACE), None, None),
            (Some(NM_SETTINGS_INTERFACE), None, None),
            (Some(NM_SETTINGS_CONNECTION_INTERFACE), None, None),
            (
                Some(DBUS_PROPERTIES_INTERFACE),
                Some("PropertiesChanged"),
                Some(NM_INTERFACE),
            ),
            (
                Some(DBUS_PROPERTIES_INTERFACE),
                Some("PropertiesChanged"),
                Some(NM_DEVICE_INTERFACE),
            ),
            (
                Some(DBUS_PROPERTIES_INTERFACE),
                Some("PropertiesChanged"),
                Some(NM_WIRELESS_INTERFACE),
            ),
            (
                Some(DBUS_PROPERTIES_INTERFACE),
                Some("PropertiesChanged"),
                Some(NM_ACCESS_POINT_INTERFACE),
            ),
        ];

        let mut subscriptions = Vec::with_capacity(filters.len());
        for (interface, member, arg0) in filters {
            let weak = Rc::downgrade(this);
            subscriptions.push(connection.subscribe_to_signal(
                Some(NETWORK_MANAGER_BUS_NAME),
                interface,
                member,
                None,
                arg0,
                gio::DBusSignalFlags::NONE,
                move |_| {
                    if let Some(this) = weak.upgrade() {
                        this.schedule_signal_refresh();
                    }
                },
            ));
        }
        let weak = Rc::downgrade(this);
        subscriptions.push(connection.subscribe_to_signal(
            Some(DBUS_BUS_NAME),
            Some(DBUS_BUS_INTERFACE),
            Some("NameOwnerChanged"),
            Some(DBUS_BUS_PATH),
            Some(NETWORK_MANAGER_BUS_NAME),
            gio::DBusSignalFlags::NONE,
            move |_| {
                if let Some(this) = weak.upgrade() {
                    this.schedule_signal_refresh();
                }
            },
        ));

        this.signal_subscriptions.replace(subscriptions);
    }

    fn schedule_signal_refresh(self: &Rc<Self>) {
        if self.signal_refresh_pending.replace(true) {
            return;
        }

        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            let Some(this) = weak.upgrade() else {
                return;
            };
            this.signal_refresh_pending.set(false);
            this.refresh_snapshot();
        });
    }

    fn refresh_snapshot(self: &Rc<Self>) {
        if !self.wifi_read.begin() {
            return;
        }

        let revision = self.wifi_revision.current();
        let backend = self.backend.clone();
        let weak = Rc::downgrade(self);
        run_background(
            move || backend.snapshot(),
            move |result| {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                let retry = this.wifi_read.finish();

                if !this.wifi_revision.is_current(revision) {
                    if retry {
                        this.refresh_snapshot();
                    }
                    return;
                }

                match result {
                    Ok(snapshot) => this.apply_snapshot(snapshot),
                    Err(error) => {
                        debug!(%error, "failed to refresh NetworkManager state");
                        this.apply_snapshot(WifiSnapshot::default());
                        this.set_notice(Some(&format!("NetworkManager: {error}")));
                    }
                }

                if retry {
                    this.refresh_snapshot();
                }
            },
        );
    }

    fn refresh_vless(self: &Rc<Self>) {
        if !self.vless_read.begin() {
            return;
        }

        let revision = self.vless_revision.current();
        let backend = self.backend.clone();
        let weak = Rc::downgrade(self);
        run_background(
            move || backend.vless_state(),
            move |result| {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                let retry = this.vless_read.finish();

                if !this.vless_revision.is_current(revision) {
                    if retry {
                        this.refresh_vless();
                    }
                    return;
                }

                match result {
                    Ok(vless) => this.apply_vless_state(vless),
                    Err(error) => {
                        debug!(%error, "failed to refresh VLESS state");
                        this.apply_vless_state(VlessState::default());
                    }
                }

                if retry {
                    this.refresh_vless();
                }
            },
        );
    }

    fn apply_snapshot(self: &Rc<Self>, snapshot: WifiSnapshot) {
        let initialize_header = !self.header_initialized.replace(true);
        let last_scan = snapshot.last_scan;
        let wifi_enabled = snapshot.enabled;
        let (header_changed, list_changed, password_prompt_invalidated) = {
            let mut state = self.state.borrow_mut();
            let header_changed = wifi_header_changed(&state.wifi, &snapshot);
            let list_changed = state.wifi.available != snapshot.available
                || state.wifi.enabled != snapshot.enabled
                || state.wifi.networks != snapshot.networks;
            let refreshed_password_target =
                refresh_password_target(state.password_target.as_ref(), &snapshot);
            let password_prompt_invalidated =
                state.password_target.is_some() && refreshed_password_target.is_none();
            state.password_target = refreshed_password_target;
            state.wifi = snapshot;
            (header_changed, list_changed, password_prompt_invalidated)
        };

        if password_prompt_invalidated {
            self.hide_password_prompt();
            self.set_notice(None);
        }

        if !wifi_enabled && self.rescan_busy.get() {
            self.cancel_rescan_tracking();
            self.set_notice(None);
        }

        let finished_rescan = self
            .rescan_baseline
            .get()
            .is_some_and(|baseline| last_scan != baseline);
        if finished_rescan {
            self.rescan_baseline.set(None);
            self.rescan_busy.set(false);
            self.set_notice(None);
            self.set_rescan_animating(false);
        }

        if header_changed || initialize_header || finished_rescan {
            self.update_header();
        }
        if list_changed || !self.list_initialized.get() {
            if self.popup.is_visible() {
                self.sync_network_list();
            } else {
                self.list_dirty.set(true);
            }
        }
    }

    fn apply_vless_state(&self, vless: VlessState) {
        let changed = {
            let mut state = self.state.borrow_mut();
            if state.vless == vless {
                false
            } else {
                state.vless = vless;
                true
            }
        };
        if changed {
            self.update_header();
        }
    }

    fn update_header(&self) {
        let state = self.state.borrow();
        let snapshot = &state.wifi;
        let active = snapshot.networks.iter().find(|network| network.active);

        let icon = if !snapshot.available || !snapshot.enabled {
            ICON_WIFI_OFF
        } else if let Some(network) = active {
            wifi_signal_icon(network.signal)
        } else {
            ICON_WIFI_NONE
        };
        self.trigger_label.set_text(icon);

        let (title, status, tooltip) = if !snapshot.available {
            (
                "Wi-Fi".to_owned(),
                "No Wi-Fi device".to_owned(),
                "Wi-Fi • no NetworkManager device".to_owned(),
            )
        } else if !snapshot.enabled {
            (
                "Wi-Fi".to_owned(),
                "Disabled".to_owned(),
                "Wi-Fi disabled".to_owned(),
            )
        } else if let Some(network) = active {
            let security = network.security.label();
            (
                network.ssid.clone(),
                format!("{}% • {security}", network.signal),
                format!("{} • {}% • {security}", network.ssid, network.signal),
            )
        } else {
            let count = snapshot.networks.len();
            (
                "Wi-Fi".to_owned(),
                if count == 0 {
                    "Ready".to_owned()
                } else {
                    format!("{count} networks")
                },
                "Wi-Fi ready".to_owned(),
            )
        };
        let status = append_service_meta(status, state.vless.active);
        let tooltip = append_service_meta(tooltip, state.vless.active);

        self.popup_title.set_text(&title);
        self.popup_status.set_text(&status);
        self.trigger.set_bar_tooltip_text(Some(&tooltip));

        let actions_enabled = !self.action_busy.get();
        let show_vless = state.vless.available;
        let show_rescan = snapshot.available && snapshot.enabled;
        self.header_actions.set_visible(show_vless || show_rescan);
        self.vless_button.set_visible(show_vless);
        self.vless_button
            .set_sensitive(show_vless && actions_enabled);
        self.vless_icon.set_text(if state.vless.active {
            ICON_VLESS_ACTIVE
        } else {
            ICON_VLESS_INACTIVE
        });
        if state.vless.active {
            self.vless_button.add_css_class("network-vless-active");
        } else {
            self.vless_button.remove_css_class("network-vless-active");
        }

        self.rescan_button.set_visible(show_rescan);
        self.rescan_button
            .set_sensitive(show_rescan && actions_enabled && !self.rescan_busy.get());

        self.syncing_switch.set(true);
        self.wifi_switch
            .set_sensitive(snapshot.available && actions_enabled);
        self.wifi_switch.set_active(snapshot.enabled);
        self.syncing_switch.set(false);

        self.password_entry.set_sensitive(actions_enabled);
        self.password_connect_button.set_sensitive(actions_enabled);
        for row in self.network_rows.borrow().values() {
            row.set_action_enabled(actions_enabled);
        }
    }

    fn sync_network_list(self: &Rc<Self>) {
        self.list_initialized.set(true);
        self.list_dirty.set(false);

        let state = self.state.borrow();
        let snapshot = &state.wifi;
        if !snapshot.available {
            self.show_empty("No Wi-Fi device found");
            return;
        }
        if !snapshot.enabled {
            self.show_empty("Wi-Fi is disabled");
            return;
        }
        if snapshot.networks.is_empty() {
            self.show_empty("No networks found");
            return;
        }

        if self.network_rows.borrow().is_empty() && self.list.first_child().is_some() {
            self.clear_network_list_widgets();
        }

        let desired_ssids = snapshot
            .networks
            .iter()
            .map(|network| network.ssid.as_str())
            .collect::<HashSet<_>>();
        {
            let mut rows = self.network_rows.borrow_mut();
            rows.retain(|ssid, row| {
                let keep = desired_ssids.contains(ssid.as_str());
                if !keep {
                    self.list.remove(&row.row);
                }
                keep
            });
        }

        let mut previous: Option<gtk::Widget> = None;
        let mut rows = self.network_rows.borrow_mut();
        for network in &snapshot.networks {
            let row = if let Some(row) = rows.get_mut(network.ssid.as_str()) {
                row.update(network);
                row.row.clone()
            } else {
                let row = self.build_network_row(network.clone());
                let widget = row.row.clone();
                self.list.append(&widget);
                rows.insert(network.ssid.clone(), row);
                widget
            };

            if row.prev_sibling().as_ref() != previous.as_ref() {
                self.list.reorder_child_after(&row, previous.as_ref());
            }
            previous = Some(row.upcast::<gtk::Widget>());
        }
    }

    fn clear_network_list_widgets(&self) {
        clear_box(&self.list);
        self.network_rows.borrow_mut().clear();
    }

    fn show_empty(&self, text: &str) {
        if self.network_rows.borrow().is_empty()
            && let Some(child) = self.list.first_child()
            && child.next_sibling().is_none()
            && let Ok(label) = child.downcast::<gtk::Label>()
            && label.text() == text
        {
            return;
        }

        self.clear_network_list_widgets();
        self.list.append(&empty_state_label(text, None));
    }

    fn build_network_row(self: &Rc<Self>, network: WifiNetwork) -> NetworkRow {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        row.add_css_class("network-row-container");
        row.set_valign(gtk::Align::Center);
        let saved = network.saved();

        let main = gtk::Button::new();
        main.add_css_class("network-row-main");
        main.set_hexpand(true);
        main.set_halign(gtk::Align::Fill);

        let body = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        body.add_css_class("network-row-body");
        body.set_hexpand(true);
        body.set_valign(gtk::Align::Center);

        let icon_frame = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        icon_frame.add_css_class("network-row-icon-frame");
        icon_frame.set_halign(gtk::Align::Center);
        icon_frame.set_valign(gtk::Align::Center);

        let icon = gtk::Label::new(Some(wifi_signal_icon(network.signal)));
        icon.add_css_class("network-row-icon");
        icon.set_halign(gtk::Align::Center);
        icon.set_valign(gtk::Align::Center);
        icon_frame.append(&icon);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 2);
        content.add_css_class("network-row-content");
        content.set_hexpand(true);
        content.set_valign(gtk::Align::Center);

        let name = gtk::Label::new(Some(&network.ssid));
        name.add_css_class("network-row-title");
        name.set_xalign(0.0);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        name.set_max_width_chars(28);

        let meta_text = network_meta(&network);
        let meta = gtk::Label::new(Some(&meta_text));
        meta.add_css_class("network-row-meta");
        meta.set_xalign(0.0);
        meta.set_ellipsize(gtk::pango::EllipsizeMode::End);
        meta.set_max_width_chars(38);

        content.append(&name);
        content.append(&meta);

        let status = gtk::Label::new(Some(network_status_icon(&network)));
        status.add_css_class("network-row-status");
        status.set_halign(gtk::Align::Center);
        status.set_valign(gtk::Align::Center);

        body.append(&icon_frame);
        body.append(&content);
        body.append(&status);
        main.set_child(Some(&body));

        let model = Rc::new(RefCell::new(network));
        let selected = model.clone();
        let weak = Rc::downgrade(self);
        main.connect_clicked(move |_| {
            let Some(this) = weak.upgrade() else {
                return;
            };
            let selected = selected.borrow().clone();
            if selected.active {
                return;
            }
            if !selected.saved() && !selected.security.supports_new_profile() {
                this.set_notice(Some(
                    "Enterprise Wi-Fi needs a saved NetworkManager 802.1X profile",
                ));
            } else if !selected.saved() && selected.security.requires_password() {
                this.show_password_prompt(selected);
            } else {
                this.connect_network(selected, None);
            }
        });
        row.append(&main);

        let forget_icon = gtk::Label::new(Some(ICON_FORGET));
        forget_icon.add_css_class("network-row-side-icon");

        let forget_button = gtk::Button::new();
        forget_button.add_css_class("network-row-side-button");
        forget_button.set_valign(gtk::Align::Center);
        forget_button.set_child(Some(&forget_icon));
        forget_button.set_visible(saved);

        let forget_model = model.clone();
        let weak = Rc::downgrade(self);
        forget_button.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.forget_network(forget_model.borrow().clone());
            }
        });
        row.append(&forget_button);

        let network_row = NetworkRow {
            row,
            main,
            model,
            icon,
            meta,
            status,
            forget_button,
        };
        network_row.set_action_enabled(!self.action_busy.get());
        network_row
    }

    fn toggle_popup(self: &Rc<Self>) {
        if self.popup_reveal.is_revealed() {
            self.close_popup();
        } else {
            self.open_popup();
        }
    }

    fn open_popup(self: &Rc<Self>) {
        self.focus_armed.set(false);
        self.set_notice(None);
        let generation = self.popup_reveal.show(&self.popup);
        self.trigger.add_css_class("network-popup-open");
        if self.list_dirty.get() || !self.list_initialized.get() {
            self.sync_network_list();
        }
        self.refresh_vless();
        let should_scan = {
            let state = self.state.borrow();
            state.wifi.available && state.wifi.enabled && !self.action_busy.get()
        };
        if should_scan {
            self.rescan();
        } else {
            self.refresh_snapshot();
        }

        run_when_popup_visible(
            &self.popup,
            &self.popup_reveal,
            generation,
            Rc::downgrade(self),
            |this| {
                this.popup_root.grab_focus();
            },
        );
    }

    fn close_popup(self: &Rc<Self>) {
        if !self.popup.is_visible() {
            return;
        }
        self.focus_armed.set(false);
        self.trigger.remove_css_class("network-popup-open");
        self.hide_password_prompt();
        self.popup_reveal.hide(&self.popup);
    }

    fn set_wifi_enabled(self: &Rc<Self>, enabled: bool) {
        {
            let mut state = self.state.borrow_mut();
            state.wifi.enabled = enabled;
            if !enabled {
                for network in &mut state.wifi.networks {
                    network.active = false;
                }
            }
        }
        if !enabled {
            self.hide_password_prompt();
        }
        self.update_header();
        self.sync_network_list();

        let text = if enabled {
            "Enabling Wi-Fi…"
        } else {
            "Disabling Wi-Fi…"
        };
        self.run_action(text, RefreshTarget::Wifi, move |backend| {
            backend.set_enabled(enabled)
        });
    }

    fn toggle_vless(self: &Rc<Self>) {
        let active = self.state.borrow().vless.active;
        let target = !active;
        let status = if target {
            "Starting VLESS…"
        } else {
            "Stopping VLESS…"
        };
        self.run_action(status, RefreshTarget::Vless, move |backend| {
            backend.set_vless_active(target)
        });
    }

    fn rescan(self: &Rc<Self>) {
        if self.action_busy.get() || self.rescan_busy.replace(true) {
            return;
        }

        let generation = self.rescan_generation.bump();
        self.wifi_revision.bump();
        self.set_notice(Some("Scanning…"));
        self.set_rescan_animating(true);
        self.update_header();

        let backend = self.backend.clone();
        let weak = Rc::downgrade(self);
        run_background(
            move || backend.request_scan(),
            move |result| {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                if !this.rescan_generation.is_current(generation) {
                    return;
                }

                match result {
                    Ok(previous_last_scan) => {
                        this.rescan_baseline.set(Some(previous_last_scan));

                        this.refresh_snapshot();

                        let weak = Rc::downgrade(&this);
                        glib::timeout_add_local_once(SCAN_COMPLETION_TIMEOUT, move || {
                            let Some(this) = weak.upgrade() else {
                                return;
                            };
                            if !this.rescan_generation.is_current(generation) {
                                return;
                            }
                            if this.rescan_baseline.get() == Some(previous_last_scan) {
                                this.rescan_baseline.set(None);
                                this.rescan_busy.set(false);
                                this.set_rescan_animating(false);
                                this.set_notice(Some("Wi-Fi scan completion timed out"));
                                this.update_header();
                                this.refresh_snapshot();
                            }
                        });
                    }
                    Err(error) => {
                        warn!(%error, "Wi-Fi scan failed");
                        this.rescan_busy.set(false);
                        this.rescan_baseline.set(None);
                        this.set_rescan_animating(false);
                        this.set_notice(Some(&error));
                        this.update_header();
                        this.refresh_snapshot();
                    }
                }
            },
        );
    }

    fn connect_network(self: &Rc<Self>, network: WifiNetwork, password: Option<String>) {
        let status = format!("Connecting to {}…", network.ssid);
        self.run_action(&status, RefreshTarget::Wifi, move |backend| {
            backend.connect(&network, password.as_deref())
        });
    }

    fn forget_network(self: &Rc<Self>, network: WifiNetwork) {
        let status = format!("Forgetting {}…", network.ssid);
        self.run_action(&status, RefreshTarget::Wifi, move |backend| {
            backend.forget(&network)
        });
    }

    fn run_action<F>(self: &Rc<Self>, status: &str, refresh_target: RefreshTarget, job: F)
    where
        F: FnOnce(NetworkBackend) -> Result<(), String> + Send + 'static,
    {
        if self.action_busy.replace(true) {
            return;
        }

        if matches!(refresh_target, RefreshTarget::Wifi) {
            self.cancel_rescan_tracking();
        }

        match refresh_target {
            RefreshTarget::Wifi => self.wifi_revision.bump(),
            RefreshTarget::Vless => self.vless_revision.bump(),
        };
        self.set_notice(Some(status));
        self.update_header();

        let backend = self.backend.clone();
        let weak = Rc::downgrade(self);
        run_background(
            move || job(backend),
            move |result| {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                this.action_busy.set(false);

                let succeeded = result.is_ok();
                match result {
                    Ok(()) => this.set_notice(None),
                    Err(error) => {
                        warn!(%error, "network action failed");
                        this.set_notice(Some(&error));
                    }
                }
                this.update_header();
                if succeeded && matches!(refresh_target, RefreshTarget::Vless) {
                    let weak = Rc::downgrade(&this);
                    glib::timeout_add_local_once(VLESS_ACTION_REFRESH_DELAY, move || {
                        if let Some(this) = weak.upgrade() {
                            this.refresh_vless();
                        }
                    });
                } else {
                    this.refresh_target(refresh_target);
                }
            },
        );
    }

    fn refresh_target(self: &Rc<Self>, target: RefreshTarget) {
        match target {
            RefreshTarget::Wifi => self.refresh_snapshot(),
            RefreshTarget::Vless => self.refresh_vless(),
        }
    }

    fn set_rescan_animating(&self, active: bool) {
        set_spinner_active(&self.rescan_icon, &self.rescan_spinner, active);
    }

    fn cancel_rescan_tracking(&self) {
        self.rescan_generation.bump();
        self.rescan_busy.set(false);
        self.rescan_baseline.set(None);
        self.set_rescan_animating(false);
    }

    fn show_password_prompt(&self, network: WifiNetwork) {
        self.password_title
            .set_text(&format!("Connect to {}", network.ssid));
        self.state.borrow_mut().password_target = Some(network);
        self.password_entry.set_text("");
        self.password_box.set_visible(true);
        self.set_notice(None);
        self.password_entry.grab_focus();
    }

    fn hide_password_prompt(&self) {
        self.state.borrow_mut().password_target = None;
        self.password_entry.set_text("");
        self.password_box.set_visible(false);
    }

    fn connect_password_target(self: &Rc<Self>) {
        if self.action_busy.get() {
            return;
        }

        let Some(network) = self.state.borrow().password_target.clone() else {
            return;
        };
        let password = self.password_entry.text().to_string();
        if password.is_empty() {
            self.set_notice(Some("Password is required"));
            return;
        }

        self.hide_password_prompt();
        self.connect_network(network, Some(password));
    }

    fn set_notice(&self, text: Option<&str>) {
        set_optional_label(&self.notice, text);
    }
}

fn refresh_password_target(
    target: Option<&WifiNetwork>,
    snapshot: &WifiSnapshot,
) -> Option<WifiNetwork> {
    let target = target?;
    snapshot
        .networks
        .iter()
        .find(|network| {
            network.ssid == target.ssid && !network.saved() && network.security.requires_password()
        })
        .cloned()
}

fn wifi_header_changed(previous: &WifiSnapshot, next: &WifiSnapshot) -> bool {
    if previous.available != next.available || previous.enabled != next.enabled {
        return true;
    }
    if !next.available || !next.enabled {
        return false;
    }

    let previous_active = previous.networks.iter().find(|network| network.active);
    let next_active = next.networks.iter().find(|network| network.active);
    match (previous_active, next_active) {
        (Some(previous), Some(next)) => {
            previous.ssid != next.ssid
                || previous.signal != next.signal
                || previous.security != next.security
        }
        (None, None) => previous.networks.len() != next.networks.len(),
        _ => true,
    }
}

fn network_meta(network: &WifiNetwork) -> String {
    let saved = if network.saved() { " • saved" } else { "" };
    format!("{}% • {}{saved}", network.signal, network.security.label())
}

fn network_status_icon(network: &WifiNetwork) -> &'static str {
    if network.active {
        ICON_CHECK
    } else if network.security.secured() && !network.saved() {
        ICON_LOCK
    } else {
        ""
    }
}

fn append_service_meta(base: String, vless_active: bool) -> String {
    if vless_active {
        format!("{base} • VLESS")
    } else {
        base
    }
}

fn wifi_signal_icon(signal: u8) -> &'static str {
    match signal {
        80..=u8::MAX => ICON_WIFI_HIGH,
        60..=79 => ICON_WIFI_GOOD,
        40..=59 => ICON_WIFI_MID,
        20..=39 => ICON_WIFI_LOW,
        _ => ICON_WIFI_NONE,
    }
}

#[cfg(test)]
mod tests {
    use super::{ICON_WIFI_GOOD, ICON_WIFI_HIGH, ICON_WIFI_LOW, ICON_WIFI_MID, wifi_signal_icon};

    #[test]
    fn signal_icon_thresholds_are_stable() {
        assert_eq!(wifi_signal_icon(80), ICON_WIFI_HIGH);
        assert_eq!(wifi_signal_icon(60), ICON_WIFI_GOOD);
        assert_eq!(wifi_signal_icon(40), ICON_WIFI_MID);
        assert_eq!(wifi_signal_icon(20), ICON_WIFI_LOW);
    }
}
