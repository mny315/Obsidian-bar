use super::drawer::PanelGeometry;
use super::settings_ui::*;
use super::*;
use gtk::{gdk, prelude::*};
use gtk4_layer_shell::{Edge, Layer, LayerShell};

#[test]
fn battery_total_uses_capacities_and_excludes_peripherals_and_absent_packs() {
    let root = std::env::temp_dir().join(format!("obsidian-batteries-{}", std::process::id()));
    let battery = |name: &str, fields: &[(&str, &str)]| {
        let path = root.join(name);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("type"), "Battery").unwrap();
        for (name, value) in fields {
            fs::write(path.join(name), value).unwrap();
        }
        path
    };
    battery(
        "BAT0",
        &[
            ("energy_now", "20000000"),
            ("energy_full", "20000000"),
            ("status", "Full"),
            ("power_now", "0"),
        ],
    );
    battery(
        "BAT1",
        &[
            ("energy_now", "0"),
            ("energy_full", "80000000"),
            ("status", "Discharging"),
            ("power_now", "10000000"),
        ],
    );
    battery(
        "mouse",
        &[
            ("scope", "Device"),
            ("capacity", "95"),
            ("status", "Charging"),
            ("power_now", "5000000"),
        ],
    );
    battery(
        "removed",
        &[("present", "0"), ("capacity", "95"), ("status", "Charging")],
    );
    let snapshot = read_battery_snapshot_at(&root).unwrap();
    assert_eq!(snapshot.percent, 20.0);
    assert_eq!(snapshot.status, "Discharging");
    assert_eq!(snapshot.power_watts, Some(10.0));

    let charge = battery(
        "charge",
        &[
            ("charge_now", "1000000"),
            ("charge_full", "2000000"),
            ("voltage_min_design", "10000000"),
        ],
    );
    assert_eq!(battery_capacity(&charge), Some((50.0, Some(20_000_000.0))));
    assert!((read_battery_snapshot_at(&root).unwrap().percent - 25.0).abs() < 0.001);
    fs::write(charge.join("charge_full"), "0").unwrap();
    assert!(battery_capacity(&charge).is_none());
    fs::write(charge.join("capacity"), "70").unwrap();
    assert_eq!(battery_capacity(&charge), Some((70.0, None)));
    fs::remove_dir_all(root).unwrap();
}

fn metric_selection_snapshot() -> SystemSnapshot {
    SystemSnapshot {
        ready: true,
        cpu_percent: Some(25.0),
        cpu_frequency_mhz: Some(3500.0),
        uptime_seconds: Some(7200),
        memory_used: Some(4 * 1024 * 1024 * 1024),
        memory_total: Some(16 * 1024 * 1024 * 1024),
        network_available: true,
        network_interfaces: vec!["enp1s0".into()],
        disks: ["/", "/mnt/games"]
            .into_iter()
            .map(|mount| DiskSnapshot {
                id: format!("mount:{mount}"),
                name: storage_display_name(Some("Test SSD"), None, Path::new(mount)),
                used: 256 * 1024 * 1024 * 1024,
                total: 1024 * 1024 * 1024 * 1024,
                available: 768 * 1024 * 1024 * 1024,
            })
            .collect(),
        temperatures: vec![
            SensorReading {
                id: "cpu:temp1".into(),
                label: "CPU Package Temperature".into(),
                value: 50.0,
                state: gpu::DataState::Ready,
            },
            SensorReading {
                id: "ssd:temp1".into(),
                label: "Samsung 990 PRO SSD Overall Temperature".into(),
                value: 35.0,
                state: gpu::DataState::Ready,
            },
        ],
        fans: vec![SensorReading {
            id: "board:fan1".into(),
            label: "CPU Fan".into(),
            value: 1200.0,
            state: gpu::DataState::Ready,
        }],
        ..SystemSnapshot::default()
    }
}

#[test]
fn individual_metric_choices_survive_save_and_group_toggles() {
    let mut settings = MonitorSettings::default();
    let id = "disk:games,cache;данные\\\"\n:used";
    settings.set_metric_visible(MonitorSection::Storage, id, false);
    settings.set_metric_visible(MonitorSection::Cpu, "cpu-frequency", false);
    settings.sections[0].visible = false;
    let saved = settings.to_key_file().to_data();
    let key_file = glib::KeyFile::new();
    key_file
        .load_from_data(&saved, glib::KeyFileFlags::NONE)
        .unwrap();
    let mut restored = MonitorSettings::from_key_file(&key_file);
    assert_eq!(restored, settings);
    restored.sections[0].visible = true;
    assert!(!restored.metric_visible(MonitorSection::Cpu, "cpu-frequency"));
    assert!(!restored.metric_visible(MonitorSection::Storage, id));
    assert!(restored.metric_visible(MonitorSection::Network, id));
    assert!(restored.metric_visible(MonitorSection::Cpu, "uptime"));
    restored.set_metric_visible(MonitorSection::Cpu, "cpu-frequency", true);
    assert!(restored.metric_visible(MonitorSection::Cpu, "cpu-frequency"));
}

#[test]
fn metrics_sensors_volumes_and_charts_are_selected_independently() {
    let snapshot = metric_selection_snapshot();
    let mut settings = MonitorSettings::default();
    settings.set_metric_visible(MonitorSection::Temperatures, "cpu:temp1", false);
    settings.set_metric_visible(MonitorSection::Storage, "mount:/:used", false);
    let (temperatures, _, _) = selected_metrics(MonitorSection::Temperatures, &snapshot, &settings);
    assert_eq!(temperatures.len(), 1);
    assert_eq!(temperatures[0].id, "ssd:temp1");
    let (disks, _, _) = selected_metrics(MonitorSection::Storage, &snapshot, &settings);
    assert_eq!(disks.len(), 7);
    assert!(
        disks
            .iter()
            .any(|metric| metric.id == "mount:/mnt/games:used")
    );
    for section in [MonitorSection::Network, MonitorSection::Memory] {
        for metric in display_metrics(section, &snapshot).0 {
            settings.set_metric_visible(section, &metric.id, false);
        }
    }
    let (rows, _, graph) = selected_metrics(MonitorSection::Network, &snapshot, &settings);
    assert!(rows.is_empty());
    assert!(graph);
    let (rows, meter, _) = selected_metrics(MonitorSection::Memory, &snapshot, &settings);
    assert!(rows.is_empty());
    assert_eq!(meter, Some(0.25));
    settings.set_metric_visible(MonitorSection::Network, METRIC_GRAPH, false);
    settings.set_metric_visible(MonitorSection::Memory, METRIC_METER, false);
    assert!(!selected_metrics(MonitorSection::Network, &snapshot, &settings).2);
    assert!(
        selected_metrics(MonitorSection::Memory, &snapshot, &settings)
            .1
            .is_none()
    );
    assert!(metric_options(MonitorSection::Cpu, &SystemSnapshot::default()).is_empty());
}

#[test]
fn sensor_preferences_do_not_depend_on_hwmon_enumeration() {
    let first = Path::new("/sys/devices/test-board/hwmon/hwmon1");
    let renumbered = Path::new("/sys/devices/test-board/hwmon/hwmon42");
    let other = Path::new("/sys/devices/other-board/hwmon/hwmon1");
    assert_eq!(
        hwmon_device_key(first, "nct6775"),
        hwmon_device_key(renumbered, "nct6775")
    );
    assert_ne!(
        hwmon_device_key(first, "nct6775"),
        hwmon_device_key(other, "nct6775")
    );
}

