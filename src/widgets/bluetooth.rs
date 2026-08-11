use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use gtk::{gdk, glib, prelude::*};
use tracing::{debug, warn};

pub(crate) use super::bluetooth_backend::BluetoothAgent;
use super::tooltip::BarTooltipExt;
use super::{
    BAR_POPUP_WIDTH, Generation, PopupReveal, RefreshGate, attach_popup_escape_handler,
    attach_popup_lifecycle,
    bluetooth_backend::{
        AgentEvent, AgentPromptKind, BluetoothBackend, BluetoothDevice, BluetoothSnapshot,
    },
    build_bar_popup, build_quick_toggle_button, build_refresh_button, clear_box,
    detach_application_window, empty_state_label, reset_hidden_popup_state, run_background,
    run_when_popup_visible, set_optional_label, set_spinner_active,
};

const DEVICE_LIST_MIN_HEIGHT: i32 = 120;
const DEVICE_LIST_MAX_HEIGHT: i32 = 220;
const BLUETOOTH_POPUP_NAMESPACE: &str = "obsidian-bar-bluetooth";
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
const NOTICE_TIMEOUT: Duration = Duration::from_millis(4200);
const SIGNAL_SUBSCRIPTION_RETRY_BASE_DELAY: Duration = Duration::from_secs(1);
const SIGNAL_SUBSCRIPTION_RETRY_MAX_DELAY: Duration = Duration::from_secs(30);

const ICON_BLUETOOTH_OFF: &str = "󰂲";
const ICON_BLUETOOTH_CONNECTED: &str = "󰂱";
const ICON_BLUETOOTH_ON: &str = "󰂯";
const ICON_REFRESH: &str = "󰑐";
const ICON_REMOVE: &str = "󰅖";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingDeviceAction {
    Pairing,
    Connecting,
    Disconnecting,
    Removing,
}

impl PendingDeviceAction {
    fn target_connected(self, current: bool) -> bool {
        match self {
            Self::Pairing | Self::Connecting => true,
            Self::Disconnecting => false,
            Self::Removing => current,
        }
    }

    fn status(self) -> &'static str {
        match self {
            Self::Pairing => "Pairing…",
            Self::Connecting => "Connecting…",
            Self::Disconnecting => "Disconnecting…",
            Self::Removing => "Removing…",
        }
    }
}

#[derive(Debug)]
struct PendingDevice {
    path: String,
    action: PendingDeviceAction,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum PairingPromptMode {
    #[default]
    Hidden,
    Input,
    Confirmation,
    Display,
}

struct BluetoothController {
    root: gtk::Box,
    trigger: gtk::Button,
    trigger_label: gtk::Label,

    popup: gtk::ApplicationWindow,
    popup_root: gtk::Box,
    popup_title: gtk::Label,
    popup_status: gtk::Label,
    power_switch: gtk::Switch,
    header_actions: gtk::Box,
    scan_button: gtk::Button,
    scan_icon: gtk::Label,
    scan_spinner: gtk::Spinner,
    pairing_box: gtk::Box,
    pairing_title: gtk::Label,
    pairing_message: gtk::Label,
    pairing_code: gtk::Label,
    pairing_entry: gtk::Entry,
    pairing_cancel_button: gtk::Button,
    pairing_confirm_button: gtk::Button,
    list: gtk::Box,
    notice: gtk::Label,

    backend: BluetoothBackend,
    agent: BluetoothAgent,
    snapshot: RefCell<Arc<BluetoothSnapshot>>,
    read_gate: RefreshGate,
    snapshot_generation: Generation,
    action_busy: Cell<bool>,
    pending_power: Cell<Option<bool>>,
    pending_scan: Cell<Option<bool>>,
    pending_device: RefCell<Option<PendingDevice>>,
    pairing_mode: Cell<PairingPromptMode>,
    pairing_device_path: RefCell<Option<String>>,
    pairing_cancelled: Cell<bool>,
    syncing_power_switch: Cell<bool>,
    popup_reveal: PopupReveal,
    focus_armed: Rc<Cell<bool>>,
    list_initialized: Cell<bool>,
    list_dirty: Cell<bool>,

    signal_refresh_pending: Cell<bool>,
    signal_subscription_pending: Cell<bool>,
    signal_subscription_retry_pending: Cell<bool>,
    signal_subscription_retry_attempt: Cell<u32>,
    signal_subscriptions: RefCell<Vec<gio::SignalSubscription>>,
    discovery_timeout_generation: Generation,
    notice_generation: Generation,
}

pub struct BluetoothIndicator {
    root: gtk::Box,
    _controller: Rc<BluetoothController>,
}

impl Drop for BluetoothIndicator {
    fn drop(&mut self) {
        detach_application_window(&self._controller.popup);
    }
}

impl BluetoothIndicator {
    pub fn new(
        application: &gtk::Application,
        bar_window: &gtk::ApplicationWindow,
        monitor: &gdk::Monitor,
        agent: &BluetoothAgent,
    ) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        root.add_css_class("quick-control");
        root.set_valign(gtk::Align::Center);
        root.set_visible(false);

        let (trigger, trigger_label) = build_quick_toggle_button(
            ICON_BLUETOOTH_OFF,
            "bluetooth-trigger",
            &["bluetooth-trigger-icon"],
        );
        trigger.set_bar_tooltip_text(Some("Bluetooth"));
        root.append(&trigger);

