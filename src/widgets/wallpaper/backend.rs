use super::{AWWW_TRANSITION_DURATION, WallpaperError, is_image_wallpaper, is_video_wallpaper};
use crate::widgets::{command, run_background_async};
use gtk::glib;
use std::cell::Cell;
use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{env, fs, thread};
use tracing::warn;

const DEFAULT_MPVPAPER_OUTPUT: &str = "ALL";
const DEFAULT_MPV_OPTIONS: &str = "config=no no-audio loop-file=inf image-display-duration=inf reset-on-next-file=pause hwdec=auto-safe panscan=1.0 terminal=no input-terminal=no input-default-bindings=no osc=no osd-level=0";
static MPVPAPER: command::ExternalProgram = command::ExternalProgram::new(
    "OBSIDIAN_BAR_MPVPAPER_BIN",
    option_env!("OBSIDIAN_BAR_MPVPAPER_BIN"),
    "mpvpaper",
);
static AWWW: command::ExternalProgram = command::ExternalProgram::new(
    "OBSIDIAN_BAR_AWWW_BIN",
    option_env!("OBSIDIAN_BAR_AWWW_BIN"),
    "awww",
);
static AWWW_DAEMON: command::ExternalProgram = command::ExternalProgram::new(
    "OBSIDIAN_BAR_AWWW_DAEMON_BIN",
    option_env!("OBSIDIAN_BAR_AWWW_DAEMON_BIN"),
    "awww-daemon",
);
const AWWW_LAYER: &str = "background";
const MPVPAPER_LAYER: &str = "background";
const AWWW_TRANSITION_FPS: &str = "120";
const AWWW_TRANSITION_STEP: &str = "45";
const AWWW_TRANSPARENT: &str = "00000000";
pub(super) const AWWW_TRANSITIONS: [&str; 7] =
    ["left", "right", "top", "bottom", "wipe", "wave", "grow"];
const AWWW_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const AWWW_QUERY_TIMEOUT: Duration = Duration::from_millis(500);
const AWWW_READY_TIMEOUT: Duration = Duration::from_secs(2);
const MANAGED_SUBPROCESS_STOP_TIMEOUT: Duration = Duration::from_secs(1);
pub(super) const MPV_REQUEST_VO_CONFIGURED: u64 = 1;
const MPVPAPER_SOCKET_PREFIX: &str = "obsidian-mpv-";

pub(super) struct ManagedSubprocess {
    name: &'static str,
    pub(super) process: gio::Subprocess,
    pub(super) running: Rc<Cell<bool>>,
    stopping: Rc<Cell<bool>>,
}

impl ManagedSubprocess {
    pub(super) fn spawn(name: &'static str, argv: &[&OsStr]) -> Result<Self, WallpaperError> {
        let launcher = gio::SubprocessLauncher::new(gio::SubprocessFlags::NONE);
        configure_child_lifetime(&launcher);
        let process = launcher.spawn(argv).map_err(WallpaperError::Glib)?;
        let running = Rc::new(Cell::new(true));
        let stopping = Rc::new(Cell::new(false));
        let running_after_exit = Rc::clone(&running);
        let stopping_after_exit = Rc::clone(&stopping);
        let observed_process = process.clone();
        process.wait_async(None::<&gio::Cancellable>, move |result| {
            if let Err(error) = result {
                warn!(process = name, %error, "failed to monitor wallpaper subprocess");
                return;
            }
            running_after_exit.set(false);
            if !stopping_after_exit.get() {
                warn!(
                    process = name,
                    status = observed_process.status(),
                    "wallpaper subprocess exited unexpectedly"
                );
            }
        });

        Ok(Self {
            name,
            process,
            running,
            stopping,
        })
    }

    pub(super) fn is_running(&self) -> bool {
        self.running.get()
    }

    pub(super) fn request_stop(&self) {
        self.stopping.set(true);
        if self.running.get() {
            self.process.force_exit();
        }
    }

    pub(super) async fn stop(&self) {
        self.request_stop();
        let started_at = Instant::now();
        while self.running.get() && started_at.elapsed() < MANAGED_SUBPROCESS_STOP_TIMEOUT {
            wait_local(Duration::from_millis(10)).await;
        }
        if self.running.get() {
            warn!(
                process = self.name,
                "wallpaper subprocess did not exit before the stop timeout"
            );
        }
    }
}

impl Drop for ManagedSubprocess {
    fn drop(&mut self) {
        self.request_stop();
    }
}

#[cfg(target_os = "linux")]
fn configure_child_lifetime(launcher: &gio::SubprocessLauncher) {
    let parent_pid = unsafe { libc::getpid() };
    launcher.set_child_setup(move || unsafe {
        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 || libc::getppid() != parent_pid
        {
            libc::_exit(1);
        }
    });
}