#[test]
fn storage_bars_belong_to_individual_named_volumes() {
    let mut snapshot = metric_selection_snapshot();
    snapshot.disks[1].used = snapshot.disks[1].total * 3 / 4;
    let mut settings = MonitorSettings::default();
    let (metrics, aggregate, _) = selected_metrics(MonitorSection::Storage, &snapshot, &settings);
    assert!(aggregate.is_none());
    assert_eq!(
        metrics
            .iter()
            .filter_map(|metric| metric.fraction)
            .collect::<Vec<_>>(),
        [0.25, 0.75]
    );
    assert_eq!(
        metrics[0].group.as_ref().unwrap().label,
        "Test SSD · System"
    );
    settings.set_metric_visible(MonitorSection::Storage, "mount:/:usage-meter", false);
    let (metrics, _, _) = selected_metrics(MonitorSection::Storage, &snapshot, &settings);
    assert_eq!(
        metrics
            .iter()
            .filter_map(|metric| metric.fraction)
            .collect::<Vec<_>>(),
        [0.75]
    );
    settings.set_metric_visible(MonitorSection::Storage, METRIC_METER, false);
    assert!(
        selected_metrics(MonitorSection::Storage, &snapshot, &settings)
            .0
            .iter()
            .all(|metric| metric.fraction.is_none())
    );
    assert_eq!(
        storage_display_name(Some("Samsung 990 PRO"), None, Path::new("/boot")),
        "Samsung 990 PRO · Boot"
    );
    assert_eq!(
        storage_display_name(
            Some("WD BLACK SN850X"),
            Some("Games"),
            Path::new("/mnt/games")
        ),
        "WD BLACK SN850X · Games"
    );
}

fn assert_monitor_text_fits(widget: &gtk::Widget) {
    if !widget.is_visible() {
        return;
    }
    if let Some(label) = widget.downcast_ref::<gtk::Label>() {
        let layout = label.layout();
        let (width, height) = layout.pixel_size();
        assert!(!layout.is_ellipsized(), "truncated text: {}", label.text());
        assert!(
            width <= label.width() + 1,
            "text exceeds width: {} ({width} > {})",
            label.text(),
            label.width()
        );
        assert!(
            height <= label.height() + 1,
            "text exceeds height: {} ({height} > {})",
            label.text(),
            label.height()
        );
    }
    let mut child = widget.first_child();
    while let Some(widget) = child {
        assert_monitor_text_fits(&widget);
        child = widget.next_sibling();
    }
}

fn last_monitor_label(widget: &gtk::Widget) -> Option<gtk::Label> {
    if !widget.is_visible() {
        return None;
    }
    let mut child = widget.last_child();
    while let Some(widget) = child {
        if let Some(label) = last_monitor_label(&widget) {
            return Some(label);
        }
        child = widget.prev_sibling();
    }
    widget.downcast_ref::<gtk::Label>().cloned()
}

fn assert_monitor_bottom_visible(runtime: &MonitorRuntime) {
    let last = last_monitor_label(runtime.layout.root.upcast_ref()).unwrap();
    let bounds = last.compute_bounds(&runtime.scroller).unwrap();
    assert!(bounds.y() >= 0.0, "last line must be inside the viewport");
    assert!(
        bounds.y() + bounds.height() < runtime.scroller.height() as f32,
        "last line needs room for its bottom padding: {}",
        last.text(),
    );
}

fn assert_monitor_height_fits(runtime: &MonitorRuntime) {
    let scroll = runtime.scroller.vadjustment();
    if scroll.upper() > scroll.page_size() + 1.0 {
        assert_eq!(
            runtime.card.height(),
            runtime.geometry().height_limits().1,
            "content may scroll only after reaching the screen height limit"
        );
    } else {
        assert_eq!(scroll.value(), 0.0);
        assert_monitor_bottom_visible(runtime);
    }
}

