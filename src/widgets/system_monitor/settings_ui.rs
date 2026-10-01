use super::drawer::monitor_output_id;
use super::gpu;
use super::metrics::{MetricOption, metric_options};
use super::{
    HIDE_DELAY_MAX_MS, ICON_MONITOR, MonitorSection, MonitorSettings, PANEL_MAX_WIDTH,
    PANEL_MIN_HEIGHT, PANEL_MIN_WIDTH, SETTINGS_PANEL_PADDING, SETTINGS_PANEL_WIDTH,
    SystemMonitorController, SystemSnapshot,
};
use crate::widgets::clear_box;
use gtk::{gdk, glib, prelude::*};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

const ICON_UP: &str = "\u{f005d}";
const ICON_DOWN: &str = "\u{f0045}";
const ICON_VISIBLE: &str = "\u{f0208}";
const ICON_HIDDEN: &str = "\u{f0209}";
const ICON_EXPAND: &str = "\u{f0142}";
const ICON_COLLAPSE: &str = "\u{f0140}";

fn monitor_dimension_slider(
    title: &str,
    min: f64,
    max: f64,
    step: f64,
    value: f64,
    digits: i32,
    unit: &'static str,
) -> (gtk::Box, gtk::Scale) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.add_css_class("system-monitor-settings-slider-row");
    let label = gtk::Label::new(Some(title));
    label.add_css_class("system-monitor-settings-section-title");
    label.set_xalign(0.0);
    label.set_size_request(88, -1);
    let slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, min, max, step);
    slider.set_hexpand(true);
    slider.add_css_class("slider-control");
    slider.add_css_class("system-monitor-settings-scale");
    slider.set_value(value);
    slider.set_digits(digits);
    slider.set_draw_value(true);
    slider.set_value_pos(gtk::PositionType::Right);
    slider.set_format_value_func(move |_, value| {
        format!("{value:.precision$} {unit}", precision = digits as usize)
    });
    // Let wheel and touchpad scrolling reach the settings scroller. Keep the
    // range's pointer, touch and keyboard controllers intact.
    let controllers = slider.observe_controllers();
    for index in 0..controllers.n_items() {
        if let Some(scroll) = controllers
            .item(index)
            .and_then(|item| item.downcast::<gtk::EventControllerScroll>().ok())
        {
            scroll.set_propagation_phase(gtk::PropagationPhase::None);
        }
    }
    row.append(&label);
    row.append(&slider);
    (row, slider)
}

fn settings_control_row(title: &str, control: &impl IsA<gtk::Widget>) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Vertical, 5);
    let label = gtk::Label::new(Some(title));
    label.set_xalign(0.0);
    label.add_css_class("system-monitor-settings-section-title");
    row.append(&label);
    row.append(control);
    row
}

fn settings_dropdown() -> gtk::DropDown {
    let dropdown = gtk::DropDown::from_strings(&["Automatic"]);
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let label = gtk::Label::new(None);
        label.set_xalign(0.0);
        label.set_width_chars(1);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        item.set_child(Some(&label));
    });
    factory.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let label = item.child().unwrap().downcast::<gtk::Label>().unwrap();
        if let Some(value) = item
            .item()
            .and_then(|item| item.downcast::<gtk::StringObject>().ok())
        {
            label.set_text(&value.string());
        }
    });
    dropdown.set_factory(Some(&factory));
    dropdown.add_css_class("system-monitor-settings-dropdown");
    dropdown
}

fn constrain_metric_name(widget: &impl IsA<gtk::Widget>) {
    let widget = widget.as_ref();
    if let Some(label) = widget.downcast_ref::<gtk::Label>() {
        label.set_width_chars(1);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    }
    if let Some(text) = widget.downcast_ref::<gtk::Text>() {
        text.set_width_chars(1);
    }
    let mut child = widget.first_child();
    while let Some(widget) = child {
        constrain_metric_name(&widget);
        child = widget.next_sibling();
    }
}

pub(super) fn settings_popover(widget: &impl IsA<gtk::Widget>) -> Option<gtk::Popover> {
    let widget = widget.as_ref();
    if let Some(popover) = widget.downcast_ref::<gtk::Popover>() {
        return Some(popover.clone());
    }
    let mut child = widget.first_child();
    while let Some(widget) = child {
        if let Some(popover) = settings_popover(&widget) {
            return Some(popover);
        }
        child = widget.next_sibling();
    }
    None
}