        let popup = build_bar_popup(
            application,
            monitor,
            BLUETOOTH_POPUP_NAMESPACE,
            "bluetooth-popup-window",
        );

        let popup_root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        popup_root.add_css_class("widget-popup-root");
        popup_root.set_focusable(true);

        let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
        frame.add_css_class("widget-popup-frame");
        frame.add_css_class("bluetooth-popover-window");
        frame.set_overflow(gtk::Overflow::Hidden);
        frame.set_size_request(BAR_POPUP_WIDTH, -1);

        let popup_content = gtk::Box::new(gtk::Orientation::Vertical, 10);
        popup_content.add_css_class("network-popover");

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        header.add_css_class("network-header");
        header.set_valign(gtk::Align::Center);

        let header_text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        header_text.set_hexpand(true);
        header_text.set_valign(gtk::Align::Center);

        let popup_title = gtk::Label::new(Some("Bluetooth"));
        popup_title.add_css_class("network-header-title");
        popup_title.set_xalign(0.0);
        popup_title.set_ellipsize(gtk::pango::EllipsizeMode::End);

        let popup_status = gtk::Label::new(Some("Loading…"));
        popup_status.add_css_class("network-header-meta");
        popup_status.set_xalign(0.0);

        header_text.append(&popup_title);
        header_text.append(&popup_status);

        let (scan_button, scan_icon, scan_spinner) = build_refresh_button(ICON_REFRESH);

        let header_actions = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        header_actions.add_css_class("network-header-action-capsule");
        header_actions.set_valign(gtk::Align::Center);
        header_actions.set_visible(false);
        header_actions.append(&scan_button);

        let power_switch = gtk::Switch::new();
        power_switch.add_css_class("network-wifi-switch");
        power_switch.set_valign(gtk::Align::Center);

        header.append(&header_text);
        header.append(&header_actions);
        header.append(&power_switch);

        let pairing_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
        pairing_box.add_css_class("network-password-box");
        pairing_box.set_visible(false);

        let pairing_title = gtk::Label::new(Some("Pair Bluetooth device"));
        pairing_title.add_css_class("network-password-title");
        pairing_title.set_xalign(0.0);
        pairing_title.set_ellipsize(gtk::pango::EllipsizeMode::End);

        let pairing_message = gtk::Label::new(None);
        pairing_message.add_css_class("network-row-meta");
        pairing_message.add_css_class("bluetooth-pairing-message");
        pairing_message.set_xalign(0.0);
        pairing_message.set_wrap(true);

        let pairing_code = gtk::Label::new(None);
        pairing_code.add_css_class("bluetooth-pairing-code");
        pairing_code.set_xalign(0.0);
        pairing_code.set_selectable(true);
        pairing_code.set_visible(false);

        let pairing_entry = gtk::Entry::new();
        pairing_entry.add_css_class("network-password-entry");
        pairing_entry.set_visibility(false);
        pairing_entry.set_input_purpose(gtk::InputPurpose::Password);
        pairing_entry.set_placeholder_text(Some("PIN / passkey"));
        pairing_entry.set_hexpand(true);
        pairing_entry.set_max_length(16);
        pairing_entry.set_visible(false);

        let pairing_actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        pairing_actions.set_halign(gtk::Align::End);

        let pairing_cancel_button = gtk::Button::with_label("Cancel");
        pairing_cancel_button.add_css_class("network-password-action");
        let pairing_confirm_button = gtk::Button::with_label("Pair");
        pairing_confirm_button.add_css_class("network-password-action");
        pairing_confirm_button.set_visible(false);

        pairing_actions.append(&pairing_cancel_button);
        pairing_actions.append(&pairing_confirm_button);
        pairing_box.append(&pairing_title);
        pairing_box.append(&pairing_message);
        pairing_box.append(&pairing_code);
        pairing_box.append(&pairing_entry);
        pairing_box.append(&pairing_actions);

        let section_title = gtk::Label::new(Some("Devices"));
        section_title.add_css_class("network-section-title");
        section_title.set_xalign(0.0);

        let list_capsule = gtk::Box::new(gtk::Orientation::Vertical, 0);
        list_capsule.add_css_class("network-list-capsule");

        let scroller = gtk::ScrolledWindow::new();
        scroller.add_css_class("network-list-scroller");
        scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Never);
        scroller.set_kinetic_scrolling(false);
        scroller.set_propagate_natural_height(true);
        scroller.set_min_content_height(DEVICE_LIST_MIN_HEIGHT);
        scroller.set_max_content_height(DEVICE_LIST_MAX_HEIGHT);

        let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
        list.add_css_class("network-list-inner");
        scroller.set_child(Some(&list));
        list_capsule.append(&scroller);

        let notice = gtk::Label::new(None);
        notice.add_css_class("network-section-title");
        notice.add_css_class("network-notice");
        notice.set_xalign(0.0);
        notice.set_ellipsize(gtk::pango::EllipsizeMode::End);
        notice.set_visible(false);

        popup_content.append(&header);
        popup_content.append(&pairing_box);
        popup_content.append(&section_title);
        popup_content.append(&list_capsule);
        popup_content.append(&notice);
        frame.append(&popup_content);

        let popup_reveal = PopupReveal::masked(frame.clone().upcast::<gtk::Widget>());
        popup_root.append(popup_reveal.widget());
        popup.set_child(Some(&popup_root));

