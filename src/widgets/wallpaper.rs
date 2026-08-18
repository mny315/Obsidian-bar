use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    env,
    ffi::{OsStr, OsString},
    fmt, fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use gdk_pixbuf::{InterpType, Pixbuf};
use gio::prelude::*;
use gtk::gdk::prelude::GdkCairoContextExt;
use gtk::{gdk, glib, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use tracing::{info, warn};

use super::tooltip::BarTooltipExt;
use super::{
    BAR_POPUP_TOP_MARGIN, Generation, PopupReveal, SmoothScrollConfig, attach_bar_click_dismiss,
    attach_popup_focus_dismiss,
    audio_spectrum::AudioSpectrumController,
    bar_features::{BarFeatureController, BarFeatureState},
    clear_box, command, detach_application_window, install_smooth_scroll, run_background,
    run_background_async, set_optional_label, set_spinner_active,
};

const SETTINGS_GROUP: &str = "wallpaper";
const SETTINGS_FILE: &str = "wallpaper.ini";
const DEFAULT_OUTPUT: &str = "ALL";
const DEFAULT_MPV_OPTIONS: &str = "config=no no-audio loop-file=inf image-display-duration=inf reset-on-next-file=pause hwdec=auto-safe panscan=1.0 terminal=no input-terminal=no input-default-bindings=no osc=no osd-level=0";
static MPVPAPER: command::ExternalProgram = command::ExternalProgram::new(
    "OBSIDIAN_BAR_MPVPAPER_BIN",
    option_env!("OBSIDIAN_BAR_MPVPAPER_BIN"),
    "mpvpaper",
);
static SWAYBG: command::ExternalProgram = command::ExternalProgram::new(
    "OBSIDIAN_BAR_SWAYBG_BIN",
    option_env!("OBSIDIAN_BAR_SWAYBG_BIN"),
    "swaybg",
);
static FFMPEG: command::ExternalProgram = command::ExternalProgram::new(
    "OBSIDIAN_BAR_FFMPEG_BIN",
    option_env!("OBSIDIAN_BAR_FFMPEG_BIN"),
    "ffmpeg",
);

const GRID_COLUMNS: i32 = 3;
const CARD_WIDTH: i32 = 144;
const CARD_HEIGHT: i32 = 84;
const GRID_GAP: u32 = 8;
const GALLERY_WIDTH: i32 = CARD_WIDTH * GRID_COLUMNS + (GRID_GAP as i32) * (GRID_COLUMNS - 1);
const GALLERY_HEIGHT: i32 = CARD_HEIGHT * 6 + (GRID_GAP as i32) * 5;
const SMOOTH_SCROLL: SmoothScrollConfig = SmoothScrollConfig::new(96.0, 130.0, 72.0);
const DEFAULT_RANDOM_INTERVAL_MINUTES: u32 = 30;
const MIN_RANDOM_INTERVAL_MINUTES: u32 = 1;
const MAX_RANDOM_INTERVAL_MINUTES: u32 = 24 * 60;
const THUMBNAIL_VERSION: &str = "cover-144x84-v2";
const VIDEO_STILL_VERSION: &str = "video-still-v1";
const TRANSITION_DURATION: Duration = Duration::from_millis(620);
const REFRESH_ANIMATION_MIN_DURATION: Duration = Duration::from_secs(2);
const MPV_IPC_LOAD_TIMEOUT: Duration = Duration::from_millis(2500);
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(15);
const TRANSITION_SETTLE_FRAMES: u32 = 2;
const TRANSITION_BACKEND_SETTLE_DELAY: Duration = Duration::from_millis(160);
const RESUME_SUBSCRIPTION_RETRY_BASE_DELAY: Duration = Duration::from_secs(1);
const RESUME_SUBSCRIPTION_RETRY_MAX_DELAY: Duration = Duration::from_secs(30);
const TRANSITION_WARMUP_FRAMES: u32 = 2;
const TRANSITION_WARMUP_PROGRESS: f64 = 0.000_001;
const TRANSITION_FRAME_CLOCK_TIMEOUT: Duration = Duration::from_millis(1800);
const MPVPAPER_READY_TIMEOUT: Duration = Duration::from_millis(1500);
const MPV_REQUEST_VO_CONFIGURED: u64 = 1;
const MPV_REQUEST_LOAD_FILE: u64 = 2;
const MPV_REQUEST_UNPAUSE: u64 = 3;
const IMAGE_THUMBNAIL_WORKERS: usize = 2;
const VIDEO_THUMBNAIL_WORKERS: usize = 2;
const THUMBNAIL_QUEUE_CAPACITY: usize = 256;
const TRANSITION_NAMESPACE: &str = "obsidian-wallpaper-transition";
const PICKER_NAMESPACE: &str = "obsidian-bar-wallpaper";

const ICON_WALLPAPER: &str = "\u{f0e09}";
const ICON_FOLDER: &str = "\u{f024b}";
const ICON_REFRESH: &str = "\u{f0450}";
const ICON_VIDEO: &str = "\u{f040a}";
const ICON_BACK: &str = "\u{f004d}";
const ICON_UP: &str = "\u{f005d}";
const ICON_HOME: &str = "\u{f02dc}";
const ICON_SHUFFLE: &str = "\u{f049d}";
const ICON_PLAYER_ENABLED: &str = "󰎇";
const ICON_PLAYER_DISABLED: &str = "󰎈";
const ICON_WORKSPACE: &str = "󰕰";
const ICON_EQUALIZER: &str = "󰺢";

const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp"];
const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mkv", "webm", "mov", "m4v", "avi"];

#[derive(Clone, Debug)]
struct WallpaperSettings {
    directory: PathBuf,
    current: Option<PathBuf>,
    random_enabled: bool,
    random_interval_minutes: u32,
}

impl Default for WallpaperSettings {
    fn default() -> Self {
        Self {
            directory: default_wallpaper_directory(),
            current: None,
            random_enabled: false,
            random_interval_minutes: DEFAULT_RANDOM_INTERVAL_MINUTES,
        }
    }
}

impl WallpaperSettings {
    fn load() -> Self {
        let defaults = Self::default();
        let key_file = glib::KeyFile::new();

        if key_file
            .load_from_file(settings_path(), glib::KeyFileFlags::NONE)
            .is_err()
        {
            return defaults;
        }

        let directory = key_file
            .string(SETTINGS_GROUP, "directory")
            .ok()
            .map(PathBuf::from)
            .filter(|candidate| candidate.is_absolute() && candidate.is_dir())
            .unwrap_or_else(|| defaults.directory.clone());

        let current = key_file
            .string(SETTINGS_GROUP, "current")
            .ok()
            .map(PathBuf::from)
            .filter(|candidate| candidate.is_absolute() && candidate.is_file());

        let random_enabled = key_file
            .boolean(SETTINGS_GROUP, "random_enabled")
            .unwrap_or(false);
        let random_interval_minutes = key_file
            .integer(SETTINGS_GROUP, "random_interval_minutes")
            .ok()
            .and_then(|value| u32::try_from(value).ok())
            .map(|value| value.clamp(MIN_RANDOM_INTERVAL_MINUTES, MAX_RANDOM_INTERVAL_MINUTES))
            .unwrap_or(defaults.random_interval_minutes);

        Self {
            directory,
            current,
            random_enabled,
            random_interval_minutes,
        }
    }

    fn save(&self) -> Result<(), WallpaperError> {
        let path = settings_path();
        let parent = path.parent().ok_or(WallpaperError::StatePath)?;
        fs::create_dir_all(parent).map_err(WallpaperError::Io)?;

        let key_file = glib::KeyFile::new();
        key_file.set_string(
            SETTINGS_GROUP,
            "directory",
            &self.directory.to_string_lossy(),
        );
        if let Some(current) = &self.current {
            key_file.set_string(SETTINGS_GROUP, "current", &current.to_string_lossy());
        }
        key_file.set_boolean(SETTINGS_GROUP, "random_enabled", self.random_enabled);
        key_file.set_integer(
            SETTINGS_GROUP,
            "random_interval_minutes",
            self.random_interval_minutes as i32,
        );

        let temporary = path.with_extension("ini.tmp");
        if let Err(error) = key_file.save_to_file(&temporary) {
            let _ = fs::remove_file(&temporary);
            return Err(WallpaperError::Glib(error));
        }
        if let Err(error) = fs::rename(&temporary, &path) {
            let _ = fs::remove_file(&temporary);
            return Err(WallpaperError::Io(error));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct WallpaperSnapshot {
    directory: PathBuf,
    current: Option<PathBuf>,
}

struct RenderedGallery {
    directory: PathBuf,
    current: Option<PathBuf>,
    loaded: bool,
}

type LiveWallpaperCards = Rc<RefCell<HashMap<PathBuf, glib::WeakRef<gtk::Overlay>>>>;

#[derive(Clone)]
struct GalleryView {
    model: gio::ListStore,
    active_wallpaper: Rc<RefCell<Option<PathBuf>>>,
    live_wallpaper_cards: LiveWallpaperCards,
    scroller: glib::WeakRef<gtk::ScrolledWindow>,
    empty: glib::WeakRef<gtk::Box>,
    count: glib::WeakRef<gtk::Label>,
    notice: glib::WeakRef<gtk::Label>,
    rendered: Rc<RefCell<RenderedGallery>>,
    refresh_generation: Rc<Generation>,
    refresh_button: glib::WeakRef<gtk::Button>,
    refresh_icon: glib::WeakRef<gtk::Label>,
    refresh_spinner: glib::WeakRef<gtk::Spinner>,
}

struct GalleryRenderWidgets<'a> {
    scroller: &'a gtk::ScrolledWindow,
    empty: &'a gtk::Box,
    count: &'a gtk::Label,
    notice: &'a gtk::Label,
}

struct OwnedMpvpaper {
    process: gio::Subprocess,
    ipc_socket: PathBuf,
}

impl OwnedMpvpaper {
    fn is_alive(&self) -> bool {
        self.process.identifier().is_some()
    }
}

impl Drop for OwnedMpvpaper {
    fn drop(&mut self) {
        if self.process.identifier().is_some() {
            self.process.force_exit();
        }
        let _ = fs::remove_file(&self.ipc_socket);
    }
}

struct OwnedSwaybg {
    process: gio::Subprocess,
}

impl OwnedSwaybg {
    fn is_alive(&self) -> bool {
        self.process.identifier().is_some()
    }
}

impl Drop for OwnedSwaybg {
    fn drop(&mut self) {
        if self.process.identifier().is_some() {
            self.process.force_exit();
        }
    }
}

enum OwnedWallpaperBackend {
    Image(OwnedSwaybg),
    Video(OwnedMpvpaper),
}

impl OwnedWallpaperBackend {
    fn is_alive(&self) -> bool {
        match self {
            Self::Image(process) => process.is_alive(),
            Self::Video(process) => process.is_alive(),
        }
    }

    fn matches_path_kind(&self, path: &Path) -> bool {
        match self {
            Self::Image(_) => is_image_wallpaper(path),
            Self::Video(_) => is_video_wallpaper(path),
        }
    }

    fn mpv_ipc_socket(&self) -> Option<&Path> {
        match self {
            Self::Video(process) => Some(&process.ipc_socket),
            Self::Image(_) => None,
        }
    }
}

type ApplyErrorHandler = Rc<dyn Fn(&WallpaperError)>;

struct PendingApply {
    path: PathBuf,
    on_error: ApplyErrorHandler,
}

pub struct WallpaperController {
    settings: RefCell<WallpaperSettings>,
    process: RefCell<Option<OwnedWallpaperBackend>>,
    application: RefCell<Option<gtk::Application>>,
    subscribers: RefCell<Vec<async_channel::Sender<WallpaperSnapshot>>>,
    sleep_subscription: RefCell<Option<gio::SignalSubscription>>,
    sleep_subscription_pending: Cell<bool>,
    sleep_subscription_retry_pending: Cell<bool>,
    sleep_subscription_retry_attempt: Cell<u32>,
    started: Cell<bool>,
    applying: Cell<bool>,
    pending_apply: RefCell<Option<PendingApply>>,
    lifecycle_generation: Generation,
    random_source: RefCell<Option<glib::SourceId>>,
    random_pick_busy: Cell<bool>,
    random_nonce: Cell<u64>,
    transition_nonce: Cell<u64>,
    last_transition: Cell<Option<WallpaperTransitionKind>>,
}

impl WallpaperController {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            settings: RefCell::new(WallpaperSettings::load()),
            process: RefCell::new(None),
            application: RefCell::new(None),
            subscribers: RefCell::new(Vec::new()),
            sleep_subscription: RefCell::new(None),
            sleep_subscription_pending: Cell::new(false),
            sleep_subscription_retry_pending: Cell::new(false),
            sleep_subscription_retry_attempt: Cell::new(0),
            started: Cell::new(false),
            applying: Cell::new(false),
            pending_apply: RefCell::new(None),
            lifecycle_generation: Generation::default(),
            random_source: RefCell::new(None),
            random_pick_busy: Cell::new(false),
            random_nonce: Cell::new(0),
            transition_nonce: Cell::new(0),
            last_transition: Cell::new(None),
        })
    }

    pub fn start(self: &Rc<Self>, application: &gtk::Application) {
        self.application.replace(Some(application.clone()));
        if self.started.replace(true) {
            return;
        }
        self.lifecycle_generation.bump();

        if let Err(error) = self.restore_animated_at_startup() {
            warn!(%error, "failed to restore wallpaper at startup");
        }
        self.subscribe_to_resume();
        self.reschedule_random_timer();
    }

    pub fn shutdown(&self) {
        self.lifecycle_generation.bump();
        self.started.set(false);

        if let Some(source) = self.random_source.borrow_mut().take() {
            source.remove();
        }
        drop(self.sleep_subscription.borrow_mut().take());
        self.sleep_subscription_pending.set(false);
        self.sleep_subscription_retry_pending.set(false);
        self.stop_owned_process();
        drop(self.application.borrow_mut().take());
        self.applying.set(false);
        self.random_pick_busy.set(false);
        drop(self.pending_apply.borrow_mut().take());
    }

    fn subscribe(&self) -> async_channel::Receiver<WallpaperSnapshot> {
        let (sender, receiver) = async_channel::bounded(1);
        let mut subscribers = self.subscribers.borrow_mut();
        subscribers.retain(|subscriber| !subscriber.is_closed());
        subscribers.push(sender);
        receiver
    }

    fn snapshot(&self) -> WallpaperSnapshot {
        let settings = self.settings.borrow();
        WallpaperSnapshot {
            directory: settings.directory.clone(),
            current: settings.current.clone(),
        }
    }

    fn broadcast(&self) {
        let snapshot = self.snapshot();
        self.subscribers
            .borrow_mut()
            .retain(|sender| sender.force_send(snapshot.clone()).is_ok());
    }

    fn persist_settings_update(
        &self,
        update: impl FnOnce(&mut WallpaperSettings),
    ) -> Result<(), WallpaperError> {
        let mut next = self.settings.borrow().clone();
        update(&mut next);
        next.save()?;
        self.settings.replace(next);
        Ok(())
    }

    fn set_current_runtime(&self, path: PathBuf) -> Result<(), WallpaperError> {
        let save_result = {
            let mut settings = self.settings.borrow_mut();
            settings.current = Some(path.clone());
            settings.save()
        };
        self.broadcast();
        save_result.map_err(|error| WallpaperError::AppliedButNotSaved(path, error.to_string()))
    }

    fn set_directory(&self, directory: PathBuf) -> Result<(), WallpaperError> {
        if !directory.is_absolute() || !directory.is_dir() {
            return Err(WallpaperError::InvalidDirectory(directory));
        }

        self.persist_settings_update(|settings| settings.directory = directory)?;
        self.broadcast();
        Ok(())
    }

    fn random_config(&self) -> (bool, u32) {
        let settings = self.settings.borrow();
        (settings.random_enabled, settings.random_interval_minutes)
    }

    fn set_random_enabled(self: &Rc<Self>, enabled: bool) -> Result<(), WallpaperError> {
        self.persist_settings_update(|settings| settings.random_enabled = enabled)?;
        self.reschedule_random_timer();
        Ok(())
    }

    fn set_random_interval_minutes(self: &Rc<Self>, minutes: u32) -> Result<(), WallpaperError> {
        let minutes = minutes.clamp(MIN_RANDOM_INTERVAL_MINUTES, MAX_RANDOM_INTERVAL_MINUTES);
        self.persist_settings_update(|settings| settings.random_interval_minutes = minutes)?;
        self.reschedule_random_timer();
        Ok(())
    }

    fn reschedule_random_timer(self: &Rc<Self>) {
        if let Some(source) = self.random_source.borrow_mut().take() {
            source.remove();
        }
        if !self.started.get() {
            return;
        }

        let (enabled, minutes) = self.random_config();
        if !enabled {
            return;
        }

        let weak_controller = Rc::downgrade(self);
        let interval = Duration::from_secs(u64::from(minutes) * 60);
        let source = glib::timeout_add_local(interval, move || {
            let Some(controller) = weak_controller.upgrade() else {
                return glib::ControlFlow::Break;
            };

            if !controller.applying.get()
                && let Err(error) = controller.apply_random_wallpaper()
            {
                warn!(%error, "failed to apply random wallpaper");
            }
            glib::ControlFlow::Continue
        });
        self.random_source.replace(Some(source));
    }

    fn apply_random_wallpaper(self: &Rc<Self>) -> Result<(), WallpaperError> {
        if self.random_pick_busy.replace(true) {
            return Ok(());
        }

        let (directory, current) = {
            let settings = self.settings.borrow();
            (settings.directory.clone(), settings.current.clone())
        };
        if !directory.is_absolute() || !directory.is_dir() {
            self.random_pick_busy.set(false);
            return Err(WallpaperError::InvalidDirectory(directory));
        }

        let nonce = self.random_nonce.get().wrapping_add(1);
        self.random_nonce.set(nonce);
        let lifecycle = self.lifecycle_generation.current();
        let weak = Rc::downgrade(self);
        run_background(
            move || choose_random_wallpaper(&directory, current.as_deref(), nonce),
            move |result| {
                let Some(controller) = weak.upgrade() else {
                    return;
                };
                controller.random_pick_busy.set(false);
                if !controller.lifecycle_is_current(lifecycle) {
                    return;
                }
                match result {
                    Ok(Some(path)) => controller.request_apply_silent(path),
                    Ok(None) => {}
                    Err(error) => warn!(%error, "failed to choose random wallpaper"),
                }
            },
        );
        Ok(())
    }

    fn request_apply_silent(self: &Rc<Self>, path: PathBuf) {
        self.request_apply(path, |_| {});
    }

    fn request_apply<F>(self: &Rc<Self>, path: PathBuf, on_error: F)
    where
        F: Fn(&WallpaperError) + 'static,
    {
        if !self.started.get() {
            return;
        }

        let request = PendingApply {
            path,
            on_error: Rc::new(on_error),
        };

        if !self.try_begin_apply() {
            self.queue_apply(request);
            return;
        }

        let lifecycle = self.lifecycle_generation.current();
        let controller = Rc::clone(self);
        glib::MainContext::default().spawn_local(async move {
            let mut request = request;
            loop {
                if !controller.lifecycle_is_current(lifecycle) {
                    break;
                }

                let result = controller.apply_animated(request.path, lifecycle).await;
                if !controller.lifecycle_is_current(lifecycle) {
                    break;
                }

                if let Err(error) = result {
                    warn!(%error, "failed to apply wallpaper");
                    (request.on_error)(&error);
                }

                let Some(pending) = controller.take_pending_apply() else {
                    controller.finish_apply(lifecycle);
                    break;
                };
                request = pending;
            }
        });
    }

    fn try_begin_apply(&self) -> bool {
        !self.applying.replace(true)
    }

    fn queue_apply(&self, request: PendingApply) {
        self.pending_apply.replace(Some(request));
    }

    fn take_pending_apply(&self) -> Option<PendingApply> {
        self.pending_apply.borrow_mut().take()
    }

    fn lifecycle_is_current(&self, generation: u64) -> bool {
        self.started.get() && self.lifecycle_generation.is_current(generation)
    }

    fn finish_apply(&self, generation: u64) {
        if self.lifecycle_is_current(generation) {
            self.applying.set(false);
        }
    }

    async fn apply_animated(&self, path: PathBuf, lifecycle: u64) -> Result<(), WallpaperError> {
        if !self.lifecycle_is_current(lifecycle) {
            return Ok(());
        }
        if !path.is_absolute() || !path.is_file() || !is_supported_wallpaper(&path) {
            return Err(WallpaperError::InvalidWallpaper(path));
        }

        let same_path = self.settings.borrow().current.as_deref() == Some(path.as_path());
        let backend_matches = self
            .process
            .borrow()
            .as_ref()
            .is_some_and(|backend| backend.is_alive() && backend.matches_path_kind(&path));
        if same_path && backend_matches {
            return Ok(());
        }

        let transition_frame = if is_video_wallpaper(&path) {
            match cached_video_still_async(path.clone()).await {
                Ok(still) if self.lifecycle_is_current(lifecycle) => Some(still),
                Ok(_) => return Ok(()),
                Err(error) => {
                    warn!(path = %path.display(), %error, "failed to prepare video transition frame");
                    None
                }
            }
        } else {
            Some(path.clone())
        };

        let transition = match transition_frame.as_deref() {
            Some(frame) => match self.create_random_transition(frame) {
                Ok(transition) => Some(transition),
                Err(error) => {
                    warn!(%error, "native wallpaper transition unavailable; applying directly");
                    None
                }
            },
            None => None,
        };

        if let Some(transition) = transition.as_ref() {
            transition.present();
            transition.animate().await;
            if !self.lifecycle_is_current(lifecycle) {
                return Ok(());
            }
            transition.wait_frames(1).await;
            if !self.lifecycle_is_current(lifecycle) {
                return Ok(());
            }
        }

        let reuse_socket = if is_video_wallpaper(&path) {
            self.process
                .borrow()
                .as_ref()
                .filter(|backend| backend.is_alive())
                .and_then(|backend| backend.mpv_ipc_socket())
                .map(Path::to_path_buf)
        } else {
            None
        };

        if let Some(socket) = reuse_socket {
            if load_mpvpaper_file(socket, path.clone()).await {
                if !self.lifecycle_is_current(lifecycle) {
                    return Ok(());
                }
                let save_result = self.set_current_runtime(path.clone());
                info!(path = %path.display(), "mpvpaper wallpaper changed through IPC");
                if let Err(error) = save_result {
                    if let Some(transition) = transition.as_ref() {
                        transition.close();
                    }
                    return Err(error);
                }
                if let Some(transition) = transition.as_ref() {
                    transition.wait_for_backend_settle().await;
                    transition.close();
                }
                return Ok(());
            }

            warn!(
                path = %path.display(),
                "mpvpaper IPC switch failed; restarting wallpaper backend"
            );
        }

        if !self.lifecycle_is_current(lifecycle) {
            return Ok(());
        }
        let old_process = self.process.borrow_mut().take();
        let process = match spawn_wallpaper_backend(&path) {
            Ok(process) => process,
            Err(error) => {
                self.process.replace(old_process);
                if let Some(transition) = transition.as_ref() {
                    transition.close();
                }
                return Err(error);
            }
        };

        let mut backend_settled_before_swap = false;
        if let OwnedWallpaperBackend::Video(video) = &process {
            let ready = wait_for_mpvpaper_ready(video.ipc_socket.clone()).await;
            if !self.lifecycle_is_current(lifecycle) {
                return Ok(());
            }
            if !video.is_alive() {
                self.process.replace(old_process);
                if let Some(transition) = transition.as_ref() {
                    transition.close();
                }
                return Err(WallpaperError::Transition(
                    "mpvpaper exited before its video output became ready".into(),
                ));
            }
            if !ready {
                warn!(path = %path.display(), "mpvpaper VO readiness timed out; using guarded transition hold");
            }
        } else {
            if let Some(transition) = transition.as_ref() {
                transition.wait_for_backend_settle().await;
                backend_settled_before_swap = true;
                if !self.lifecycle_is_current(lifecycle) {
                    return Ok(());
                }
            }
            if !process.is_alive() {
                self.process.replace(old_process);
                if let Some(transition) = transition.as_ref() {
                    transition.close();
                }
                return Err(WallpaperError::Transition(
                    "swaybg exited before the image wallpaper became ready".into(),
                ));
            }
        }

        if let Some(old_process) = old_process {
            drop(old_process);
        }
        self.process.replace(Some(process));

        let save_result = self.set_current_runtime(path.clone());
        if is_video_wallpaper(&path) {
            info!(path = %path.display(), "mpvpaper video wallpaper started");
        } else {
            info!(path = %path.display(), "swaybg image wallpaper started");
        }
        if let Err(error) = save_result {
            if let Some(transition) = transition.as_ref() {
                transition.close();
            }
            return Err(error);
        }

        if let Some(transition) = transition.as_ref() {
            if backend_settled_before_swap {
                transition.wait_frames(1).await;
            } else {
                transition.wait_for_backend_settle().await;
            }
            transition.close();
        }

        Ok(())
    }

    fn create_random_transition(
        &self,
        frame: &Path,
    ) -> Result<WallpaperTransition, WallpaperError> {
        let application =
            self.application.borrow().clone().ok_or_else(|| {
                WallpaperError::Transition("GTK application is unavailable".into())
            })?;

        let nonce = self.transition_nonce.get().wrapping_add(1);
        self.transition_nonce.set(nonce);
        let kind = choose_transition_kind(self.last_transition.get(), nonce);
        self.last_transition.set(Some(kind));
        info!(transition = kind.name(), "selected wallpaper transition");

        WallpaperTransition::new(&application, frame, kind)
    }

    fn restore_animated_at_startup(self: &Rc<Self>) -> Result<(), WallpaperError> {
        if !self.started.get() {
            return Ok(());
        }

        let Some(path) = self.settings.borrow().current.clone() else {
            return Ok(());
        };

        if !path.is_file() {
            warn!(path = %path.display(), "saved wallpaper no longer exists");
            return Ok(());
        }

        if !path.is_absolute() || !is_supported_wallpaper(&path) {
            return Err(WallpaperError::InvalidWallpaper(path));
        }

        self.request_apply_silent(path);
        Ok(())
    }

    fn restore(&self) -> Result<(), WallpaperError> {
        if !self.started.get() {
            return Ok(());
        }

        let Some(path) = self.settings.borrow().current.clone() else {
            return Ok(());
        };

        if !path.is_file() {
            warn!(path = %path.display(), "saved wallpaper no longer exists");
            return Ok(());
        }

        if !path.is_absolute() || !is_supported_wallpaper(&path) {
            return Err(WallpaperError::InvalidWallpaper(path));
        }

        let process = spawn_wallpaper_backend(&path)?;
        let old_process = self.process.replace(Some(process));
        if let Some(old_process) = old_process {
            drop(old_process);
        }

        if is_video_wallpaper(&path) {
            info!(path = %path.display(), "mpvpaper video wallpaper restored");
        } else {
            info!(path = %path.display(), "swaybg image wallpaper restored");
        }
        self.broadcast();
        Ok(())
    }

    fn stop_owned_process(&self) {
        let Some(process) = self.process.borrow_mut().take() else {
            return;
        };
        drop(process);
    }

    fn subscribe_to_resume(self: &Rc<Self>) {
        if self.sleep_subscription.borrow().is_some()
            || self.sleep_subscription_pending.replace(true)
        {
            return;
        }

        let lifecycle = self.lifecycle_generation.current();
        let weak_controller = Rc::downgrade(self);
        gio::bus_get(
            gio::BusType::System,
            None::<&gio::Cancellable>,
            move |result| {
                let Some(controller) = weak_controller.upgrade() else {
                    return;
                };
                controller.sleep_subscription_pending.set(false);
                if !controller.lifecycle_is_current(lifecycle) {
                    return;
                }
                let connection = match result {
                    Ok(connection) => {
                        controller.sleep_subscription_retry_attempt.set(0);
                        connection
                    }
                    Err(error) => {
                        warn!(%error, "system D-Bus unavailable; retrying resume subscription");
                        controller.schedule_resume_subscription_retry(lifecycle);
                        return;
                    }
                };

                let weak_self = Rc::downgrade(&controller);
                let subscription = connection.subscribe_to_signal(
                    Some("org.freedesktop.login1"),
                    Some("org.freedesktop.login1.Manager"),
                    Some("PrepareForSleep"),
                    Some("/org/freedesktop/login1"),
                    None,
                    gio::DBusSignalFlags::NONE,
                    move |signal| {
                        let Some((going_to_sleep,)) = signal.parameters.get::<(bool,)>() else {
                            warn!("invalid PrepareForSleep payload from logind");
                            return;
                        };

                        if going_to_sleep {
                            return;
                        }

                        let Some(controller) = weak_self.upgrade() else {
                            return;
                        };
                        if let Err(error) = controller.restore() {
                            warn!(%error, "failed to restore wallpaper after resume");
                        }
                    },
                );

                controller.sleep_subscription.replace(Some(subscription));
            },
        );
    }

    fn schedule_resume_subscription_retry(self: &Rc<Self>, lifecycle: u64) {
        if !self.lifecycle_is_current(lifecycle)
            || self.sleep_subscription_retry_pending.replace(true)
        {
            return;
        }

        let attempt = self.sleep_subscription_retry_attempt.get();
        self.sleep_subscription_retry_attempt
            .set(attempt.saturating_add(1));
        let multiplier = 1_u32 << attempt.min(5);
        let delay = RESUME_SUBSCRIPTION_RETRY_BASE_DELAY
            .saturating_mul(multiplier)
            .min(RESUME_SUBSCRIPTION_RETRY_MAX_DELAY);

        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(delay, move || {
            let Some(controller) = weak.upgrade() else {
                return;
            };
            if !controller.lifecycle_is_current(lifecycle) {
                return;
            }
            controller.sleep_subscription_retry_pending.set(false);
            controller.subscribe_to_resume();
        });
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WallpaperTransitionKind {
    Circle,
    HorizontalWipe,
    VerticalWipe,
    DiagonalWipe,
    Blinds,
}

impl WallpaperTransitionKind {
    const ALL: [Self; 5] = [
        Self::Circle,
        Self::HorizontalWipe,
        Self::VerticalWipe,
        Self::DiagonalWipe,
        Self::Blinds,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::Circle => "circle",
            Self::HorizontalWipe => "horizontal-wipe",
            Self::VerticalWipe => "vertical-wipe",
            Self::DiagonalWipe => "diagonal-wipe",
            Self::Blinds => "blinds",
        }
    }
}

struct WallpaperTransition {
    windows: Vec<gtk::ApplicationWindow>,
    canvases: Vec<gtk::DrawingArea>,
    progress: Rc<Cell<f64>>,
}

impl WallpaperTransition {
    fn new(
        application: &gtk::Application,
        frame: &Path,
        kind: WallpaperTransitionKind,
    ) -> Result<Self, WallpaperError> {
        if !gtk4_layer_shell::is_supported() {
            return Err(WallpaperError::Transition(
                "the compositor does not support layer-shell".into(),
            ));
        }

        let display = gdk::Display::default()
            .ok_or_else(|| WallpaperError::Transition("GTK display is unavailable".into()))?;
        let model = display.monitors();
        let progress = Rc::new(Cell::new(0.0_f64));
        let mut windows = Vec::new();
        let mut canvases = Vec::new();

        for index in 0..model.n_items() {
            let Some(item) = model.item(index) else {
                continue;
            };
            let Ok(monitor) = item.downcast::<gdk::Monitor>() else {
                continue;
            };

            let geometry = monitor.geometry();
            let width = geometry.width().max(1);
            let height = geometry.height().max(1);
            let pixbuf = transition_pixbuf(frame, width, height)?;
            // Converting a large Pixbuf to Cairo's native surface can otherwise happen
            // lazily in the first visible draw.  That one-time conversion is expensive
            // enough to make the first wallpaper transition miss a frame or two.
            let surface = transition_surface(&pixbuf)?;

            let canvas = gtk::DrawingArea::new();
            canvas.add_css_class("wallpaper-transition-canvas");
            canvas.set_hexpand(true);
            canvas.set_vexpand(true);
            canvas.set_can_target(false);

            let draw_progress = Rc::clone(&progress);
            canvas.set_draw_func(move |_, context, draw_width, draw_height| {
                let reveal = ease_out_cubic(draw_progress.get().clamp(0.0, 1.0));
                if reveal <= 0.0 {
                    return;
                }

                let width = f64::from(draw_width.max(1));
                let height = f64::from(draw_height.max(1));

                let _ = context.save();
                apply_transition_clip(context, kind, reveal, width, height);
                context.clip();
                let _ = context.set_source_surface(&surface, 0.0, 0.0);
                let _ = context.paint();
                let _ = context.restore();
            });

            let window = gtk::ApplicationWindow::builder().decorated(false).build();
            window.set_focusable(false);
            window.add_css_class("wallpaper-transition-window");
            window.init_layer_shell();
            window.set_namespace(Some(TRANSITION_NAMESPACE));
            window.set_layer(Layer::Background);
            window.set_keyboard_mode(KeyboardMode::None);
            window.set_monitor(Some(&monitor));
            window.set_anchor(Edge::Top, true);
            window.set_anchor(Edge::Bottom, true);
            window.set_anchor(Edge::Left, true);
            window.set_anchor(Edge::Right, true);
            window.set_exclusive_zone(-1);
            window.set_child(Some(&canvas));

            canvases.push(canvas);
            windows.push(window);
        }

        if windows.is_empty() {
            return Err(WallpaperError::Transition(
                "no monitors are available".into(),
            ));
        }

        for window in &windows {
            window.set_application(Some(application));
        }

        Ok(Self {
            windows,
            canvases,
            progress,
        })
    }

    fn present(&self) {
        self.progress.set(0.0);
        for window in &self.windows {
            window.set_opacity(1.0);
            window.present();
        }
    }

    async fn animate(&self) {
        let Some(clock_canvas) = self.canvases.first() else {
            return;
        };

        let progress = Rc::clone(&self.progress);
        let canvases = self
            .canvases
            .iter()
            .map(|canvas| canvas.downgrade())
            .collect::<Vec<_>>();
        let (sender, receiver) = async_channel::bounded(1);
        let completed = Rc::new(Cell::new(false));
        let timeout_sender = sender.clone();
        let timeout_completed = Rc::clone(&completed);
        glib::timeout_add_local_once(TRANSITION_FRAME_CLOCK_TIMEOUT, move || {
            if !timeout_completed.replace(true) {
                let _ = timeout_sender.try_send(());
            }
        });
        let warmup_frames = Cell::new(TRANSITION_WARMUP_FRAMES);
        let started_at = Cell::new(None::<i64>);
        let tick_completed = Rc::clone(&completed);

        clock_canvas.add_tick_callback(move |_, frame_clock| {
            if tick_completed.get() {
                return glib::ControlFlow::Break;
            }

            let remaining = warmup_frames.get();
            if remaining > 0 {
                warmup_frames.set(remaining - 1);

                progress.set(TRANSITION_WARMUP_PROGRESS);
                for weak_canvas in &canvases {
                    if let Some(canvas) = weak_canvas.upgrade() {
                        canvas.queue_draw();
                    }
                }
                return glib::ControlFlow::Continue;
            }

            let now = frame_clock.frame_time();
            let started = match started_at.get() {
                Some(started) => started,
                None => {
                    started_at.set(Some(now));
                    now
                }
            };
            let elapsed = (now - started).max(0) as f64 / 1_000_000.0;
            let duration = TRANSITION_DURATION.as_secs_f64().max(f64::EPSILON);
            let next = (elapsed / duration).clamp(0.0, 1.0);
            progress.set(next);

            for weak_canvas in &canvases {
                if let Some(canvas) = weak_canvas.upgrade() {
                    canvas.queue_draw();
                }
            }

            if next >= 1.0 {
                if !tick_completed.replace(true) {
                    let _ = sender.try_send(());
                }
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });

        let _ = receiver.recv().await;
    }

    async fn wait_for_backend_settle(&self) {
        let (sender, receiver) = async_channel::bounded(1);
        glib::timeout_add_local_once(TRANSITION_BACKEND_SETTLE_DELAY, move || {
            let _ = sender.try_send(());
        });
        let _ = receiver.recv().await;
        self.wait_frames(TRANSITION_SETTLE_FRAMES).await;
    }

    async fn wait_frames(&self, frame_count: u32) {
        if frame_count == 0 {
            return;
        }

        let Some(clock_canvas) = self.canvases.first() else {
            return;
        };

        let (sender, receiver) = async_channel::bounded(1);
        let completed = Rc::new(Cell::new(false));
        let timeout_sender = sender.clone();
        let timeout_completed = Rc::clone(&completed);
        glib::timeout_add_local_once(TRANSITION_FRAME_CLOCK_TIMEOUT, move || {
            if !timeout_completed.replace(true) {
                let _ = timeout_sender.try_send(());
            }
        });
        let remaining = Cell::new(frame_count);
        let tick_completed = Rc::clone(&completed);
        clock_canvas.add_tick_callback(move |_, _| {
            if tick_completed.get() {
                return glib::ControlFlow::Break;
            }

            let next = remaining.get().saturating_sub(1);
            remaining.set(next);
            if next == 0 {
                if !tick_completed.replace(true) {
                    let _ = sender.try_send(());
                }
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });

        let _ = receiver.recv().await;
    }

    fn close(&self) {
        for window in &self.windows {
            detach_application_window(window);
        }
    }
}

impl Drop for WallpaperTransition {
    fn drop(&mut self) {
        self.close();
    }
}

fn apply_transition_clip(
    context: &gtk::cairo::Context,
    kind: WallpaperTransitionKind,
    reveal: f64,
    width: f64,
    height: f64,
) {
    match kind {
        WallpaperTransitionKind::Circle => {
            let center_x = width / 2.0;
            let center_y = height / 2.0;
            let max_radius = center_x.hypot(center_y);
            context.arc(
                center_x,
                center_y,
                max_radius * reveal,
                0.0,
                std::f64::consts::TAU,
            );
        }
        WallpaperTransitionKind::HorizontalWipe => {
            context.rectangle(0.0, 0.0, width * reveal, height);
        }
        WallpaperTransitionKind::VerticalWipe => {
            context.rectangle(0.0, 0.0, width, height * reveal);
        }
        WallpaperTransitionKind::DiagonalWipe => {
            let extent = width + height;
            let edge = extent * reveal;
            context.move_to(-extent, -extent);
            context.line_to(edge + extent, -extent);
            context.line_to(-extent, edge + extent);
            context.close_path();
        }
        WallpaperTransitionKind::Blinds => {
            const STRIP_COUNT: usize = 12;
            const STAGGER: f64 = 0.38;

            let strip_height = height / STRIP_COUNT as f64;
            for index in 0..STRIP_COUNT {
                let delay = index as f64 / (STRIP_COUNT - 1) as f64 * STAGGER;
                let local = ((reveal - delay) / (1.0 - STAGGER)).clamp(0.0, 1.0);
                if local <= 0.0 {
                    continue;
                }

                let revealed_width = width * local;
                let x = if index % 2 == 0 {
                    0.0
                } else {
                    width - revealed_width
                };
                let y = index as f64 * strip_height;
                let actual_height = if index + 1 == STRIP_COUNT {
                    height - y
                } else {
                    strip_height + 0.5
                };
                context.rectangle(x, y, revealed_width, actual_height);
            }
        }
    }
}

fn choose_transition_kind(
    previous: Option<WallpaperTransitionKind>,
    nonce: u64,
) -> WallpaperTransitionKind {
    let time_seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    choose_transition_kind_from_seed(previous, time_seed ^ mix_nonce(nonce))
}

fn choose_transition_kind_from_seed(
    previous: Option<WallpaperTransitionKind>,
    seed: u64,
) -> WallpaperTransitionKind {
    let choices = WallpaperTransitionKind::ALL;
    let mut index = (seed as usize) % choices.len();

    if choices.len() > 1
        && let Some(previous) = previous
        && choices[index] == previous
    {
        index = (index + 1 + ((seed >> 32) as usize % (choices.len() - 1))) % choices.len();
    }

    choices[index]
}

fn mix_nonce(nonce: u64) -> u64 {
    let mut value = nonce.wrapping_add(0x9E37_79B9_7F4A_7C15);
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

fn transition_pixbuf(source: &Path, width: i32, height: i32) -> Result<Pixbuf, WallpaperError> {
    let (_, source_width, source_height) = Pixbuf::file_info(source)
        .ok_or_else(|| WallpaperError::InvalidWallpaper(source.to_path_buf()))?;
    if source_width <= 0 || source_height <= 0 || width <= 0 || height <= 0 {
        return Err(WallpaperError::InvalidWallpaper(source.to_path_buf()));
    }

    let source_aspect = source_width as f64 / source_height as f64;
    let target_aspect = width as f64 / height as f64;
    let (scaled_width, scaled_height) = if source_aspect >= target_aspect {
        (
            ((height as f64 * source_aspect).ceil() as i32).max(width),
            height,
        )
    } else {
        (
            width,
            ((width as f64 / source_aspect).ceil() as i32).max(height),
        )
    };

    let scaled = Pixbuf::from_file_at_scale(source, scaled_width, scaled_height, false)
        .map_err(WallpaperError::Glib)?;
    let crop_x = ((scaled.width() - width) / 2).max(0);
    let crop_y = ((scaled.height() - height) / 2).max(0);
    let crop_width = width.min(scaled.width());
    let crop_height = height.min(scaled.height());
    let cropped = scaled.new_subpixbuf(crop_x, crop_y, crop_width, crop_height);

    if crop_width == width && crop_height == height {
        Ok(cropped)
    } else {
        cropped
            .scale_simple(width, height, InterpType::Bilinear)
            .ok_or_else(|| WallpaperError::Thumbnail(source.to_path_buf()))
    }
}

fn transition_surface(pixbuf: &Pixbuf) -> Result<gtk::cairo::ImageSurface, WallpaperError> {
    let surface = gtk::cairo::ImageSurface::create(
        gtk::cairo::Format::ARgb32,
        pixbuf.width(),
        pixbuf.height(),
    )
    .map_err(|error| WallpaperError::Transition(error.to_string()))?;
    let context = gtk::cairo::Context::new(&surface)
        .map_err(|error| WallpaperError::Transition(error.to_string()))?;
    context.set_source_pixbuf(pixbuf, 0.0, 0.0);
    context
        .paint()
        .map_err(|error| WallpaperError::Transition(error.to_string()))?;
    drop(context);
    surface.flush();
    Ok(surface)
}

fn ease_out_cubic(value: f64) -> f64 {
    1.0 - (1.0 - value).powi(3)
}

pub struct WallpaperIndicator {
    button: gtk::Button,
    _picker: gtk::ApplicationWindow,
    picker_reveal: PopupReveal,
    focus_armed: Rc<Cell<bool>>,
}

impl Drop for WallpaperIndicator {
    fn drop(&mut self) {
        detach_application_window(&self._picker);
    }
}

impl WallpaperIndicator {
    pub fn new(
        application: &gtk::Application,
        bar_window: &gtk::ApplicationWindow,
        monitor: &gdk::Monitor,
        controller: &Rc<WallpaperController>,
        bar_features: &Rc<BarFeatureController>,
        audio_spectrum: &Rc<AudioSpectrumController>,
    ) -> Self {
        let picker = gtk::ApplicationWindow::builder()
            .application(application)
            .decorated(false)
            .resizable(false)
            .build();
        picker.add_css_class("wallpaper-picker-window");
        picker.init_layer_shell();
        picker.set_namespace(Some(PICKER_NAMESPACE));
        picker.set_layer(Layer::Top);
        picker.set_keyboard_mode(KeyboardMode::OnDemand);
        picker.set_monitor(Some(monitor));
        picker.set_anchor(Edge::Top, true);
        picker.set_anchor(Edge::Left, true);
        picker.set_anchor(Edge::Right, false);
        picker.set_anchor(Edge::Bottom, false);
        picker.set_exclusive_zone(-1);
        picker.set_margin(Edge::Top, 0);
        picker.set_margin(Edge::Left, 0);
        picker.set_hide_on_close(true);

        let picker_root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        picker_root.add_css_class("widget-popup-root");

        let surface = gtk::Box::new(gtk::Orientation::Vertical, 0);
        surface.add_css_class("wallpaper-picker-surface");

        let popup_content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        surface.append(&popup_content);
        let picker_reveal = PopupReveal::masked(surface.clone().upcast::<gtk::Widget>());
        picker_root.append(picker_reveal.widget());
        picker.set_child(Some(&picker_root));

        popup_content.append(&build_picker_content(
            controller,
            bar_features,
            audio_spectrum,
        ));

        let button = gtk::Button::new();
        button.add_css_class("wallpaper-widget-trigger");
        button.set_bar_tooltip_text(Some("Wallpapers"));
        button.set_valign(gtk::Align::Center);

        let trigger_icon = gtk::Label::new(Some(ICON_WALLPAPER));
        trigger_icon.add_css_class("wallpaper-trigger-icon");
        button.set_child(Some(&trigger_icon));

        let focus_armed = Rc::new(Cell::new(false));
        {
            let weak_picker = picker.downgrade();
            let close_focus = Rc::clone(&focus_armed);
            let close_reveal = picker_reveal.clone();
            attach_popup_focus_dismiss(&picker, bar_window, &focus_armed, move || {
                if let Some(picker) = weak_picker.upgrade() {
                    close_focus.set(false);
                    close_reveal.hide(&picker);
                }
            });
        }

        {
            let focus_armed = Rc::clone(&focus_armed);
            let picker_reveal = picker_reveal.clone();
            picker.connect_visible_notify(move |picker| {
                if picker.is_visible() {
                    return;
                }
                focus_armed.set(false);
                picker_reveal.reset_hidden();
            });
        }

        let key = gtk::EventControllerKey::new();
        {
            let weak_picker = picker.downgrade();
            let focus_armed = Rc::clone(&focus_armed);
            let picker_reveal = picker_reveal.clone();
            key.connect_key_pressed(move |_, key, _, _| {
                if key == gdk::Key::Escape {
                    if let Some(picker) = weak_picker.upgrade() {
                        focus_armed.set(false);
                        picker_reveal.hide(&picker);
                    }
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
        }
        picker.add_controller(key);

        {
            let weak_picker = picker.downgrade();
            let close_focus = Rc::clone(&focus_armed);
            let close_reveal = picker_reveal.clone();
            attach_bar_click_dismiss(bar_window, &button, &picker, move || {
                if let Some(picker) = weak_picker.upgrade() {
                    close_focus.set(false);
                    close_reveal.hide(&picker);
                }
            });
        }

        {
            let weak_picker = picker.downgrade();
            let weak_bar_window = bar_window.downgrade();
            let monitor = monitor.clone();
            let focus_armed = Rc::clone(&focus_armed);
            let picker_reveal = picker_reveal.clone();
            button.connect_clicked(move |button| {
                let Some(picker) = weak_picker.upgrade() else {
                    return;
                };
                if picker_reveal.is_revealed() {
                    focus_armed.set(false);
                    picker_reveal.hide(&picker);
                } else {
                    let Some(bar_window) = weak_bar_window.upgrade() else {
                        return;
                    };
                    position_picker_at_trigger(&picker, button, &bar_window, &monitor);
                    focus_armed.set(false);
                    picker_reveal.show(&picker);
                }
            });
        }

        Self {
            button,
            _picker: picker,
            picker_reveal,
            focus_armed,
        }
    }

    pub fn widget(&self) -> &gtk::Button {
        &self.button
    }

    pub fn dismiss(&self) {
        self.focus_armed.set(false);
        self.picker_reveal.hide(&self._picker);
    }
}

fn position_picker_at_trigger(
    picker: &gtk::ApplicationWindow,
    button: &gtk::Button,
    bar_window: &gtk::ApplicationWindow,
    monitor: &gdk::Monitor,
) {
    const BAR_MARGIN_LEFT: f32 = 9.0;
    const PICKER_WIDTH: f32 = (GALLERY_WIDTH + 48) as f32;

    let Some(bounds) = button.compute_bounds(bar_window) else {
        return;
    };

    let monitor_width = monitor.geometry().width() as f32;
    let anchor_center_x = BAR_MARGIN_LEFT + bounds.x() + bounds.width() / 2.0;
    let min_left = BAR_MARGIN_LEFT;
    let max_left = (monitor_width - PICKER_WIDTH - BAR_MARGIN_LEFT).max(min_left);
    let left = (anchor_center_x - PICKER_WIDTH / 2.0).clamp(min_left, max_left);
    picker.set_margin(Edge::Left, left.round() as i32);
    picker.set_margin(Edge::Top, BAR_POPUP_TOP_MARGIN);
}

struct GalleryPage {
    root: gtk::Box,
    model: gio::ListStore,
    active_wallpaper: Rc<RefCell<Option<PathBuf>>>,
    live_wallpaper_cards: LiveWallpaperCards,
    scroller: gtk::ScrolledWindow,
    empty: gtk::Box,
    count: gtk::Label,
    notice: gtk::Label,
    folder_button: gtk::Button,
    refresh_button: gtk::Button,
    refresh_icon: gtk::Label,
    refresh_spinner: gtk::Spinner,
}

fn build_gallery_page(
    controller: &Rc<WallpaperController>,
    bar_features: &Rc<BarFeatureController>,
    audio_spectrum: &Rc<AudioSpectrumController>,
) -> GalleryPage {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 10);
    root.add_css_class("wallpaper-selector-page");

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    header.add_css_class("wallpaper-header");
    header.set_valign(gtk::Align::Start);

    let title_column = gtk::Box::new(gtk::Orientation::Vertical, 2);
    title_column.set_hexpand(true);

    let title_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    title_row.set_valign(gtk::Align::Center);

    let header_icon = gtk::Label::new(Some(ICON_WALLPAPER));
    header_icon.add_css_class("wallpaper-header-icon");

    let title = gtk::Label::new(Some("Wallpapers"));
    title.add_css_class("wallpaper-title");
    title.set_xalign(0.0);

    let count = gtk::Label::new(None);
    count.add_css_class("wallpaper-count");

    title_row.append(&header_icon);
    title_row.append(&title);
    title_row.append(&count);

    let path_actions = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    path_actions.add_css_class("wallpaper-path-actions");
    path_actions.set_halign(gtk::Align::Start);
    path_actions.set_valign(gtk::Align::Center);

    let folder_button = header_button(ICON_FOLDER);
    let (refresh_button, refresh_icon, refresh_spinner) = wallpaper_refresh_button(ICON_REFRESH);
    let random_button = random_menu_button(controller);
    path_actions.append(&folder_button);
    path_actions.append(&refresh_button);
    path_actions.append(&random_button);

    title_column.append(&title_row);
    title_column.append(&path_actions);

    let feature_actions = bar_feature_actions(bar_features, audio_spectrum);
    header.append(&title_column);
    header.append(&feature_actions);

    let gallery_frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
    gallery_frame.add_css_class("wallpaper-gallery-frame");

    let notice = gtk::Label::new(None);
    notice.add_css_class("wallpaper-notice");
    notice.set_xalign(0.0);
    notice.set_wrap(true);
    notice.set_visible(false);

    let scroller = gtk::ScrolledWindow::new();
    scroller.add_css_class("wallpaper-list-wrap");
    scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::External);
    scroller.set_min_content_width(GALLERY_WIDTH);
    scroller.set_min_content_height(GALLERY_HEIGHT);
    scroller.set_max_content_height(GALLERY_HEIGHT);
    scroller.set_propagate_natural_height(false);
    scroller.set_propagate_natural_width(false);
    scroller.set_halign(gtk::Align::Start);
    scroller.set_valign(gtk::Align::Start);

    let model = gio::ListStore::new::<glib::BoxedAnyObject>();
    let selection = gtk::NoSelection::new(Some(model.clone()));
    let factory = gtk::SignalListItemFactory::new();
    let active_wallpaper = Rc::new(RefCell::new(None::<PathBuf>));
    let live_wallpaper_cards: LiveWallpaperCards = Rc::new(RefCell::new(HashMap::new()));

    factory.connect_setup(|_, object| {
        let Some(list_item) = object.downcast_ref::<gtk::ListItem>() else {
            return;
        };

        list_item.set_selectable(false);
        list_item.set_activatable(false);

        let row = gtk::Box::new(gtk::Orientation::Horizontal, GRID_GAP as i32);
        row.add_css_class("wallpaper-grid-row");
        row.set_size_request(GALLERY_WIDTH, CARD_HEIGHT);
        row.set_halign(gtk::Align::Start);
        row.set_hexpand(false);
        list_item.set_child(Some(&row));
    });

    {
        let controller = Rc::clone(controller);
        let weak_notice = notice.downgrade();
        let active_wallpaper = Rc::clone(&active_wallpaper);
        let live_wallpaper_cards = Rc::clone(&live_wallpaper_cards);
        let model = model.clone();
        factory.connect_bind(move |_, object| {
            let Some(list_item) = object.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let Some(row_data) = list_item.item().and_downcast::<glib::BoxedAnyObject>() else {
                return;
            };
            let Some(row) = list_item.child().and_downcast::<gtk::Box>() else {
                return;
            };

            clear_box(&row);
            let current = active_wallpaper.borrow();
            let paths = row_data.borrow::<Vec<PathBuf>>();
            for path in paths.iter().cloned() {
                let active = current.as_deref() == Some(path.as_path());
                row.append(&wallpaper_card(
                    &controller,
                    &weak_notice,
                    &live_wallpaper_cards,
                    path,
                    active,
                ));
            }

            let has_next_row = list_item.position().saturating_add(1) < model.n_items();
            row.set_margin_bottom(if has_next_row { GRID_GAP as i32 } else { 0 });
        });
    }

    factory.connect_unbind(|_, object| {
        let Some(list_item) = object.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(row) = list_item.child().and_downcast::<gtk::Box>() else {
            return;
        };
        clear_box(&row);
    });

    let gallery = gtk::ListView::new(Some(selection), Some(factory));
    gallery.add_css_class("wallpaper-grid-rows");
    gallery.set_show_separators(false);
    gallery.set_size_request(GALLERY_WIDTH, -1);
    gallery.set_halign(gtk::Align::Fill);
    gallery.set_valign(gtk::Align::Fill);
    gallery.set_hexpand(true);
    scroller.set_child(Some(&gallery));
    install_smooth_scroll(&scroller, SMOOTH_SCROLL);

    let empty = gtk::Box::new(gtk::Orientation::Vertical, 4);
    empty.add_css_class("wallpaper-empty");
    empty.set_size_request(GALLERY_WIDTH, CARD_HEIGHT * 2 + GRID_GAP as i32);
    empty.set_halign(gtk::Align::Center);
    empty.set_valign(gtk::Align::Center);

    let empty_icon = gtk::Label::new(Some(ICON_WALLPAPER));
    empty_icon.add_css_class("wallpaper-empty-icon");
    let empty_title = gtk::Label::new(Some("No wallpapers found"));
    let empty_meta = gtk::Label::new(Some("Choose a folder with images or videos"));
    empty_meta.add_css_class("wallpaper-empty-meta");
    empty.append(&empty_icon);
    empty.append(&empty_title);
    empty.append(&empty_meta);

    gallery_frame.append(&scroller);
    gallery_frame.append(&empty);

    root.append(&header);
    root.append(&gallery_frame);
    root.append(&notice);

    GalleryPage {
        root,
        model,
        active_wallpaper,
        live_wallpaper_cards,
        scroller,
        empty,
        count,
        notice,
        folder_button,
        refresh_button,
        refresh_icon,
        refresh_spinner,
    }
}

struct DirectoryPage {
    root: gtk::Box,
    back_button: gtk::Button,
    home_button: gtk::Button,
    up_button: gtk::Button,
    select_button: gtk::Button,
    path_label: gtk::Label,
    list: gtk::Box,
    notice: gtk::Label,
}

#[derive(Clone)]
struct DirectoryBrowser {
    current: Rc<RefCell<PathBuf>>,
    list: glib::WeakRef<gtk::Box>,
    path_label: glib::WeakRef<gtk::Label>,
    notice: glib::WeakRef<gtk::Label>,
}

impl DirectoryBrowser {
    fn new(initial: PathBuf, page: &DirectoryPage) -> Self {
        page.path_label.set_label(&initial.to_string_lossy());
        Self {
            current: Rc::new(RefCell::new(initial)),
            list: page.list.downgrade(),
            path_label: page.path_label.downgrade(),
            notice: page.notice.downgrade(),
        }
    }

    fn path(&self) -> PathBuf {
        self.current.borrow().clone()
    }

    fn replace_if_changed(&self, path: PathBuf) -> bool {
        if self.current.borrow().as_path() == path.as_path() {
            return false;
        }
        self.current.replace(path);
        true
    }

    fn navigate_to(&self, path: PathBuf) {
        self.current.replace(path);
        self.render();
    }

    fn navigate_up(&self) {
        let parent = self.current.borrow().parent().map(Path::to_path_buf);
        if let Some(parent) = parent {
            self.navigate_to(parent);
        }
    }

    fn render(&self) {
        let (Some(list), Some(path_label), Some(notice)) = (
            self.list.upgrade(),
            self.path_label.upgrade(),
            self.notice.upgrade(),
        ) else {
            return;
        };
        render_directory_browser(self, &list, &path_label, &notice);
    }
}

fn build_directory_page() -> DirectoryPage {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 10);
    root.add_css_class("wallpaper-directory-page");

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    header.add_css_class("wallpaper-directory-header");
    header.set_valign(gtk::Align::Center);

    let back_button = header_button(ICON_BACK);
    let title = gtk::Label::new(Some("Wallpaper folder"));
    title.add_css_class("wallpaper-title");
    title.set_xalign(0.0);
    title.set_hexpand(true);

    let home_button = header_button(ICON_HOME);
    let up_button = header_button(ICON_UP);

    header.append(&back_button);
    header.append(&title);
    header.append(&home_button);
    header.append(&up_button);

    let path_label = gtk::Label::new(None);
    path_label.add_css_class("wallpaper-directory-path");
    path_label.set_xalign(0.0);
    path_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);

    let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
    frame.add_css_class("wallpaper-directory-frame");

    let scroller = gtk::ScrolledWindow::new();
    scroller.add_css_class("wallpaper-directory-list-wrap");
    scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroller.set_min_content_width(GALLERY_WIDTH);
    scroller.set_min_content_height(GALLERY_HEIGHT);
    scroller.set_max_content_height(GALLERY_HEIGHT);
    scroller.set_propagate_natural_height(false);

    let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
    list.add_css_class("wallpaper-directory-list");
    scroller.set_child(Some(&list));
    install_smooth_scroll(&scroller, SMOOTH_SCROLL);
    frame.append(&scroller);

    let notice = gtk::Label::new(None);
    notice.add_css_class("wallpaper-notice");
    notice.set_xalign(0.0);
    notice.set_wrap(true);
    notice.set_visible(false);

    let select_button = gtk::Button::with_label("Use this folder");
    select_button.add_css_class("wallpaper-directory-select");
    select_button.set_halign(gtk::Align::End);

    root.append(&header);
    root.append(&path_label);
    root.append(&frame);
    root.append(&notice);
    root.append(&select_button);

    DirectoryPage {
        root,
        back_button,
        home_button,
        up_button,
        select_button,
        path_label,
        list,
        notice,
    }
}

fn build_picker_content(
    controller: &Rc<WallpaperController>,
    bar_features: &Rc<BarFeatureController>,
    audio_spectrum: &Rc<AudioSpectrumController>,
) -> gtk::Box {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.add_css_class("wallpaper-picker");
    root.set_size_request(GALLERY_WIDTH + 24, -1);

    let stack = gtk::Stack::new();
    stack.add_css_class("wallpaper-stack");
    stack.set_hhomogeneous(true);
    stack.set_vhomogeneous(true);
    stack.set_transition_type(gtk::StackTransitionType::SlideLeftRight);
    stack.set_transition_duration(240);

    let GalleryPage {
        root: selector_page,
        model,
        active_wallpaper,
        live_wallpaper_cards,
        scroller,
        empty,
        count,
        notice,
        folder_button,
        refresh_button,
        refresh_icon,
        refresh_spinner,
    } = build_gallery_page(controller, bar_features, audio_spectrum);
    let directory_page = build_directory_page();

    stack.add_named(&selector_page, Some("wallpapers"));
    stack.add_named(&directory_page.root, Some("directories"));
    stack.set_visible_child_name("wallpapers");
    root.append(&stack);

    let initial_snapshot = controller.snapshot();
    let browser = DirectoryBrowser::new(initial_snapshot.directory.clone(), &directory_page);
    let directory_loaded = Rc::new(Cell::new(false));

    let rendered = Rc::new(RefCell::new(RenderedGallery {
        directory: initial_snapshot.directory.clone(),
        current: initial_snapshot.current.clone(),
        loaded: false,
    }));
    let gallery_view = GalleryView {
        model,
        active_wallpaper,
        live_wallpaper_cards,
        scroller: scroller.downgrade(),
        empty: empty.downgrade(),
        count: count.downgrade(),
        notice: notice.downgrade(),
        rendered,
        refresh_generation: Rc::new(Generation::default()),
        refresh_button: refresh_button.downgrade(),
        refresh_icon: refresh_icon.downgrade(),
        refresh_spinner: refresh_spinner.downgrade(),
    };
    gallery_view.refresh(initial_snapshot, false);

    let receiver = controller.subscribe();
    let receiver_on_destroy = receiver.clone();
    root.connect_destroy(move |_| {
        receiver_on_destroy.close();
    });

    let gallery_view_for_updates = gallery_view.clone();
    glib::MainContext::default().spawn_local(async move {
        while let Ok(snapshot) = receiver.recv().await {
            if gallery_view_for_updates.scroller.upgrade().is_none() {
                receiver.close();
                break;
            }
            gallery_view_for_updates.apply_snapshot(snapshot);
        }
    });

    {
        let controller = Rc::clone(controller);
        let browser = browser.clone();
        let weak_stack = stack.downgrade();
        let directory_loaded = Rc::clone(&directory_loaded);
        folder_button.connect_clicked(move |_| {
            let Some(stack) = weak_stack.upgrade() else {
                return;
            };

            let directory_changed = browser.replace_if_changed(controller.snapshot().directory);
            if directory_changed {
                directory_loaded.set(false);
            }
            if !directory_loaded.replace(true) {
                browser.render();
            }

            stack.set_visible_child_name("directories");
        });
    }

    {
        let weak_stack = stack.downgrade();
        directory_page.back_button.connect_clicked(move |_| {
            if let Some(stack) = weak_stack.upgrade() {
                stack.set_visible_child_name("wallpapers");
            }
        });
    }

    {
        let browser = browser.clone();
        directory_page.home_button.connect_clicked(move |_| {
            browser.navigate_to(glib::home_dir());
        });
    }

    {
        let browser = browser.clone();
        directory_page.up_button.connect_clicked(move |_| {
            browser.navigate_up();
        });
    }

    {
        let controller = Rc::clone(controller);
        let browser = browser.clone();
        let weak_stack = stack.downgrade();
        let weak_notice = directory_page.notice.downgrade();
        directory_page.select_button.connect_clicked(move |_| {
            let (Some(stack), Some(notice)) = (weak_stack.upgrade(), weak_notice.upgrade()) else {
                return;
            };

            match controller.set_directory(browser.path()) {
                Ok(()) => {
                    set_optional_label(&notice, None);
                    stack.set_visible_child_name("wallpapers");
                }
                Err(error) => set_optional_label(&notice, Some(&error.to_string())),
            }
        });
    }

    {
        let controller = Rc::clone(controller);
        let gallery_view = gallery_view.clone();
        refresh_button.connect_clicked(move |_| {
            gallery_view.refresh(controller.snapshot(), true);
        });
    }

    root
}

fn render_directory_browser(
    browser: &DirectoryBrowser,
    directory_list: &gtk::Box,
    path_label: &gtk::Label,
    notice: &gtk::Label,
) {
    clear_box(directory_list);

    let directory = browser.path();
    path_label.set_label(&directory.to_string_lossy());

    let directories = match list_directories(&directory) {
        Ok(directories) => {
            set_optional_label(notice, None);
            directories
        }
        Err(error) => {
            set_optional_label(notice, Some(&error.to_string()));
            return;
        }
    };

    if directories.is_empty() {
        let label = gtk::Label::new(Some("No subdirectories"));
        label.add_css_class("wallpaper-directory-empty");
        label.set_halign(gtk::Align::Start);
        directory_list.append(&label);
        return;
    }

    for path in directories {
        let button = gtk::Button::new();
        button.add_css_class("wallpaper-directory-row");

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.add_css_class("wallpaper-directory-row-content");

        let icon = gtk::Label::new(Some(ICON_FOLDER));
        icon.add_css_class("wallpaper-directory-row-icon");

        let name = path.file_name().and_then(OsStr::to_str).unwrap_or("/");
        let label = gtk::Label::new(Some(name));
        label.add_css_class("wallpaper-directory-row-label");
        label.set_xalign(0.0);
        label.set_hexpand(true);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);

        row.append(&icon);
        row.append(&label);
        button.set_child(Some(&row));

        let browser = browser.clone();
        button.connect_clicked(move |_| {
            browser.navigate_to(path.clone());
        });

        directory_list.append(&button);
    }
}

fn list_directories(directory: &Path) -> Result<Vec<PathBuf>, WallpaperError> {
    let entries = fs::read_dir(directory).map_err(WallpaperError::Io)?;
    let mut directories = Vec::new();

    for entry in entries {
        let entry = entry.map_err(WallpaperError::Io)?;
        let path = entry.path();
        if path.is_dir() {
            directories.push(path);
        }
    }

    directories.sort_by_cached_key(|path| {
        path.file_name()
            .and_then(OsStr::to_str)
            .unwrap_or_default()
            .to_ascii_lowercase()
    });
    Ok(directories)
}

fn render_gallery_items(
    model: &gio::ListStore,
    active_wallpaper: &Rc<RefCell<Option<PathBuf>>>,
    widgets: GalleryRenderWidgets<'_>,
    current: Option<PathBuf>,
    items: Vec<PathBuf>,
) {
    let adjustment = widgets.scroller.vadjustment();
    let previous_scroll = adjustment.value();

    set_optional_label(widgets.notice, None);
    active_wallpaper.replace(current);
    widgets.count.set_label(&items.len().to_string());
    widgets.scroller.set_visible(!items.is_empty());
    widgets.empty.set_visible(items.is_empty());

    let rows = items
        .chunks(GRID_COLUMNS as usize)
        .map(|paths| glib::BoxedAnyObject::new(paths.to_vec()))
        .collect::<Vec<_>>();
    model.splice(0, model.n_items(), &rows);

    glib::idle_add_local_once(move || {
        let lower = adjustment.lower();
        let upper = (adjustment.upper() - adjustment.page_size()).max(lower);
        adjustment.set_value(previous_scroll.clamp(lower, upper));
    });
}

impl GalleryView {
    fn refresh(&self, snapshot: WallpaperSnapshot, animate: bool) {
        let generation = self.refresh_generation.bump();
        let animation_started = animate.then(Instant::now);
        if animate {
            self.set_refresh_animating(true);
        } else {
            self.set_refresh_animating(false);
        }

        let reset_model = {
            let rendered = self.rendered.borrow();
            rendered.directory != snapshot.directory
                || !rendered.loaded && self.model.n_items() == 0
        };
        {
            let mut rendered = self.rendered.borrow_mut();
            rendered.directory = snapshot.directory.clone();
            rendered.current = snapshot.current.clone();
            rendered.loaded = false;
        }
        self.active_wallpaper.replace(snapshot.current);

        let (Some(scroller), Some(empty), Some(count), Some(notice)) = (
            self.scroller.upgrade(),
            self.empty.upgrade(),
            self.count.upgrade(),
            self.notice.upgrade(),
        ) else {
            self.set_refresh_animating(false);
            return;
        };

        if reset_model {
            self.model.remove_all();
            self.live_wallpaper_cards.borrow_mut().clear();
            count.set_label("0");
            scroller.set_visible(false);
            empty.set_visible(false);
        }
        set_optional_label(&notice, Some("Loading wallpapers…"));

        let directory = snapshot.directory;
        let view = self.clone();
        glib::MainContext::default().spawn_local(async move {
            let result = run_background_async({
                let directory = directory.clone();
                move || list_wallpapers(&directory).map_err(|error| error.to_string())
            })
            .await
            .unwrap_or_else(|| Err("wallpaper scan worker stopped".to_owned()));

            view.finish_refresh(generation, directory, result, animation_started);
        });
    }

    fn apply_snapshot(&self, snapshot: WallpaperSnapshot) {
        let same_directory = self.rendered.borrow().directory == snapshot.directory;
        if !same_directory {
            self.refresh(snapshot, false);
            return;
        }

        let mut rendered = self.rendered.borrow_mut();
        let previous = rendered.current.clone();
        rendered.current = snapshot.current.clone();
        if rendered.loaded {
            update_gallery_active(
                &self.active_wallpaper,
                &self.live_wallpaper_cards,
                previous.as_deref(),
                snapshot.current.as_deref(),
            );
        } else {
            self.active_wallpaper.replace(snapshot.current);
        }
    }

    fn finish_refresh(
        &self,
        generation: u64,
        directory: PathBuf,
        result: Result<Vec<PathBuf>, String>,
        animation_started: Option<Instant>,
    ) {
        if !self.refresh_generation.is_current(generation) {
            return;
        }

        let (Some(scroller), Some(empty), Some(count), Some(notice)) = (
            self.scroller.upgrade(),
            self.empty.upgrade(),
            self.count.upgrade(),
            self.notice.upgrade(),
        ) else {
            self.finish_refresh_animation(generation, animation_started);
            return;
        };

        let mut rendered = self.rendered.borrow_mut();
        if rendered.directory != directory {
            drop(rendered);
            self.finish_refresh_animation(generation, animation_started);
            return;
        }

        match result {
            Ok(items) => render_gallery_items(
                &self.model,
                &self.active_wallpaper,
                GalleryRenderWidgets {
                    scroller: &scroller,
                    empty: &empty,
                    count: &count,
                    notice: &notice,
                },
                rendered.current.clone(),
                items,
            ),
            Err(error) => {
                set_optional_label(&notice, Some(&error));
                if self.model.n_items() == 0 {
                    scroller.set_visible(false);
                    empty.set_visible(true);
                }
            }
        }
        rendered.loaded = true;
        drop(rendered);
        self.finish_refresh_animation(generation, animation_started);
    }

    fn finish_refresh_animation(&self, generation: u64, animation_started: Option<Instant>) {
        let Some(started) = animation_started else {
            self.set_refresh_animating(false);
            return;
        };

        let remaining = REFRESH_ANIMATION_MIN_DURATION.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            self.set_refresh_animating(false);
            return;
        }

        let view = self.clone();
        glib::timeout_add_local_once(remaining, move || {
            if view.refresh_generation.is_current(generation) {
                view.set_refresh_animating(false);
            }
        });
    }

    fn set_refresh_animating(&self, active: bool) {
        if let (Some(button), Some(icon), Some(spinner)) = (
            self.refresh_button.upgrade(),
            self.refresh_icon.upgrade(),
            self.refresh_spinner.upgrade(),
        ) {
            button.set_sensitive(!active);
            set_spinner_active(&icon, &spinner, active);
        }
    }
}

fn update_gallery_active(
    active_wallpaper: &Rc<RefCell<Option<PathBuf>>>,
    live_wallpaper_cards: &LiveWallpaperCards,
    previous: Option<&Path>,
    current: Option<&Path>,
) {
    if previous == current {
        return;
    }

    active_wallpaper.replace(current.map(Path::to_path_buf));

    if let Some(path) = previous {
        set_live_wallpaper_card_active(live_wallpaper_cards, path, false);
    }
    if let Some(path) = current {
        set_live_wallpaper_card_active(live_wallpaper_cards, path, true);
    }
}

fn set_live_wallpaper_card_active(
    live_wallpaper_cards: &LiveWallpaperCards,
    path: &Path,
    active: bool,
) {
    let overlay = live_wallpaper_cards
        .borrow()
        .get(path)
        .and_then(|weak| weak.upgrade());

    let Some(overlay) = overlay else {
        return;
    };

    if active {
        overlay.add_css_class("wallpaper-thumb-wrap-active");
    } else {
        overlay.remove_css_class("wallpaper-thumb-wrap-active");
    }
}

fn wallpaper_card(
    controller: &Rc<WallpaperController>,
    weak_notice: &glib::WeakRef<gtk::Label>,
    live_wallpaper_cards: &LiveWallpaperCards,
    path: PathBuf,
    active: bool,
) -> gtk::Button {
    let button = gtk::Button::new();
    button.add_css_class("wallpaper-card");
    button.set_size_request(CARD_WIDTH, CARD_HEIGHT);
    button.set_halign(gtk::Align::Start);
    button.set_valign(gtk::Align::Start);
    button.set_hexpand(false);
    button.set_vexpand(false);

    let overlay = gtk::Overlay::new();
    overlay.add_css_class("wallpaper-thumb-wrap");
    if active {
        overlay.add_css_class("wallpaper-thumb-wrap-active");
    }
    overlay.set_size_request(CARD_WIDTH, CARD_HEIGHT);
    overlay.set_overflow(gtk::Overflow::Hidden);

    let is_video = is_video_wallpaper(&path);
    match thumbnail_cache_path(&path) {
        Ok(thumbnail_path) if thumbnail_path.is_file() => {
            overlay.set_child(Some(&thumbnail_picture(&thumbnail_path)));
        }
        Ok(_) if is_video => {
            overlay.set_child(Some(&video_preview_placeholder(&path)));
            queue_thumbnail(&overlay, path.clone(), video_thumbnail_queue(), "video");
        }
        Ok(_) => {
            overlay.set_child(Some(&image_preview_placeholder()));
            queue_thumbnail(&overlay, path.clone(), image_thumbnail_queue(), "image");
        }
        Err(error) => {
            let kind = if is_video { "video" } else { "image" };
            warn!(
                path = %path.display(),
                thumbnail_kind = kind,
                %error,
                "failed to resolve wallpaper thumbnail cache path"
            );
            if is_video {
                overlay.set_child(Some(&video_preview_placeholder(&path)));
            } else {
                overlay.set_child(Some(&image_preview_placeholder()));
            }
        }
    }

    if is_video {
        let play_icon = gtk::Label::new(Some(ICON_VIDEO));
        play_icon.add_css_class("wallpaper-video-play-icon");
        play_icon.set_halign(gtk::Align::Start);
        play_icon.set_valign(gtk::Align::Start);
        play_icon.set_margin_start(8);
        play_icon.set_margin_top(6);
        play_icon.set_can_target(false);
        overlay.add_overlay(&play_icon);
    }

    button.set_child(Some(&overlay));

    {
        let mut live_cards = live_wallpaper_cards.borrow_mut();
        live_cards.retain(|_, weak| weak.upgrade().is_some());
        live_cards.insert(path.clone(), overlay.downgrade());
    }

    let controller = Rc::clone(controller);
    let weak_notice = weak_notice.clone();
    button.connect_clicked(move |_| {
        let Some(notice) = weak_notice.upgrade() else {
            return;
        };

        set_optional_label(&notice, None);
        let weak_notice = notice.downgrade();
        controller.request_apply(path.clone(), move |error| {
            if let Some(notice) = weak_notice.upgrade() {
                set_optional_label(&notice, Some(&error.to_string()));
            }
        });
    });

    button
}

fn thumbnail_picture(path: &Path) -> gtk::Picture {
    let picture = gtk::Picture::for_filename(path);
    picture.add_css_class("wallpaper-thumb");
    picture.set_content_fit(gtk::ContentFit::Cover);
    picture.set_can_shrink(true);
    picture.set_size_request(CARD_WIDTH, CARD_HEIGHT);
    picture.set_halign(gtk::Align::Start);
    picture.set_valign(gtk::Align::Start);
    picture.set_hexpand(false);
    picture.set_vexpand(false);
    picture
}

fn video_preview_placeholder(path: &Path) -> gtk::Box {
    let placeholder = gtk::Box::new(gtk::Orientation::Vertical, 4);
    placeholder.add_css_class("wallpaper-video-placeholder");
    placeholder.set_size_request(CARD_WIDTH, CARD_HEIGHT);

    let filename = path
        .file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or("Video wallpaper");
    let label = gtk::Label::new(Some(filename));
    label.add_css_class("wallpaper-video-title");
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_max_width_chars(18);
    label.set_halign(gtk::Align::Center);
    label.set_valign(gtk::Align::Center);
    label.set_vexpand(true);
    placeholder.append(&label);
    placeholder
}

struct ThumbnailJob {
    source: PathBuf,
}

type ThumbnailResult = Result<PathBuf, String>;
type ThumbnailWaiters = HashMap<PathBuf, Vec<async_channel::Sender<ThumbnailResult>>>;

fn thumbnail_waiters() -> &'static Mutex<ThumbnailWaiters> {
    static WAITERS: OnceLock<Mutex<ThumbnailWaiters>> = OnceLock::new();
    WAITERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn thumbnail_job_needed(source: &Path) -> bool {
    thumbnail_waiters()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(source)
        .is_some_and(|waiters| waiters.iter().any(|waiter| !waiter.is_closed()))
}

fn finish_thumbnail_job(source: &Path, result: ThumbnailResult) {
    let waiters = thumbnail_waiters()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(source)
        .unwrap_or_default();
    for waiter in waiters {
        let _ = waiter.send_blocking(result.clone());
    }
}

fn thumbnail_worker_queue(
    queue: &'static OnceLock<async_channel::Sender<ThumbnailJob>>,
    worker_count: usize,
    build: fn(&Path) -> Result<PathBuf, WallpaperError>,
) -> &'static async_channel::Sender<ThumbnailJob> {
    queue.get_or_init(|| {
        let (sender, receiver) = async_channel::bounded::<ThumbnailJob>(THUMBNAIL_QUEUE_CAPACITY);
        for worker_index in 0..worker_count {
            let receiver = receiver.clone();
            let spawn_result = thread::Builder::new()
                .name(format!("wallpaper-thumb-{worker_index}"))
                .spawn(move || {
                    while let Ok(job) = receiver.recv_blocking() {
                        if !thumbnail_job_needed(&job.source) {
                            finish_thumbnail_job(
                                &job.source,
                                Err("thumbnail request was cancelled".to_owned()),
                            );
                            continue;
                        }
                        let result = build(&job.source).map_err(|error| error.to_string());
                        finish_thumbnail_job(&job.source, result);
                    }
                });
            if let Err(error) = spawn_result {
                warn!(%error, worker_index, "failed to start wallpaper thumbnail worker");
            }
        }
        sender
    })
}

