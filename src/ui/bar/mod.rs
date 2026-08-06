use std::{cell::Cell, error::Error, fmt, rc::Rc, time::Duration};

use gtk::{gdk, glib, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use niri_ipc::{Window, Workspace};

use crate::widgets::{
    audio::AudioIndicator,
    audio_spectrum::AudioSpectrumController,
    bar_features::BarFeatureController,
    bluetooth::{BluetoothAgent, BluetoothIndicator},
    brightness::BrightnessIndicator,
    clock::ClockIndicator,
    keyboard::KeyboardIndicator,
    launcher::{LauncherCatalog, LauncherIndicator},
    network::NetworkIndicator,
    osd::OsdController,
    player::{PlayerController, PlayerIndicator},
    power::PowerIndicator,
    tooltip::BarTooltip,
    tray::{TrayController, TrayIndicator},
    wallpaper::{WallpaperController, WallpaperIndicator},
    workspace::WorkspaceIndicator,
};

const NAMESPACE: &str = "obsidian-bar-main";
const BAR_VISIBLE_TOP_MARGIN: i32 = 4;
const BAR_FALLBACK_HEIGHT: i32 = 42;
// At the hidden endpoint the bar bottom is exactly at the output's top edge.
// The always-enabled automatic exclusive zone follows the animated margin, so
// the client window moves away at the same time as the bar enters the screen.
const BAR_HIDDEN_TOP_MARGIN: i32 = -BAR_FALLBACK_HEIGHT;
const BAR_STARTUP_DELAY: Duration = Duration::from_secs(1);
const BAR_SHOW_DURATION_MS: f64 = 420.0;
const BAR_HIDE_DURATION_MS: f64 = 280.0;
const BAR_MIN_ANIMATION_DURATION_MS: f64 = 70.0;
const FULLSCREEN_SIZE_TOLERANCE: f64 = 1.5;

struct BarSlideState {
    generation: Cell<u64>,
    target_hidden: Cell<bool>,
    current_margin: Cell<f64>,
    applied_margin: Cell<i32>,
    started: Cell<bool>,
    startup_complete: Cell<bool>,
}

pub struct BarDependencies<'a> {
    pub(crate) bluetooth_agent: &'a BluetoothAgent,
    pub(crate) bar_features: &'a Rc<BarFeatureController>,
    pub(crate) audio_spectrum: &'a Rc<AudioSpectrumController>,
    pub(crate) launcher_catalog: &'a Rc<LauncherCatalog>,
    pub(crate) player_controller: &'a PlayerController,
    pub(crate) tray_controller: &'a TrayController,
    pub(crate) wallpaper_controller: &'a Rc<WallpaperController>,
    pub(crate) osd_controller: &'a OsdController,
}

pub struct Bar {
    window: gtk::ApplicationWindow,
    monitor: gdk::Monitor,
    keyboard: KeyboardIndicator,
    keyboard_section: gtk::Box,
    workspaces: WorkspaceIndicator,
    clock: ClockIndicator,
    player: PlayerIndicator,
    audio: AudioIndicator,
    bluetooth: BluetoothIndicator,
    network: NetworkIndicator,
    launcher: LauncherIndicator,
    tray: TrayIndicator,
    brightness: BrightnessIndicator,
    power: PowerIndicator,
    wallpaper: WallpaperIndicator,
    tooltip: BarTooltip,
    slide: Rc<BarSlideState>,
}