pub(super) struct MonitorSettingsPanel {
    pub(super) root: gtk::Box,
    pub(super) list: gtk::Box,
    pub(super) pin: gtk::ToggleButton,
    pub(super) syncing_pin: Cell<bool>,
    pub(super) scale: gtk::Scale,
    pub(super) width: gtk::Scale,
    pub(super) height: gtk::Scale,
    pub(super) hide_delay: gtk::Scale,
    pub(super) interval: gtk::Scale,
    pub(super) economy: gtk::ToggleButton,
    pub(super) output: gtk::DropDown,
    pub(super) network: gtk::DropDown,
    pub(super) popovers: Vec<gtk::Popover>,
    pub(super) output_choices: RefCell<Vec<(String, String)>>,
    pub(super) network_choices: RefCell<Vec<(String, String)>>,
    pub(super) editing_changed: RefCell<Option<Box<dyn Fn()>>>,
    pub(super) syncing_scale: Cell<bool>,
    pub(super) rebuild_pending: Cell<bool>,
    pub(super) syncing_selection: Cell<bool>,
    pub(super) groups: RefCell<HashMap<MonitorSection, SettingsGroupView>>,
    pub(super) order: RefCell<Vec<MonitorSection>>,
    pub(super) controller: Rc<SystemMonitorController>,
}