#[cfg(not(target_os = "linux"))]
fn configure_child_lifetime(_launcher: &gio::SubprocessLauncher) {}

pub(super) struct OwnedMpvpaper {
    pub(super) process: ManagedSubprocess,
    pub(super) ipc_socket: PathBuf,
}

impl OwnedMpvpaper {
    pub(super) fn is_alive(&self) -> bool {
        self.process.is_running()
    }

    pub(super) async fn stop(&self) {
        self.process.stop().await;
    }
}

impl Drop for OwnedMpvpaper {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.ipc_socket);
    }
}

pub(super) struct OwnedAwwwDaemon {
    pub(super) process: ManagedSubprocess,
}

impl OwnedAwwwDaemon {
    pub(super) fn is_alive(&self) -> bool {
        self.process.is_running()
    }

    pub(super) async fn stop(&self) {
        self.process.stop().await;
    }
}

pub(super) enum WallpaperBackend {
    Image {
        source: PathBuf,
        frame: PathBuf,
    },
    Video {
        source: PathBuf,
        process: OwnedMpvpaper,
    },
}

impl WallpaperBackend {
    pub(super) fn matches(&self, path: &Path) -> bool {
        match self {
            Self::Image { source, .. } => is_image_wallpaper(path) && source == path,
            Self::Video {
                source, process, ..
            } => is_video_wallpaper(path) && source == path && process.is_alive(),
        }
    }
}

pub(super) type ApplyErrorHandler = Rc<dyn Fn(&WallpaperError)>;

#[derive(Debug, serde::Deserialize)]
pub(super) struct MpvIpcMessage {
    pub(super) request_id: Option<u64>,
    pub(super) error: Option<String>,
    pub(super) data: Option<serde_json::Value>,
}

pub(super) fn write_mpv_request(
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
fn mpv_ipc_request_blocking(
    socket: &Path,
    request_id: u64,
    command: serde_json::Value,
    timeout: Duration,
) -> Result<MpvIpcMessage, String> {
    let mut stream = crate::unix_socket::connect(socket, timeout)
        .map_err(|error| format!("failed to connect to mpv IPC: {error}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|error| format!("failed to configure mpv IPC read timeout: {error}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|error| format!("failed to configure mpv IPC write timeout: {error}"))?;
    write_mpv_request(&mut stream, request_id, command)
        .map_err(|error| format!("failed to write mpv IPC request: {error}"))?;

    let mut reader = BufReader::new(stream);
    for _ in 0..16 {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return Err("mpv IPC closed before replying".to_owned()),
            Ok(_) => {
                let Ok(message) = serde_json::from_str::<MpvIpcMessage>(&line) else {
                    continue;
                };
                if message.request_id == Some(request_id) {
                    return Ok(message);
                }
            }
            Err(error) => return Err(format!("failed to read mpv IPC response: {error}")),
        }
    }

    Err(format!("mpv did not reply to request {request_id}"))
}

pub(super) fn spawn_mpvpaper(path: &Path) -> Result<OwnedMpvpaper, WallpaperError> {
    let program = MPVPAPER.get();
    let output = env::var_os("OBSIDIAN_BAR_MPVPAPER_OUTPUT")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OsString::from(DEFAULT_MPVPAPER_OUTPUT));
    let base_options = env::var_os("OBSIDIAN_BAR_MPVPAPER_OPTIONS")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OsString::from(DEFAULT_MPV_OPTIONS));
    let ipc_socket = mpvpaper_ipc_socket_path();
    let _ = fs::remove_file(&ipc_socket);

    let mut options = base_options;
    options.push(" input-ipc-server=");
    options.push(ipc_socket.as_os_str());

    let argv = mpvpaper_argv(
        program,
        options.as_os_str(),
        output.as_os_str(),
        path.as_os_str(),
    );

    let process = ManagedSubprocess::spawn("mpvpaper", &argv)?;

    Ok(OwnedMpvpaper {
        process,
        ipc_socket,
    })
}

pub(super) fn mpvpaper_argv<'a>(
    program: &'a OsStr,
    options: &'a OsStr,
    output: &'a OsStr,
    path: &'a OsStr,
) -> [&'a OsStr; 8] {
    [
        program,
        OsStr::new("--auto-pause"),
        OsStr::new("--layer"),
        OsStr::new(MPVPAPER_LAYER),
        OsStr::new("-o"),
        options,
        output,
        path,
    ]
}

pub(super) fn spawn_awww_daemon(
    namespace: &'static str,
) -> Result<OwnedAwwwDaemon, WallpaperError> {
    let argv = awww_daemon_argv(AWWW_DAEMON.get(), OsStr::new(namespace));
    let process = ManagedSubprocess::spawn("awww-daemon", &argv)?;
    Ok(OwnedAwwwDaemon { process })
}

