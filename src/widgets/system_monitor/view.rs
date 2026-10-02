use super::drawer;
use super::drawer::{MonitorDrawer, PanelGeometry};
use super::metrics::MonitorLayout;
use super::settings_ui::MonitorSettingsPanel;
use super::{
    PANEL_EDGE_MARGIN, PANEL_MAX_WIDTH, PANEL_MIN_WIDTH, SETTINGS_PANEL_PADDING,
    SETTINGS_PANEL_WIDTH, SystemMonitorController,
};
use crate::widgets::{Generation, detach_application_window};
use gtk::{gdk, glib, prelude::*};
use gtk4_layer_shell::LayerShell;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer};
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

const CONTENT_NAMESPACE: &str = "obsidian-system-monitor";
const SETTINGS_HIDE_DELAY: Duration = Duration::from_secs(1);
const SETTINGS_TRIGGER_SIZE: i32 = 36;
const ICON_SETTINGS: &str = "\u{f0493}";

pub(super) fn monitor_input_region(
    width: i32,
    height: i32,
    open: bool,
    header_width: i32,
    header_height: i32,
) -> gtk::cairo::Region {
    let edge_width = drawer::EDGE_TRIGGER_WIDTH.min(width.max(0));
    let tail_top = drawer::TAIL_TOP.min(height.max(0));
    let tail_height = drawer::TAIL_HEIGHT.min((height - tail_top).max(0));
    let region = gtk::cairo::Region::create_rectangle(&gtk::cairo::RectangleInt::new(
        width - edge_width,
        tail_top,
        edge_width,
        tail_height,
    ));
    if open {
        // Only the drag handle needs a stationary input surface. Let GTK route
        // button and scrolling events directly to the content below.
        let _ = region.union_rectangle(&gtk::cairo::RectangleInt::new(
            0,
            0,
            header_width.clamp(0, width.max(0)),
            header_height.clamp(0, height.max(0)),
        ));
    }
    region
}

pub(super) struct MonitorRuntime {
    pub(super) monitor: gdk::Monitor,
    pub(super) window: gtk::ApplicationWindow,
    pub(super) surface: gtk::Box,
    pub(super) settings_reveal: gtk::Revealer,
    pub(super) settings_scroller: gtk::ScrolledWindow,
    pub(super) settings_panel: Rc<MonitorSettingsPanel>,
    pub(super) drawer: Rc<MonitorDrawer>,
    pub(super) hotspot_window: gtk::ApplicationWindow,
    pub(super) hotspot: gtk::Box,
    pub(super) tail: gtk::Button,
    pub(super) desktop_available: Cell<bool>,
    restore_on_desktop: Cell<bool>,
    pub(super) card: gtk::Box,
    pub(super) header: gtk::Box,
    pub(super) scroller: gtk::ScrolledWindow,
    pub(super) layout: Rc<MonitorLayout>,
    pub(super) requested_y: Cell<i32>,
    pub(super) requested_width: Cell<i32>,
    pub(super) requested_height: Cell<Option<i32>>,
    pub(super) y: Cell<i32>,
    pub(super) pinned: Cell<bool>,
    pub(super) hide_delay: Cell<Duration>,
    pub(super) settings_hovered: Cell<bool>,
    pub(super) settings_hide_generation: Generation,
    pub(super) hovered: Cell<bool>,
    pub(super) hotspot_hovered: Cell<bool>,
    pub(super) dragging: Cell<bool>,
    pub(super) drag_start_y: Cell<i32>,
    pub(super) hide_generation: Generation,
    pub(super) panel_width: Cell<i32>,
    pub(super) adjusting_scale: Cell<bool>,
}