impl MonitorSettingsPanel {
    pub(super) fn new(controller: &Rc<SystemMonitorController>) -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
        root.add_css_class("system-monitor-settings-panel");
        root.set_size_request(SETTINGS_PANEL_WIDTH - SETTINGS_PANEL_PADDING * 2, -1);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 9);
        header.add_css_class("system-monitor-settings-header");
        let icon = gtk::Label::new(Some(ICON_MONITOR));
        icon.add_css_class("system-monitor-settings-icon");
        let title = gtk::Label::new(Some("System monitor"));
        title.add_css_class("system-monitor-settings-title");
        title.set_xalign(0.0);
        title.set_hexpand(true);
        header.append(&icon);
        header.append(&title);

        let pin = gtk::ToggleButton::new();
        let pin_icon = gtk::Label::new(Some("\u{f0403}"));
        pin_icon.add_css_class("system-monitor-settings-button-icon");
        pin.set_child(Some(&pin_icon));
        pin.add_css_class("system-monitor-settings-pin");
        pin.update_property(&[gtk::accessible::Property::Label("Pin panel")]);
        pin.set_active(controller.settings().pinned);
        let economy = gtk::ToggleButton::new();
        let economy_icon = gtk::Label::new(Some("\u{f032a}"));
        economy_icon.add_css_class("system-monitor-settings-button-icon");
        economy.set_child(Some(&economy_icon));
        economy.add_css_class("system-monitor-settings-pin");
        economy.update_property(&[gtk::accessible::Property::Label(
            "Reduce updates when hidden",
        )]);
        economy.set_active(controller.settings().economy);
        header.append(&economy);
        header.append(&pin);

        let settings = controller.settings();
        let (width_row, width) = monitor_dimension_slider(
            "Width",
            f64::from(PANEL_MIN_WIDTH),
            f64::from(PANEL_MAX_WIDTH),
            1.0,
            f64::from(settings.width),
            0,
            "px",
        );
        let (height_row, height) = monitor_dimension_slider(
            "Height",
            f64::from(PANEL_MIN_HEIGHT),
            2160.0,
            1.0,
            f64::from(settings.height.unwrap_or(720)),
            0,
            "px",
        );
        let (scale_row, scale) = monitor_dimension_slider(
            "Font size",
            8.0,
            24.0,
            0.5,
            11.0 * f64::from(settings.scale_milli) / 1000.0,
            1,
            "px",
        );
        let (delay_row, hide_delay) = monitor_dimension_slider(
            "Hide delay",
            0.0,
            f64::from(HIDE_DELAY_MAX_MS) / 1000.0,
            0.5,
            f64::from(settings.hide_delay_ms) / 1000.0,
            1,
            "s",
        );

        let section_title = gtk::Label::new(Some("Groups and metrics"));
        section_title.add_css_class("system-monitor-settings-section-title");
        section_title.set_xalign(0.0);

        let list = gtk::Box::new(gtk::Orientation::Vertical, 4);
        list.add_css_class("system-monitor-settings-list");

        root.append(&header);
        let (interval_row, interval) = monitor_dimension_slider(
            "Update interval",
            0.5,
            10.0,
            0.5,
            f64::from(settings.interval_ms) / 1000.0,
            1,
            "s",
        );
        let output = settings_dropdown();
        let network = settings_dropdown();
        let popovers = [&output, &network]
            .into_iter()
            .filter_map(settings_popover)
            .collect();
        let adjustments = gtk::Expander::new(Some("Size and timing"));
        adjustments.add_css_class("system-monitor-settings-adjustments");
        let sliders = gtk::Box::new(gtk::Orientation::Vertical, 2);
        sliders.set_margin_top(6);
        for row in [
            &width_row,
            &height_row,
            &scale_row,
            &delay_row,
            &interval_row,
        ] {
            sliders.append(row);
        }
        adjustments.set_child(Some(&sliders));
        root.append(&adjustments);
        root.append(&settings_control_row("Display", &output));
        root.append(&settings_control_row("Network interface", &network));
        root.append(&section_title);
        root.append(&list);

        let panel = Rc::new(Self {
            root,
            list,
            pin,
            syncing_pin: Cell::new(false),
            scale,
            width,
            height,
            hide_delay,
            interval,
            economy,
            output,
            network,
            popovers,
            output_choices: RefCell::new(Vec::new()),
            network_choices: RefCell::new(Vec::new()),
            editing_changed: RefCell::new(None),
            syncing_scale: Cell::new(false),
            rebuild_pending: Cell::new(false),
            syncing_selection: Cell::new(false),
            groups: RefCell::new(HashMap::new()),
            order: RefCell::new(Vec::new()),
            controller: Rc::clone(controller),
        });

        {
            let weak = Rc::downgrade(&panel);
            panel.pin.connect_toggled(move |pin| {
                let Some(panel) = weak.upgrade() else {
                    return;
                };
                if panel.syncing_pin.get() {
                    return;
                }
                if !panel.controller.set_pinned(pin.is_active()) {
                    panel.sync_pin(panel.controller.settings().pinned);
                }
            });
        }

        for slider in [&panel.width, &panel.height, &panel.scale] {
            let weak = Rc::downgrade(&panel);
            slider.connect_value_changed(move |_| {
                let Some(panel) = weak.upgrade() else {
                    return;
                };
                if panel.syncing_scale.get() {
                    return;
                }
                if !panel.controller.set_dimensions(
                    panel.width.value().round() as i32,
                    panel.height.value().round() as i32,
                    (panel.scale.value() * 1000.0 / 11.0).round() as i32,
                ) {
                    panel.sync_dimensions(&panel.controller.settings());
                }
            });
        }

        {
            let weak = Rc::downgrade(&panel);
            panel.hide_delay.connect_value_changed(move |slider| {
                if let Some(panel) = weak.upgrade()
                    && !panel.syncing_scale.get()
                    && !panel
                        .controller
                        .set_hide_delay((slider.value() * 1000.0).round() as i32)
                {
                    panel.sync_dimensions(&panel.controller.settings());
                }
            });
        }

        {
            let weak = Rc::downgrade(&panel);
            panel.interval.connect_value_changed(move |slider| {
                if let Some(panel) = weak.upgrade()
                    && !panel.syncing_scale.get()
                    && !panel.controller.update_settings(|settings| {
                        settings.interval_ms = (slider.value() * 1000.0).round() as i32
                    })
                {
                    panel.sync_dimensions(&panel.controller.settings());
                }
            });
        }
        {
            let weak = Rc::downgrade(&panel);
            panel.economy.connect_toggled(move |button| {
                if let Some(panel) = weak.upgrade()
                    && !panel.syncing_scale.get()
                    && !panel
                        .controller
                        .update_settings(|settings| settings.economy = button.is_active())
                {
                    panel.sync_dimensions(&panel.controller.settings());
                }
            });
        }
        {
            let weak = Rc::downgrade(&panel);
            panel.output.connect_selected_notify(move |dropdown| {
                if let Some(panel) = weak.upgrade()
                    && !panel.syncing_scale.get()
                {
                    let output = panel
                        .output_choices
                        .borrow()
                        .get(dropdown.selected() as usize)
                        .map(|(id, _)| id.clone())
                        .filter(|id| !id.is_empty());
                    if !panel.controller.set_output(output) {
                        panel.schedule_rebuild();
                    }
                }
            });
        }
        {
            let weak = Rc::downgrade(&panel);
            panel.network.connect_selected_notify(move |dropdown| {
                if let Some(panel) = weak.upgrade()
                    && !panel.syncing_scale.get()
                {
                    let network = panel
                        .network_choices
                        .borrow()
                        .get(dropdown.selected() as usize)
                        .map(|(id, _)| id.clone())
                        .filter(|id| !id.is_empty());
                    if !panel
                        .controller
                        .update_settings(|settings| settings.network_interface = network)
                    {
                        panel.schedule_rebuild();
                    }
                }
            });
        }
        if let Some(display) = gdk::Display::default() {
            let weak = Rc::downgrade(&panel);
            display.monitors().connect_items_changed(move |_, _, _, _| {
                if let Some(panel) = weak.upgrade() {
                    panel.schedule_rebuild();
                }
            });
        }
        {
            let weak = Rc::downgrade(&panel);
            controller.subscribe_settings(move |_| {
                let Some(panel) = weak.upgrade() else {
                    return false;
                };
                panel.schedule_rebuild();
                true
            });
        }

        {
            let weak = Rc::downgrade(&panel);
            controller.subscribe_snapshot(move |_| {
                let Some(panel) = weak.upgrade() else {
                    return false;
                };
                if panel.root.is_mapped() {
                    // Values update in place; refreshing also tracks device state
                    // and interface changes without replacing expanded groups.
                    panel.schedule_rebuild();
                }
                true
            });
        }

        panel
    }

    pub(super) fn is_interacting(&self) -> bool {
        self.popovers.iter().any(|popover| popover.is_visible())
            || self
                .groups
                .borrow()
                .values()
                .any(|group| group.renamers.iter().any(|name| name.is_editing()))
    }

    pub(super) fn sync_dimensions(&self, settings: &MonitorSettings) {
        self.syncing_scale.set(true);
        self.width.set_value(f64::from(settings.width));
        if let Some(height) = settings.height {
            self.height.set_value(f64::from(height));
        }
        self.scale
            .set_value(11.0 * f64::from(settings.scale_milli) / 1000.0);
        self.hide_delay
            .set_value(f64::from(settings.hide_delay_ms) / 1000.0);
        self.interval
            .set_value(f64::from(settings.interval_ms) / 1000.0);
        self.economy.set_active(settings.economy);
        self.syncing_scale.set(false);
    }

    pub(super) fn sync_sources(&self, settings: &MonitorSettings, snapshot: &SystemSnapshot) {
        self.syncing_scale.set(true);
        let mut outputs = vec![(String::new(), "Automatic".to_owned())];
        if let Some(display) = gdk::Display::default() {
            let monitors = display.monitors();
            for index in 0..monitors.n_items() {
                if let Some(monitor) = monitors
                    .item(index)
                    .and_then(|item| item.downcast::<gdk::Monitor>().ok())
                {
                    let id = monitor_output_id(&monitor);
                    outputs.push((
                        id.clone(),
                        format!("{id} · {}", monitor.model().unwrap_or_default()),
                    ));
                }
            }
        }
        if let Some(output) = &settings.output
            && !outputs.iter().any(|(id, _)| id == output)
        {
            outputs.push((output.clone(), format!("{output} · disconnected")));
        }
        if *self.output_choices.borrow() != outputs {
            let strings = gtk::StringList::new(
                &outputs
                    .iter()
                    .map(|(_, label)| label.as_str())
                    .collect::<Vec<_>>(),
            );
            self.output.set_model(Some(&strings));
            self.output_choices.replace(outputs.clone());
        }
        self.output.set_selected(
            outputs
                .iter()
                .position(|(id, _)| Some(id.as_str()) == settings.output.as_deref())
                .unwrap_or(0) as u32,
        );
        let mut networks = vec![String::new()];
        networks.extend(snapshot.network_choices.clone());
        if let Some(network) = &settings.network_interface
            && !networks.contains(network)
        {
            networks.push(network.clone());
        }
        let networks = networks
            .iter()
            .map(|name| {
                let label = if name.is_empty() {
                    "Automatic".to_owned()
                } else if !snapshot.network_choices.contains(name) {
                    format!("{name} · unavailable")
                } else {
                    name.clone()
                };
                (name.clone(), label)
            })
            .collect::<Vec<_>>();
        if *self.network_choices.borrow() != networks {
            self.network.set_model(Some(&gtk::StringList::new(
                &networks
                    .iter()
                    .map(|(_, label)| label.as_str())
                    .collect::<Vec<_>>(),
            )));
            self.network_choices.replace(networks.clone());
        }
        self.network.set_selected(
            networks
                .iter()
                .position(|(id, _)| Some(id.as_str()) == settings.network_interface.as_deref())
                .unwrap_or(0) as u32,
        );
        self.syncing_scale.set(false);
    }

    pub(super) fn sync_limits(
        &self,
        width_limits: (i32, i32),
        height_limits: (i32, i32),
        height: i32,
    ) {
        self.syncing_scale.set(true);
        self.width
            .set_range(f64::from(width_limits.0), f64::from(width_limits.1));
        self.width
            .set_value(f64::from(self.controller.settings().width));
        self.height
            .set_range(f64::from(height_limits.0), f64::from(height_limits.1));
        self.height.set_value(f64::from(
            self.controller.settings().height.unwrap_or(height),
        ));
        self.syncing_scale.set(false);
    }

    pub(super) fn sync_pin(&self, pinned: bool) {
        self.syncing_pin.set(true);
        self.pin.set_active(pinned);
        self.pin
            .update_property(&[gtk::accessible::Property::Label(if pinned {
                "Unpin panel"
            } else {
                "Pin panel"
            })]);
        self.syncing_pin.set(false);
    }

    pub(super) fn schedule_rebuild(self: &Rc<Self>) {
        if self.rebuild_pending.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            let Some(panel) = weak.upgrade() else {
                return;
            };
            panel.rebuild_pending.set(false);
            panel.rebuild();
        });
    }

    pub(super) fn rebuild(self: &Rc<Self>) {
        let settings = self.controller.settings();
        self.sync_pin(settings.pinned);
        self.sync_dimensions(&settings);
        let snapshot = self.controller.latest();
        self.sync_sources(&settings, &snapshot);
        let last_index = settings.sections.len().saturating_sub(1);
        self.syncing_selection.set(true);
        let mut groups = self.groups.borrow_mut();

        for (index, preference) in settings.sections.iter().enumerate() {
            let section = preference.section;
            let group = groups
                .entry(section)
                .or_insert_with(|| SettingsGroupView::new(self, section));
            let options = metric_options(section, &snapshot);
            if group.options.as_ref() != Some(&options) {
                clear_box(&group.choices);
                group.toggles.clear();
                group.renamers.clear();
                let mut previous_group = None;
                for option in &options {
                    if option.group.as_ref() != previous_group {
                        if let Some(device) = &option.group {
                            let heading = gtk::Label::new(Some(&device.label));
                            heading.set_xalign(0.0);
                            heading.set_width_chars(1);
                            heading.set_ellipsize(gtk::pango::EllipsizeMode::End);
                            heading.add_css_class("system-monitor-settings-row-title");
                            heading.set_margin_top(6);
                            group.choices.append(&heading);
                        }
                        previous_group = option.group.as_ref();
                    }
                    let weak = Rc::downgrade(self);
                    let id = option.id.clone();
                    let toggle = settings_visibility_button(
                        settings.metric_visible(section, &id),
                        &option.label,
                        move |visible| {
                            weak.upgrade().is_some_and(|panel| {
                                panel.syncing_selection.get()
                                    || panel.controller.set_metric_visible(section, &id, visible)
                            })
                        },
                    );
                    let row = gtk::Box::new(gtk::Orientation::Horizontal, 5);
                    row.add_css_class("system-monitor-settings-metric-row");
                    let name = gtk::EditableLabel::new(&settings.metric_name(
                        section,
                        &option.id,
                        &option.label,
                    ));
                    name.set_hexpand(true);
                    name.set_size_request(0, -1);
                    name.add_css_class("system-monitor-settings-name");
                    constrain_metric_name(&name);
                    let weak = Rc::downgrade(self);
                    let id = option.id.clone();
                    let default = option.label.clone();
                    name.connect_notify_local(Some("editing"), move |name, _| {
                        let Some(panel) = weak.upgrade() else { return };
                        if let Some(changed) = panel.editing_changed.borrow().as_ref() {
                            changed();
                        }
                        if !name.is_editing() && !panel.syncing_selection.get() {
                            panel.controller.set_metric_name(
                                section,
                                &id,
                                name.text().as_str(),
                                &default,
                            );
                            panel.schedule_rebuild();
                        }
                    });
                    let edit = settings_move_button("\u{f03eb}", "Rename metric", true);
                    let field = name.clone();
                    edit.connect_clicked(move |_| field.start_editing());
                    row.append(&name);
                    row.append(&edit);
                    row.append(&toggle);
                    group.choices.append(&row);
                    group.toggles.push(toggle);
                    group.renamers.push(name);
                }
                if options.is_empty() {
                    let label = gtk::Label::new(Some(if snapshot.ready {
                        "Not reported by driver"
                    } else {
                        "Waiting for sensor data…"
                    }));
                    label.add_css_class("system-monitor-settings-row-meta");
                    label.set_wrap(true);
                    label.set_xalign(0.0);
                    group.choices.append(&label);
                }
                group.options = Some(options.clone());
            }
            let selected = options
                .iter()
                .filter(|option| settings.metric_visible(section, &option.id))
                .count();
            group.meta.set_label(
                &if let Some(state) = snapshot.section_states.get(&section) {
                    state.label().into()
                } else if section == MonitorSection::Gpu
                    && snapshot
                        .gpus
                        .iter()
                        .any(|device| device.state != gpu::DataState::Ready)
                {
                    snapshot
                        .gpus
                        .iter()
                        .filter(|device| device.state != gpu::DataState::Ready)
                        .map(|device| format!("{}: {}", device.name, device.state.label()))
                        .collect::<Vec<_>>()
                        .join(" · ")
                } else if options.is_empty() {
                    "Not reported by driver".into()
                } else if preference.visible {
                    format!("{selected} of {} selected", options.len())
                } else {
                    format!("Hidden · {selected}/{} selected", options.len())
                },
            );
            group.visible.set_active(preference.visible);
            group.expand.set_sensitive(!options.is_empty());
            group.up.set_sensitive(index > 0);
            group.down.set_sensitive(index < last_index);
            for ((option, toggle), name) in options.iter().zip(&group.toggles).zip(&group.renamers)
            {
                toggle.set_active(settings.metric_visible(section, &option.id));
                if !name.is_editing() {
                    name.set_text(&settings.metric_name(section, &option.id, &option.label));
                }
            }
        }
        let order = settings
            .sections
            .iter()
            .map(|preference| preference.section)
            .collect::<Vec<_>>();
        if *self.order.borrow() != order {
            clear_box(&self.list);
            for section in &order {
                self.list.append(&groups[section].root);
            }
            self.order.replace(order);
        }
        self.syncing_selection.set(false);
    }
}

