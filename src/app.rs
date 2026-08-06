use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

use gtk::{gdk, gio, glib, prelude::*};
use niri_ipc::{Window, Workspace};
use tracing::{error, info, warn};

use crate::{
    niri::ipc,
    ui::bar::{Bar, BarDependencies},
    widgets::{
        audio_spectrum::AudioSpectrumController, bar_features::BarFeatureController,
        bluetooth::BluetoothAgent, launcher::LauncherCatalog, osd::OsdController,
        player::PlayerController, tray::TrayController, wallpaper::WallpaperController,
    },
};

const APP_ID: &str = "dev.obsidian.Bar";
const WINDOW_CSS: &str = include_str!("../assets/window.css");
const FULLSCREEN_SYNC_DELAY: Duration = Duration::from_millis(75);

pub struct App {
    application: gtk::Application,
    bars: RefCell<Vec<Bar>>,
    monitor_model: RefCell<Option<gio::ListModel>>,
    monitor_handler: RefCell<Option<glib::SignalHandlerId>>,
    hold_guard: RefCell<Option<gio::ApplicationHoldGuard>>,
    shutting_down: Cell<bool>,
    niri_listener: RefCell<Option<ipc::EventListener>>,
    keyboard_layout: RefCell<String>,
    windows: RefCell<Vec<Window>>,
    workspaces: RefCell<Vec<Workspace>>,
    fullscreen_sync_generation: Cell<u64>,
    bluetooth_agent: BluetoothAgent,
    bar_features: Rc<BarFeatureController>,
    audio_spectrum: Rc<AudioSpectrumController>,
    player: PlayerController,
    tray: TrayController,
    launcher_catalog: Rc<LauncherCatalog>,
    wallpaper: Rc<WallpaperController>,
    osd: RefCell<Option<OsdController>>,
}