impl Bar {
    pub fn new(
        application: &gtk::Application,
        monitor: &gdk::Monitor,
        initial_layout: &str,
        initial_workspaces: &[Workspace],
        dependencies: BarDependencies<'_>,
    ) -> Result<Self, BarError> {
        if !gtk4_layer_shell::is_supported() {
            return Err(BarError::LayerShellUnsupported);
        }

        let window = gtk::ApplicationWindow::builder()
            .application(application)
            .decorated(false)
            .build();

        window.add_css_class("bar-window");
        window.init_layer_shell();
        window.set_namespace(Some(NAMESPACE));
        window.set_layer(Layer::Top);
        // Keep keyboard mode stable during the slide. Geometry is updated in
        // one frame callback so margin and exclusive zone stay synchronized.
        window.set_keyboard_mode(KeyboardMode::OnDemand);
        window.set_focusable(false);
        window.set_can_target(false);
        window.set_monitor(Some(monitor));

        window.set_anchor(Edge::Top, true);
        window.set_anchor(Edge::Left, true);
        window.set_anchor(Edge::Right, true);
        window.set_anchor(Edge::Bottom, false);
        window.set_margin(Edge::Top, BAR_HIDDEN_TOP_MARGIN);
        window.set_margin(Edge::Left, 9);
        window.set_margin(Edge::Right, 9);
        window.auto_exclusive_zone_enable();

        let tooltip = BarTooltip::new(application, monitor);
        let keyboard = KeyboardIndicator::new(initial_layout);
        let player =
            PlayerIndicator::new(dependencies.player_controller, dependencies.bar_features);
        let wallpaper = WallpaperIndicator::new(
            application,
            &window,
            monitor,
            dependencies.wallpaper_controller,
            dependencies.bar_features,
            dependencies.audio_spectrum,
        );
        let launcher =
            LauncherIndicator::new(application, &window, monitor, dependencies.launcher_catalog);
        let tray = TrayIndicator::new(dependencies.tray_controller);
        let bluetooth =
            BluetoothIndicator::new(application, &window, monitor, dependencies.bluetooth_agent);
        let network = NetworkIndicator::new(application, &window, monitor);
        let brightness = BrightnessIndicator::new();
        let audio = AudioIndicator::new(application, &window, monitor, dependencies.osd_controller);
        let power = PowerIndicator::new();
        let workspaces =
            WorkspaceIndicator::new(monitor, initial_workspaces, dependencies.bar_features);

        make_revealers_exclusive(&[brightness.revealer(), audio.revealer(), power.revealer()]);

        let clock = ClockIndicator::new(application, &window, monitor);

        let clock_section = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        clock_section.add_css_class("section");
        clock_section.set_valign(gtk::Align::Center);
        clock_section.append(wallpaper.widget());
        clock_section.append(clock.widget());

        let keyboard_section = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        keyboard_section.add_css_class("section");
        keyboard_section.set_valign(gtk::Align::Center);
        keyboard_section.set_visible(initial_layout != "--");
        keyboard_section.append(keyboard.widget());

        let modules = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        modules.add_css_class("bar-modules");
        modules.set_halign(gtk::Align::Start);
        modules.set_valign(gtk::Align::Center);
        modules.append(&clock_section);
        modules.append(&keyboard_section);
        modules.append(player.widget());

        let controls_section = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        controls_section.add_css_class("section");
        controls_section.add_css_class("section-right-main");
        controls_section.add_css_class("controls-container");
        controls_section.set_halign(gtk::Align::End);
        controls_section.set_valign(gtk::Align::Center);
        controls_section.append(launcher.widget());
        controls_section.append(bluetooth.widget());
        controls_section.append(network.widget());
        controls_section.append(brightness.widget());
        controls_section.append(audio.widget());
        controls_section.append(power.widget());

        let right_cluster = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        right_cluster.add_css_class("bar-end");
        right_cluster.set_halign(gtk::Align::End);
        right_cluster.set_valign(gtk::Align::Center);
        right_cluster.append(tray.widget());
        right_cluster.append(&controls_section);

        let content = gtk::CenterBox::new();
        content.add_css_class("bar-content");
        content.set_hexpand(true);
        content.set_overflow(gtk::Overflow::Hidden);
        content.set_start_widget(Some(&modules));
        content.set_center_widget(Some(workspaces.widget()));
        content.set_end_widget(Some(&right_cluster));

        window.set_child(Some(&content));

        Ok(Self {
            window,
            monitor: monitor.clone(),
            keyboard,
            keyboard_section,
            workspaces,
            clock,
            player,
            audio,
            bluetooth,
            network,
            launcher,
            tray,
            brightness,
            power,
            wallpaper,
            tooltip,
            slide: Rc::new(BarSlideState {
                generation: Cell::new(0),
                target_hidden: Cell::new(false),
                current_margin: Cell::new(BAR_HIDDEN_TOP_MARGIN as f64),
                applied_margin: Cell::new(BAR_HIDDEN_TOP_MARGIN),
                started: Cell::new(false),
                startup_complete: Cell::new(false),
            }),
        })
    }

    pub fn monitor(&self) -> &gdk::Monitor {
        &self.monitor
    }

    pub fn present(&self) {
        self.window.present();
        if !self.slide.started.replace(true) {
            let weak_window = self.window.downgrade();
            let weak_slide = Rc::downgrade(&self.slide);
            let tooltip = self.tooltip.clone();
            glib::timeout_add_local_once(BAR_STARTUP_DELAY, move || {
                let (Some(window), Some(slide)) = (weak_window.upgrade(), weak_slide.upgrade())
                else {
                    return;
                };
                if !slide.started.get() || slide.startup_complete.replace(true) {
                    return;
                }

                let hidden = slide.target_hidden.get();
                animate_bar_slide(window, slide, tooltip, hidden);
            });
        }
    }