fn image_thumbnail_queue() -> &'static async_channel::Sender<ThumbnailJob> {
    static QUEUE: OnceLock<async_channel::Sender<ThumbnailJob>> = OnceLock::new();
    thumbnail_worker_queue(&QUEUE, IMAGE_THUMBNAIL_WORKERS, cached_thumbnail)
}

fn video_thumbnail_queue() -> &'static async_channel::Sender<ThumbnailJob> {
    static QUEUE: OnceLock<async_channel::Sender<ThumbnailJob>> = OnceLock::new();
    thumbnail_worker_queue(&QUEUE, VIDEO_THUMBNAIL_WORKERS, cached_video_thumbnail)
}

fn queue_thumbnail(
    overlay: &gtk::Overlay,
    source: PathBuf,
    queue: &'static async_channel::Sender<ThumbnailJob>,
    kind: &'static str,
) {
    let weak_overlay = overlay.downgrade();
    let (sender, receiver) = async_channel::bounded::<ThumbnailResult>(1);
    let should_queue = {
        let mut waiters = thumbnail_waiters()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match waiters.entry(source.clone()) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                entry.get_mut().push(sender);
                false
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(vec![sender]);
                true
            }
        }
    };

    if should_queue
        && queue
            .try_send(ThumbnailJob {
                source: source.clone(),
            })
            .is_err()
    {
        finish_thumbnail_job(
            &source,
            Err(format!("{kind} thumbnail queue is full or unavailable")),
        );
    }

    glib::MainContext::default().spawn_local(async move {
        let Ok(result) = receiver.recv().await else {
            return;
        };
        let Some(overlay) = weak_overlay.upgrade() else {
            return;
        };

        match result {
            Ok(thumbnail_path) => {
                overlay.set_child(Some(&thumbnail_picture(&thumbnail_path)));
            }
            Err(error) => {
                warn!(thumbnail_kind = kind, %error, "failed to build wallpaper thumbnail")
            }
        }
    });
}