        let controller = Rc::new(BluetoothController {
            root: root.clone(),
            trigger,
            trigger_label,
            popup,
            popup_root,
            popup_title,
            popup_status,
            power_switch,
            header_actions,
            scan_button,
            scan_icon,
            scan_spinner,
            pairing_box,
            pairing_title,
            pairing_message,
            pairing_code,
            pairing_entry,
            pairing_cancel_button,
            pairing_confirm_button,
            list,
            notice,
            backend: BluetoothBackend,
            agent: agent.clone(),
            snapshot: RefCell::new(Arc::new(BluetoothSnapshot::default())),
            read_gate: RefreshGate::default(),
            snapshot_generation: Generation::default(),
            action_busy: Cell::new(false),
            pending_power: Cell::new(None),
            pending_scan: Cell::new(None),
            pending_device: RefCell::new(None),
            pairing_mode: Cell::new(PairingPromptMode::Hidden),
            pairing_device_path: RefCell::new(None),
            pairing_cancelled: Cell::new(false),
            syncing_power_switch: Cell::new(false),
            popup_reveal,
            focus_armed: Rc::new(Cell::new(false)),
            list_initialized: Cell::new(false),
            list_dirty: Cell::new(false),
            signal_refresh_pending: Cell::new(false),
            signal_subscription_pending: Cell::new(false),
            signal_subscription_retry_pending: Cell::new(false),
            signal_subscription_retry_attempt: Cell::new(0),
            signal_subscriptions: RefCell::new(Vec::new()),
            discovery_timeout_generation: Generation::default(),
            notice_generation: Generation::default(),
        });