    pub fn close(&self) {
        self.slide.started.set(false);
        self.slide
            .generation
            .set(self.slide.generation.get().wrapping_add(1));
        self.dismiss_transient_ui();
        self.tooltip.close();
        self.window.close();
    }

    pub fn show_launcher(&self) -> bool {
        if self.slide.target_hidden.get() {
            return false;
        }

        self.launcher.show_launcher();
        true
    }

    pub fn set_keyboard_layout(&self, layout: &str) {
        self.keyboard.set_layout(layout);
        self.keyboard_section.set_visible(layout != "--");
    }

    pub fn set_workspaces(&self, workspaces: &[Workspace]) {
        self.workspaces.set_workspaces(workspaces);
    }

    pub fn set_fullscreen_state(&self, windows: &[Window], workspaces: &[Workspace]) {
        self.set_hidden(output_has_fullscreen_window(
            &self.monitor,
            windows,
            workspaces,
        ));
    }

    fn set_hidden(&self, hidden: bool) {
        let changed = self.slide.target_hidden.replace(hidden) != hidden;
        if !self.slide.started.get() || !self.slide.startup_complete.get() {
            return;
        }

        if changed {
            if hidden {
                self.dismiss_transient_ui();
            }
            self.animate_slide(hidden);
        }
    }

    fn dismiss_transient_ui(&self) {
        self.tooltip.hide();
        self.clock.dismiss();
        self.player.dismiss();
        self.audio.dismiss();
        self.bluetooth.dismiss();
        self.network.dismiss();
        self.launcher.dismiss();
        self.tray.dismiss();
        self.brightness.dismiss();
        self.power.dismiss();
        self.wallpaper.dismiss();
    }

    fn animate_slide(&self, hidden: bool) {
        animate_bar_slide(
            self.window.clone(),
            Rc::clone(&self.slide),
            self.tooltip.clone(),
            hidden,
        );
    }
}

fn animate_bar_slide(
    window: gtk::ApplicationWindow,
    slide: Rc<BarSlideState>,
    tooltip: BarTooltip,
    hidden: bool,
) {
    let generation = slide.generation.get().wrapping_add(1);
    slide.generation.set(generation);

    let target_margin = if hidden {
        BAR_HIDDEN_TOP_MARGIN as f64
    } else {
        BAR_VISIBLE_TOP_MARGIN as f64
    };
    let start_margin = slide.current_margin.get();

    if hidden {
        tooltip.hide();
        window.set_can_target(false);
        window.set_focusable(false);
    } else {
        window.set_focusable(true);
        window.set_can_target(true);
    }

    if (start_margin - target_margin).abs() < f64::EPSILON {
        finish_bar_slide(&window, &slide, target_margin);
        return;
    }

    let duration_ms = bar_slide_duration_ms(hidden, start_margin, target_margin);
    let weak_window = window.downgrade();
    let weak_slide = Rc::downgrade(&slide);
    let start_time_us = Cell::new(None::<i64>);

    window.add_tick_callback(move |_, frame_clock| {
        let (Some(window), Some(slide)) = (weak_window.upgrade(), weak_slide.upgrade()) else {
            return glib::ControlFlow::Break;
        };
        if slide.generation.get() != generation {
            return glib::ControlFlow::Break;
        }

        let now_us = frame_clock.frame_time();
        let start_us = match start_time_us.get() {
            Some(value) => value,
            None => {
                start_time_us.set(Some(now_us));
                now_us
            }
        };
        let elapsed_ms = (now_us - start_us).max(0) as f64 / 1_000.0;
        let progress = (elapsed_ms / duration_ms).clamp(0.0, 1.0);
        let eased = if hidden {
            progress * progress * progress
        } else {
            1.0 - (1.0 - progress).powi(3)
        };
        let margin = start_margin + (target_margin - start_margin) * eased;

        apply_bar_margin(&window, &slide, margin);

        if progress < 1.0 {
            glib::ControlFlow::Continue
        } else {
            finish_bar_slide(&window, &slide, target_margin);
            glib::ControlFlow::Break
        }
    });
}