fn image_preview_placeholder() -> gtk::Box {
    let placeholder = gtk::Box::new(gtk::Orientation::Vertical, 0);
    placeholder.add_css_class("wallpaper-image-placeholder");
    placeholder.set_size_request(CARD_WIDTH, CARD_HEIGHT);
    placeholder.set_halign(gtk::Align::Start);
    placeholder.set_valign(gtk::Align::Start);
    placeholder.set_hexpand(false);
    placeholder.set_vexpand(false);

    let icon = gtk::Label::new(Some(ICON_WALLPAPER));
    icon.add_css_class("wallpaper-image-placeholder-icon");
    icon.set_halign(gtk::Align::Center);
    icon.set_valign(gtk::Align::Center);
    icon.set_hexpand(true);
    icon.set_vexpand(true);
    placeholder.append(&icon);
    placeholder
}

fn cached_thumbnail(source: &Path) -> Result<PathBuf, WallpaperError> {
    let cache_path = thumbnail_cache_path(source)?;
    if cache_path.is_file() {
        return Ok(cache_path);
    }

    let cache_dir = cache_path.parent().ok_or(WallpaperError::StatePath)?;
    fs::create_dir_all(cache_dir).map_err(WallpaperError::Io)?;

    let (_, source_width, source_height) = Pixbuf::file_info(source)
        .ok_or_else(|| WallpaperError::InvalidWallpaper(source.to_path_buf()))?;
    if source_width <= 0 || source_height <= 0 {
        return Err(WallpaperError::InvalidWallpaper(source.to_path_buf()));
    }

    let (scaled_width, scaled_height) = cover_dimensions(source_width, source_height);
    let scaled = Pixbuf::from_file_at_scale(source, scaled_width, scaled_height, false)
        .map_err(WallpaperError::Glib)?;

    let crop_x = ((scaled.width() - CARD_WIDTH) / 2).max(0);
    let crop_y = ((scaled.height() - CARD_HEIGHT) / 2).max(0);
    let crop_width = CARD_WIDTH.min(scaled.width());
    let crop_height = CARD_HEIGHT.min(scaled.height());
    let cropped = scaled.new_subpixbuf(crop_x, crop_y, crop_width, crop_height);
    let thumbnail = if crop_width == CARD_WIDTH && crop_height == CARD_HEIGHT {
        cropped
    } else {
        cropped
            .scale_simple(CARD_WIDTH, CARD_HEIGHT, InterpType::Bilinear)
            .ok_or_else(|| WallpaperError::Thumbnail(source.to_path_buf()))?
    };

    let temporary = unique_temporary_path(&cache_path);
    let _ = fs::remove_file(&temporary);
    if let Err(error) = thumbnail.savev(&temporary, "png", &[]) {
        let _ = fs::remove_file(&temporary);
        return Err(WallpaperError::Glib(error));
    }
    if let Err(error) = fs::rename(&temporary, &cache_path) {
        let _ = fs::remove_file(&temporary);
        return Err(WallpaperError::Io(error));
    }
    Ok(cache_path)
}