impl App {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            application: gtk::Application::builder()
                .application_id(APP_ID)
                .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
                .build(),
            bars: RefCell::new(Vec::new()),
            monitor_model: RefCell::new(None),
            monitor_handler: RefCell::new(None),
            hold_guard: RefCell::new(None),
            shutting_down: Cell::new(false),
            niri_listener: RefCell::new(None),
            keyboard_layout: RefCell::new("--".to_owned()),
            windows: RefCell::new(Vec::new()),
            workspaces: RefCell::new(Vec::new()),
            fullscreen_sync_generation: Cell::new(0),
            bluetooth_agent: BluetoothAgent::default(),
            bar_features: BarFeatureController::new(),
            audio_spectrum: AudioSpectrumController::new(),
            player: PlayerController::new(),
            tray: TrayController::new(),
            launcher_catalog: LauncherCatalog::new(),
            wallpaper: WallpaperController::new(),
            osd: RefCell::new(None),
        })
    }

    pub fn run(self: Rc<Self>) -> glib::ExitCode {
        self.connect_signals();
        self.application.run()
    }

    fn connect_signals(self: &Rc<Self>) {
        let weak_self = Rc::downgrade(self);
        self.application.connect_startup(move |_| {
            let Some(this) = weak_self.upgrade() else {
                return;
            };

            if let Err(err) = this.load_window_css() {
                error!(%err, "failed to load window css");
            }
        });

        let weak_self = Rc::downgrade(self);
        self.application.connect_activate(move |application| {
            let Some(this) = weak_self.upgrade() else {
                return;
            };

            this.activate(application);
        });

        let weak_self = Rc::downgrade(self);
        self.application
            .connect_command_line(move |application, command_line| {
                let Some(this) = weak_self.upgrade() else {
                    return glib::ExitCode::FAILURE;
                };

                let show_launcher = command_line
                    .arguments()
                    .iter()
                    .skip(1)
                    .any(|argument| argument.to_str() == Some("launcher"));

                application.activate();
                if show_launcher {
                    let weak_self = Rc::downgrade(&this);
                    glib::idle_add_local_once(move || {
                        if let Some(this) = weak_self.upgrade() {
                            this.show_launcher();
                        }
                    });
                }

                glib::ExitCode::SUCCESS
            });

        let weak_self = Rc::downgrade(self);
        self.application.connect_shutdown(move |_| {
            if let Some(this) = weak_self.upgrade() {
                this.shutdown();
            }
        });
    }

    fn activate(self: &Rc<Self>, application: &gtk::Application) {
        if self.hold_guard.borrow().is_none() {
            self.hold_guard.replace(Some(application.hold()));
        }

        let Some(display) = gdk::Display::default() else {
            error!("GTK display is unavailable");
            application.quit();
            return;
        };

        self.wallpaper.start(application);
        self.audio_spectrum.start();
        self.ensure_niri_listener();

        if self.monitor_model.borrow().is_none() {
            let monitors = display.monitors();
            let weak_self = Rc::downgrade(self);
            let application = application.clone();
            let handler = monitors.connect_items_changed(move |_, _, _, _| {
                if let Some(this) = weak_self.upgrade() {
                    this.sync_monitors(&application);
                }
            });

            self.monitor_handler.replace(Some(handler));
            self.monitor_model.replace(Some(monitors));
        }

        self.sync_monitors(application);
    }

    fn ensure_niri_listener(self: &Rc<Self>) {
        if self.niri_listener.borrow().is_some() {
            return;
        }

        let (sender, receiver) = async_channel::bounded::<ipc::Update>(8);
        self.niri_listener
            .replace(Some(ipc::spawn_event_listener(sender)));

        let weak_self = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            while let Ok(update) = receiver.recv().await {
                let Some(this) = weak_self.upgrade() else {
                    receiver.close();
                    break;
                };

                match update {
                    ipc::Update::KeyboardLayout(layout) => {
                        for bar in this.bars.borrow().iter() {
                            bar.set_keyboard_layout(&layout);
                        }
                        this.keyboard_layout.replace(layout);
                    }
                    ipc::Update::Windows(windows) => {
                        this.windows.replace(windows);
                        this.schedule_bar_fullscreen_sync();
                    }
                    ipc::Update::Workspaces(workspaces) => {
                        for bar in this.bars.borrow().iter() {
                            bar.set_workspaces(&workspaces);
                        }
                        this.workspaces.replace(workspaces);
                        this.schedule_bar_fullscreen_sync();
                    }
                }
            }
        });
    }

    fn sync_monitors(&self, application: &gtk::Application) {
        if self.shutting_down.get() {
            return;
        }

        let monitors: Vec<gdk::Monitor> = {
            let model = self.monitor_model.borrow();
            let Some(model) = model.as_ref() else {
                return;
            };

            (0..model.n_items())
                .filter_map(|index| model.item(index))
                .filter_map(|item| item.downcast::<gdk::Monitor>().ok())
                .collect()
        };

        let osd = monitors.first().map(|monitor| {
            let mut osd = self.osd.borrow_mut();
            let controller = osd
                .get_or_insert_with(|| OsdController::new(application, monitor))
                .clone();
            controller.set_monitor(monitor);
            controller
        });

        {
            let mut bars = self.bars.borrow_mut();
            bars.retain(|bar| {
                let keep = monitors.iter().any(|monitor| monitor == bar.monitor());
                if !keep {
                    info!(monitor = ?bar.monitor().connector(), "removing bar for monitor");
                    bar.close();
                }
                keep
            });

            if let Some(osd) = osd.as_ref() {
                let current_layout = self.keyboard_layout.borrow();
                let current_windows = self.windows.borrow();
                let current_workspaces = self.workspaces.borrow();
                for monitor in &monitors {
                    if bars.iter().any(|bar| bar.monitor() == monitor) {
                        continue;
                    }

                    match Bar::new(
                        application,
                        monitor,
                        current_layout.as_str(),
                        current_workspaces.as_slice(),
                        BarDependencies {
                            bluetooth_agent: &self.bluetooth_agent,
                            bar_features: &self.bar_features,
                            audio_spectrum: &self.audio_spectrum,
                            launcher_catalog: &self.launcher_catalog,
                            player_controller: &self.player,
                            tray_controller: &self.tray,
                            wallpaper_controller: &self.wallpaper,
                            osd_controller: osd,
                        },
                    ) {
                        Ok(bar) => {
                            info!(monitor = ?monitor.connector(), "bar created");
                            bar.set_fullscreen_state(
                                current_windows.as_slice(),
                                current_workspaces.as_slice(),
                            );
                            bar.present();
                            bars.push(bar);
                        }
                        Err(error) => {
                            error!(%error, monitor = ?monitor.connector(), "failed to create bar");
                        }
                    }
                }
            }
        }

        if monitors.is_empty() {
            warn!("no monitors are currently available; waiting for hotplug");
        }
    }

    fn sync_bar_fullscreen_state(&self) {
        let windows = self.windows.borrow();
        let workspaces = self.workspaces.borrow();
        for bar in self.bars.borrow().iter() {
            bar.set_fullscreen_state(windows.as_slice(), workspaces.as_slice());
        }
    }

    fn schedule_bar_fullscreen_sync(self: &Rc<Self>) {
        let generation = self.fullscreen_sync_generation.get().wrapping_add(1);
        self.fullscreen_sync_generation.set(generation);

        let weak_self = Rc::downgrade(self);
        glib::timeout_add_local_once(FULLSCREEN_SYNC_DELAY, move || {
            let Some(this) = weak_self.upgrade() else {
                return;
            };
            if this.shutting_down.get() || this.fullscreen_sync_generation.get() != generation {
                return;
            }

            this.sync_bar_fullscreen_state();
        });
    }

    fn show_launcher(&self) {
        for bar in self.bars.borrow().iter() {
            if bar.show_launcher() {
                break;
            }
        }
    }

    fn shutdown(&self) {
        if self.shutting_down.replace(true) {
            return;
        }
        self.fullscreen_sync_generation
            .set(self.fullscreen_sync_generation.get().wrapping_add(1));

        self.wallpaper.shutdown();
        self.audio_spectrum.shutdown();
        if let Some(osd) = self.osd.borrow_mut().take() {
            osd.shutdown();
        }

        {
            let bars = self.bars.borrow();
            for bar in bars.iter() {
                bar.close();
            }
        }
        self.bars.borrow_mut().clear();

        drop(self.niri_listener.borrow_mut().take());

        if let Some(handler) = self.monitor_handler.borrow_mut().take()
            && let Some(model) = self.monitor_model.borrow().as_ref()
        {
            model.disconnect(handler);
        }
        drop(self.monitor_model.borrow_mut().take());
        drop(self.hold_guard.borrow_mut().take());
    }

    fn load_window_css(&self) -> Result<(), &'static str> {
        let Some(display) = gdk::Display::default() else {
            return Err("GTK display is unavailable");
        };

        let provider = gtk::CssProvider::new();
        provider.connect_parsing_error(|_, _, error| {
            error!(%error, "window css parsing error");
        });
        provider.load_from_data(WINDOW_CSS);

        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );

        Ok(())
    }
}