pub(super) struct SettingsGroupView {
    pub(super) root: gtk::Box,
    pub(super) expand: gtk::ToggleButton,
    pub(super) meta: gtk::Label,
    pub(super) up: gtk::Button,
    pub(super) down: gtk::Button,
    pub(super) visible: gtk::ToggleButton,
    pub(super) choices: gtk::Box,
    pub(super) options: Option<Vec<MetricOption>>,
    pub(super) toggles: Vec<gtk::ToggleButton>,
    pub(super) renamers: Vec<gtk::EditableLabel>,
}

impl SettingsGroupView {
    pub(super) fn new(panel: &Rc<MonitorSettingsPanel>, section: MonitorSection) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("system-monitor-settings-row");
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let copy = gtk::Box::new(gtk::Orientation::Vertical, 1);
        copy.set_hexpand(true);
        let title = gtk::Label::new(Some(section.title()));
        title.add_css_class("system-monitor-settings-row-title");
        title.set_xalign(0.0);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let meta = gtk::Label::new(None);
        meta.add_css_class("system-monitor-settings-row-meta");
        meta.set_xalign(0.0);
        meta.set_ellipsize(gtk::pango::EllipsizeMode::End);
        copy.append(&title);
        copy.append(&meta);
        let chevron = gtk::Label::new(Some(ICON_EXPAND));
        chevron.add_css_class("system-monitor-settings-button-icon");
        let heading = gtk::Box::new(gtk::Orientation::Horizontal, 5);
        heading.append(&chevron);
        heading.append(&copy);
        let expand = gtk::ToggleButton::new();
        expand.add_css_class("system-monitor-settings-expand");
        expand.set_hexpand(true);
        expand.set_child(Some(&heading));
        let choices = gtk::Box::new(gtk::Orientation::Vertical, 2);
        choices.add_css_class("system-monitor-settings-choices");
        let reveal = gtk::Revealer::new();
        reveal.set_transition_type(gtk::RevealerTransitionType::SlideDown);
        reveal.set_transition_duration(160);
        reveal.set_child(Some(&choices));
        let expansion = reveal.clone();
        expand.connect_toggled(move |toggle| {
            expansion.set_reveal_child(toggle.is_active());
            chevron.set_label(if toggle.is_active() {
                ICON_COLLAPSE
            } else {
                ICON_EXPAND
            });
        });
        let up = settings_move_button(ICON_UP, "Move up", true);
        let down = settings_move_button(ICON_DOWN, "Move down", true);
        for (button, direction) in [(&up, -1), (&down, 1)] {
            let weak = Rc::downgrade(panel);
            button.connect_clicked(move |_| {
                if let Some(panel) = weak.upgrade() {
                    panel.controller.move_section(section, direction);
                }
            });
        }
        let weak = Rc::downgrade(panel);
        let visible = settings_visibility_button(true, section.title(), move |visible| {
            weak.upgrade().is_some_and(|panel| {
                panel.syncing_selection.get()
                    || panel.controller.set_section_visible(section, visible)
            })
        });
        header.append(&expand);
        header.append(&up);
        header.append(&down);
        header.append(&visible);
        root.append(&header);
        root.append(&reveal);
        Self {
            root,
            expand,
            meta,
            up,
            down,
            visible,
            choices,
            options: None,
            toggles: Vec::new(),
            renamers: Vec::new(),
        }
    }
}