fn cached_video_thumbnail(source: &Path) -> Result<PathBuf, WallpaperError> {
    let cache_path = thumbnail_cache_path(source)?;
    if cache_path.is_file() {
        return Ok(cache_path);
    }

    let cache_dir = cache_path.parent().ok_or(WallpaperError::StatePath)?;
    fs::create_dir_all(cache_dir).map_err(WallpaperError::Io)?;

    let filter = format!(
        "scale={CARD_WIDTH}:{CARD_HEIGHT}:force_original_aspect_ratio=increase,crop={CARD_WIDTH}:{CARD_HEIGHT}"
    );
    extract_video_frame(source, &cache_path, Some(&filter))?;
    Ok(cache_path)
}

fn cached_video_still(source: &Path) -> Result<PathBuf, WallpaperError> {
    let cache_path = video_still_cache_path(source)?;
    if cache_path.is_file() {
        return Ok(cache_path);
    }

    let cache_dir = cache_path.parent().ok_or(WallpaperError::StatePath)?;
    fs::create_dir_all(cache_dir).map_err(WallpaperError::Io)?;
    extract_video_frame(source, &cache_path, None)?;
    Ok(cache_path)
}

async fn cached_video_still_async(source: PathBuf) -> Result<PathBuf, WallpaperError> {
    run_background_async(move || cached_video_still(&source).map_err(|error| error.to_string()))
        .await
        .ok_or_else(|| WallpaperError::Worker("video still worker stopped".into()))?
        .map_err(WallpaperError::Worker)
}