pub(super) fn awww_daemon_argv<'a>(program: &'a OsStr, namespace: &'a OsStr) -> [&'a OsStr; 7] {
    [
        program,
        OsStr::new("--namespace"),
        namespace,
        OsStr::new("--layer"),
        OsStr::new(AWWW_LAYER),
        OsStr::new("--no-cache"),
        OsStr::new("--quiet"),
    ]
}

pub(super) async fn awww_is_ready(namespace: &'static str) -> bool {
    run_background_async(move || awww_is_ready_blocking(namespace))
        .await
        .unwrap_or(false)
}

pub(super) async fn wait_for_awww_ready(namespace: &'static str) -> bool {
    run_background_async(move || {
        let started_at = Instant::now();
        while started_at.elapsed() < AWWW_READY_TIMEOUT {
            if command::output(
                AWWW.get(),
                &["query", "--namespace", namespace],
                AWWW_QUERY_TIMEOUT,
            )
            .is_ok_and(|output| awww_query_has_outputs(&output))
            {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    })
    .await
    .unwrap_or(false)
}

pub(super) async fn wait_for_awww_stopped(namespace: &'static str) -> bool {
    run_background_async(move || {
        let started_at = Instant::now();
        while started_at.elapsed() < AWWW_READY_TIMEOUT {
            if !awww_is_ready_blocking(namespace) {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    })
    .await
    .unwrap_or(false)
}

pub(super) async fn wait_for_awww_image(namespace: &'static str, path: PathBuf) -> bool {
    run_background_async(move || {
        let started_at = Instant::now();
        while started_at.elapsed() < AWWW_READY_TIMEOUT {
            if awww_displays_image_blocking(namespace, &path) {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    })
    .await
    .unwrap_or(false)
}

fn awww_is_ready_blocking(namespace: &str) -> bool {
    command::status(
        AWWW.get(),
        &["query", "--namespace", namespace],
        AWWW_QUERY_TIMEOUT,
    )
    .is_ok()
}

pub(super) fn awww_query_has_outputs(output: &str) -> bool {
    // The socket can answer query before Wayland has announced any outputs.
    // Sending img at that point fails with "none of the requested outputs".
    output.lines().any(|line| {
        let Some((geometry, _)) = line.split_once(", scale:") else {
            return false;
        };
        let Some((_, size)) = geometry.rsplit_once(": ") else {
            return false;
        };
        let Some((width, height)) = size.split_once('x') else {
            return false;
        };
        width.parse::<u32>().is_ok_and(|width| width > 0)
            && height.parse::<u32>().is_ok_and(|height| height > 0)
    })
}

fn awww_displays_image_blocking(namespace: &str, path: &Path) -> bool {
    let Ok(output) = command::output(
        AWWW.get(),
        &["query", "--namespace", namespace],
        AWWW_QUERY_TIMEOUT,
    ) else {
        return false;
    };
    awww_query_displays_image(&output, path)
}

pub(super) fn awww_query_displays_image(output: &str, path: &Path) -> bool {
    const IMAGE_MARKER: &str = "currently displaying: image: ";

    let expected = path.to_string_lossy();
    let mut found_output = false;
    for line in output.lines() {
        let Some((_, displayed)) = line.split_once(IMAGE_MARKER) else {
            continue;
        };
        found_output = true;
        if displayed.trim() != expected {
            return false;
        }
    }
    found_output
}

pub(super) async fn apply_awww_image(
    path: PathBuf,
    namespace: &'static str,
) -> Result<(), WallpaperError> {
    let transition = next_awww_transition();
    let mut args = vec![
        OsString::from("img"),
        OsString::from("--namespace"),
        OsString::from(namespace),
        OsString::from("--transition-type"),
        OsString::from(transition),
        OsString::from("--transition-duration"),
        OsString::from(AWWW_TRANSITION_DURATION.as_secs_f64().to_string()),
        OsString::from("--transition-fps"),
        OsString::from(AWWW_TRANSITION_FPS),
        OsString::from("--transition-step"),
        OsString::from(AWWW_TRANSITION_STEP),
    ];
    append_awww_outputs(&mut args);
    args.push(path.into_os_string());
    run_awww_command(args, "img").await
}

pub(super) async fn apply_awww_image_immediate(
    path: PathBuf,
    namespace: &'static str,
) -> Result<(), WallpaperError> {
    let mut args = vec![
        OsString::from("img"),
        OsString::from("--namespace"),
        OsString::from(namespace),
        OsString::from("--transition-type"),
        OsString::from("simple"),
        OsString::from("--transition-step"),
        OsString::from("255"),
    ];
    append_awww_outputs(&mut args);
    args.push(path.into_os_string());
    run_awww_command(args, "img").await
}

fn next_awww_transition() -> &'static str {
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    let nonce = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seed = now.as_secs()
        ^ u64::from(now.subsec_nanos()).rotate_left(32)
        ^ nonce.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    awww_transition_for_seed(seed)
}

pub(super) fn awww_transition_for_seed(seed: u64) -> &'static str {
    AWWW_TRANSITIONS[(seed % AWWW_TRANSITIONS.len() as u64) as usize]
}

pub(super) async fn make_awww_transparent(namespace: &'static str) -> Result<(), WallpaperError> {
    let mut args = vec![
        OsString::from("clear"),
        OsString::from("--namespace"),
        OsString::from(namespace),
    ];
    append_awww_outputs(&mut args);
    args.push(OsString::from(AWWW_TRANSPARENT));
    run_awww_command(args, "clear").await
}

pub(super) async fn stop_awww_daemon(namespace: &'static str) -> Result<(), WallpaperError> {
    run_awww_command(
        vec![
            OsString::from("kill"),
            OsString::from("--namespace"),
            OsString::from(namespace),
        ],
        "kill",
    )
    .await
}

fn append_awww_outputs(args: &mut Vec<OsString>) {
    if let Some(outputs) =
        env::var_os("OBSIDIAN_BAR_AWWW_OUTPUTS").filter(|value| !value.is_empty())
    {
        args.push(OsString::from("--outputs"));
        args.push(outputs);
    }
}

async fn run_awww_command(
    args: Vec<OsString>,
    command_name: &'static str,
) -> Result<(), WallpaperError> {
    let result = run_background_async(move || {
        command::status_inherited(AWWW.get(), &args, AWWW_COMMAND_TIMEOUT).map_err(|error| {
            match error {
                command::StatusError::Io(error) => format!("failed to start awww: {error}"),
                command::StatusError::TimedOut => format!(
                    "awww {command_name} timed out after {} ms",
                    AWWW_COMMAND_TIMEOUT.as_millis()
                ),
                command::StatusError::Failed => {
                    format!("awww {command_name} exited unsuccessfully")
                }
            }
        })
    })
    .await
    .ok_or_else(|| WallpaperError::Worker(format!("awww {command_name} was cancelled")))?;

    result.map_err(WallpaperError::Backend)
}

pub(super) async fn wait_awww_transition() {
    wait_local(AWWW_TRANSITION_DURATION).await;
}

pub(super) async fn wait_local(duration: Duration) {
    let (sender, receiver) = async_channel::bounded(1);
    glib::timeout_add_local_once(duration, move || {
        let _ = sender.try_send(());
    });
    let _ = receiver.recv().await;
}

fn mpvpaper_ipc_socket_path() -> PathBuf {
    let runtime_dir = env::var_os("XDG_RUNTIME_DIR").map_or_else(env::temp_dir, PathBuf::from);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();

    runtime_dir.join(format!(
        "{MPVPAPER_SOCKET_PREFIX}{}-{nonce}.sock",
        std::process::id()
    ))
}

pub(super) fn mpvpaper_socket_owner(path: &Path) -> Option<u32> {
    path.file_name()?
        .to_str()?
        .strip_prefix(MPVPAPER_SOCKET_PREFIX)?
        .split_once('-')?
        .0
        .parse()
        .ok()
}

#[cfg(target_os = "linux")]
pub(super) fn cleanup_stale_mpvpaper_sockets() {
    let runtime_dir = env::var_os("XDG_RUNTIME_DIR").map_or_else(env::temp_dir, PathBuf::from);
    let Ok(entries) = fs::read_dir(runtime_dir) else {
        return;
    };

    for path in entries.flatten().map(|entry| entry.path()) {
        let Some(owner) = mpvpaper_socket_owner(&path) else {
            continue;
        };
        if !Path::new("/proc").join(owner.to_string()).exists() {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub(super) fn cleanup_stale_mpvpaper_sockets() {}

pub(super) async fn mpvpaper_is_ready(socket: PathBuf) -> bool {
    run_background_async(move || mpvpaper_is_ready_blocking(&socket))
        .await
        .unwrap_or(false)
}

#[cfg(unix)]
fn mpvpaper_is_ready_blocking(socket: &Path) -> bool {
    let command = serde_json::json!(["get_property", "vo-configured"]);
    matches!(mpv_ipc_request_blocking(
        socket, MPV_REQUEST_VO_CONFIGURED, command, Duration::from_millis(120),
    ), Ok(message) if message.error.as_deref() == Some("success")
        && message.data == Some(serde_json::Value::Bool(true)))
}

#[cfg(not(unix))]
fn mpvpaper_is_ready_blocking(_socket: &Path) -> bool {
    false
}