impl MonitorRuntime {
    fn set_desktop_available(self: &Rc<Self>, available: bool) {
        if self.desktop_available.replace(available) == available {
            return;
        }
        self.tail.set_sensitive(available);
        self.hide_generation.bump();
        if available {
            self.hotspot_window.present();
            if self.restore_on_desktop.replace(false) && self.pinned.get() {
                self.drawer.set_revealed(true);
            }
        } else {
            // Occlusion suspends an open pinned panel; it isn't a manual close.
            self.restore_on_desktop
                .set(self.pinned.get() && self.drawer.is_revealed());
            self.hotspot_window.set_visible(false);
            self.hovered.set(false);
            self.hotspot_hovered.set(false);
            self.settings_hovered.set(false);
            self.dragging.set(false);
            self.adjusting_scale.set(false);
            self.settings_hide_generation.bump();
            for popover in &self.settings_panel.popovers {
                popover.popdown();
            }
            self.window.set_keyboard_mode(KeyboardMode::None);
            self.drawer.set_revealed(false);
        }
        self.sync_input_region();
    }

    fn toggle_open(self: &Rc<Self>) {
        if !self.desktop_available.get() {
            return;
        }
        self.hide_generation.bump();
        self.restore_on_desktop.set(false);
        let open = !self.drawer.is_revealed();
        if !open {
            self.window.set_keyboard_mode(KeyboardMode::None);
        }
        self.drawer.set_revealed(open);
        self.sync_input_region();
        self.sync_reveal();
    }

    pub(super) fn measure_after_font_update(self: &Rc<Self>) {
        // Inherited CSS fonts settle during layout. Measure after that frame,
        // including the first map with a font restored from saved settings.
        let weak = Rc::downgrade(self);
        self.window.add_tick_callback(move |_, _| {
            let weak = weak.clone();
            glib::idle_add_local_once(move || {
                if let Some(runtime) = weak.upgrade() {
                    for section in runtime.layout.sections.values() {
                        section.minimum_width.set(0);
                    }
                    runtime.apply_size();
                }
            });
            glib::ControlFlow::Break
        });
    }

    pub(super) fn geometry(&self) -> PanelGeometry {
        let mut geometry = PanelGeometry::for_monitor(&self.monitor, self.requested_width.get());
        if let Some(application) = self.window.application() {
            for window in application.windows() {
                if window.has_css_class("bar-window")
                    && window.monitor().as_ref() == Some(&self.monitor)
                {
                    geometry.top_margin = geometry.top_margin.max(
                        window.height()
                            + crate::ui::bar::BAR_VISIBLE_TOP_MARGIN
                            + PANEL_EDGE_MARGIN,
                    );
                }
            }
        }
        geometry
    }