fn extract_video_frame(
    source: &Path,
    destination: &Path,
    video_filter: Option<&str>,
) -> Result<(), WallpaperError> {
    let temporary = unique_temporary_path(destination);
    let _ = fs::remove_file(&temporary);

    let attempt = |seek: bool| -> Result<(), WallpaperError> {
        let mut args = vec![
            OsString::from("-hide_banner"),
            OsString::from("-loglevel"),
            OsString::from("error"),
            OsString::from("-nostdin"),
        ];
        if seek {
            args.extend([OsString::from("-ss"), OsString::from("1")]);
        }
        args.push(OsString::from("-i"));
        args.push(source.as_os_str().to_owned());
        args.extend(
            ["-frames:v", "1", "-an", "-sn", "-dn"]
                .into_iter()
                .map(OsString::from),
        );
        if let Some(filter) = video_filter {
            args.extend([OsString::from("-vf"), OsString::from(filter)]);
        }
        args.push(OsString::from("-y"));
        args.push(temporary.as_os_str().to_owned());

        match command::status_inherited(FFMPEG.get(), &args, FFMPEG_TIMEOUT) {
            Ok(()) => Ok(()),
            Err(command::StatusError::Io(error)) => Err(WallpaperError::Io(error)),
            Err(command::StatusError::TimedOut | command::StatusError::Failed) => {
                Err(WallpaperError::Ffmpeg(source.to_path_buf()))
            }
        }
    };

    let result = attempt(true).or_else(|_| {
        let _ = fs::remove_file(&temporary);
        attempt(false)
    });

    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }

    if let Err(error) = fs::rename(&temporary, destination) {
        let _ = fs::remove_file(&temporary);
        return Err(WallpaperError::Io(error));
    }
    Ok(())
}