        BluetoothController::connect(&controller, bar_window);
        BluetoothController::subscribe_to_bluez(&controller);
        controller.refresh_snapshot();

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

impl BluetoothController {
    fn connect(this: &Rc<Self>, bar_window: &gtk::ApplicationWindow) {
        let weak = Rc::downgrade(this);
        this.trigger.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.toggle_popup();
            }
        });

        let weak = Rc::downgrade(this);
        this.power_switch.connect_active_notify(move |switch| {
            let Some(this) = weak.upgrade() else {
                return;
            };
            if this.syncing_power_switch.get() {
                return;
            }
            if this.action_busy.get() {
                this.update_header();
                return;
            }

            let target = switch.is_active();
            let current = this.snapshot.borrow().powered();
            if target != current {
                this.set_powered(target);
            }
        });

        let weak = Rc::downgrade(this);
        this.scan_button.connect_clicked(move |_| {
            let Some(this) = weak.upgrade() else {
                return;
            };
            let scanning = this.display_scanning();
            this.set_scanning(!scanning);
        });

        let weak = Rc::downgrade(this);
        this.pairing_cancel_button.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.cancel_pairing_prompt();
            }
        });

        let weak = Rc::downgrade(this);
        this.pairing_confirm_button.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.submit_pairing_prompt();
            }
        });

        let weak = Rc::downgrade(this);
        this.pairing_entry.connect_activate(move |_| {
            if let Some(this) = weak.upgrade() {
                this.submit_pairing_prompt();
            }
        });

        attach_popup_escape_handler(&this.popup, Rc::downgrade(this), |this| {
            if !this.popup.is_visible() {
                return false;
            }
            if this.pairing_box.is_visible() {
                this.cancel_pairing_prompt();
            } else {
                this.close_popup();
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
                    "bluetooth-popup-open",
                );
            },
        );
    }

    fn subscribe_to_bluez(this: &Rc<Self>) {
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
                let connection = match result {
                    Ok(connection) => connection,
                    Err(error) => {
                        debug!(%error, "system D-Bus is unavailable; retrying BlueZ signals");
                        this.schedule_signal_subscription_retry();
                        return;
                    }
                };

                this.signal_subscription_retry_attempt.set(0);
                let weak = Rc::downgrade(&this);
                let handler = Rc::new(move || {
                    if let Some(this) = weak.upgrade() {
                        this.schedule_signal_refresh();
                    }
                });
                let subscriptions = this.backend.subscribe_changes(&connection, handler);
                this.signal_subscriptions.replace(subscriptions);
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
            Self::subscribe_to_bluez(&this);
        });
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
        if !self.read_gate.begin() {
            return;
        }

        let generation = self.snapshot_generation.current();
        let backend = self.backend;
        let weak = Rc::downgrade(self);
        run_background(
            move || backend.snapshot(),
            move |result| {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                let retry = this.read_gate.finish();

                if this.snapshot_generation.is_current(generation) {
                    match result {
                        Ok(snapshot) => this.apply_snapshot(snapshot),
                        Err(error) => {
                            debug!(%error, "failed to refresh BlueZ state");
                        }
                    }
                }

                if retry {
                    this.refresh_snapshot();
                }
            },
        );
    }

    fn apply_snapshot(self: &Rc<Self>, snapshot: Arc<BluetoothSnapshot>) {
        let list_changed = {
            let current = self.snapshot.borrow();
            let current_adapter = current
                .adapter
                .as_ref()
                .map(|adapter| (&adapter.path, adapter.powered));
            let next_adapter = snapshot
                .adapter
                .as_ref()
                .map(|adapter| (&adapter.path, adapter.powered));
            current_adapter != next_adapter
                || current.devices != snapshot.devices
                || (current.devices.is_empty()
                    && snapshot.devices.is_empty()
                    && current.discovering() != snapshot.discovering())
        };
        let available = snapshot.available();

        if self
            .pending_power
            .get()
            .is_some_and(|target| snapshot.powered() == target)
        {
            self.pending_power.set(None);
        }
        if self
            .pending_scan
            .get()
            .is_some_and(|target| snapshot.discovering() == target)
        {
            self.pending_scan.set(None);
        }

        self.snapshot.replace(snapshot);
        self.update_header();

        self.root.set_visible(available);
        if !available && self.popup.is_visible() {
            self.close_popup();
        }

        if list_changed || !self.list_initialized.get() {
            self.refresh_device_list();
        }
    }

    fn refresh_device_list(self: &Rc<Self>) {
        if self.popup.is_visible() {
            self.sync_device_list();
        } else {
            self.list_dirty.set(true);
        }
    }

    fn update_header(&self) {
        let snapshot = self.snapshot.borrow();
        let available = snapshot.available();
        let connected = snapshot.connected();
        let display_powered = self
            .pending_power
            .get()
            .unwrap_or_else(|| snapshot.powered());
        let display_scanning = self
            .pending_scan
            .get()
            .unwrap_or_else(|| snapshot.discovering());

        self.trigger.set_sensitive(available);
        self.trigger_label
            .set_text(trigger_glyph(display_powered, display_powered && connected));

        let tooltip = if !available {
            "Bluetooth unavailable".to_owned()
        } else if !display_powered {
            "Bluetooth off".to_owned()
        } else {
            let mut lines = vec![if display_scanning {
                "Bluetooth scanning".to_owned()
            } else if connected {
                "Bluetooth connected".to_owned()
            } else {
                "Bluetooth on".to_owned()
            }];
            let connected_names = snapshot
                .devices
                .iter()
                .filter(|device| device.connected)
                .map(|device| device.name.as_str())
                .collect::<Vec<_>>();
            if !connected_names.is_empty() {
                lines.push(connected_names.join(", "));
            }
            lines.join("\n")
        };
        self.trigger.set_bar_tooltip_text(Some(&tooltip));

        if let Some(adapter) = snapshot.adapter.as_ref() {
            self.popup_title.set_text(&adapter.alias);
        } else {
            self.popup_title.set_text("Bluetooth");
        }

        let status = match self.pending_power.get() {
            Some(true) if !snapshot.powered() => "Turning on…",
            Some(false) if snapshot.powered() => "Turning off…",
            _ if !available => "Unavailable",
            _ if !display_powered => "Off",
            _ if display_scanning => "Scanning…",
            _ => "Ready",
        };
        self.popup_status.set_text(status);

        self.syncing_power_switch.set(true);
        self.power_switch
            .set_sensitive(available && !self.action_busy.get());
        self.power_switch.set_active(display_powered);
        self.syncing_power_switch.set(false);

        let show_scan = available && display_powered;
        self.header_actions.set_visible(show_scan);
        self.scan_button.set_visible(show_scan);
        self.scan_button
            .set_sensitive(show_scan && !self.action_busy.get());
        self.set_scan_animating(show_scan && display_scanning);
    }

    fn display_scanning(&self) -> bool {
        self.pending_scan
            .get()
            .unwrap_or_else(|| self.snapshot.borrow().discovering())
    }

    fn sync_device_list(self: &Rc<Self>) {
        self.list_initialized.set(true);
        self.list_dirty.set(false);
        self.clear_device_list();

        let snapshot = self.snapshot.borrow();
        let Some(adapter) = snapshot.adapter.as_ref() else {
            self.show_empty("Bluetooth unavailable");
            return;
        };

        let display_powered = self.pending_power.get().unwrap_or(adapter.powered);
        if !display_powered {
            self.build_off_state();
            return;
        }

        if snapshot.devices.is_empty() {
            let display_scanning = self.pending_scan.get().unwrap_or(adapter.discovering);
            let text = if display_scanning {
                "Scanning devices…"
            } else {
                "No devices found"
            };
            self.show_empty(text);
            return;
        }

        for device in &snapshot.devices {
            self.list.append(&self.build_device_row(device));
        }
    }

    fn clear_device_list(&self) {
        clear_box(&self.list);
    }

    fn show_empty(&self, text: &str) {
        self.list.append(&empty_state_label(text, Some(0.0)));
    }

    fn build_off_state(&self) {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        row.add_css_class("bluetooth-off-state");
        row.set_valign(gtk::Align::Center);
        row.set_margin_top(8);
        row.set_margin_bottom(8);
        row.set_margin_start(10);
        row.set_margin_end(10);

        let icon = gtk::Label::new(Some(ICON_BLUETOOTH_OFF));
        icon.add_css_class("network-row-icon");
        icon.set_valign(gtk::Align::Center);

        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text.set_hexpand(true);
        text.set_valign(gtk::Align::Center);

        let title = gtk::Label::new(Some("Bluetooth is turned off"));
        title.add_css_class("network-row-title");
        title.set_xalign(0.0);
        let meta = gtk::Label::new(Some("Turn it on to see available devices"));
        meta.add_css_class("network-row-meta");
        meta.set_xalign(0.0);

        text.append(&title);
        text.append(&meta);
        row.append(&icon);
        row.append(&text);
        self.list.append(&row);
    }

    fn build_device_row(self: &Rc<Self>, device: &BluetoothDevice) -> gtk::Box {
        let pending_state = self.pending_device.borrow();
        let pending = pending_state
            .as_ref()
            .filter(|pending| pending.path == device.path);
        let busy = self.action_busy.get();

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        row.add_css_class("network-row-container");
        row.set_valign(gtk::Align::Center);

        let icon = gtk::Label::new(Some(device_glyph(&device.icon, &device.name)));
        icon.add_css_class("network-row-icon");
        icon.set_valign(gtk::Align::Center);
        row.append(&icon);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 2);
        content.set_hexpand(true);
        content.set_valign(gtk::Align::Center);

        let title = gtk::Label::new(Some(&device.name));
        title.add_css_class("network-row-title");
        title.set_xalign(0.0);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        title.set_max_width_chars(26);

        let meta = gtk::Label::new(Some(&device_meta(device, pending)));
        meta.add_css_class("network-row-meta");
        meta.set_xalign(0.0);

        content.append(&title);
        content.append(&meta);
        row.append(&content);

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        actions.set_valign(gtk::Align::Center);

        let show_pair_button = !device.connected && !device.paired;
        if show_pair_button {
            let pair_label = gtk::Label::new(Some(
                if pending.is_some_and(|pending| pending.action == PendingDeviceAction::Pairing) {
                    "Pairing…"
                } else {
                    "Pair"
                },
            ));
            pair_label.add_css_class("bluetooth-pair-button-label");

            let pair_button = gtk::Button::new();
            pair_button.add_css_class("network-row-button");
            pair_button.add_css_class("bluetooth-pair-button");
            pair_button.set_focusable(false);
            pair_button.set_sensitive(!busy);
            pair_button.set_child(Some(&pair_label));

            let weak = Rc::downgrade(self);
            let device_path = device.path.clone();
            pair_button.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.set_device_connected(&device_path, true);
                }
            });
            actions.append(&pair_button);
        } else {
            let device_switch = gtk::Switch::new();
            device_switch.add_css_class("network-wifi-switch");
            device_switch.set_valign(gtk::Align::Center);
            device_switch.set_halign(gtk::Align::End);
            device_switch.set_sensitive(!busy);
            device_switch.set_active(pending.map_or(device.connected, |pending| {
                pending.action.target_connected(device.connected)
            }));

            let weak = Rc::downgrade(self);
            let device_path = device.path.clone();
            device_switch.connect_active_notify(move |switch| {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                if this.action_busy.get() {
                    return;
                }
                let target = switch.is_active();
                this.set_device_connected(&device_path, target);
            });
            actions.append(&device_switch);
        }

        let remove_icon = gtk::Label::new(Some(ICON_REMOVE));
        remove_icon.add_css_class("network-action-icon");
        let remove_button = gtk::Button::new();
        remove_button.add_css_class("network-icon-button");
        remove_button.set_valign(gtk::Align::Center);
        remove_button.set_sensitive(!busy);
        remove_button.set_child(Some(&remove_icon));

        let weak = Rc::downgrade(self);
        let device_path = device.path.clone();
        remove_button.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.remove_device(&device_path);
            }
        });
        actions.append(&remove_button);

        row.append(&actions);
        row
    }

    fn handle_agent_event(self: &Rc<Self>, event: AgentEvent) {
        if self.pairing_cancelled.get() {
            if matches!(event, AgentEvent::Prompt { .. }) {
                self.agent.reject_request();
            }
            return;
        }

        match event {
            AgentEvent::Prompt { device_path, kind } => {
                let name = self.device_name_for_path(&device_path);
                self.pairing_device_path.replace(Some(device_path));
                self.pairing_title.set_text(&format!("Pair with {name}"));
                self.pairing_code.set_text("");
                self.pairing_code.set_visible(false);
                self.pairing_entry.set_text("");

                match kind {
                    AgentPromptKind::PinCode => {
                        self.pairing_mode.set(PairingPromptMode::Input);
                        self.pairing_message
                            .set_text("Enter the PIN requested by the Bluetooth device.");
                        self.pairing_entry.set_placeholder_text(Some("PIN"));
                        self.pairing_entry.set_visible(true);
                        self.pairing_confirm_button.set_label("Pair");
                        self.pairing_confirm_button.set_visible(true);
                    }
                    AgentPromptKind::Passkey => {
                        self.pairing_mode.set(PairingPromptMode::Input);
                        self.pairing_message
                            .set_text("Enter the passkey shown on the Bluetooth device.");
                        self.pairing_entry
                            .set_placeholder_text(Some("6-digit passkey"));
                        self.pairing_entry.set_visible(true);
                        self.pairing_confirm_button.set_label("Pair");
                        self.pairing_confirm_button.set_visible(true);
                    }
                    AgentPromptKind::Confirmation { passkey } => {
                        self.pairing_mode.set(PairingPromptMode::Confirmation);
                        self.pairing_message
                            .set_text("Confirm that this code matches on both devices.");
                        self.pairing_code.set_text(&format!("{passkey:06}"));
                        self.pairing_code.set_visible(true);
                        self.pairing_entry.set_visible(false);
                        self.pairing_confirm_button.set_label("Confirm");
                        self.pairing_confirm_button.set_visible(true);
                    }
                    AgentPromptKind::Authorization => {
                        self.pairing_mode.set(PairingPromptMode::Confirmation);
                        self.pairing_message
                            .set_text("Allow this Bluetooth pairing request?");
                        self.pairing_entry.set_visible(false);
                        self.pairing_confirm_button.set_label("Allow");
                        self.pairing_confirm_button.set_visible(true);
                    }
                }

                self.show_pairing_prompt();
                if self.pairing_mode.get() == PairingPromptMode::Input {
                    self.pairing_entry.grab_focus();
                }
            }
            AgentEvent::DisplayPinCode {
                device_path,
                pincode,
            } => {
                let name = self.device_name_for_path(&device_path);
                self.pairing_device_path.replace(Some(device_path));
                self.pairing_mode.set(PairingPromptMode::Display);
                self.pairing_title.set_text(&format!("Pair with {name}"));
                self.pairing_message
                    .set_text("Type this code on the Bluetooth device, then press Enter.");
                self.pairing_code.set_text(&pincode);
                self.pairing_code.set_visible(true);
                self.pairing_entry.set_visible(false);
                self.pairing_confirm_button.set_visible(false);
                self.show_pairing_prompt();
            }
            AgentEvent::DisplayPasskey {
                device_path,
                passkey,
                entered,
            } => {
                let name = self.device_name_for_path(&device_path);
                self.pairing_device_path.replace(Some(device_path));
                self.pairing_mode.set(PairingPromptMode::Display);
                self.pairing_title.set_text(&format!("Pair with {name}"));
                self.pairing_message.set_text(&format!(
                    "Type this code on the Bluetooth device, then press Enter. {}/6 digits entered.",
                    entered.min(6)
                ));
                self.pairing_code.set_text(&format!("{passkey:06}"));
                self.pairing_code.set_visible(true);
                self.pairing_entry.set_visible(false);
                self.pairing_confirm_button.set_visible(false);
                self.show_pairing_prompt();
            }
            AgentEvent::Cancel | AgentEvent::Release => self.hide_pairing_prompt(),
        }
    }

    fn device_name_for_path(&self, device_path: &str) -> String {
        self.snapshot
            .borrow()
            .devices
            .iter()
            .find(|device| device.path == device_path)
            .map(|device| device.name.clone())
            .unwrap_or_else(|| "Bluetooth device".to_owned())
    }

    fn show_pairing_prompt(self: &Rc<Self>) {
        self.pairing_box.set_visible(true);
        self.set_notice(None);
        if !self.popup.is_visible() {
            self.open_popup();
        }
    }

    fn hide_pairing_prompt(&self) {
        self.pairing_mode.set(PairingPromptMode::Hidden);
        self.pairing_device_path.replace(None);
        self.pairing_title.set_text("Pair Bluetooth device");
        self.pairing_message.set_text("");
        self.pairing_code.set_text("");
        self.pairing_code.set_visible(false);
        self.pairing_entry.set_text("");
        self.pairing_entry.set_visible(false);
        self.pairing_confirm_button.set_visible(false);
        self.pairing_box.set_visible(false);
    }

    fn submit_pairing_prompt(self: &Rc<Self>) {
        let result = match self.pairing_mode.get() {
            PairingPromptMode::Input => {
                let value = self.pairing_entry.text().to_string();
                self.agent.submit_input(&value)
            }
            PairingPromptMode::Confirmation => self.agent.confirm_request(),
            PairingPromptMode::Hidden | PairingPromptMode::Display => return,
        };

        match result {
            Ok(()) => self.hide_pairing_prompt(),
            Err(error) => {
                self.set_notice(Some(&error));
                if self.pairing_mode.get() == PairingPromptMode::Input {
                    self.pairing_entry.grab_focus();
                }
            }
        }
    }

    fn cancel_pairing_prompt(self: &Rc<Self>) {
        let prompt_device_path = self.pairing_device_path.borrow_mut().take();
        let pending_pairing_path = self
            .pending_device
            .borrow()
            .as_ref()
            .filter(|pending| pending.action == PendingDeviceAction::Pairing)
            .map(|pending| pending.path.clone());
        let device_path = prompt_device_path.or(pending_pairing_path);
        let cancels_own_pairing = device_path.as_ref().is_some_and(|device_path| {
            self.pending_device
                .borrow()
                .as_ref()
                .is_some_and(|pending| {
                    pending.action == PendingDeviceAction::Pairing
                        && pending.path.as_str() == device_path.as_str()
                })
        });
        if cancels_own_pairing {
            self.pairing_cancelled.set(true);
        }

        self.agent.reject_request();
        self.hide_pairing_prompt();

        let Some(device_path) = device_path else {
            return;
        };
        let backend = self.backend;
        run_background(move || backend.cancel_pairing(&device_path), |_| {});
    }

    fn toggle_popup(self: &Rc<Self>) {
        if self.popup_reveal.is_revealed() {
            self.close_popup();
        } else {
            self.open_popup();
        }
    }

    fn open_popup(self: &Rc<Self>) {
        if !self.snapshot.borrow().available() {
            return;
        }
        self.focus_armed.set(false);
        self.set_notice(None);
        let generation = self.popup_reveal.show(&self.popup);
        self.trigger.add_css_class("bluetooth-popup-open");
        if self.list_dirty.get() || !self.list_initialized.get() {
            self.sync_device_list();
        }
        self.refresh_snapshot();
        if self.display_scanning() {
            self.schedule_discovery_timeout();
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
        let pairing_in_progress = self.pairing_box.is_visible()
            || self
                .pending_device
                .borrow()
                .as_ref()
                .is_some_and(|pending| pending.action == PendingDeviceAction::Pairing);
        if pairing_in_progress {
            self.cancel_pairing_prompt();
        }
        self.focus_armed.set(false);
        self.trigger.remove_css_class("bluetooth-popup-open");
        self.popup_reveal.hide(&self.popup);
    }

    fn set_powered(self: &Rc<Self>, powered: bool) {
        if self.action_busy.replace(true) {
            self.update_header();
            return;
        }
        let Some((adapter_path, was_discovering)) = self
            .snapshot
            .borrow()
            .adapter
            .as_ref()
            .map(|adapter| (adapter.path.clone(), adapter.discovering))
        else {
            self.action_busy.set(false);
            self.update_header();
            return;
        };

        self.snapshot_generation.bump();
        self.pending_power.set(Some(powered));
        if !powered {
            self.discovery_timeout_generation.bump();
            self.pending_scan.set(Some(false));
        }
        self.update_header();
        self.refresh_device_list();

        let backend = self.backend;
        let weak = Rc::downgrade(self);
        run_background(
            move || {
                if !powered && was_discovering {
                    let _ = backend.stop_discovery(&adapter_path);
                }
                backend.set_powered(&adapter_path, powered)
            },
            move |result| {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                this.action_busy.set(false);
                if let Err(error) = result {
                    warn!(%error, "Bluetooth power action failed");
                    this.pending_power.set(None);
                    this.pending_scan.set(None);
                    this.set_notice(Some(&error));
                }
                this.finish_action_refresh();
            },
        );
    }

    fn set_scanning(self: &Rc<Self>, scanning: bool) {
        if self.action_busy.replace(true) {
            return;
        }
        let Some((adapter_path, display_powered)) = ({
            let snapshot = self.snapshot.borrow();
            snapshot.adapter.as_ref().map(|adapter| {
                (
                    adapter.path.clone(),
                    self.pending_power.get().unwrap_or(adapter.powered),
                )
            })
        }) else {
            self.action_busy.set(false);
            return;
        };
        if !display_powered {
            self.action_busy.set(false);
            return;
        }

        self.snapshot_generation.bump();
        self.pending_scan.set(Some(scanning));
        if !scanning {
            self.discovery_timeout_generation.bump();
        }
        self.update_header();
        self.refresh_device_list();

        let backend = self.backend;
        let weak = Rc::downgrade(self);
        run_background(
            move || {
                if scanning {
                    backend.start_discovery(&adapter_path)
                } else {
                    backend.stop_discovery(&adapter_path)
                }
            },
            move |result| {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                this.action_busy.set(false);
                match result {
                    Ok(()) => {
                        this.set_notice(None);
                        if scanning {
                            this.schedule_discovery_timeout();
                        }
                    }
                    Err(error) => {
                        warn!(%error, "Bluetooth discovery action failed");
                        this.pending_scan.set(None);
                        this.set_notice(Some(&error));
                    }
                }
                this.finish_action_refresh();
            },
        );
    }

    fn set_device_connected(self: &Rc<Self>, device_path: &str, connected: bool) {
        let Some((device_path, device_name, paired, trusted, was_connected)) = ({
            let snapshot = self.snapshot.borrow();
            snapshot
                .devices
                .iter()
                .find(|device| device.path == device_path)
                .map(|device| {
                    (
                        device.path.clone(),
                        device.name.clone(),
                        device.paired,
                        device.trusted,
                        device.connected,
                    )
                })
        }) else {
            return;
        };

        if self.action_busy.get() || was_connected == connected {
            return;
        }

        let pairing = connected && !paired;
        let agent_session = if pairing {
            self.pairing_cancelled.set(false);
            let weak = Rc::downgrade(self);
            let handler = Rc::new(move |event| {
                if let Some(controller) = weak.upgrade() {
                    controller.handle_agent_event(event);
                }
            });
            let Some(session) = self.agent.begin_session(&device_path, handler) else {
                self.set_notice(Some("Another Bluetooth pairing is already in progress"));
                return;
            };
            if let Err(error) = self.agent.ensure_registered() {
                drop(session);
                warn!(%error, "failed to prepare Bluetooth pairing agent");
                self.set_notice(Some(&error));
                return;
            }
            Some(session)
        } else {
            None
        };

        let adapter = self
            .snapshot
            .borrow()
            .adapter
            .as_ref()
            .map(|adapter| (adapter.path.clone(), adapter.discovering));
        self.action_busy.set(true);
        self.snapshot_generation.bump();
        if connected {
            self.discovery_timeout_generation.bump();
            self.pending_scan.set(Some(false));
        }
        self.pending_device.replace(Some(PendingDevice {
            path: device_path.clone(),
            action: if pairing {
                PendingDeviceAction::Pairing
            } else if connected {
                PendingDeviceAction::Connecting
            } else {
                PendingDeviceAction::Disconnecting
            },
        }));
        self.update_header();
        self.refresh_device_list();

        let backend = self.backend;
        let weak = Rc::downgrade(self);
        run_background(
            move || {
                if connected && let Some((adapter_path, true)) = adapter.as_ref() {
                    let _ = backend.stop_discovery(adapter_path);
                }
                backend.set_connected(&device_path, &device_name, paired, trusted, connected)
            },
            move |result| {
                let _agent_session = agent_session;
                let Some(this) = weak.upgrade() else {
                    return;
                };
                this.action_busy.set(false);
                this.pending_device.replace(None);
                this.pending_scan.set(None);
                this.hide_pairing_prompt();
                let pairing_cancelled = this.pairing_cancelled.replace(false);
                if pairing_cancelled {
                    this.set_notice(None);
                } else {
                    match result {
                        Ok(()) => this.set_notice(None),
                        Err(error) => {
                            warn!(%error, "Bluetooth device action failed");
                            this.set_notice(Some(&error));
                        }
                    }
                }
                this.finish_action_refresh();
            },
        );
    }

    fn remove_device(self: &Rc<Self>, device_path: &str) {
        if self.action_busy.replace(true) {
            return;
        }
        let Some((device_path, device_name, adapter_path)) = ({
            let snapshot = self.snapshot.borrow();
            let device = snapshot
                .devices
                .iter()
                .find(|device| device.path == device_path);
            device
                .zip(snapshot.adapter.as_ref())
                .map(|(device, adapter)| {
                    (
                        device.path.clone(),
                        device.name.clone(),
                        adapter.path.clone(),
                    )
                })
        }) else {
            self.action_busy.set(false);
            return;
        };

        self.snapshot_generation.bump();
        self.pending_device.replace(Some(PendingDevice {
            path: device_path.clone(),
            action: PendingDeviceAction::Removing,
        }));
        self.update_header();
        self.refresh_device_list();

        let backend = self.backend;
        let weak = Rc::downgrade(self);
        run_background(
            move || backend.remove_device(&adapter_path, &device_path),
            move |result| {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                this.action_busy.set(false);
                this.pending_device.replace(None);
                match result {
                    Ok(()) => this.set_notice(None),
                    Err(error) => {
                        warn!(device = %device_name, %error, "failed to remove Bluetooth device");
                        this.set_notice(Some(&error));
                    }
                }
                this.finish_action_refresh();
            },
        );
    }

    fn finish_action_refresh(self: &Rc<Self>) {
        self.update_header();
        self.refresh_device_list();
        self.refresh_snapshot();
    }

    fn schedule_discovery_timeout(self: &Rc<Self>) {
        let token = self.discovery_timeout_generation.bump();
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(DISCOVERY_TIMEOUT, move || {
            let Some(this) = weak.upgrade() else {
                return;
            };
            if !this.discovery_timeout_generation.is_current(token) || !this.display_scanning() {
                return;
            }
            this.set_scanning(false);
        });
    }

    fn set_scan_animating(&self, active: bool) {
        set_spinner_active(&self.scan_icon, &self.scan_spinner, active);
    }

    fn set_notice(self: &Rc<Self>, text: Option<&str>) {
        let generation = self.notice_generation.bump();
        match text {
            Some(text) if !text.trim().is_empty() => {
                set_optional_label(&self.notice, Some(text));

                let weak = Rc::downgrade(self);
                glib::timeout_add_local_once(NOTICE_TIMEOUT, move || {
                    let Some(this) = weak.upgrade() else {
                        return;
                    };
                    if this.notice_generation.is_current(generation) {
                        set_optional_label(&this.notice, None);
                    }
                });
            }
            _ => {
                set_optional_label(&self.notice, None);
            }
        }
    }
}