    pub(super) fn apply_size(&self) {
        let geometry = self.geometry();
        let (min_height, max_height) = geometry.height_limits();
        let settings_open =
            self.settings_reveal.reveals_child() || self.settings_reveal.is_child_revealed();
        let settings_width = if settings_open {
            // Larger settings fonts may need more than the default 330 px.
            self.settings_reveal
                .child()
                .map(|surface| surface.measure(gtk::Orientation::Horizontal, -1).0)
                .unwrap_or(SETTINGS_PANEL_WIDTH)
                + PANEL_EDGE_MARGIN
        } else {
            0
        };
        let maximum_width = (geometry.screen_width - drawer::RIGHT_MARGIN - settings_width)
            .clamp(1, PANEL_MAX_WIDTH);
        let (_, header_width, _, _) = self.header.measure(gtk::Orientation::Horizontal, -1);
        let minimum_width = self
            .layout
            .minimum_width()
            .max(header_width)
            .max(PANEL_MIN_WIDTH)
            .min(maximum_width);
        let width = geometry.panel_width.clamp(minimum_width, maximum_width);
        self.panel_width.set(width);
        self.card.set_size_request(width, -1);
        let (_, header_height, _, _) = self.header.measure(gtk::Orientation::Vertical, width);
        // The body measurement includes wrapped lines, meters, group gaps and
        // bottom padding. A saved height must not hide any of that content.
        let minimum_height =
            (header_height + self.layout.natural_height(width)).clamp(min_height, max_height);
        let height = self
            .requested_height
            .get()
            .unwrap_or(minimum_height)
            .clamp(minimum_height, max_height);
        // Scrolling is only necessary when the content exceeds the screen.
        self.scroller.set_propagate_natural_height(false);
        self.scroller.set_min_content_height(1);
        self.scroller.set_max_content_height(-1);
        self.card.set_size_request(width, height);
        let (_, natural, _, _) = self.card.measure(gtk::Orientation::Vertical, width);
        self.settings_panel.sync_limits(
            (minimum_width, maximum_width),
            (minimum_height, max_height),
            natural,
        );
        let height = natural.max(drawer::TAIL_TOP + drawer::TAIL_HEIGHT);
        let settings_height = (height - SETTINGS_PANEL_PADDING * 2).max(1);
        self.settings_scroller.set_min_content_height(-1);
        self.settings_scroller
            .set_max_content_height(settings_height);
        self.settings_scroller
            .set_min_content_height(settings_height);
        let y = geometry.clamp_y(self.requested_y.get(), height);
        self.y.set(y);
        self.window.set_default_size(1, height);
        self.window.set_margin(Edge::Top, y);
        self.drawer.sync_position();
        if !self.dragging.get() {
            self.hotspot_window.set_margin(Edge::Top, y);
            self.hotspot
                .set_size_request(width + drawer::RIGHT_MARGIN, height);
            self.hotspot_window
                .set_default_size(width + drawer::RIGHT_MARGIN, height);
            self.sync_input_region();
        }
    }

    pub(super) fn set_settings_open(self: &Rc<Self>, open: bool) {
        if open && (!self.desktop_available.get() || !self.drawer.is_revealed()) {
            return;
        }
        if open {
            self.settings_panel.rebuild();
        }
        self.settings_reveal.set_reveal_child(open);
        self.window.set_keyboard_mode(if open {
            KeyboardMode::OnDemand
        } else {
            KeyboardMode::None
        });
        self.apply_size();
        self.sync_reveal();
    }

    pub(super) fn sync_input_region(&self) {
        if let Some(surface) = self.hotspot_window.surface() {
            let width = surface.width();
            let (header_width, header_height) = self.drag_handle_size();
            let region = monitor_input_region(
                width,
                surface.height(),
                self.drawer.is_open(),
                header_width,
                header_height,
            );
            surface.set_input_region(Some(&region));
        }
    }

    pub(super) fn drag_handle_size(&self) -> (i32, i32) {
        let width = self
            .header
            .first_child()
            .and_then(|handle| handle.compute_bounds(&self.card))
            .map_or(0, |bounds| (bounds.x() + bounds.width()).floor() as i32);
        (width, self.header.height())
    }

    pub(super) fn header_hit(&self, x: f64, y: f64) -> bool {
        let (width, height) = self.drag_handle_size();
        self.drawer.is_open()
            && x >= 0.0
            && y >= 0.0
            && y < f64::from(height)
            && x < f64::from(width)
    }

    pub(super) fn keep_open(&self) -> bool {
        self.pinned.get()
            || self.hovered.get()
            || self.hotspot_hovered.get()
            || self.dragging.get()
            || self.adjusting_scale.get()
            || self.settings_hovered.get()
            || self.settings_panel.is_interacting()
    }

    pub(super) fn keep_settings_open(&self) -> bool {
        self.settings_hovered.get()
            || self.dragging.get()
            || self.adjusting_scale.get()
            || self.settings_panel.is_interacting()
    }