fn unique_temporary_path(destination: &Path) -> PathBuf {
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    let nonce = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let filename = destination
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("wallpaper.png");
    destination.with_file_name(format!(
        ".{filename}.{}.{}.tmp.png",
        std::process::id(),
        nonce
    ))
}

fn video_still_cache_path(source: &Path) -> Result<PathBuf, WallpaperError> {
    let digest = cache_fingerprint(source, VIDEO_STILL_VERSION)?;
    Ok(glib::user_cache_dir()
        .join("obsidian-bar")
        .join("wallpaper-stills")
        .join(format!("{digest}.png")))
}

fn thumbnail_cache_path(source: &Path) -> Result<PathBuf, WallpaperError> {
    let digest = cache_fingerprint(source, THUMBNAIL_VERSION)?;
    Ok(glib::user_cache_dir()
        .join("obsidian-bar")
        .join("wallpaper-thumbs")
        .join(format!("{digest}.png")))
}

fn cache_fingerprint(source: &Path, version: &str) -> Result<String, WallpaperError> {
    let metadata = fs::metadata(source).map_err(WallpaperError::Io)?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok());
    let modified_seconds = modified.map_or(0, |duration| duration.as_secs());
    let modified_nanos = modified.map_or(0, |duration| duration.subsec_nanos());
    let fingerprint = format!(
        "{}:{}:{}:{}:{}",
        source.to_string_lossy(),
        metadata.len(),
        modified_seconds,
        modified_nanos,
        version,
    );

    glib::compute_checksum_for_string(glib::ChecksumType::Sha256, fingerprint)
        .map(|digest| digest.to_string())
        .ok_or_else(|| WallpaperError::Thumbnail(source.to_path_buf()))
}