#[test]
#[ignore = "requires Wayland layer-shell and XDG_STATE_HOME=/tmp/obsidian-monitor-items-test-state"]
fn monitor_metric_groups_keep_selection_and_scroll_position() {
    assert!(settings_path().starts_with("/tmp/obsidian-monitor-items-test-state"));
    gtk::init().unwrap();
    let application = gtk::Application::builder()
        .application_id("dev.obsidian.MonitorItemsTest")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    application.register(None::<&gio::Cancellable>).unwrap();
    let display = gdk::Display::default().unwrap();
    let css = gtk::CssProvider::new();
    css.load_from_data(include_str!("../../../assets/window.css"));
    gtk::style_context_add_provider_for_display(
        &display,
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    let monitor = display
        .monitors()
        .item(0)
        .unwrap()
        .downcast::<gdk::Monitor>()
        .unwrap();
    let controller = SystemMonitorController::new();
    assert!(
        controller.update_settings(|settings| *settings = MonitorSettings {
            width: 364,
            height: Some(650),
            pinned: true,
            ..MonitorSettings::default()
        })
    );
    controller.latest.replace(metric_selection_snapshot());
    let view = SystemMonitorView::new(&application, &monitor, &controller);
    let runtime = &view.runtime;
    runtime.window.set_layer(Layer::Overlay);
    // This test drives settings hover explicitly. Native enter events after a
    // popover closes must not depend on the user's current pointer position.
    let settings_surface = runtime.settings_reveal.child().unwrap();
    let controllers = settings_surface.observe_controllers();
    let motions = (0..controllers.n_items())
        .filter_map(|index| {
            controllers
                .item(index)?
                .downcast::<gtk::EventControllerMotion>()
                .ok()
        })
        .collect::<Vec<_>>();
    for motion in motions {
        settings_surface.remove_controller(&motion);
    }
    view.set_desktop_available(true);
    runtime.tail.emit_clicked();
    let pump = |ms: u64| {
        let until = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < until {
            glib::MainContext::default().iteration(false);
            std::thread::sleep(Duration::from_millis(1));
        }
    };
    pump(350);
    runtime.set_settings_open(true);
    runtime.settings_hovered.set(true);
    pump(300);
    let adjustments = runtime
        .settings_panel
        .root
        .first_child()
        .unwrap()
        .next_sibling()
        .unwrap()
        .downcast::<gtk::Expander>()
        .unwrap();
    adjustments.set_expanded(true);
    pump(200);
    runtime
        .settings_panel
        .height
        .set_value(f64::from(PANEL_MIN_HEIGHT));
    pump(50);
    assert_eq!(
        runtime.settings_panel.height.value(),
        runtime.settings_panel.height.adjustment().lower()
    );
    assert!(runtime.card.height() > PANEL_MIN_HEIGHT);
    assert_monitor_height_fits(runtime);
    controller.set_dimensions(364, 650, SCALE_MILLI_DEFAULT);
    pump(50);
    // Exercise value-changed with the pointer still held: releasing the
    // drag must not be necessary to preview either growing or shrinking.
    let before_drag = runtime.window.height();
    runtime.adjusting_scale.set(true);
    for height in [before_drag + 120, before_drag + 60, before_drag] {
        runtime.settings_panel.height.set_value(f64::from(height));
        pump(100);
        assert!(runtime.adjusting_scale.get());
        assert_eq!(runtime.card.height(), height, "height previews during drag");
        assert_eq!(
            runtime.window.height(),
            height,
            "outer surface previews during drag"
        );
        assert_monitor_height_fits(runtime);
    }
    runtime.adjusting_scale.set(false);
    runtime.apply_size();
    pump(50);
    assert_eq!(
        runtime.window.height(),
        before_drag,
        "release must not change the preview"
    );

    let before_width = runtime.card.width();
    let settings_width = runtime.window.width() - before_width;
    runtime.adjusting_scale.set(true);
    for width in [before_width + 120, before_width + 60, before_width] {
        runtime.settings_panel.width.set_value(f64::from(width));
        pump(100);
        assert!(runtime.adjusting_scale.get());
        assert_eq!(runtime.card.width(), width, "width previews during drag");
        assert_eq!(runtime.window.width(), width + settings_width);
        assert_monitor_text_fits(runtime.card.upcast_ref());
    }
    runtime.adjusting_scale.set(false);
    runtime.apply_size();
    pump(50);
    assert_eq!(runtime.card.width(), before_width);

    // GTK applies CSS font attributes to label layouts; a bare Box's
    // default Pango context is not a measure of the rendered text.
    let text_height = |label: &gtk::Label| label.layout().pixel_size().1;
    let settings_text = [
        runtime
            .settings_panel
            .root
            .first_child()
            .unwrap()
            .first_child()
            .unwrap()
            .next_sibling()
            .unwrap()
            .downcast::<gtk::Label>()
            .unwrap(),
        runtime.settings_panel.groups.borrow()[&MonitorSection::Cpu]
            .meta
            .clone(),
        runtime
            .settings_panel
            .width
            .parent()
            .unwrap()
            .first_child()
            .unwrap()
            .downcast::<gtk::Label>()
            .unwrap(),
    ];
    let original_fonts = settings_text.iter().map(text_height).collect::<Vec<_>>();
    runtime.settings_panel.scale.set_value(22.0);
    pump(200);
    assert_eq!(controller.settings().scale_milli, 2000);
    for (widget, original) in settings_text.iter().zip(original_fonts) {
        assert!(
            (text_height(widget) - original * 2).abs() <= 3,
            "settings text follows the monitoring font scale: {} -> {}",
            original,
            text_height(widget)
        );
    }
    assert!(runtime.window.width() <= monitor.geometry().width());
    controller.set_dimensions(364, 650, SCALE_MILLI_DEFAULT);
    pump(200);
    let height = runtime.window.height();
    let panel = &runtime.settings_panel;
    let width = runtime.window.width();
    panel.interval.set_value(2.5);
    panel.economy.set_active(false);
    pump(50);
    assert_eq!(controller.settings().interval_ms, 2500);
    assert!(!controller.settings().economy);
    controller.set_view_visible(false);
    assert!(controller.gpu_sampling_enabled.load(Ordering::Acquire));
    panel.economy.set_active(true);
    assert!(!controller.gpu_sampling_enabled.load(Ordering::Acquire));
    controller.set_view_visible(true);
    assert!(controller.gpu_sampling_enabled.load(Ordering::Acquire));
    panel.output.set_selected(1);
    assert_eq!(
        controller.preferred_output(),
        Some(monitor_output_id(&monitor))
    );
    let expand = panel.groups.borrow()[&MonitorSection::Cpu].expand.clone();
    expand.set_active(true);
    pump(200);
    let name = panel.groups.borrow()[&MonitorSection::Cpu].renamers[0].clone();
    name.start_editing();
    name.set_text("Мой процессор — длинное имя показателя для проверки размеров панели");
    name.stop_editing(true);
    let resize_deadline = Instant::now() + Duration::from_secs(2);
    while runtime.window.width() <= width && Instant::now() < resize_deadline {
        pump(5);
    }
    assert_eq!(
        controller
            .settings()
            .metric_name(MonitorSection::Cpu, "cpu", "CPU"),
        name.text()
    );
    assert!(
        runtime.window.width() > width,
        "long names raise the minimum width: old={width}, new={}, card={}, requested={}",
        runtime.window.width(),
        runtime.card.width(),
        runtime.panel_width.get()
    );
    assert_eq!(runtime.window.height(), height);
    assert_monitor_text_fits(runtime.card.upcast_ref());
    let minimum = panel.width.adjustment().lower();
    panel.width.set_value(180.0);
    pump(50);
    assert_eq!(panel.width.value(), minimum);
    assert_eq!(runtime.card.width(), minimum as i32);
    controller.set_dimensions(364, 650, SCALE_MILLI_MAX);
    pump(100);
    assert_eq!(runtime.card.width(), PANEL_MAX_WIDTH);
    assert!(
        runtime.window.height() > height,
        "larger fonts also raise the minimum height"
    );
    assert_monitor_height_fits(runtime);
    assert_monitor_text_fits(runtime.card.upcast_ref());
    assert!(
        runtime.layout.sections[&MonitorSection::Cpu]
            .rows
            .first_child()
            .unwrap()
            .height()
            > 24
    );
    controller.set_dimensions(364, 650, SCALE_MILLI_DEFAULT);
    pump(100);
    let mut snapshot = metric_selection_snapshot();
    snapshot.cpu_percent = Some(100.0);
    runtime.layout.update_snapshot(&snapshot);
    runtime.apply_size();
    pump(30);
    let reserved_width = runtime.card.width();
    snapshot.cpu_percent = Some(0.0);
    runtime.layout.update_snapshot(&snapshot);
    runtime.apply_size();
    pump(30);
    assert_eq!(
        runtime.card.width(),
        reserved_width,
        "sampling must not shrink the panel"
    );
    assert_monitor_text_fits(runtime.card.upcast_ref());
    let toggle = |section: MonitorSection, id: &str| {
        let groups = panel.groups.borrow();
        let group = &groups[&section];
        let index = group
            .options
            .as_ref()
            .unwrap()
            .iter()
            .position(|option| option.id == id)
            .unwrap();
        group.toggles[index].clone()
    };
    let frequency = toggle(MonitorSection::Cpu, "cpu-frequency");
    let storage_bar_count = || {
        runtime.layout.sections[&MonitorSection::Storage]
            .metric_views
            .borrow()
            .iter()
            .filter(|view| matches!(view, MetricView::Meter(_)))
            .count()
    };
    assert_eq!(storage_bar_count(), 2);
    toggle(MonitorSection::Storage, "mount:/:usage-meter").set_active(false);
    assert_eq!(storage_bar_count(), 1);
    toggle(MonitorSection::Storage, "mount:/:usage-meter").set_active(true);
    let scroll = runtime.settings_scroller.vadjustment();
    scroll.set_value(100.0);
    pump(30);
    let position = scroll.value();
    frequency.set_active(false);
    pump(80);
    assert!(expand.is_active());
    assert_eq!(panel.groups.borrow()[&MonitorSection::Cpu].expand, expand);
    assert_eq!(toggle(MonitorSection::Cpu, "cpu-frequency"), frequency);
    assert_eq!(scroll.value(), position);
    assert_eq!(runtime.window.height(), height);
    assert!(!MonitorSettings::load().metric_visible(MonitorSection::Cpu, "cpu-frequency"));
    assert_eq!(
        runtime.layout.sections[&MonitorSection::Cpu]
            .signature
            .borrow()
            .len(),
        2
    );
    for id in ["cpu", "uptime"] {
        toggle(MonitorSection::Cpu, id).set_active(false);
    }
    pump(30);
    assert!(
        !runtime.layout.sections[&MonitorSection::Cpu]
            .root
            .is_visible()
    );
    assert_eq!(
        runtime.window.width(),
        width,
        "hidden labels do not reserve width"
    );
    toggle(MonitorSection::Cpu, "uptime").set_active(true);
    let group_visible = panel.groups.borrow()[&MonitorSection::Cpu].visible.clone();
    group_visible.set_active(false);
    pump(30);
    group_visible.set_active(true);
    pump(30);
    assert!(
        runtime.layout.sections[&MonitorSection::Cpu]
            .root
            .is_visible()
    );
    assert_eq!(
        runtime.layout.sections[&MonitorSection::Cpu]
            .signature
            .borrow()[0]
            .0,
        "uptime"
    );
    assert!(!frequency.is_active());
    panel.groups.borrow()[&MonitorSection::Temperatures]
        .expand
        .set_active(true);
    toggle(MonitorSection::Temperatures, "cpu:temp1").set_active(false);
    let mut snapshot = metric_selection_snapshot();
    snapshot.temperatures.remove(0);
    let publish = |snapshot: &SystemSnapshot| {
        controller.latest.replace(snapshot.clone());
        controller
            .snapshot_subscribers
            .borrow_mut()
            .retain(|subscriber| subscriber(snapshot));
        pump(60);
    };
    publish(&snapshot);
    publish(&metric_selection_snapshot());
    assert!(
        controller.update_settings(|settings| settings.network_interface = Some("test0".into()))
    );
    pump(50);
    assert!(
        panel
            .network_choices
            .borrow()
            .iter()
            .any(|(id, label)| id == "test0" && label.contains("unavailable"))
    );
    let mut connected = metric_selection_snapshot();
    connected.network_choices = vec!["test0".into()];
    publish(&connected);
    assert!(
        panel
            .network_choices
            .borrow()
            .iter()
            .any(|(id, label)| id == "test0" && label == "test0")
    );
    assert!(!toggle(MonitorSection::Temperatures, "cpu:temp1").is_active());
    assert!(
        panel.groups.borrow()[&MonitorSection::Temperatures]
            .expand
            .is_active()
    );
    assert_eq!(
        runtime.layout.sections[&MonitorSection::Temperatures]
            .signature
            .borrow()
            .len(),
        1
    );
    scroll.set_value(200.0);
    pump(200);
    if let Ok(path) = std::env::var("OBSIDIAN_MONITOR_TEST_SNAPSHOT") {
        let snapshot = gtk::Snapshot::new();
        let paintable = gtk::WidgetPaintable::new(Some(&runtime.surface));
        paintable.snapshot(
            &snapshot,
            f64::from(runtime.surface.width()),
            f64::from(runtime.surface.height()),
        );
        runtime
            .window
            .renderer()
            .unwrap()
            .render_texture(snapshot.to_node().unwrap(), None)
            .save_to_png(path)
            .unwrap();
    }
    let toggles = panel
        .groups
        .borrow()
        .values()
        .flat_map(|group| group.toggles.clone())
        .collect::<Vec<_>>();
    for toggle in toggles {
        toggle.set_active(false);
    }
    pump(50);
    assert!(runtime.layout.empty_state.is_visible());
    assert_eq!(runtime.layout.empty_title.text(), "No metrics selected");
    runtime.set_settings_open(false);
    pump(300);
    runtime.set_settings_open(true);
    pump(300);
    assert!(expand.is_active());
    assert!(!frequency.is_active());
    assert_eq!(MonitorSettings::load(), controller.settings());
    // Settings have their own timeout even when the monitor is pinned.
    runtime.settings_hovered.set(false);
    runtime.sync_reveal();
    pump(400);
    assert!(runtime.settings_reveal.reveals_child());
    runtime.settings_hovered.set(true);
    runtime.sync_reveal();
    pump(1100);
    assert!(
        runtime.settings_reveal.reveals_child(),
        "returning cancels settings timeout"
    );
    let popup = settings_popover(&panel.output).unwrap();
    popup.set_autohide(false);
    popup.popup();
    runtime.settings_hovered.set(false);
    runtime.sync_reveal();
    pump(1100);
    assert!(
        runtime.settings_reveal.reveals_child(),
        "an open menu keeps settings available"
    );
    popup.popdown();
    // The one-second delay starts when the menu finishes hiding, not when
    // popdown starts its closing animation. Allow for compositor latency.
    let menu_deadline = Instant::now() + Duration::from_secs(1);
    while popup.is_visible() && Instant::now() < menu_deadline {
        pump(5);
    }
    assert!(!popup.is_visible(), "the output menu must finish closing");
    pump(400);
    assert!(runtime.settings_reveal.reveals_child());
    pump(750);
    assert!(!runtime.settings_reveal.reveals_child());
    pump(250);
    assert!(
        runtime.window.is_visible(),
        "pin keeps monitoring visible after settings close"
    );
    assert!(controller.set_enabled(false));
    controller.start();
    assert!(!controller.gpu_sampling_enabled.load(Ordering::Acquire));
    assert!(controller.timer.borrow().is_none());
    drop(view);
    pump(20);
}

#[test]
fn network_selection_does_not_fall_back_when_selected_device_disappears() {
    let content = "header\nheader\n eth0: 100 0 0 0 0 0 0 0 200 0 0 0 0 0 0 0\n wlan0: 300 0 0 0 0 0 0 0 400 0 0 0 0 0 0 0\n";
    let defaults = HashSet::from(["eth0".to_owned()]);
    let (automatic, _) = parse_network_counters(content, None, &defaults, |_| true).unwrap();
    let (manual, _) = parse_network_counters(content, Some("wlan0"), &defaults, |_| true).unwrap();
    assert_eq!(automatic.received, 100);
    assert_eq!(manual.received, 300);
    assert_eq!(manual.transmitted, 400);
    assert!(network_rates(&automatic, &manual).is_none());
    assert!(parse_network_counters(content, Some("missing0"), &defaults, |_| true).is_none());
    assert!(parse_network_counters(content, Some("wlan0"), &defaults, |_| false).is_none());
    let ipv6 = "00000000000000000000000000000000 00 00000000000000000000000000000000 00 fe800000000000000000000000000001 00000400 00000001 00000000 00000003 wlan0";
    assert_eq!(
        parse_default_interfaces("", ipv6),
        HashSet::from(["wlan0".to_owned()])
    );
}

#[test]
fn economy_interval_and_hidden_sensor_queries_are_independent() {
    let mut settings = MonitorSettings {
        interval_ms: 500,
        ..MonitorSettings::default()
    };
    assert_eq!(
        settings.effective_interval(true),
        Duration::from_millis(500)
    );
    assert_eq!(settings.effective_interval(false), Duration::from_secs(5));
    settings.economy = false;
    assert_eq!(
        settings.effective_interval(false),
        Duration::from_millis(500)
    );
    settings.interval_ms = 10_000;
    settings.economy = true;
    assert_eq!(settings.effective_interval(false), Duration::from_secs(10));
    settings.set_metric_visible(MonitorSection::Temperatures, "test", false);
    // A hidden metric must skip the filesystem read, even if it would fail.
    assert_eq!(
        sensor_value(
            Path::new("/no-such-monitor-sensor"),
            &settings,
            MonitorSection::Temperatures,
            "test",
            false,
            1000.0,
            -50.0..=200.0
        )
        .1,
        gpu::DataState::Paused
    );
}

#[test]
fn monitor_dimensions_migrate_and_save_independently() {
    let old = glib::KeyFile::new();
    old.set_integer(SETTINGS_GROUP, "scale", 1090);
    let mut settings = MonitorSettings::from_key_file(&old);
    assert_eq!(settings.width, 305);
    assert_eq!(settings.scale_milli, 1090);
    assert_eq!(settings.height, None);
    settings.width = 420;
    settings.height = Some(600);
    settings.scale_milli = 1500;
    settings.hide_delay_ms = 8500;
    settings.interval_ms = 2500;
    settings.economy = false;
    settings.output = Some("DP-2".into());
    settings.network_interface = Some("wlan0".into());
    settings
        .metric_names
        .insert("cpu:cpu".into(), "Процессор · загрузка".into());
    let restored = MonitorSettings::from_key_file(&settings.to_key_file());
    assert_eq!(restored, settings);
    assert_eq!(
        MonitorSettings::from_key_file(&glib::KeyFile::new()),
        MonitorSettings::default()
    );
}

#[test]
fn grouped_sections_migrate_to_independent_switches() {
    let key_file = glib::KeyFile::new();
    key_file.set_string(SETTINGS_GROUP, "order", "performance,hardware,memory");
    key_file.set_string(SETTINGS_GROUP, "hidden", "hardware");
    let settings = MonitorSettings::from_key_file(&key_file);
    assert_eq!(
        settings.sections[..4]
            .iter()
            .map(|item| item.section)
            .collect::<Vec<_>>(),
        [
            MonitorSection::Cpu,
            MonitorSection::Gpu,
            MonitorSection::Power,
            MonitorSection::Cooling
        ]
    );
    assert!(settings.sections[..2].iter().all(|item| item.visible));
    assert!(settings.sections[2..4].iter().all(|item| !item.visible));
    assert_eq!(settings.hide_delay_ms, 2000);
    assert_eq!(
        MonitorSettings::from_key_file(&settings.to_key_file()),
        settings
    );
}

#[test]
fn gpu_power_and_cooling_are_independent_of_cpu_and_memory() {
    let snapshot = SystemSnapshot {
        ready: true,
        gpus: vec![gpu::GpuDevice {
            id: "0000:01:00.0".into(),
            name: "Test GPU".into(),
            state: gpu::DataState::Ready,
            data: GpuSnapshot {
                utilization_percent: Some(50.0),
                memory_used: Some(1024),
                power_watts: Some(75.0),
                ..GpuSnapshot::default()
            },
        }],
        fans: vec![SensorReading {
            id: "fan1".into(),
            label: "CPU Fan".into(),
            value: 1200.0,
            state: gpu::DataState::Ready,
        }],
        battery: Some(BatterySnapshot {
            percent: 80.0,
            status: "Discharging".into(),
            power_watts: None,
        }),
        ..SystemSnapshot::default()
    };
    assert!(display_metrics(MonitorSection::Cpu, &snapshot).0.is_empty());
    assert!(
        display_metrics(MonitorSection::Memory, &snapshot)
            .0
            .is_empty()
    );
    assert_eq!(display_metrics(MonitorSection::Gpu, &snapshot).0.len(), 4);
    assert_eq!(display_metrics(MonitorSection::Power, &snapshot).0.len(), 1);
    assert_eq!(
        display_metrics(MonitorSection::Cooling, &snapshot).0.len(),
        1
    );
    assert!(!metric_options(MonitorSection::Battery, &snapshot).is_empty());
}

#[test]
fn disk_mounts_include_second_ssd_without_counting_bind_mounts_twice() {
    let mounts = disk_mounts(
        "1 0 259:2 / / rw - ext4 /dev/nvme0n1p2 rw\n2 1 259:2 /nix/store /nix/store ro - ext4 /dev/nvme0n1p2 ro\n3 0 259:4 / /mnt/games rw - ext4 /dev/nvme1n1p1 rw\n4 0 0:7 / /tmp rw - tmpfs tmpfs rw\n5 0 7:0 / /app ro - squashfs /dev/loop0 ro\n6 0 8:1 / /mnt/My\\040Disk rw - ext4 /dev/sda1 rw",
    );
    assert_eq!(
        mounts
            .iter()
            .map(|(_, path)| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        ["/", "/mnt/My Disk", "/mnt/games"]
    );
    let btrfs = disk_mounts(
        "1 0 0:21 /@ / rw - btrfs /dev/nvme0n1p2 rw\n2 0 0:22 /@home /home rw - btrfs /dev/nvme0n1p2 rw",
    );
    assert_eq!(btrfs.len(), 1);
    assert_eq!(btrfs[0].1, Path::new("/"));
}

#[test]
fn hidden_monitor_only_captures_the_visible_tab() {
    for width in [159, 285, 565] {
        let region = monitor_input_region(width, 600, false, width - 48, 40);
        for y in [drawer::TAIL_TOP, drawer::TAIL_TOP + drawer::TAIL_HEIGHT - 1] {
            assert!(!region.contains_point(width - 11, y));
            assert!(region.contains_point(width - 10, y));
            assert!(region.contains_point(width - 1, y));
        }
        for y in [
            0,
            drawer::TAIL_TOP - 1,
            drawer::TAIL_TOP + drawer::TAIL_HEIGHT,
            300,
            599,
        ] {
            assert!(
                !region.contains_point(width - 1, y),
                "empty edge must pass input through"
            );
        }
        assert!(!region.contains_point(width, 300));
        assert!(!region.contains_point(width - 1, 600));
    }
}

#[test]
fn open_monitor_passes_buttons_and_scrolls_to_content() {
    let region = monitor_input_region(285, 600, true, 236, 40);
    assert!(
        region.contains_point(100, 20),
        "drag handle stays stationary"
    );
    assert!(
        !region.contains_point(260, 20),
        "settings button gets native input"
    );
    assert!(
        !region.contains_point(100, 200),
        "body gets native scrolling"
    );
    assert!(
        region.contains_point(280, drawer::TAIL_TOP),
        "tab remains clickable"
    );
    assert!(
        !region.contains_point(280, 200),
        "empty edge passes input through"
    );
}

#[test]
#[ignore = "requires Wayland layer-shell and XDG_STATE_HOME=/tmp/obsidian-monitor-items-test-state"]
fn monitor_drawer_animation_keeps_width_and_survives_reversal() {
    assert!(settings_path().starts_with("/tmp/obsidian-monitor-items-test-state"));
    gtk::init().unwrap();
    let application = gtk::Application::builder()
        .application_id("dev.obsidian.MonitorDrawerTest")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    application.register(None::<&gio::Cancellable>).unwrap();
    let display = gdk::Display::default().unwrap();
    let css = gtk::CssProvider::new();
    css.load_from_data(include_str!("../../../assets/window.css"));
    gtk::style_context_add_provider_for_display(
        &display,
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    let monitor = display
        .monitors()
        .item(0)
        .unwrap()
        .downcast::<gdk::Monitor>()
        .unwrap();
    let controller = SystemMonitorController::new();
    // The metric-selection test deliberately hides every row. Never inherit
    // those saved settings (or the user's real settings) in a layout test.
    controller.settings.replace(MonitorSettings::default());
    controller.latest.replace(SystemSampler::default().sample());
    let view = SystemMonitorView::new(&application, &monitor, &controller);
    view.set_desktop_available(true);
    let runtime = &view.runtime;
    // Isolate animation assertions from the user's pointer and hover timer.
    for widget in [&runtime.surface, &runtime.hotspot] {
        let controllers = widget.observe_controllers();
        while let Some(controller) = controllers.item(0) {
            widget.remove_controller(&controller.downcast::<gtk::EventController>().unwrap());
        }
    }
    runtime.window.set_layer(Layer::Overlay);
    runtime.window.settings().set_gtk_enable_animations(true);
    runtime.pinned.set(true);
    runtime.hide_generation.bump();
    let pump = |ms: u64| {
        let until = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < until {
            glib::MainContext::default().iteration(false);
            std::thread::sleep(Duration::from_millis(1));
        }
    };
    // Warm up the renderer before checking intermediate animation frames.
    runtime.drawer.set_revealed(true);
    pump(800);
    runtime.drawer.set_revealed(false);
    pump(300);
    let frames = Rc::new(RefCell::new(Vec::new()));
    let sampled_frames = Rc::clone(&frames);
    let tick = runtime.window.add_tick_callback(move |window, clock| {
        if let Some(surface) = window.surface() {
            sampled_frames
                .borrow_mut()
                .push((clock.frame_time(), surface.width()));
        }
        glib::ControlFlow::Continue
    });
    runtime.drawer.set_revealed(true);
    pump(80);
    let width = runtime.window.surface().unwrap().width();
    let opening_margin = runtime.window.margin(Edge::Right);
    assert!(opening_margin < drawer::RIGHT_MARGIN);
    pump(80);
    assert_eq!(runtime.window.surface().unwrap().width(), width);
    assert!(runtime.window.margin(Edge::Right) > opening_margin);
    tick.remove();
    runtime.drawer.set_revealed(false);
    pump(45);
    let reversing_margin = runtime.window.margin(Edge::Right);
    runtime.drawer.set_revealed(true);
    assert_eq!(runtime.window.margin(Edge::Right), reversing_margin);
    pump(350);
    assert!(runtime.drawer.is_open());
    assert!(runtime.window.is_visible());
    assert_eq!(runtime.window.margin(Edge::Right), drawer::RIGHT_MARGIN);
    assert_eq!(runtime.window.surface().unwrap().width(), width);
    let frames = frames.borrow();
    let mut intervals = frames
        .windows(2)
        .map(|pair| pair[1].0 - pair[0].0)
        .collect::<Vec<_>>();
    assert!(frames.len() > 2);
    intervals.sort_unstable();
    let median = intervals[intervals.len() / 2];
    eprintln!(
        "Monitor refresh: {} mHz; animation frames: {}; median interval: {} us; longest interval: {} us",
        monitor.refresh_rate(),
        frames.len(),
        median,
        intervals.last().unwrap(),
    );
    if monitor.refresh_rate() > 0 {
        let refresh_interval = 1_000_000_000 / i64::from(monitor.refresh_rate());
        assert!(
            median < refresh_interval * 3 / 2,
            "animation must follow the output refresh rate"
        );
    }
    assert!(frames.iter().all(|(_, frame_width)| *frame_width == width));
    let height = runtime.window.surface().unwrap().height();
    let y = runtime.window.margin(Edge::Top);
    runtime.set_settings_open(true);
    runtime.settings_hovered.set(true);
    pump(300);
    assert!(runtime.settings_reveal.is_child_revealed());
    assert!(runtime.window.surface().unwrap().width() > width);
    assert_eq!(runtime.window.surface().unwrap().height(), height);
    assert_eq!(runtime.window.margin(Edge::Top), y);
    assert_monitor_height_fits(runtime);
    if let Ok(path) = std::env::var("OBSIDIAN_MONITOR_TEST_SNAPSHOT") {
        fn descendants(widget: &gtk::Widget) -> Vec<gtk::Widget> {
            let mut result = vec![widget.clone()];
            let mut child = widget.first_child();
            while let Some(widget) = child {
                result.extend(descendants(&widget));
                child = widget.next_sibling();
            }
            result
        }
        let capture = |widget: &gtk::Widget, path: &str| {
            let snapshot = gtk::Snapshot::new();
            gtk::WidgetPaintable::new(Some(widget)).snapshot(
                &snapshot,
                f64::from(widget.width()),
                f64::from(widget.height()),
            );
            runtime
                .window
                .renderer()
                .unwrap()
                .render_texture(
                    snapshot.to_node().unwrap_or_else(|| {
                        panic!(
                            "empty snapshot {path}: visible={}, mapped={}, size={}x{}",
                            widget.is_visible(),
                            widget.is_mapped(),
                            widget.width(),
                            widget.height()
                        )
                    }),
                    None,
                )
                .save_to_png(path)
                .unwrap();
        };
        capture(
            runtime.surface.upcast_ref(),
            &format!("{path}.collapsed.png"),
        );
        let adjustments = descendants(runtime.settings_panel.root.upcast_ref())
            .into_iter()
            .find_map(|widget| widget.downcast::<gtk::Expander>().ok())
            .unwrap();
        assert!(!adjustments.is_expanded());
        adjustments.set_expanded(true);
        pump(200);
        assert_eq!(runtime.window.surface().unwrap().height(), height);
        for hovered in [false, true] {
            for slider in [
                &runtime.settings_panel.width,
                &runtime.settings_panel.height,
                &runtime.settings_panel.scale,
                &runtime.settings_panel.hide_delay,
                &runtime.settings_panel.interval,
            ] {
                if hovered {
                    slider.set_state_flags(gtk::StateFlags::PRELIGHT, false);
                }
            }
            pump(50);
            let output = if hovered {
                format!("{path}.hover.png")
            } else {
                path.clone()
            };
            capture(runtime.surface.upcast_ref(), &output);
        }
        for slider in [
            &runtime.settings_panel.width,
            &runtime.settings_panel.height,
            &runtime.settings_panel.scale,
            &runtime.settings_panel.hide_delay,
            &runtime.settings_panel.interval,
        ] {
            slider.unset_state_flags(gtk::StateFlags::PRELIGHT);
            let controllers = slider.observe_controllers();
            for index in 0..controllers.n_items() {
                if let Some(scroll) = controllers
                    .item(index)
                    .and_then(|item| item.downcast::<gtk::EventControllerScroll>().ok())
                {
                    eprintln!(
                        "Monitor slider scroll phase: {:?}",
                        scroll.propagation_phase()
                    );
                }
            }
        }
        for (name, dropdown) in [
            ("display", &runtime.settings_panel.output),
            ("network", &runtime.settings_panel.network),
        ] {
            let popup = descendants(dropdown.upcast_ref())
                .into_iter()
                .find_map(|widget| widget.downcast::<gtk::Popover>().ok())
                .unwrap();
            // Keep manual desktop input from dismissing the visual fixture.
            popup.set_autohide(false);
            popup.popup();
            pump(150);
            eprintln!(
                "Monitor {name} menu: visible={}, mapped={}, size={}x{}, settings={}",
                popup.is_visible(),
                popup.is_mapped(),
                popup.width(),
                popup.height(),
                runtime.settings_reveal.reveals_child()
            );
            if std::env::var_os("OBSIDIAN_MONITOR_TEST_DESKTOP").is_some() {
                let output = format!("{path}.{name}.desktop.png");
                assert!(
                    std::process::Command::new("niri")
                        .args([
                            "msg",
                            "action",
                            "screenshot-screen",
                            "--path",
                            &output,
                            "--show-pointer",
                            "false"
                        ])
                        .status()
                        .unwrap()
                        .success()
                );
                pump(150);
            }
            capture(popup.upcast_ref(), &format!("{path}.{name}.png"));
            if let Some(row) = descendants(popup.upcast_ref()).into_iter().find(|widget| {
                widget.css_name() == "row"
                    && !widget.state_flags().contains(gtk::StateFlags::SELECTED)
            }) {
                row.set_state_flags(gtk::StateFlags::PRELIGHT, false);
                pump(150);
                capture(popup.upcast_ref(), &format!("{path}.{name}.hover.png"));
            }
            for label in descendants(popup.upcast_ref())
                .into_iter()
                .filter_map(|widget| widget.downcast::<gtk::Label>().ok())
            {
                if label.is_mapped() {
                    let color = label.color();
                    assert!(
                        color.red() > 0.9 && color.green() > 0.9 && color.blue() > 0.9,
                        "popup text must remain readable: {} ({color})",
                        label.text()
                    );
                }
            }
            popup.popdown();
            pump(150);
        }
    }
    let settings_width = runtime.window.surface().unwrap().width();
    runtime.drawer.set_revealed(false);
    pump(80);
    assert_eq!(runtime.window.surface().unwrap().width(), settings_width);
    pump(300);
    assert!(!runtime.window.is_visible());
    assert!(!runtime.settings_reveal.reveals_child());
    assert!(runtime.hotspot_window.is_visible());
    runtime.drawer.set_revealed(true);
    pump(300);
    for (width, height, font_scale) in [
        (420, 500, 1000),
        (356, 851, 1090),
        (420, 500, 2000),
        (240, 350, 1000),
        (180, 160, 2182),
        (800, 9999, 1000),
    ] {
        let mut settings = controller.settings();
        settings.width = width;
        settings.height = Some(height);
        settings.scale_milli = font_scale;
        settings.pinned = true;
        controller.settings.replace(settings.clone());
        controller
            .settings_subscribers
            .borrow_mut()
            .retain(|subscriber| subscriber(&settings));
        pump(100);
        assert!(runtime.card.width() >= width);
        assert!(runtime.card.width() <= PANEL_MAX_WIDTH);
        assert_monitor_text_fits(runtime.card.upcast_ref());
        let geometry = runtime.geometry();
        assert!(runtime.card.height() >= height.min(geometry.height_limits().1));
        assert!(runtime.card.height() <= geometry.height_limits().1);
        assert_monitor_height_fits(runtime);
        let height = runtime.card.height();
        let scroll = runtime.scroller.vadjustment();
        scroll.set_value(scroll.upper() - scroll.page_size());
        pump(30);
        assert_monitor_bottom_visible(runtime);
        scroll.set_value(0.0);
        assert_eq!(
            runtime.settings_panel.width.value(),
            f64::from(runtime.card.width())
        );
        assert_eq!(runtime.settings_panel.height.value(), f64::from(height));
        assert!(runtime.y.get() >= geometry.top_margin);
        assert!(
            runtime.y.get() + runtime.window.height() <= geometry.screen_height - PANEL_EDGE_MARGIN
        );
        let trigger = runtime
            .header
            .last_child()
            .unwrap()
            .compute_bounds(&runtime.card)
            .unwrap();
        assert!(runtime.drag_handle_size().0 as f32 <= trigger.x());
        let window_height = runtime.window.surface().unwrap().height();
        let y = runtime.window.margin(Edge::Top);
        for open in [true, false] {
            runtime.set_settings_open(open);
            for elapsed in [80, 220] {
                pump(elapsed);
                assert_eq!(runtime.window.surface().unwrap().height(), window_height);
                assert_eq!(runtime.card.height(), height);
                assert_eq!(runtime.window.margin(Edge::Top), y);
            }
            if open && height == PANEL_MIN_HEIGHT {
                let scroll = runtime.settings_scroller.vadjustment();
                assert!(scroll.upper() > scroll.page_size());
                let bottom = scroll.upper() - scroll.page_size();
                scroll.set_value(bottom);
                pump(20);
                assert_eq!(scroll.value(), bottom, "all settings must remain reachable");
                scroll.set_value(0.0);
            }
        }
    }
    runtime.hide_delay.set(Duration::from_millis(150));
    runtime.hovered.set(false);
    runtime.hotspot_hovered.set(false);
    runtime.pinned.set(false);
    runtime.sync_reveal();
    pump(60);
    assert!(runtime.window.is_visible());
    runtime.hovered.set(true);
    runtime.sync_reveal();
    pump(400);
    assert!(
        runtime.window.is_visible(),
        "returning to the panel cancels the timeout"
    );
    runtime.hovered.set(false);
    runtime.sync_reveal();
    pump(450);
    assert!(
        !runtime.window.is_visible(),
        "panel hides after the configured delay"
    );
    runtime.window.settings().set_gtk_enable_animations(false);
    runtime.drawer.set_revealed(true);
    assert!(runtime.drawer.is_open());
    runtime.drawer.set_revealed(false);
    assert!(!runtime.window.is_visible());
    drop(view);
    pump(20);
}

#[test]
fn section_order_is_deduplicated_and_completed() {
    let hidden = HashSet::from([MonitorSection::Network]);
    let sections = normalized_sections(
        vec![
            MonitorSection::Storage,
            MonitorSection::Cpu,
            MonitorSection::Storage,
        ],
        &hidden,
    );
    assert_eq!(sections.len(), MonitorSection::ALL.len());
    assert_eq!(sections[0].section, MonitorSection::Storage);
    assert_eq!(sections[1].section, MonitorSection::Cpu);
    assert!(
        !sections
            .iter()
            .find(|preference| preference.section == MonitorSection::Network)
            .unwrap()
            .visible
    );
}

#[test]
fn cpu_usage_uses_deltas_and_includes_iowait_as_idle() {
    let previous = parse_cpu_times("cpu  100 0 50 800 50 0 0 0").unwrap();
    let current = parse_cpu_times("cpu  150 0 70 850 70 0 0 0").unwrap();
    assert!((cpu_usage(previous, current).unwrap() - 50.0).abs() < 0.001);
}

#[test]
fn guest_cpu_time_is_not_counted_twice() {
    let previous = parse_cpu_times("cpu 100 0 50 800 50 0 0 0 80 0").unwrap();
    let current = parse_cpu_times("cpu 150 0 70 850 70 0 0 0 120 0").unwrap();
    assert!((cpu_usage(previous, current).unwrap() - 50.0).abs() < 0.001);
    assert!(parse_cpu_times("cpu 18446744073709551615 1 0 0").is_none());
    assert!(parse_cpu_times("cpu 1 2").is_none());
}

#[test]
fn rates_ignore_counter_resets() {
    let previous = NetworkCounters {
        read_at: Instant::now(),
        received: 10_000,
        transmitted: 20_000,
        interfaces: vec!["wlan0".to_owned()],
    };
    let current = NetworkCounters {
        read_at: previous.read_at + Duration::from_secs(2),
        received: 100,
        transmitted: 200,
        interfaces: vec!["wlan0".to_owned()],
    };
    assert_eq!(network_rates(&previous, &current), Some((0.0, 0.0)));
}

#[test]
fn rates_reset_when_the_interface_set_changes() {
    let previous = NetworkCounters {
        read_at: Instant::now(),
        received: 10_000,
        transmitted: 20_000,
        interfaces: vec!["wlan0".to_owned()],
    };
    let current = NetworkCounters {
        read_at: previous.read_at + Duration::from_secs(1),
        received: 50_000,
        transmitted: 80_000,
        interfaces: vec!["eth0".to_owned()],
    };

    assert_eq!(network_rates(&previous, &current), None);
}

#[test]
fn byte_format_is_compact_and_binary() {
    assert_eq!(format_bytes(1536), "2 KiB");
    assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
}

#[test]
fn panel_stays_on_screen_as_content_grows_and_shrinks() {
    let geometry = PanelGeometry {
        screen_width: 1280,
        screen_height: 720,
        panel_width: 230,
        top_margin: 54,
    };
    assert_eq!(geometry.clamp_y(-100, 480), 54);
    assert_eq!(geometry.clamp_y(500, 480), 232);
    assert_eq!(geometry.clamp_y(500, 120), 500);
    assert_eq!(geometry.clamp_y(500, 900), 54);
    assert_eq!(geometry.height_limits(), (160, 658));
    assert_eq!(geometry.clamp_y(500, geometry.height_limits().1), 54);
}

#[test]
fn common_kernel_sensor_labels_keep_their_device_context() {
    assert_eq!(
        sensor_display_label(
            "CPU",
            "k10temp",
            Some("Tccd2"),
            SensorKind::Temperature,
            "4",
        ),
        "CPU CCD 2 Temperature"
    );
    assert_eq!(
        sensor_display_label(
            "Example FastDrive SSD",
            "nvme",
            Some("Sensor 1"),
            SensorKind::Temperature,
            "2",
        ),
        "Example FastDrive SSD Sensor 1 Temperature"
    );
    assert_eq!(
        sensor_display_label(
            "RAM Module 2",
            "spd5118",
            None,
            SensorKind::Temperature,
            "1",
        ),
        "RAM Module 2 Temperature"
    );
}

#[test]
fn storage_models_are_compacted_without_model_specific_rules() {
    assert_eq!(
        compact_storage_model("ExampleCorp SSD FastDrive with Heatsink 2TB"),
        Some("ExampleCorp FastDrive".to_owned())
    );
    assert_eq!(
        compact_storage_model("Vendor_Model NVMe 1000GB"),
        Some("Vendor Model".to_owned())
    );
}

#[test]
fn laptop_thermal_zone_names_are_humanized() {
    assert_eq!(thermal_zone_device_label("x86_pkg_temp"), "CPU Package");
    assert_eq!(thermal_zone_device_label("acpitz"), "ACPI Thermal Zone");
    assert_eq!(thermal_zone_device_label("iwlwifi_1"), "Wi-Fi Adapter");
}

#[test]
#[ignore = "requires Wayland layer-shell and XDG_STATE_HOME=/tmp/obsidian-monitor-items-test-state"]
fn monitor_opens_only_on_click_and_closes_on_occupied_desktop() {
    use crate::widgets::test_support::pump;
    assert!(settings_path().starts_with("/tmp/obsidian-monitor-items-test-state"));
    gtk::init().unwrap();
    let css = gtk::CssProvider::new();
    css.load_from_data(include_str!("../../../assets/window.css"));
    gtk::style_context_add_provider_for_display(
        &gdk::Display::default().unwrap(),
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    let application = gtk::Application::builder()
        .application_id("dev.obsidian.MonitorClickTest")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    application.register(gio::Cancellable::NONE).unwrap();
    let monitor = gdk::Display::default()
        .unwrap()
        .monitors()
        .item(0)
        .unwrap()
        .downcast::<gdk::Monitor>()
        .unwrap();
    let controller = SystemMonitorController::new();
    controller.settings.replace(MonitorSettings {
        pinned: true,
        ..MonitorSettings::default()
    });
    let view = SystemMonitorView::new(&application, &monitor, &controller);
    let runtime = &view.runtime;
    runtime.window.set_layer(Layer::Overlay);
    runtime.hotspot_window.set_layer(Layer::Overlay);
    assert!(
        !runtime.hotspot_window.is_visible(),
        "wait for known desktop state at startup"
    );
    view.set_desktop_available(true);
    pump(100);
    let controllers = runtime.hotspot.observe_controllers();
    let motion = (0..controllers.n_items())
        .find_map(|index| {
            controllers
                .item(index)?
                .downcast::<gtk::EventControllerMotion>()
                .ok()
        })
        .unwrap();
    motion.emit_by_name::<()>("enter", &[&0.0_f64, &0.0_f64]);
    pump(350);
    assert!(
        !runtime.window.is_visible(),
        "hover and pin must not open a closed monitor"
    );
    runtime.tail.emit_clicked();
    assert!(runtime.drawer.is_revealed());
    let deadline = Instant::now() + Duration::from_secs(2);
    while !runtime.drawer.is_open() && Instant::now() < deadline {
        pump(10);
    }
    assert!(
        runtime.drawer.is_open(),
        "visible={} mapped={} margin={} width={}",
        runtime.window.is_visible(),
        runtime.window.is_mapped(),
        runtime.window.margin(Edge::Right),
        runtime.window.width()
    );
    runtime.tail.emit_clicked();
    pump(250);
    assert!(
        !runtime.window.is_visible(),
        "the same tab closes even a pinned panel"
    );
    runtime.tail.emit_clicked();
    pump(50);
    view.set_desktop_available(false);
    assert!(!runtime.window.is_visible());
    assert!(!runtime.hotspot_window.is_visible());
    pump(350);
    assert!(
        !runtime.drawer.is_revealed(),
        "a cancelled animation must not restore the monitor"
    );
    runtime.tail.emit_clicked();
    assert!(!runtime.window.is_visible());
    view.set_desktop_available(true);
    pump(350);
    assert!(runtime.hotspot_window.is_visible());
    assert!(
        !runtime.window.is_visible(),
        "returning to an empty desktop still requires a click"
    );
    runtime.tail.emit_clicked();
    pump(350);
    runtime.set_settings_open(true);
    pump(250);
    view.set_desktop_available(false);
    assert!(!runtime.settings_reveal.reveals_child());
    assert!(!runtime.window.is_visible());
    assert_eq!(
        runtime.window.keyboard_mode(),
        gtk4_layer_shell::KeyboardMode::None
    );
    drop(view);
}