    pub(super) fn sync_settings_timeout(self: &Rc<Self>) {
        let generation = self.settings_hide_generation.bump();
        if !self.settings_reveal.reveals_child() || self.keep_settings_open() {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(SETTINGS_HIDE_DELAY, move || {
            if let Some(runtime) = weak.upgrade()
                && runtime.settings_hide_generation.is_current(generation)
                && runtime.settings_reveal.reveals_child()
                && !runtime.keep_settings_open()
            {
                runtime.set_settings_open(false);
            }
        });
    }

    pub(super) fn sync_reveal(self: &Rc<Self>) {
        self.sync_settings_timeout();
        let generation = self.hide_generation.bump();
        if !self.desktop_available.get() || !self.drawer.is_revealed() || self.keep_open() {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(self.hide_delay.get(), move || {
            if let Some(runtime) = weak.upgrade()
                && runtime.hide_generation.is_current(generation)
                && runtime.desktop_available.get()
                && !runtime.keep_open()
            {
                runtime.window.set_keyboard_mode(KeyboardMode::None);
                runtime.drawer.set_revealed(false);
                runtime.sync_input_region();
            }
        });
    }

    pub(super) fn install_drag(
        self: &Rc<Self>,
        widget: &impl IsA<gtk::Widget>,
        controller: &Rc<SystemMonitorController>,
    ) {
        let drag = gtk::GestureDrag::new();
        drag.set_button(gdk::BUTTON_PRIMARY);
        {
            let weak = Rc::downgrade(self);
            drag.connect_drag_begin(move |gesture, x, y| {
                let Some(runtime) = weak.upgrade() else {
                    return;
                };
                if !runtime.header_hit(x, y) {
                    gesture.set_state(gtk::EventSequenceState::Denied);
                    return;
                }
                gesture.set_state(gtk::EventSequenceState::Claimed);
                runtime.drag_start_y.set(runtime.y.get());
                runtime.dragging.set(true);
                runtime.sync_reveal();
            });
        }
        {
            let weak = Rc::downgrade(self);
            drag.connect_drag_update(move |_, _, offset_y| {
                let Some(runtime) = weak.upgrade() else {
                    return;
                };
                if !runtime.dragging.get() {
                    return;
                }
                // Keep the input surface stationary until release, so GTK's
                // offsets stay relative to one origin while the panel moves.
                runtime.requested_y.set(
                    runtime
                        .drag_start_y
                        .get()
                        .saturating_add(offset_y.round() as i32),
                );
                runtime.apply_size();
            });
        }
        {
            let weak = Rc::downgrade(self);
            let controller = Rc::clone(controller);
            drag.connect_drag_end(move |_, _, offset_y| {
                let Some(runtime) = weak.upgrade() else {
                    return;
                };
                if !runtime.dragging.replace(false) {
                    return;
                }
                runtime.requested_y.set(
                    runtime
                        .drag_start_y
                        .get()
                        .saturating_add(offset_y.round() as i32),
                );
                runtime.apply_size();
                if !controller.set_position(runtime.y.get()) {
                    let geometry =
                        PanelGeometry::for_monitor(&runtime.monitor, runtime.requested_width.get());
                    runtime.requested_y.set(
                        controller
                            .settings()
                            .position_y
                            .unwrap_or(geometry.default_y()),
                    );
                    runtime.apply_size();
                }
                runtime.sync_reveal();
            });
        }
        {
            let weak = Rc::downgrade(self);
            drag.connect_cancel(move |_, _| {
                if let Some(runtime) = weak.upgrade()
                    && runtime.dragging.replace(false)
                {
                    runtime.requested_y.set(runtime.drag_start_y.get());
                    runtime.apply_size();
                    runtime.sync_reveal();
                }
            });
        }
        widget.add_controller(drag);
    }
}

pub struct SystemMonitorView {
    pub(super) runtime: Rc<MonitorRuntime>,
    pub(super) geometry_handler: Option<glib::SignalHandlerId>,
}

impl SystemMonitorView {
    pub fn new(
        application: &gtk::Application,
        monitor: &gdk::Monitor,
        controller: &Rc<SystemMonitorController>,
    ) -> Self {
        let settings = controller.settings();
        let geometry = PanelGeometry::for_monitor(monitor, settings.width);
        let layout = MonitorLayout::new(&monitor.display(), settings.clone(), controller.latest());
        let content_window = gtk::ApplicationWindow::builder()
            .application(application)
            .decorated(false)
            .resizable(false)
            .build();
        content_window.add_css_class("system-monitor-window");
        content_window.init_layer_shell();
        content_window.set_namespace(Some(CONTENT_NAMESPACE));
        content_window.set_layer(Layer::Bottom);
        content_window.set_keyboard_mode(KeyboardMode::None);
        content_window.set_monitor(Some(monitor));
        content_window.set_anchor(Edge::Top, true);
        content_window.set_anchor(Edge::Right, true);
        content_window.set_exclusive_zone(-1);

        let tail = gtk::Button::new();
        tail.add_css_class("system-monitor-tail");
        tail.set_size_request(drawer::TAIL_WIDTH, drawer::TAIL_HEIGHT);
        tail.set_valign(gtk::Align::Start);
        tail.set_halign(gtk::Align::End);
        tail.set_hexpand(true);
        tail.set_cursor_from_name(Some("pointer"));
        tail.set_sensitive(false);
        tail.update_property(&[gtk::accessible::Property::Label("Toggle system monitor")]);
        tail.set_margin_top(drawer::TAIL_TOP);
        let grip = gtk::Box::new(gtk::Orientation::Vertical, 0);
        grip.add_css_class("system-monitor-tail-grip");
        grip.set_halign(gtk::Align::Center);
        grip.set_valign(gtk::Align::Center);
        grip.set_vexpand(true);
        tail.set_child(Some(&grip));

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        header.add_css_class("system-monitor-header");
        let drag_handle = gtk::Label::new(Some("System monitor"));
        drag_handle.add_css_class("system-monitor-header-title");
        drag_handle.set_xalign(0.0);
        drag_handle.set_hexpand(true);
        drag_handle.set_ellipsize(gtk::pango::EllipsizeMode::End);
        drag_handle.set_width_chars(1);
        let trigger = gtk::Button::new();
        trigger.add_css_class("system-monitor-settings-trigger");
        trigger.set_size_request(SETTINGS_TRIGGER_SIZE, SETTINGS_TRIGGER_SIZE);
        trigger.update_property(&[gtk::accessible::Property::Label("Configure system monitor")]);
        let trigger_icon = gtk::Label::new(Some(ICON_SETTINGS));
        trigger_icon.add_css_class("system-monitor-settings-trigger-icon");
        trigger.set_child(Some(&trigger_icon));
        header.append(&drag_handle);
        header.append(&trigger);

        let scroller = gtk::ScrolledWindow::new();
        scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::External);
        scroller.set_propagate_natural_height(true);
        scroller.set_vexpand(true);
        scroller.set_child(Some(&layout.root));
        let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
        card.add_css_class("system-monitor-body");
        card.set_valign(gtk::Align::Start);
        card.append(&header);
        card.append(&scroller);
        let hotspot_window = gtk::ApplicationWindow::builder()
            .application(application)
            .decorated(false)
            .resizable(false)
            .build();
        hotspot_window.add_css_class("system-monitor-hotspot-window");
        hotspot_window.init_layer_shell();
        hotspot_window.set_namespace(Some("obsidian-system-monitor-hotspot"));
        hotspot_window.set_layer(Layer::Bottom);
        hotspot_window.set_keyboard_mode(KeyboardMode::None);
        hotspot_window.set_monitor(Some(monitor));
        hotspot_window.set_anchor(Edge::Top, true);
        hotspot_window.set_anchor(Edge::Right, true);
        hotspot_window.set_exclusive_zone(-1);
        let hotspot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        hotspot.append(&tail);
        hotspot_window.set_child(Some(&hotspot));
        let settings_panel = MonitorSettingsPanel::new(controller);
        let settings_scroller = gtk::ScrolledWindow::new();
        settings_scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::External);
        settings_scroller.set_child(Some(&settings_panel.root));
        let settings_surface = gtk::Box::new(gtk::Orientation::Vertical, 0);
        settings_surface.add_css_class("system-monitor-settings-surface");
        settings_surface.append(&settings_scroller);
        let settings_reveal = gtk::Revealer::new();
        settings_reveal.set_transition_type(gtk::RevealerTransitionType::SlideRight);
        settings_reveal.set_transition_duration(220);
        settings_reveal.set_child(Some(&settings_surface));
        let surface = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        surface.add_css_class("system-monitor-card");
        surface.set_overflow(gtk::Overflow::Hidden);
        surface.append(&settings_reveal);
        surface.append(&card);
        let drawer = MonitorDrawer::new(&content_window, &hotspot_window, &surface);

        let runtime = Rc::new(MonitorRuntime {
            monitor: monitor.clone(),
            window: content_window.clone(),
            surface,
            settings_reveal: settings_reveal.clone(),
            settings_scroller,
            settings_panel,
            drawer,
            hotspot_window: hotspot_window.clone(),
            hotspot: hotspot.clone(),
            tail: tail.clone(),
            desktop_available: Cell::new(false),
            restore_on_desktop: Cell::new(false),
            card,
            header,
            scroller,
            layout,
            requested_y: Cell::new(settings.position_y.unwrap_or(geometry.default_y())),
            requested_width: Cell::new(settings.width),
            requested_height: Cell::new(settings.height),
            y: Cell::new(0),
            pinned: Cell::new(settings.pinned),
            hide_delay: Cell::new(Duration::from_millis(settings.hide_delay_ms as u64)),
            settings_hovered: Cell::new(false),
            settings_hide_generation: Generation::default(),
            hovered: Cell::new(false),
            hotspot_hovered: Cell::new(false),
            dragging: Cell::new(false),
            drag_start_y: Cell::new(0),
            hide_generation: Generation::default(),
            panel_width: Cell::new(geometry.panel_width),
            adjusting_scale: Cell::new(false),
        });
        runtime.install_drag(&hotspot, controller);
        {
            let weak = Rc::downgrade(&runtime);
            tail.connect_clicked(move |_| {
                if let Some(runtime) = weak.upgrade() {
                    runtime.toggle_open();
                }
            });
        }
        {
            let motion = gtk::EventControllerMotion::new();
            let weak = Rc::downgrade(&runtime);
            motion.connect_enter(move |_, _, _| {
                if let Some(runtime) = weak.upgrade() {
                    runtime.settings_hovered.set(true);
                    runtime.sync_reveal();
                }
            });
            let weak = Rc::downgrade(&runtime);
            motion.connect_leave(move |_| {
                if let Some(runtime) = weak.upgrade() {
                    runtime.settings_hovered.set(false);
                    runtime.sync_reveal();
                }
            });
            settings_surface.add_controller(motion);
            let weak = Rc::downgrade(&runtime);
            settings_surface.connect_unmap(move |_| {
                if let Some(runtime) = weak.upgrade() {
                    runtime.settings_hovered.set(false);
                    runtime.settings_hide_generation.bump();
                }
            });
        }
        for popover in &runtime.settings_panel.popovers {
            let weak = Rc::downgrade(&runtime);
            popover.connect_visible_notify(move |_| {
                let weak = weak.clone();
                glib::idle_add_local_once(move || {
                    if let Some(runtime) = weak.upgrade() {
                        runtime.sync_reveal();
                    }
                });
            });
        }
        {
            let weak = Rc::downgrade(&runtime);
            runtime
                .settings_panel
                .editing_changed
                .replace(Some(Box::new(move || {
                    let weak = weak.clone();
                    glib::idle_add_local_once(move || {
                        if let Some(runtime) = weak.upgrade() {
                            runtime.sync_reveal();
                        }
                    });
                })));
        }
        for slider in [
            &runtime.settings_panel.width,
            &runtime.settings_panel.height,
            &runtime.settings_panel.scale,
            &runtime.settings_panel.hide_delay,
            &runtime.settings_panel.interval,
        ] {
            // Keep the panel open while dragging, but let both dimensions
            // preview every value change before the pointer is released.
            let events = gtk::EventControllerLegacy::new();
            events.set_propagation_phase(gtk::PropagationPhase::Capture);
            let pressed = Rc::new(Cell::new(false));
            let active = Rc::clone(&pressed);
            let weak = Rc::downgrade(&runtime);
            events.connect_event(move |_, event| {
                if let Some(runtime) = weak.upgrade() {
                    match event.event_type() {
                        gdk::EventType::ButtonPress | gdk::EventType::TouchBegin => {
                            active.set(true);
                            runtime.adjusting_scale.set(true);
                            runtime.sync_reveal();
                        }
                        gdk::EventType::ButtonRelease
                        | gdk::EventType::TouchEnd
                        | gdk::EventType::TouchCancel
                            if active.replace(false) && runtime.adjusting_scale.replace(false) =>
                        {
                            runtime.apply_size();
                            runtime.sync_reveal();
                        }
                        _ => {}
                    }
                }
                glib::Propagation::Proceed
            });
            slider.add_controller(events);
            let weak = Rc::downgrade(&runtime);
            slider.connect_unmap(move |_| {
                if let Some(runtime) = weak.upgrade()
                    && pressed.replace(false)
                    && runtime.adjusting_scale.replace(false)
                {
                    runtime.apply_size();
                }
            });
        }
        {
            let weak = Rc::downgrade(&runtime);
            runtime.drawer.connect_settled(move || {
                if let Some(runtime) = weak.upgrade() {
                    if !runtime.window.is_visible() {
                        runtime.settings_hide_generation.bump();
                        runtime.settings_reveal.set_transition_duration(0);
                        runtime.settings_reveal.set_reveal_child(false);
                        runtime.settings_reveal.set_transition_duration(220);
                        runtime.apply_size();
                    }
                    runtime.sync_input_region();
                }
            });
        }
        for window in [&hotspot_window, &content_window] {
            let weak = Rc::downgrade(&runtime);
            window.connect_realize(move |window| {
                if let Some(runtime) = weak.upgrade() {
                    runtime.sync_input_region();
                }
                if let Some(surface) = window.surface() {
                    let weak = weak.clone();
                    surface.connect_layout(move |_, _, _| {
                        if let Some(runtime) = weak.upgrade() {
                            runtime.sync_input_region();
                        }
                    });
                }
            });
        }
        {
            let weak = Rc::downgrade(&runtime);
            hotspot_window.connect_map(move |_| {
                if let Some(runtime) = weak.upgrade() {
                    runtime.sync_input_region();
                }
            });
        }
        {
            let controller = Rc::downgrade(controller);
            content_window.connect_visible_notify(move |window| {
                if let Some(controller) = controller.upgrade() {
                    controller.set_view_visible(window.is_visible());
                }
            });
        }
        {
            let weak = Rc::downgrade(&runtime);
            let first_map = Cell::new(true);
            content_window.connect_map(move |_| {
                if first_map.replace(false)
                    && let Some(runtime) = weak.upgrade()
                {
                    runtime.measure_after_font_update();
                }
            });
        }
        for (widget, is_tail) in [(runtime.surface.clone(), false), (hotspot.clone(), true)] {
            let motion = gtk::EventControllerMotion::new();
            let weak = Rc::downgrade(&runtime);
            motion.connect_enter(move |_, _, _| {
                if let Some(runtime) = weak.upgrade() {
                    if is_tail {
                        runtime.hotspot_hovered.set(true);
                    } else {
                        runtime.hovered.set(true);
                    }
                    runtime.sync_reveal();
                }
            });
            if is_tail {
                let weak = Rc::downgrade(&runtime);
                motion.connect_motion(move |_, x, y| {
                    if let Some(runtime) = weak.upgrade() {
                        runtime
                            .hotspot
                            .set_cursor_from_name(if runtime.header_hit(x, y) {
                                Some("ns-resize")
                            } else {
                                None
                            });
                    }
                });
            }
            let weak = Rc::downgrade(&runtime);
            motion.connect_leave(move |_| {
                if let Some(runtime) = weak.upgrade() {
                    if is_tail {
                        runtime.hotspot_hovered.set(false);
                    } else {
                        runtime.hovered.set(false);
                    }
                    runtime.sync_reveal();
                }
            });
            widget.add_controller(motion);
        }
        {
            let weak = Rc::downgrade(&runtime);
            controller.subscribe_snapshot(move |snapshot| {
                let Some(runtime) = weak.upgrade() else {
                    return false;
                };
                runtime.layout.update_snapshot(snapshot);
                runtime.apply_size();
                true
            });
        }
        {
            let weak = Rc::downgrade(&runtime);
            controller.subscribe_settings(move |settings| {
                let Some(runtime) = weak.upgrade() else {
                    return false;
                };
                let font_changed = runtime.layout.applied_scale.get() != settings.scale_milli;
                runtime.layout.apply_settings(settings);
                runtime.pinned.set(settings.pinned);
                if !settings.pinned {
                    runtime.restore_on_desktop.set(false);
                }
                runtime
                    .hide_delay
                    .set(Duration::from_millis(settings.hide_delay_ms as u64));
                runtime.requested_width.set(settings.width);
                runtime.requested_height.set(settings.height);
                if !runtime.dragging.get() {
                    let geometry =
                        PanelGeometry::for_monitor(&runtime.monitor, runtime.requested_width.get());
                    runtime
                        .requested_y
                        .set(settings.position_y.unwrap_or(geometry.default_y()));
                }
                if font_changed {
                    runtime.measure_after_font_update();
                } else {
                    runtime.apply_size();
                }
                runtime.sync_reveal();
                true
            });
        }
        let geometry_handler = {
            let weak = Rc::downgrade(&runtime);
            monitor.connect_geometry_notify(move |_| {
                if let Some(runtime) = weak.upgrade() {
                    runtime.apply_size();
                }
            })
        };
        {
            let weak = Rc::downgrade(&runtime);
            settings_reveal.connect_child_revealed_notify(move |_| {
                if let Some(runtime) = weak.upgrade() {
                    runtime.apply_size();
                }
            });
        }
        {
            let weak = Rc::downgrade(&runtime);
            trigger.connect_clicked(move |_| {
                if let Some(runtime) = weak.upgrade() {
                    runtime.set_settings_open(!runtime.settings_reveal.reveals_child());
                }
            });
        }
        {
            let weak = Rc::downgrade(&runtime);
            let key = gtk::EventControllerKey::new();
            key.connect_key_pressed(move |_, key, _, _| {
                if key == gdk::Key::Escape
                    && let Some(runtime) = weak.upgrade()
                    && runtime.settings_reveal.reveals_child()
                {
                    runtime.set_settings_open(false);
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
            content_window.add_controller(key);
        }

        runtime.apply_size();
        controller.set_view_visible(content_window.is_visible());
        Self {
            runtime,
            geometry_handler: Some(geometry_handler),
        }
    }

    pub fn monitor(&self) -> &gdk::Monitor {
        &self.runtime.monitor
    }

    pub fn set_desktop_available(&self, available: bool) {
        self.runtime.set_desktop_available(available);
    }
}

impl Drop for SystemMonitorView {
    fn drop(&mut self) {
        self.runtime
            .settings_panel
            .controller
            .set_view_visible(false);
        if let Some(handler) = self.geometry_handler.take() {
            self.runtime.monitor.disconnect(handler);
        }
        for window in [&self.runtime.window, &self.runtime.hotspot_window] {
            detach_application_window(window);
        }
    }
}