fn trigger_glyph(powered: bool, connected: bool) -> &'static str {
    if !powered {
        ICON_BLUETOOTH_OFF
    } else if connected {
        ICON_BLUETOOTH_CONNECTED
    } else {
        ICON_BLUETOOTH_ON
    }
}

fn device_glyph(icon: &str, name: &str) -> &'static str {
    let value = format!("{icon} {name}").to_lowercase();
    if value.contains("headset") || value.contains("headphone") || value.contains("earbud") {
        "󰋋"
    } else if value.contains("speaker") || value.contains("audio-card") {
        "󰓃"
    } else if value.contains("phone") {
        "󰄜"
    } else if value.contains("keyboard") {
        "󰌌"
    } else if value.contains("mouse") {
        "󰍽"
    } else if value.contains("gamepad")
        || value.contains("joystick")
        || value.contains("input-gaming")
    {
        "󰮂"
    } else if value.contains("watch") {
        "󰢗"
    } else if value.contains("computer") || value.contains("laptop") {
        "󰌢"
    } else if value.contains("printer") {
        "󰐪"
    } else {
        ICON_BLUETOOTH_ON
    }
}

fn device_meta(device: &BluetoothDevice, pending: Option<&PendingDevice>) -> String {
    if let Some(pending) = pending {
        return pending.action.status().to_owned();
    }

    let state = if device.connected {
        "Connected"
    } else if device.known() {
        "Disconnected"
    } else {
        "Available"
    };

    device.battery.map_or_else(
        || state.to_owned(),
        |battery| format!("{state} • {battery}%"),
    )
}

#[cfg(test)]
mod tests {
    use super::{ICON_BLUETOOTH_CONNECTED, ICON_BLUETOOTH_OFF, device_glyph, trigger_glyph};

    #[test]
    fn bluetooth_trigger_state_icons_are_stable() {
        assert_eq!(trigger_glyph(false, false), ICON_BLUETOOTH_OFF);
        assert_eq!(trigger_glyph(true, true), ICON_BLUETOOTH_CONNECTED);
    }

    #[test]
    fn device_icon_heuristics_prefer_device_kind() {
        assert_eq!(device_glyph("audio-headset", "Headphones"), "󰋋");
        assert_eq!(device_glyph("input-mouse", "Mouse"), "󰍽");
    }
}