fn cover_dimensions(source_width: i32, source_height: i32) -> (i32, i32) {
    let source_aspect = source_width as f64 / source_height as f64;
    let target_aspect = CARD_WIDTH as f64 / CARD_HEIGHT as f64;

    if source_aspect >= target_aspect {
        let width = ((CARD_HEIGHT as f64 * source_aspect).ceil() as i32).max(CARD_WIDTH);
        (width, CARD_HEIGHT)
    } else {
        let height = ((CARD_WIDTH as f64 / source_aspect).ceil() as i32).max(CARD_HEIGHT);
        (CARD_WIDTH, height)
    }
}

#[derive(Clone, Copy)]
enum BarFeature {
    Player,
    Workspace,
}

impl BarFeature {
    fn enabled(self, state: BarFeatureState) -> bool {
        match self {
            Self::Player => state.player_visible,
            Self::Workspace => state.workspace_visible,
        }
    }

    fn set(self, controller: &BarFeatureController, enabled: bool) -> bool {
        match self {
            Self::Player => controller.set_player_visible(enabled),
            Self::Workspace => controller.set_workspace_visible(enabled),
        }
    }
}

fn bar_feature_actions(
    controller: &Rc<BarFeatureController>,
    audio_spectrum: &Rc<AudioSpectrumController>,
) -> gtk::Box {
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    actions.add_css_class("wallpaper-header-actions");
    actions.set_halign(gtk::Align::End);
    actions.set_valign(gtk::Align::Start);

    actions.append(&bar_feature_toggle(
        controller,
        BarFeature::Player,
        ICON_PLAYER_ENABLED,
        ICON_PLAYER_DISABLED,
    ));
    actions.append(&bar_feature_toggle(
        controller,
        BarFeature::Workspace,
        ICON_WORKSPACE,
        ICON_WORKSPACE,
    ));

    actions.append(&audio_spectrum_toggle(audio_spectrum));

    actions
}

fn audio_spectrum_toggle(controller: &Rc<AudioSpectrumController>) -> gtk::ToggleButton {
    let button = gtk::ToggleButton::new();
    button.add_css_class("wallpaper-refresh-button");
    button.add_css_class("wallpaper-feature-button");
    button.set_focus_on_click(false);

    let icon = gtk::Label::new(Some(ICON_EQUALIZER));
    icon.add_css_class("wallpaper-refresh-icon");
    button.set_child(Some(&icon));

    let syncing = Rc::new(Cell::new(false));
    {
        let weak_button = button.downgrade();
        let syncing = Rc::clone(&syncing);
        controller.subscribe_state(move |enabled| {
            let Some(button) = weak_button.upgrade() else {
                return false;
            };
            syncing.set(true);
            button.set_active(enabled);
            syncing.set(false);
            true
        });
    }

    {
        let controller = Rc::clone(controller);
        let syncing = Rc::clone(&syncing);
        button.connect_toggled(move |button| {
            if syncing.get() {
                return;
            }
            let requested = button.is_active();
            if !controller.set_enabled(requested) {
                syncing.set(true);
                button.set_active(controller.enabled());
                syncing.set(false);
            }
        });
    }

    button
}

fn bar_feature_toggle(
    controller: &Rc<BarFeatureController>,
    feature: BarFeature,
    enabled_icon: &'static str,
    disabled_icon: &'static str,
) -> gtk::ToggleButton {
    let button = gtk::ToggleButton::new();
    button.add_css_class("wallpaper-refresh-button");
    button.add_css_class("wallpaper-feature-button");
    button.set_focus_on_click(false);

    let icon = gtk::Label::new(None);
    icon.add_css_class("wallpaper-refresh-icon");
    button.set_child(Some(&icon));

    let syncing = Rc::new(Cell::new(false));
    let apply_state = {
        let weak_button = button.downgrade();
        let weak_icon = icon.downgrade();
        let syncing = Rc::clone(&syncing);
        move |state: BarFeatureState| {
            let (Some(button), Some(icon)) = (weak_button.upgrade(), weak_icon.upgrade()) else {
                return false;
            };
            let enabled = feature.enabled(state);
            syncing.set(true);
            button.set_active(enabled);
            icon.set_label(if enabled { enabled_icon } else { disabled_icon });
            syncing.set(false);
            true
        }
    };
    controller.subscribe(apply_state);

    let controller = Rc::clone(controller);
    button.connect_toggled(move |button| {
        if syncing.get() {
            return;
        }

        let requested = button.is_active();
        if feature.set(&controller, requested) {
            return;
        }

        let saved = feature.enabled(controller.state());
        syncing.set(true);
        button.set_active(saved);
        if let Some(icon) = button
            .child()
            .and_then(|child| child.downcast::<gtk::Label>().ok())
        {
            icon.set_label(if saved { enabled_icon } else { disabled_icon });
        }
        syncing.set(false);
    });

    button
}

fn random_menu_button(controller: &Rc<WallpaperController>) -> gtk::MenuButton {
    let button = gtk::MenuButton::new();
    button.add_css_class("wallpaper-random-button");
    button.set_valign(gtk::Align::Center);

    let icon = gtk::Label::new(Some(ICON_SHUFFLE));
    icon.add_css_class("wallpaper-refresh-icon");
    button.set_child(Some(&icon));

    let popover = gtk::Popover::new();
    popover.add_css_class("wallpaper-random-popover");
    popover.set_has_arrow(false);
    popover.set_autohide(true);
    popover.set_position(gtk::PositionType::Bottom);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.add_css_class("wallpaper-random-panel");

    let title = gtk::Label::new(Some("Random wallpapers"));
    title.add_css_class("wallpaper-random-title");
    title.set_xalign(0.0);

    let now_button = gtk::Button::with_label("Change now");
    now_button.add_css_class("wallpaper-random-now");

    let (enabled_value, interval_value) = controller.random_config();
    let enabled = gtk::ToggleButton::new();
    enabled.add_css_class("wallpaper-random-toggle");
    enabled.set_hexpand(true);

    let enabled_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let enabled_label = gtk::Label::new(Some("Change automatically"));
    enabled_label.add_css_class("wallpaper-random-toggle-label");
    enabled_label.set_xalign(0.0);
    enabled_label.set_hexpand(true);

    let enabled_status = gtk::Label::new(Some(if enabled_value { "On" } else { "Off" }));
    enabled_status.add_css_class("wallpaper-random-toggle-state");
    enabled_row.append(&enabled_label);
    enabled_row.append(&enabled_status);
    enabled.set_child(Some(&enabled_row));
    enabled.set_active(enabled_value);

    let interval_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    interval_row.add_css_class("wallpaper-random-interval-row");
    let every_label = gtk::Label::new(Some("Every"));
    every_label.set_xalign(0.0);
    every_label.set_hexpand(true);

    let interval = gtk::SpinButton::with_range(
        f64::from(MIN_RANDOM_INTERVAL_MINUTES),
        f64::from(MAX_RANDOM_INTERVAL_MINUTES),
        1.0,
    );
    interval.add_css_class("wallpaper-random-interval");
    interval.set_numeric(true);
    interval.set_value(f64::from(interval_value));
    interval.set_sensitive(enabled_value);
    let syncing = Rc::new(Cell::new(false));

    let minutes_label = gtk::Label::new(Some("min"));
    interval_row.append(&every_label);
    interval_row.append(&interval);
    interval_row.append(&minutes_label);

    content.append(&title);
    content.append(&now_button);
    content.append(&enabled);
    content.append(&interval_row);
    popover.set_child(Some(&content));
    button.set_popover(Some(&popover));

    {
        let right_click = gtk::GestureClick::new();
        right_click.set_button(gdk::BUTTON_SECONDARY);
        right_click.set_propagation_phase(gtk::PropagationPhase::Capture);
        let controller = Rc::clone(controller);
        let weak_popover = popover.downgrade();
        right_click.connect_pressed(move |gesture, _, _, _| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            if let Some(popover) = weak_popover.upgrade() {
                popover.popdown();
            }
            if let Err(error) = controller.apply_random_wallpaper() {
                warn!(%error, "failed to apply random wallpaper");
            }
        });
        button.add_controller(right_click);
    }

    {
        let controller = Rc::clone(controller);
        let weak_enabled = enabled.downgrade();
        let weak_interval = interval.downgrade();
        let syncing = Rc::clone(&syncing);
        popover.connect_visible_notify(move |popover| {
            if !popover.is_visible() {
                return;
            }
            let (Some(enabled), Some(interval)) = (weak_enabled.upgrade(), weak_interval.upgrade())
            else {
                return;
            };

            let (saved_enabled, saved_interval) = controller.random_config();
            syncing.set(true);
            enabled.set_active(saved_enabled);
            interval.set_value(f64::from(saved_interval));
            interval.set_sensitive(saved_enabled);
            syncing.set(false);
        });
    }

    {
        let controller = Rc::clone(controller);
        now_button.connect_clicked(move |_| {
            if let Err(error) = controller.apply_random_wallpaper() {
                warn!(%error, "failed to apply random wallpaper");
            }
        });
    }

    {
        let weak_status = enabled_status.downgrade();
        enabled.connect_toggled(move |toggle| {
            if let Some(status) = weak_status.upgrade() {
                status.set_text(if toggle.is_active() { "On" } else { "Off" });
            }
        });
    }

    {
        let controller = Rc::clone(controller);
        let weak_interval = interval.downgrade();
        let reverting = Rc::new(Cell::new(false));
        let syncing = Rc::clone(&syncing);
        enabled.connect_toggled(move |toggle| {
            if syncing.get() || reverting.replace(false) {
                return;
            }

            let requested = toggle.is_active();
            if let Some(interval) = weak_interval.upgrade() {
                interval.set_sensitive(requested);
            }
            if let Err(error) = controller.set_random_enabled(requested) {
                warn!(%error, "failed to save random wallpaper setting");
                let (saved, _) = controller.random_config();
                if let Some(interval) = weak_interval.upgrade() {
                    interval.set_sensitive(saved);
                }
                reverting.set(true);
                toggle.set_active(saved);
            }
        });
    }

    {
        let controller = Rc::clone(controller);
        let reverting = Rc::new(Cell::new(false));
        let syncing = Rc::clone(&syncing);
        interval.connect_value_changed(move |spin| {
            if syncing.get() || reverting.replace(false) {
                return;
            }

            let minutes = spin.value_as_int().max(MIN_RANDOM_INTERVAL_MINUTES as i32) as u32;
            if let Err(error) = controller.set_random_interval_minutes(minutes) {
                warn!(%error, "failed to save random wallpaper interval");
                let (_, saved) = controller.random_config();
                reverting.set(true);
                spin.set_value(f64::from(saved));
            }
        });
    }

    button
}