fn bar_slide_duration_ms(hidden: bool, start_margin: f64, target_margin: f64) -> f64 {
    let full_duration_ms = if hidden {
        BAR_HIDE_DURATION_MS
    } else {
        BAR_SHOW_DURATION_MS
    };
    let full_distance = f64::from(BAR_VISIBLE_TOP_MARGIN - BAR_HIDDEN_TOP_MARGIN).abs();
    let distance_ratio = ((target_margin - start_margin).abs() / full_distance).clamp(0.0, 1.0);

    (full_duration_ms * distance_ratio).max(BAR_MIN_ANIMATION_DURATION_MS)
}

fn apply_bar_margin(window: &gtk::ApplicationWindow, slide: &BarSlideState, margin: f64) {
    slide.current_margin.set(margin);

    let rounded_margin = margin.round() as i32;
    if slide.applied_margin.replace(rounded_margin) != rounded_margin {
        // Auto exclusive zone stays enabled for the surface's entire lifetime.
        // gtk4-layer-shell updates the reserved area from this same margin.
        window.set_margin(Edge::Top, rounded_margin);
    }
}

fn finish_bar_slide(window: &gtk::ApplicationWindow, slide: &BarSlideState, target_margin: f64) {
    apply_bar_margin(window, slide, target_margin);
}

fn output_has_fullscreen_window(
    monitor: &gdk::Monitor,
    windows: &[Window],
    workspaces: &[Workspace],
) -> bool {
    let Some(output) = monitor.connector() else {
        return false;
    };
    let Some(workspace) = workspaces.iter().find(|workspace| {
        workspace.is_active && workspace.output.as_deref() == Some(output.as_str())
    }) else {
        return false;
    };

    let geometry = monitor.geometry();
    let width = geometry.width() as f64;
    let height = geometry.height() as f64;

    let Some(active_window_id) = workspace.active_window_id else {
        return false;
    };
    let Some(window) = windows
        .iter()
        .find(|window| window.id == active_window_id && window.workspace_id == Some(workspace.id))
    else {
        return false;
    };

    size_matches_output(window.layout.tile_size.0, width)
        && size_matches_output(window.layout.tile_size.1, height)
}

fn size_matches_output(actual: f64, expected: f64) -> bool {
    (actual - expected).abs() <= FULLSCREEN_SIZE_TOLERANCE
}

fn make_revealers_exclusive(revealers: &[&gtk::Revealer]) {
    for (index, revealer) in revealers.iter().enumerate() {
        let peers = revealers
            .iter()
            .enumerate()
            .filter_map(|(peer_index, peer)| {
                if peer_index == index {
                    None
                } else {
                    Some(peer.downgrade())
                }
            })
            .collect::<Vec<_>>();

        revealer.connect_reveal_child_notify(move |revealer| {
            if !revealer.reveals_child() {
                return;
            }

            for peer in &peers {
                if let Some(peer) = peer.upgrade() {
                    peer.set_reveal_child(false);
                }
            }
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarError {
    LayerShellUnsupported,
}

impl fmt::Display for BarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LayerShellUnsupported => {
                f.write_str("the current Wayland compositor does not support layer-shell")
            }
        }
    }
}

impl Error for BarError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(left: f64, right: f64) -> bool {
        (left - right).abs() < 0.001
    }

    #[test]
    fn full_slide_uses_configured_duration() {
        assert!(close(
            bar_slide_duration_ms(
                true,
                BAR_VISIBLE_TOP_MARGIN as f64,
                BAR_HIDDEN_TOP_MARGIN as f64,
            ),
            BAR_HIDE_DURATION_MS,
        ));
        assert!(close(
            bar_slide_duration_ms(
                false,
                BAR_HIDDEN_TOP_MARGIN as f64,
                BAR_VISIBLE_TOP_MARGIN as f64,
            ),
            BAR_SHOW_DURATION_MS,
        ));
    }

    #[test]
    fn short_reverse_slide_does_not_take_the_full_duration() {
        let duration = bar_slide_duration_ms(true, -40.0, BAR_HIDDEN_TOP_MARGIN as f64);
        assert!(close(duration, BAR_MIN_ANIMATION_DURATION_MS));
    }

    #[test]
    fn slide_endpoints_match_the_reserved_work_area() {
        assert_eq!(BAR_HIDDEN_TOP_MARGIN + BAR_FALLBACK_HEIGHT, 0);
        assert_eq!(BAR_VISIBLE_TOP_MARGIN + BAR_FALLBACK_HEIGHT, 46);
    }

    #[test]
    fn fullscreen_size_match_has_a_small_absolute_tolerance() {
        assert!(size_matches_output(1920.0, 1920.0));
        assert!(size_matches_output(1919.0, 1920.0));
        assert!(!size_matches_output(1918.0, 1920.0));
    }
}
