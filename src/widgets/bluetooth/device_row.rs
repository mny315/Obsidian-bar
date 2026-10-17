use super::{
    BluetoothController, BluetoothDevice, ICON_REMOVE, PendingDevice, PendingDeviceAction,
    device_glyph, device_meta,
};
use crate::widgets::clear_box;
use gtk::prelude::*;
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    rc::{Rc, Weak},
};

#[derive(Default)]
pub(super) struct DeviceRows(RefCell<HashMap<String, DeviceRow>>);

impl DeviceRows {
    pub(super) fn clear(&self) {
        self.0.borrow_mut().clear();
    }

    pub(super) fn sync(
        &self,
        list: &gtk::Box,
        devices: &[BluetoothDevice],
        pending: Option<&PendingDevice>,
        busy: bool,
        owner: Weak<BluetoothController>,
    ) {
        let mut rows = self.0.borrow_mut();
        if rows.is_empty() {
            clear_box(list);
        }
        let paths = devices
            .iter()
            .map(|device| device.path.as_str())
            .collect::<HashSet<_>>();
        rows.retain(|path, row| {
            let keep = paths.contains(path.as_str());
            if !keep {
                list.remove(&row.root);
            }
            keep
        });
        let mut previous: Option<gtk::Widget> = None;
        for device in devices {
            let row = rows
                .entry(device.path.clone())
                .or_insert_with(|| DeviceRow::new(owner.clone(), &device.path));
            row.update(
                device,
                pending.filter(|pending| pending.path == device.path),
                busy,
            );
            if row.root.parent().is_none() {
                list.insert_child_after(&row.root, previous.as_ref());
            } else if row.root.prev_sibling() != previous {
                list.reorder_child_after(&row.root, previous.as_ref());
            }
            previous = Some(row.root.clone().upcast());
        }
    }
}

struct DeviceRow {
    root: gtk::Box,
    icon: gtk::Label,
    title: gtk::Label,
    meta: gtk::Label,
    pair_button: gtk::Button,
    pair_label: gtk::Label,
    switch: gtk::Switch,
    remove_button: gtk::Button,
    syncing: Rc<Cell<bool>>,
}

impl DeviceRow {
    fn new(owner: Weak<BluetoothController>, path: &str) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        root.add_css_class("network-row-container");
        root.set_valign(gtk::Align::Center);