fn wallpaper_refresh_button(icon_text: &str) -> (gtk::Button, gtk::Label, gtk::Spinner) {
    let icon = gtk::Label::new(Some(icon_text));
    icon.add_css_class("wallpaper-refresh-icon");

    let spinner = gtk::Spinner::new();
    spinner.add_css_class("wallpaper-refresh-spinner");
    spinner.set_halign(gtk::Align::Center);
    spinner.set_valign(gtk::Align::Center);
    spinner.set_visible(false);

    let indicator = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    indicator.add_css_class("wallpaper-refresh-indicator");
    indicator.set_halign(gtk::Align::Center);
    indicator.set_valign(gtk::Align::Center);
    indicator.append(&icon);
    indicator.append(&spinner);

    let button = gtk::Button::new();
    button.add_css_class("wallpaper-refresh-button");
    button.set_focus_on_click(false);
    button.set_valign(gtk::Align::Center);
    button.set_child(Some(&indicator));

    (button, icon, spinner)
}

fn header_button(icon: &str) -> gtk::Button {
    let button = gtk::Button::new();
    button.add_css_class("wallpaper-refresh-button");
    button.set_focus_on_click(false);
    button.set_valign(gtk::Align::Center);

    let label = gtk::Label::new(Some(icon));
    label.add_css_class("wallpaper-refresh-icon");
    button.set_child(Some(&label));
    button
}

fn choose_random_wallpaper(
    directory: &Path,
    current: Option<&Path>,
    nonce: u64,
) -> Result<Option<PathBuf>, WallpaperError> {
    let mut items = list_wallpapers(directory)?;
    if items.len() > 1
        && let Some(current) = current
    {
        items.retain(|path| path.as_path() != current);
    }
    if items.is_empty() {
        return Ok(None);
    }

    let time_seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let seed = time_seed ^ nonce.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    Ok(items.get((seed as usize) % items.len()).cloned())
}

fn list_wallpapers(directory: &Path) -> Result<Vec<PathBuf>, WallpaperError> {
    let entries = fs::read_dir(directory).map_err(WallpaperError::Io)?;
    let mut paths = Vec::new();

    for entry in entries {
        let entry = entry.map_err(WallpaperError::Io)?;
        let path = entry.path();
        if is_supported_wallpaper(&path) && path.is_file() {
            paths.push(path);
        }
    }

    paths.sort_by_cached_key(|path| {
        path.file_name()
            .and_then(OsStr::to_str)
            .unwrap_or_default()
            .to_ascii_lowercase()
    });
    Ok(paths)
}

fn is_supported_wallpaper(path: &Path) -> bool {
    is_image_wallpaper(path) || is_video_wallpaper(path)
}

fn is_image_wallpaper(path: &Path) -> bool {
    has_extension(path, IMAGE_EXTENSIONS)
}

fn is_video_wallpaper(path: &Path) -> bool {
    has_extension(path, VIDEO_EXTENSIONS)
}

fn has_extension(path: &Path, extensions: &[&str]) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| {
            extensions
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
}

fn settings_path() -> PathBuf {
    glib::user_state_dir()
        .join("obsidian-bar")
        .join(SETTINGS_FILE)
}

fn default_wallpaper_directory() -> PathBuf {
    glib::user_special_dir(glib::UserDirectory::Pictures)
        .filter(|path| path.is_dir())
        .or_else(|| {
            let pictures = glib::home_dir().join("Pictures");
            pictures.is_dir().then_some(pictures)
        })
        .unwrap_or_else(glib::home_dir)
}

async fn load_mpvpaper_file(socket: PathBuf, path: PathBuf) -> bool {
    run_background_async(move || load_mpvpaper_file_blocking(&socket, &path))
        .await
        .unwrap_or(false)
}

#[derive(Debug, serde::Deserialize)]
struct MpvIpcMessage {
    request_id: Option<u64>,
    error: Option<String>,
    data: Option<serde_json::Value>,
    event: Option<String>,
}

fn write_mpv_request(
    writer: &mut impl Write,
    request_id: u64,
    command: serde_json::Value,
) -> std::io::Result<()> {
    let request = serde_json::json!({
        "command": command,
        "request_id": request_id,
    });
    serde_json::to_writer(&mut *writer, &request).map_err(std::io::Error::other)?;
    writer.write_all(b"\n")
}

#[cfg(unix)]
fn load_mpvpaper_file_blocking(socket: &Path, path: &Path) -> bool {
    use std::{io::ErrorKind, os::unix::net::UnixStream};

    let Ok(mut stream) = UnixStream::connect(socket) else {
        return false;
    };
    if stream
        .set_write_timeout(Some(Duration::from_millis(250)))
        .is_err()
    {
        return false;
    }

    let command = serde_json::json!(["loadfile", path.to_string_lossy().into_owned(), "replace"]);
    if write_mpv_request(&mut stream, MPV_REQUEST_LOAD_FILE, command).is_err() {
        return false;
    }

    let mut reader = BufReader::new(stream);
    let started_at = Instant::now();
    let mut accepted = false;
    let mut file_loaded = false;
    let mut playback_restarted = false;
    let mut unpause_sent = false;
    let mut unpause_accepted = false;

    loop {
        let remaining = MPV_IPC_LOAD_TIMEOUT.saturating_sub(started_at.elapsed());
        if remaining.is_zero() || reader.get_mut().set_read_timeout(Some(remaining)).is_err() {
            return false;
        }

        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let Ok(message) = serde_json::from_str::<MpvIpcMessage>(&line) else {
                    return false;
                };

                if message.request_id == Some(MPV_REQUEST_LOAD_FILE) {
                    if message.error.as_deref() != Some("success") {
                        return false;
                    }
                    accepted = true;
                }

                if message.request_id == Some(MPV_REQUEST_UNPAUSE) {
                    if message.error.as_deref() != Some("success") {
                        return false;
                    }
                    unpause_accepted = true;
                }

                if message.event.as_deref() == Some("file-loaded") {
                    file_loaded = true;
                }

                if accepted && file_loaded && !unpause_sent {
                    let command = serde_json::json!(["set_property", "pause", false]);
                    if write_mpv_request(reader.get_mut(), MPV_REQUEST_UNPAUSE, command).is_err() {
                        return false;
                    }
                    unpause_sent = true;
                }

                if accepted && file_loaded && message.event.as_deref() == Some("playback-restart") {
                    playback_restarted = true;
                }

                if accepted && file_loaded && unpause_accepted && playback_restarted {
                    return true;
                }
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return false;
            }
            Err(_) => return false,
        }
    }

    false
}

#[cfg(not(unix))]
fn load_mpvpaper_file_blocking(_socket: &Path, _path: &Path) -> bool {
    false
}

fn spawn_mpvpaper(path: &Path) -> Result<OwnedMpvpaper, WallpaperError> {
    let program = MPVPAPER.get();
    let output = env::var_os("OBSIDIAN_BAR_MPVPAPER_OUTPUT")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OsString::from(DEFAULT_OUTPUT));
    let base_options = env::var_os("OBSIDIAN_BAR_MPVPAPER_OPTIONS")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OsString::from(DEFAULT_MPV_OPTIONS));
    let ipc_socket = mpvpaper_ipc_socket_path();
    let _ = fs::remove_file(&ipc_socket);

    let mut options = base_options;
    options.push(" input-ipc-server=");
    options.push(ipc_socket.as_os_str());

    let argv: [&OsStr; 6] = [
        program,
        OsStr::new("--auto-pause"),
        OsStr::new("-o"),
        options.as_os_str(),
        output.as_os_str(),
        path.as_os_str(),
    ];

    let process =
        gio::Subprocess::newv(&argv, gio::SubprocessFlags::NONE).map_err(WallpaperError::Glib)?;

    Ok(OwnedMpvpaper {
        process,
        ipc_socket,
    })
}

fn spawn_swaybg(path: &Path) -> Result<OwnedSwaybg, WallpaperError> {
    let program = SWAYBG.get();
    let mode = env::var_os("OBSIDIAN_BAR_SWAYBG_MODE")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OsString::from("fill"));
    let argv: [&OsStr; 5] = [
        program,
        OsStr::new("-m"),
        mode.as_os_str(),
        OsStr::new("-i"),
        path.as_os_str(),
    ];

    let process = gio::Subprocess::newv(&argv, gio::SubprocessFlags::STDERR_SILENCE)
        .map_err(WallpaperError::Glib)?;

    Ok(OwnedSwaybg { process })
}

fn spawn_wallpaper_backend(path: &Path) -> Result<OwnedWallpaperBackend, WallpaperError> {
    if is_image_wallpaper(path) {
        return spawn_swaybg(path).map(OwnedWallpaperBackend::Image);
    }
    if is_video_wallpaper(path) {
        return spawn_mpvpaper(path).map(OwnedWallpaperBackend::Video);
    }
    Err(WallpaperError::InvalidWallpaper(path.to_path_buf()))
}

fn mpvpaper_ipc_socket_path() -> PathBuf {
    let runtime_dir = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();

    runtime_dir.join(format!("obsidian-mpv-{}-{nonce}.sock", std::process::id()))
}

async fn wait_for_mpvpaper_ready(socket: PathBuf) -> bool {
    run_background_async(move || wait_for_mpvpaper_ready_blocking(&socket))
        .await
        .unwrap_or(false)
}

#[cfg(unix)]
fn wait_for_mpvpaper_ready_blocking(socket: &Path) -> bool {
    use std::os::unix::net::UnixStream;

    let started_at = Instant::now();
    while started_at.elapsed() < MPVPAPER_READY_TIMEOUT {
        if let Ok(mut stream) = UnixStream::connect(socket) {
            if stream
                .set_read_timeout(Some(Duration::from_millis(120)))
                .is_err()
                || stream
                    .set_write_timeout(Some(Duration::from_millis(120)))
                    .is_err()
            {
                return false;
            }

            let command = serde_json::json!(["get_property", "vo-configured"]);
            if write_mpv_request(&mut stream, MPV_REQUEST_VO_CONFIGURED, command).is_ok() {
                let mut reader = BufReader::new(stream);
                for _ in 0..8 {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            let Ok(message) = serde_json::from_str::<MpvIpcMessage>(&line) else {
                                break;
                            };
                            if message.request_id == Some(MPV_REQUEST_VO_CONFIGURED) {
                                if message.error.as_deref() == Some("success")
                                    && message.data == Some(serde_json::Value::Bool(true))
                                {
                                    return true;
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }

        thread::sleep(Duration::from_millis(20));
    }

    false
}

#[cfg(not(unix))]
fn wait_for_mpvpaper_ready_blocking(_socket: &Path) -> bool {
    false
}

#[derive(Debug)]
enum WallpaperError {
    InvalidDirectory(PathBuf),
    InvalidWallpaper(PathBuf),
    StatePath,
    Io(std::io::Error),
    Glib(glib::Error),
    Thumbnail(PathBuf),
    Ffmpeg(PathBuf),
    Worker(String),
    Transition(String),
    AppliedButNotSaved(PathBuf, String),
}

impl fmt::Display for WallpaperError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDirectory(path) => {
                write!(f, "invalid wallpaper directory: {}", path.display())
            }
            Self::InvalidWallpaper(path) => {
                write!(f, "unsupported wallpaper: {}", path.display())
            }
            Self::StatePath => f.write_str("failed to resolve wallpaper state directory"),
            Self::Io(error) => write!(f, "wallpaper filesystem error: {error}"),
            Self::Glib(error) => write!(f, "wallpaper backend error: {error}"),
            Self::Thumbnail(path) => write!(f, "failed to create thumbnail for {}", path.display()),
            Self::Ffmpeg(path) => write!(
                f,
                "ffmpeg could not extract a frame from {}",
                path.display()
            ),
            Self::Worker(error) => write!(f, "wallpaper worker error: {error}"),
            Self::Transition(error) => write!(f, "wallpaper transition error: {error}"),
            Self::AppliedButNotSaved(path, error) => write!(
                f,
                "wallpaper was applied but could not be saved for the next start ({}): {error}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for WallpaperError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_wallpaper_avoids_the_current_item_when_possible() {
        let directory = std::env::temp_dir().join(format!(
            "obsidian-wallpaper-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
        ));
        fs::create_dir_all(&directory).expect("temporary directory should be created");
        let first = directory.join("first.png");
        let second = directory.join("second.jpg");
        fs::write(&first, b"not-an-image").expect("first test file should be created");
        fs::write(&second, b"not-an-image").expect("second test file should be created");

        let selected = choose_random_wallpaper(&directory, Some(&first), 1)
            .expect("wallpaper selection should succeed")
            .expect("one wallpaper should be selected");

        assert_eq!(selected, second);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn wallpaper_extensions_are_case_insensitive() {
        assert!(is_image_wallpaper(Path::new("/tmp/a.PNG")));
        assert!(is_video_wallpaper(Path::new("/tmp/a.WebM")));
        assert!(!is_supported_wallpaper(Path::new("/tmp/a.txt")));
    }

    #[test]
    fn transition_selection_reaches_every_kind() {
        let selected = (0..WallpaperTransitionKind::ALL.len() as u64)
            .map(|seed| choose_transition_kind_from_seed(None, seed))
            .collect::<Vec<_>>();

        assert_eq!(selected.as_slice(), &WallpaperTransitionKind::ALL);
    }

    #[test]
    fn transition_selection_does_not_repeat_the_previous_kind() {
        for previous in WallpaperTransitionKind::ALL {
            for seed in 0..128 {
                assert_ne!(
                    choose_transition_kind_from_seed(Some(previous), seed),
                    previous
                );
            }
        }
    }

    #[test]
    fn thumbnail_cover_dimensions_never_undershoot_card() {
        assert_eq!(cover_dimensions(1920, 1080), (150, 84));
        assert_eq!(cover_dimensions(1080, 1920), (144, 256));
        assert_eq!(cover_dimensions(144, 84), (144, 84));
    }

    #[test]
    fn mpv_request_serialization_preserves_command_arguments() {
        let mut output = Vec::new();
        let command = serde_json::json!(["loadfile", "/tmp/a file \"quoted\".png", "replace"]);

        write_mpv_request(&mut output, MPV_REQUEST_LOAD_FILE, command.clone()).unwrap();

        let request: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(request["command"], command);
        assert_eq!(request["request_id"], MPV_REQUEST_LOAD_FILE);
    }

    #[test]
    fn mpv_response_parser_keeps_structured_fields() {
        let response: MpvIpcMessage = serde_json::from_str(
            r#"{"request_id":2,"error":"success","data":"value with spaces"}"#,
        )
        .unwrap();

        assert_eq!(response.request_id, Some(MPV_REQUEST_LOAD_FILE));
        assert_eq!(response.error.as_deref(), Some("success"));
        assert_eq!(response.data, Some(serde_json::json!("value with spaces")));
    }
}