fn settings_visibility_button(
    active: bool,
    name: &str,
    update: impl Fn(bool) -> bool + 'static,
) -> gtk::ToggleButton {
    let button = gtk::ToggleButton::new();
    button.add_css_class("system-monitor-settings-visibility");
    button.set_valign(gtk::Align::Center);
    let icon = gtk::Label::new(Some(if active { ICON_VISIBLE } else { ICON_HIDDEN }));
    icon.add_css_class("system-monitor-settings-button-icon");
    button.set_child(Some(&icon));
    let name = name.to_owned();
    button.update_property(&[gtk::accessible::Property::Label(&format!(
        "{} {name}",
        if active { "Hide" } else { "Show" }
    ))]);
    button.set_active(active);
    let reverting = Cell::new(false);
    button.connect_toggled(move |toggle| {
        let active = toggle.is_active();
        icon.set_label(if active { ICON_VISIBLE } else { ICON_HIDDEN });
        toggle.update_property(&[gtk::accessible::Property::Label(&format!(
            "{} {name}",
            if active { "Hide" } else { "Show" }
        ))]);
        if !reverting.replace(false) && !update(active) {
            reverting.set(true);
            toggle.set_active(!active);
        }
    });
    button
}

fn settings_move_button(icon: &str, accessible_name: &str, sensitive: bool) -> gtk::Button {
    let label = gtk::Label::new(Some(icon));
    label.add_css_class("system-monitor-settings-button-icon");
    let button = gtk::Button::new();
    button.add_css_class("system-monitor-settings-move");
    button.update_property(&[gtk::accessible::Property::Label(accessible_name)]);
    button.set_sensitive(sensitive);
    button.set_child(Some(&label));
    button
}
