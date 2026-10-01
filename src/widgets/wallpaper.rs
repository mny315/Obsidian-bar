use super::{Generation, run_background};
use gtk::glib;
use std::{
    cell::{Cell, RefCell},
    ffi::OsStr,
    fmt, fs,
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tracing::{info, warn};

mod picker;
mod thumbnails;
use thumbnails::*;
mod backend;
use backend::*;
pub use picker::WallpaperIndicator;

const SETTINGS_GROUP: &str = "wallpaper";
const SETTINGS_FILE: &str = "wallpaper.ini";

const CARD_WIDTH: i32 = 144;
const CARD_HEIGHT: i32 = 84;
const DEFAULT_RANDOM_INTERVAL_MINUTES: u32 = 30;
const MIN_RANDOM_INTERVAL_MINUTES: u32 = 1;
const MAX_RANDOM_INTERVAL_MINUTES: u32 = 24 * 60;
const AWWW_NAMESPACE: &str = "obsidian-bar";
const AWWW_TRANSITION_NAMESPACE: &str = "obsidian-bar-transition";
const AWWW_TRANSITION_DURATION: Duration = Duration::from_millis(1200);
const RESUME_SUBSCRIPTION_RETRY_BASE_DELAY: Duration = Duration::from_secs(1);
const RESUME_SUBSCRIPTION_RETRY_MAX_DELAY: Duration = Duration::from_secs(30);
const MPVPAPER_READY_TIMEOUT: Duration = Duration::from_secs(5);

const ICON_WALLPAPER: &str = "\u{f0e09}";

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
            .map_or(defaults.random_interval_minutes, |value| {
                value.clamp(MIN_RANDOM_INTERVAL_MINUTES, MAX_RANDOM_INTERVAL_MINUTES)
            });

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

struct PendingApply {
    path: PathBuf,
    force: bool,
    on_error: ApplyErrorHandler,
}

pub struct WallpaperController {
    settings: RefCell<WallpaperSettings>,
    backend: RefCell<Option<WallpaperBackend>>,
    awww_daemon: RefCell<Option<OwnedAwwwDaemon>>,
    subscribers: RefCell<Vec<async_channel::Sender<WallpaperSnapshot>>>,
    sleep_subscription: RefCell<Option<gio::SignalSubscription>>,
    sleep_subscription_pending: Cell<bool>,
    sleep_subscription_retry_pending: Cell<bool>,
    sleep_subscription_retry_attempt: Cell<u32>,
    started: Cell<bool>,
    applying: Cell<bool>,
    pending_apply: RefCell<Option<PendingApply>>,
    lifecycle_generation: Generation,
    selection_generation: Generation,
    random_source: RefCell<Option<glib::SourceId>>,
    random_pick_busy: Cell<bool>,
    random_nonce: Cell<u64>,
}

impl WallpaperController {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            settings: RefCell::new(WallpaperSettings::load()),
            backend: RefCell::new(None),
            awww_daemon: RefCell::new(None),
            subscribers: RefCell::new(Vec::new()),
            sleep_subscription: RefCell::new(None),
            sleep_subscription_pending: Cell::new(false),
            sleep_subscription_retry_pending: Cell::new(false),
            sleep_subscription_retry_attempt: Cell::new(0),
            started: Cell::new(false),
            applying: Cell::new(false),
            pending_apply: RefCell::new(None),
            lifecycle_generation: Generation::default(),
            selection_generation: Generation::default(),
            random_source: RefCell::new(None),
            random_pick_busy: Cell::new(false),
            random_nonce: Cell::new(0),
        })
    }

    pub fn start(self: &Rc<Self>) {
        if self.started.replace(true) {
            return;
        }
        self.lifecycle_generation.bump();
        cleanup_stale_mpvpaper_sockets();

        if let Err(error) = self.restore_at_startup() {
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
        self.stop_wallpaper_backend();
        drop(self.awww_daemon.borrow_mut().take());
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
        self.selection_generation.bump();
        self.broadcast();
        Ok(())
    }

    fn random_config(&self) -> (bool, u32) {
        let settings = self.settings.borrow();
        (settings.random_enabled, settings.random_interval_minutes)
    }

    fn set_random_enabled(self: &Rc<Self>, enabled: bool) -> Result<(), WallpaperError> {
        self.persist_settings_update(|settings| settings.random_enabled = enabled)?;
        self.selection_generation.bump();
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
        let selection = self.selection_generation.current();
        let weak = Rc::downgrade(self);
        run_background(
            move || {
                let result = choose_random_wallpaper(&directory, current.as_deref(), nonce);
                (directory, result)
            },
            move |(directory, result)| {
                let Some(controller) = weak.upgrade() else {
                    return;
                };
                controller.random_pick_busy.set(false);
                if !controller.lifecycle_is_current(lifecycle)
                    || !controller.selection_generation.is_current(selection)
                    || controller.settings.borrow().directory != directory
                {
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
        self.request_apply_with_options(path, false, Rc::new(|_| {}));
    }

    fn request_apply<F>(self: &Rc<Self>, path: PathBuf, on_error: F)
    where
        F: Fn(&WallpaperError) + 'static,
    {
        self.request_apply_with_options(path, false, Rc::new(on_error));
    }

    fn request_force_apply_silent(self: &Rc<Self>, path: PathBuf) {
        self.request_apply_with_options(path, true, Rc::new(|_| {}));
    }

    fn request_apply_with_options(
        self: &Rc<Self>,
        path: PathBuf,
        force: bool,
        on_error: ApplyErrorHandler,
    ) {
        if !self.started.get() {
            return;
        }

        self.selection_generation.bump();
        let request = PendingApply {
            path,
            force,
            on_error,
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

                let result = controller
                    .apply_animated(request.path, request.force, lifecycle)
                    .await;
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

    async fn apply_animated(
        &self,
        path: PathBuf,
        force: bool,
        lifecycle: u64,
    ) -> Result<(), WallpaperError> {
        if !self.lifecycle_is_current(lifecycle) {
            return Ok(());
        }
        if !path.is_absolute() || !path.is_file() || !is_supported_wallpaper(&path) {
            return Err(WallpaperError::InvalidWallpaper(path));
        }

        let same_path = self.settings.borrow().current.as_deref() == Some(path.as_path());
        if !force && same_path && self.backend_matches(&path) {
            return Ok(());
        }

        let awww_frame = if is_video_wallpaper(&path) {
            match cached_video_still_async(path.clone()).await {
                Ok(still) if self.lifecycle_is_current(lifecycle) => still,
                Ok(_) => return Ok(()),
                Err(error) => return Err(error),
            }
        } else {
            path.clone()
        };

        if self.pending_apply.borrow().is_some() {
            return Ok(());
        }
        self.ensure_awww_daemon(lifecycle).await?;
        if !self.lifecycle_is_current(lifecycle) || self.pending_apply.borrow().is_some() {
            return Ok(());
        }
        let transition_daemon = if self.has_active_video_backend() {
            Some(self.start_awww_transition_daemon(lifecycle).await?)
        } else {
            None
        };
        if !self.lifecycle_is_current(lifecycle) {
            return Ok(());
        }
        let transition_namespace = if transition_daemon.is_some() {
            AWWW_TRANSITION_NAMESPACE
        } else {
            AWWW_NAMESPACE
        };

        apply_awww_image(awww_frame.clone(), transition_namespace).await?;
        if !self.lifecycle_is_current(lifecycle) {
            return Ok(());
        }
        // awww owns the animation and can replace it with the next image. Only
        // a handoff to/from mpvpaper needs a completed frame before proceeding.
        if transition_daemon.is_some() || is_video_wallpaper(&path) {
            let started = Instant::now();
            while started.elapsed() < AWWW_TRANSITION_DURATION
                && self.lifecycle_is_current(lifecycle)
                && self.pending_apply.borrow().is_none()
            {
                wait_local(Duration::from_millis(16)).await;
            }
            if self.lifecycle_is_current(lifecycle) && self.pending_apply.borrow().is_some() {
                // Finish the covering frame before retiring an old video; dropping
                // a partially transparent transition surface would expose it again.
                apply_awww_image_immediate(awww_frame.clone(), transition_namespace).await?;
                if !wait_for_awww_image(transition_namespace, awww_frame.clone()).await {
                    return Err(WallpaperError::Backend(
                        "awww did not present the replacement frame in time".into(),
                    ));
                }
            }
        }

        if !self.lifecycle_is_current(lifecycle) {
            return Ok(());
        }
        if transition_daemon.is_some() {
            apply_awww_image_immediate(awww_frame.clone(), AWWW_NAMESPACE).await?;
            if !wait_for_awww_image(AWWW_NAMESPACE, awww_frame.clone()).await {
                return Err(WallpaperError::Backend(
                    "main awww daemon did not present the synchronized wallpaper in time".into(),
                ));
            }
            if !self.lifecycle_is_current(lifecycle) {
                return Ok(());
            }
            self.stop_active_video_backend().await;
            if !self.lifecycle_is_current(lifecycle) {
                return Ok(());
            }
        }

        let started_video = if is_video_wallpaper(&path) && self.pending_apply.borrow().is_none() {
            match self.start_video_backend(path.clone(), lifecycle).await {
                Ok(started) => started,
                Err(error) => {
                    self.restore_after_video_failure(&path, &awww_frame, lifecycle)
                        .await;
                    return Err(error);
                }
            }
        } else {
            false
        };
        if !self.lifecycle_is_current(lifecycle) {
            return Ok(());
        }
        if !started_video {
            self.backend.replace(Some(WallpaperBackend::Image {
                source: path.clone(),
                frame: awww_frame,
            }));
            info!(path = %path.display(), "awww image wallpaper applied");
        }

        if let Some(daemon) = transition_daemon.as_ref() {
            daemon.stop().await;
        }
        drop(transition_daemon);
        self.set_current_runtime(path)
    }

    fn has_active_video_backend(&self) -> bool {
        matches!(
            self.backend.borrow().as_ref(),
            Some(WallpaperBackend::Video { process, .. }) if process.is_alive()
        )
    }

    async fn stop_active_video_backend(&self) {
        let backend = self.backend.borrow_mut().take();
        match backend {
            Some(WallpaperBackend::Video { process, .. }) => process.stop().await,
            backend => {
                self.backend.replace(backend);
            }
        }
    }

    async fn start_video_backend(
        &self,
        source: PathBuf,
        lifecycle: u64,
    ) -> Result<bool, WallpaperError> {
        let video = spawn_mpvpaper(&source)?;
        let started = Instant::now();
        loop {
            if !self.lifecycle_is_current(lifecycle) || self.pending_apply.borrow().is_some() {
                return Ok(false);
            }
            if !video.is_alive() {
                return Err(WallpaperError::Backend(
                    "mpvpaper exited before its video output became ready".into(),
                ));
            }
            if mpvpaper_is_ready(video.ipc_socket.clone()).await {
                break;
            }
            if started.elapsed() >= MPVPAPER_READY_TIMEOUT {
                return Err(WallpaperError::Backend(format!(
                    "mpvpaper video output did not become ready within {} ms",
                    MPVPAPER_READY_TIMEOUT.as_millis()
                )));
            }
            wait_local(Duration::from_millis(20)).await;
        }
        if !self.lifecycle_is_current(lifecycle) || self.pending_apply.borrow().is_some() {
            return Ok(false);
        }
        make_awww_transparent(AWWW_NAMESPACE).await?;
        if !self.lifecycle_is_current(lifecycle) {
            return Ok(false);
        }

        self.backend.replace(Some(WallpaperBackend::Video {
            source: source.clone(),
            process: video,
        }));
        info!(path = %source.display(), "mpvpaper video wallpaper started after awww transition");
        Ok(true)
    }

    async fn ensure_awww_daemon(&self, lifecycle: u64) -> Result<(), WallpaperError> {
        if !self.lifecycle_is_current(lifecycle) {
            return Ok(());
        }
        if self
            .awww_daemon
            .borrow()
            .as_ref()
            .is_some_and(|daemon| !daemon.is_alive())
        {
            drop(self.awww_daemon.borrow_mut().take());
        }
        let owned_daemon_running = self
            .awww_daemon
            .borrow()
            .as_ref()
            .is_some_and(OwnedAwwwDaemon::is_alive);
        let needs_start = !owned_daemon_running && !awww_is_ready(AWWW_NAMESPACE).await;
        if !self.lifecycle_is_current(lifecycle) {
            return Ok(());
        }

        if needs_start {
            self.awww_daemon
                .replace(Some(spawn_awww_daemon(AWWW_NAMESPACE)?));
        }

        let ready = wait_for_awww_ready(AWWW_NAMESPACE).await;
        if !self.lifecycle_is_current(lifecycle) {
            return Ok(());
        }
        if ready {
            if needs_start {
                info!(namespace = AWWW_NAMESPACE, "awww daemon started");
            }
            Ok(())
        } else {
            drop(self.awww_daemon.borrow_mut().take());
            Err(WallpaperError::Backend(
                "awww daemon did not become ready in time".into(),
            ))
        }
    }

    async fn start_awww_transition_daemon(
        &self,
        lifecycle: u64,
    ) -> Result<OwnedAwwwDaemon, WallpaperError> {
        if awww_is_ready(AWWW_TRANSITION_NAMESPACE).await {
            let _ = stop_awww_daemon(AWWW_TRANSITION_NAMESPACE).await;
            if !wait_for_awww_stopped(AWWW_TRANSITION_NAMESPACE).await {
                return Err(WallpaperError::Backend(
                    "previous awww transition daemon did not stop in time".into(),
                ));
            }
        }
        if !self.lifecycle_is_current(lifecycle) {
            return Err(WallpaperError::Worker(
                "awww video transition was cancelled".into(),
            ));
        }

        let daemon = spawn_awww_daemon(AWWW_TRANSITION_NAMESPACE)?;
        if !wait_for_awww_ready(AWWW_TRANSITION_NAMESPACE).await {
            return Err(WallpaperError::Backend(
                "awww transition daemon did not become ready in time".into(),
            ));
        }
        if !self.lifecycle_is_current(lifecycle) {
            return Ok(daemon);
        }
        make_awww_transparent(AWWW_TRANSITION_NAMESPACE).await?;
        Ok(daemon)
    }

    fn restore_at_startup(self: &Rc<Self>) -> Result<(), WallpaperError> {
        if let Some(path) = self.saved_wallpaper()? {
            self.request_apply_silent(path);
        }
        Ok(())
    }

    fn restore(self: &Rc<Self>) -> Result<(), WallpaperError> {
        if self.applying.get() {
            return Ok(());
        }
        if let Some(path) = self.saved_wallpaper()? {
            self.request_force_apply_silent(path);
        }
        Ok(())
    }

    fn saved_wallpaper(&self) -> Result<Option<PathBuf>, WallpaperError> {
        if !self.started.get() {
            return Ok(None);
        }

        let Some(path) = self.settings.borrow().current.clone() else {
            return Ok(None);
        };
        if !path.is_file() {
            warn!(path = %path.display(), "saved wallpaper no longer exists");
            return Ok(None);
        }
        if !path.is_absolute() || !is_supported_wallpaper(&path) {
            return Err(WallpaperError::InvalidWallpaper(path));
        }
        Ok(Some(path))
    }

    fn backend_matches(&self, path: &Path) -> bool {
        match self.backend.borrow().as_ref() {
            Some(backend @ WallpaperBackend::Image { .. }) => {
                backend.matches(path)
                    && self
                        .awww_daemon
                        .borrow()
                        .as_ref()
                        .is_some_and(OwnedAwwwDaemon::is_alive)
            }
            Some(backend @ WallpaperBackend::Video { .. }) => backend.matches(path),
            None => false,
        }
    }

    async fn restore_after_video_failure(
        &self,
        failed_path: &Path,
        failed_frame: &Path,
        lifecycle: u64,
    ) {
        if !self.lifecycle_is_current(lifecycle) {
            return;
        }
        enum RestoreAction {
            Image(PathBuf),
            RevealVideo,
            KeepFallback,
        }

        let action = match self.backend.borrow().as_ref() {
            Some(WallpaperBackend::Image { frame, .. }) => RestoreAction::Image(frame.clone()),
            Some(WallpaperBackend::Video { .. }) => RestoreAction::RevealVideo,
            None => RestoreAction::KeepFallback,
        };
        let keep_fallback = matches!(&action, RestoreAction::KeepFallback);
        let restore_result = match action {
            RestoreAction::Image(source) => {
                let result = apply_awww_image(source, AWWW_NAMESPACE).await;
                if result.is_ok() {
                    wait_awww_transition().await;
                }
                result
            }
            RestoreAction::RevealVideo => make_awww_transparent(AWWW_NAMESPACE).await,
            RestoreAction::KeepFallback => Ok(()),
        };
        if !self.lifecycle_is_current(lifecycle) {
            return;
        }

        if let Err(error) = restore_result {
            warn!(%error, "failed to restore previous wallpaper after video backend failure");
            self.backend.replace(Some(WallpaperBackend::Image {
                source: failed_path.to_path_buf(),
                frame: failed_frame.to_path_buf(),
            }));
        } else if keep_fallback {
            self.backend.replace(Some(WallpaperBackend::Image {
                source: failed_path.to_path_buf(),
                frame: failed_frame.to_path_buf(),
            }));
        }
    }

    fn stop_wallpaper_backend(&self) {
        drop(self.backend.borrow_mut().take());
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

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let time_seed = now.as_secs() ^ u64::from(now.subsec_nanos()).rotate_left(32);
    let seed = time_seed ^ nonce.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let item_count = u64::try_from(items.len()).unwrap_or(u64::MAX);
    let index = usize::try_from(seed % item_count).unwrap_or(0);
    Ok(items.get(index).cloned())
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
    Backend(String),
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
            Self::Backend(error) => write!(f, "wallpaper backend error: {error}"),
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
mod tests;