        let icon = gtk::Label::new(None);
        icon.add_css_class("network-row-icon");
        icon.set_valign(gtk::Align::Center);
        root.append(&icon);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 2);
        content.set_hexpand(true);
        content.set_valign(gtk::Align::Center);
        let title = gtk::Label::new(None);
        title.add_css_class("network-row-title");
        title.set_xalign(0.0);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        title.set_max_width_chars(26);
        let meta = gtk::Label::new(None);
        meta.add_css_class("network-row-meta");
        meta.set_xalign(0.0);
        content.append(&title);
        content.append(&meta);
        root.append(&content);

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        actions.set_valign(gtk::Align::Center);
        let pair_label = gtk::Label::new(Some("Pair"));
        pair_label.add_css_class("bluetooth-pair-button-label");
        let pair_button = gtk::Button::new();
        pair_button.add_css_class("network-row-button");
        pair_button.add_css_class("bluetooth-pair-button");
        pair_button.set_focusable(false);
        pair_button.set_child(Some(&pair_label));
        let weak = owner.clone();
        let device_path = path.to_owned();
        pair_button.connect_clicked(move |_| {
            if let Some(controller) = weak.upgrade() {
                controller.set_device_connected(&device_path, true);
            }
        });
        actions.append(&pair_button);

        let switch = gtk::Switch::new();
        switch.add_css_class("network-wifi-switch");
        switch.set_valign(gtk::Align::Center);
        switch.set_halign(gtk::Align::End);
        let syncing = Rc::new(Cell::new(false));
        let syncing_switch = Rc::clone(&syncing);
        let weak = owner.clone();
        let device_path = path.to_owned();
        switch.connect_active_notify(move |switch| {
            if syncing_switch.get() {
                return;
            }
            if let Some(controller) = weak.upgrade()
                && !controller.action_busy.get()
            {
                controller.set_device_connected(&device_path, switch.is_active());
            }
        });
        actions.append(&switch);

        let remove_icon = gtk::Label::new(Some(ICON_REMOVE));
        remove_icon.add_css_class("network-action-icon");
        let remove_button = gtk::Button::new();
        remove_button.add_css_class("network-icon-button");
        remove_button.set_valign(gtk::Align::Center);
        remove_button.set_child(Some(&remove_icon));
        let device_path = path.to_owned();
        remove_button.connect_clicked(move |_| {
            if let Some(controller) = owner.upgrade() {
                controller.remove_device(&device_path);
            }
        });
        actions.append(&remove_button);
        root.append(&actions);

        Self {
            root,
            icon,
            title,
            meta,
            pair_button,
            pair_label,
            switch,
            remove_button,
            syncing,
        }
    }

    fn update(&self, device: &BluetoothDevice, pending: Option<&PendingDevice>, busy: bool) {
        self.icon
            .set_label(device_glyph(&device.icon, &device.name));
        self.title.set_label(&device.name);
        self.meta.set_label(&device_meta(device, pending));
        let pair = !device.connected && !device.paired;
        self.pair_button.set_visible(pair);
        self.switch.set_visible(!pair);
        self.pair_button.set_sensitive(!busy);
        self.switch.set_sensitive(!busy);
        self.remove_button.set_sensitive(!busy);
        self.pair_label.set_label(
            if pending.is_some_and(|pending| pending.action == PendingDeviceAction::Pairing) {
                "Pairing…"
            } else {
                "Pair"
            },
        );
        self.syncing.set(true);
        self.switch
            .set_active(pending.map_or(device.connected, |pending| {
                pending.action.target_connected(device.connected)
            }));
        self.syncing.set(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a GTK display; uses unmapped widgets and does not access Bluetooth"]
    fn updates_preserve_rows_and_follow_device_order_and_pending_actions() {
        gtk::init().unwrap();
        let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
        list.append(&gtk::Label::new(Some("No devices found")));
        let rows = DeviceRows::default();
        let device = |path: &str| BluetoothDevice {
            path: path.into(),
            name: path.into(),
            icon: "input-mouse".into(),
            paired: false,
            trusted: false,
            connected: false,
            battery: None,
        };
        let mut devices = vec![device("first"), device("second")];
        rows.sync(&list, &devices, None, false, Weak::new());
        let first = list.first_child().unwrap();
        let second = first.next_sibling().unwrap();
        assert!(second.next_sibling().is_none());
        devices[0].battery = Some(73);
        devices[0].paired = true;
        devices[0].connected = true;
        devices.swap(0, 1);
        let pending = PendingDevice {
            path: "second".into(),
            action: PendingDeviceAction::Pairing,
        };
        rows.sync(&list, &devices, Some(&pending), true, Weak::new());
        assert_eq!(list.first_child().unwrap(), second);
        assert_eq!(second.next_sibling().unwrap(), first);
        {
            let entries = rows.0.borrow();
            let paired = &entries["first"];
            assert!(!paired.pair_button.is_visible());
            assert!(paired.switch.is_visible());
            assert!(paired.switch.is_active());
            assert!(!paired.switch.is_sensitive());
            assert_eq!(paired.meta.text(), "Connected • 73%");
            let pairing = &entries["second"];
            assert!(pairing.pair_button.is_visible());
            assert!(!pairing.switch.is_visible());
            assert_eq!(pairing.pair_label.text(), "Pairing…");
        }
        devices.remove(0);
        rows.sync(&list, &devices, None, false, Weak::new());
        assert!(second.parent().is_none());
        assert_eq!(list.first_child().unwrap(), first);
        assert!(rows.0.borrow()["first"].switch.is_sensitive());
        devices.insert(0, device("third"));
        rows.sync(&list, &devices, None, false, Weak::new());
        assert_eq!(list.first_child().unwrap().next_sibling().unwrap(), first);
        rows.clear();
        clear_box(&list);
        list.append(&gtk::Label::new(Some("Bluetooth off")));
        rows.sync(&list, &devices, None, false, Weak::new());
        assert_eq!(
            list.first_child().unwrap().next_sibling().unwrap(),
            list.last_child().unwrap()
        );
        assert_eq!(rows.0.borrow().len(), 2);
    }
}
