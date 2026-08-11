use std::{
    env,
    io::{self, BufRead, BufReader, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use niri_ipc::{
    Action, Event, LayoutSwitchTarget, Reply, Request, Response, Window, Workspace,
    WorkspaceReferenceArg,
    socket::{SOCKET_PATH_ENV, Socket},
    state::{EventStreamStatePart, KeyboardLayoutsState, WindowsState, WorkspacesState},
};

const RECONNECT_DELAY: Duration = Duration::from_secs(1);
const KNOWN_LAYOUTS: &[(&[&str], &str)] = &[
    (&["english (us)", "english (united states)", "us"], "US"),
    (
        &[
            "english (uk)",
            "english (gb)",
            "english (united kingdom)",
            "british",
            "gb",
        ],
        "GB",
    ),
    (&["english", "en"], "EN"),
    (&["russian", "ru"], "RU"),
    (&["ukrainian", "ukraine", "ua"], "UA"),
    (&["german", "deutsch", "de"], "DE"),
    (&["french", "français", "francais", "fr"], "FR"),
    (&["spanish", "español", "espanol", "es"], "ES"),
    (&["italian", "italiano", "it"], "IT"),
    (&["polish", "polski", "pl"], "PL"),
    (&["czech", "cs", "cz"], "CZ"),
    (&["turkish", "tr"], "TR"),
    (&["dutch", "nederlands", "nl"], "NL"),
];

#[derive(Debug, Clone)]
pub enum Update {
    KeyboardLayout(String),
    Windows(Vec<Window>),
    Workspaces(Vec<Workspace>),
}

pub struct EventListener {
    stop: Arc<AtomicBool>,
    sender: async_channel::Sender<Update>,
    active_stream: Arc<Mutex<Option<UnixStream>>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for EventListener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.sender.close();
        if let Ok(mut active_stream) = self.active_stream.lock()
            && let Some(stream) = active_stream.take()
        {
            let _ = stream.shutdown(Shutdown::Both);
        }
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

pub fn spawn_event_listener(sender: async_channel::Sender<Update>) -> EventListener {
    let stop = Arc::new(AtomicBool::new(false));
    let active_stream = Arc::new(Mutex::new(None));
    let thread_stop = Arc::clone(&stop);
    let thread_sender = sender.clone();
    let thread_active_stream = Arc::clone(&active_stream);
    let thread = match thread::Builder::new()
        .name("niri-event-listener".to_owned())
        .spawn(move || listen_for_events(thread_sender, thread_stop, thread_active_stream))
    {
        Ok(thread) => Some(thread),
        Err(error) => {
            tracing::error!(%error, "failed to start niri event listener");
            sender.close();
            None
        }
    };

    EventListener {
        stop,
        sender,
        active_stream,
        thread,
    }
}

pub fn switch_layout_next() -> io::Result<()> {
    send_action(Action::SwitchLayout {
        layout: LayoutSwitchTarget::Next,
    })
}

pub fn focus_workspace(id: u64) -> io::Result<()> {
    send_action(Action::FocusWorkspace {
        reference: WorkspaceReferenceArg::Id(id),
    })
}

fn send_action(action: Action) -> io::Result<()> {
    let mut socket = Socket::connect()?;

    match socket.send(Request::Action(action))? {
        Ok(Response::Handled) => Ok(()),
        Ok(response) => Err(io::Error::other(format!(
            "unexpected niri action response: {response:?}"
        ))),
        Err(message) => Err(io::Error::other(format!("niri IPC error: {message}"))),
    }
}

fn listen_for_events(
    sender: async_channel::Sender<Update>,
    stop: Arc<AtomicBool>,
    active_stream: Arc<Mutex<Option<UnixStream>>>,
) {
    while !stop.load(Ordering::Acquire) && !sender.is_closed() {
        match read_event_stream(&sender, &stop, &active_stream) {
            Ok(()) if stop.load(Ordering::Acquire) || sender.is_closed() => break,
            Ok(()) => tracing::warn!("niri event stream closed; reconnecting"),
            Err(error) => tracing::debug!(%error, "niri event stream unavailable; retrying"),
        }

        thread::park_timeout(RECONNECT_DELAY);
    }
}

fn read_event_stream(
    sender: &async_channel::Sender<Update>,
    stop: &AtomicBool,
    active_stream: &Arc<Mutex<Option<UnixStream>>>,
) -> io::Result<()> {
    let socket_path = env::var_os(SOCKET_PATH_ENV).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("{SOCKET_PATH_ENV} is not set, are you running this within niri?"),
        )
    })?;
    let stream = UnixStream::connect(socket_path)?;
    let shutdown_stream = stream.try_clone()?;
    *active_stream
        .lock()
        .map_err(|_| io::Error::other("niri stream lock is poisoned"))? = Some(shutdown_stream);
    let _active_stream_guard = ActiveStreamGuard(Arc::clone(active_stream));
    if stop.load(Ordering::Acquire) || sender.is_closed() {
        return Ok(());
    }
    let mut stream = BufReader::new(stream);

    let mut request = serde_json::to_string(&Request::EventStream).map_err(io::Error::other)?;
    request.push('\n');
    stream.get_mut().write_all(request.as_bytes())?;

    let mut line = String::new();
    if !read_line(&mut stream, &mut line)? {
        if stop.load(Ordering::Acquire) || sender.is_closed() {
            return Ok(());
        }
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "niri closed the socket before replying to EventStream",
        ));
    }

    let reply: Reply = serde_json::from_str(&line).map_err(io::Error::other)?;
    match reply {
        Ok(Response::Handled) => {}
        Ok(response) => {
            return Err(io::Error::other(format!(
                "unexpected niri EventStream response: {response:?}"
            )));
        }
        Err(message) => return Err(io::Error::other(format!("niri IPC error: {message}"))),
    }
    let _ = stream.get_mut().shutdown(Shutdown::Write);

    let mut keyboard_state = KeyboardLayoutsState::default();
    let mut windows_state = WindowsState::default();
    let mut workspaces_state = WorkspacesState::default();

    loop {
        if stop.load(Ordering::Acquire) || sender.is_closed() {
            return Ok(());
        }

        if !read_line(&mut stream, &mut line)? {
            return Ok(());
        }
        let event: Event = serde_json::from_str(&line).map_err(io::Error::other)?;

        if matches!(&event, Event::KeyboardLayoutSwitched { .. })
            && keyboard_state.keyboard_layouts.is_none()
        {
            tracing::warn!("niri switched layout before sending keyboard layout state");
            continue;
        }

        let (workspaces_changed, event) = match workspaces_state.apply(event) {
            Some(event) => (false, Some(event)),
            None => (true, None),
        };
        let (windows_changed, event) = match event {
            Some(event) => match windows_state.apply(event) {
                Some(event) => (false, Some(event)),
                None => (true, None),
            },
            None => (false, None),
        };
        let keyboard_changed = event.is_some_and(|event| keyboard_state.apply(event).is_none());

        if keyboard_changed
            && let Some(layout) = current_compact_layout(&keyboard_state)
            && sender
                .send_blocking(Update::KeyboardLayout(layout))
                .is_err()
        {
            return Ok(());
        }

        if workspaces_changed {
            let workspaces = sorted_workspaces(&workspaces_state);
            if sender
                .send_blocking(Update::Workspaces(workspaces))
                .is_err()
            {
                return Ok(());
            }
        }

        if windows_changed {
            let windows = sorted_windows(&windows_state);
            if sender.send_blocking(Update::Windows(windows)).is_err() {
                return Ok(());
            }
        }
    }
}

struct ActiveStreamGuard(Arc<Mutex<Option<UnixStream>>>);

impl Drop for ActiveStreamGuard {
    fn drop(&mut self) {
        if let Ok(mut active_stream) = self.0.lock() {
            drop(active_stream.take());
        }
    }
}

fn read_line(stream: &mut BufReader<UnixStream>, line: &mut String) -> io::Result<bool> {
    line.clear();
    match stream.read_line(line) {
        Ok(0) => Ok(false),
        Ok(_) => Ok(true),
        Err(error) => Err(error),
    }
}

fn sorted_windows(state: &WindowsState) -> Vec<Window> {
    let mut windows = state.windows.values().cloned().collect::<Vec<_>>();
    windows.sort_by_key(|window| window.id);
    windows
}

fn sorted_workspaces(state: &WorkspacesState) -> Vec<Workspace> {
    let mut workspaces = state.workspaces.values().cloned().collect::<Vec<_>>();
    workspaces.sort_by(|left, right| {
        left.output
            .cmp(&right.output)
            .then_with(|| left.idx.cmp(&right.idx))
            .then_with(|| left.id.cmp(&right.id))
    });
    workspaces
}

fn current_compact_layout(state: &KeyboardLayoutsState) -> Option<String> {
    let layouts = state.keyboard_layouts.as_ref()?;
    layouts
        .names
        .get(layouts.current_idx as usize)
        .or_else(|| layouts.names.first())
        .map(|name| compact_layout_name(name))
}

fn compact_layout_name(name: &str) -> String {
    let raw = name.trim();
    if raw.is_empty() {
        return "--".to_owned();
    }

    let lower = raw.to_lowercase();
    for (aliases, code) in KNOWN_LAYOUTS {
        if aliases.iter().any(|alias| {
            lower == *alias
                || lower
                    .strip_prefix(alias)
                    .is_some_and(|suffix| suffix.starts_with(' ') || suffix.starts_with('('))
        }) {
            return (*code).to_owned();
        }
    }

    if raw.len() == 2 && raw.chars().all(|ch| ch.is_ascii_alphabetic()) {
        return raw.to_ascii_uppercase();
    }

    let fallback: String = raw
        .chars()
        .skip_while(|ch| !ch.is_alphabetic())
        .filter(|ch| ch.is_alphabetic())
        .take(2)
        .flat_map(char::to_uppercase)
        .collect();

    if fallback.is_empty() {
        "--".to_owned()
    } else {
        fallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use niri_ipc::KeyboardLayouts;

    #[test]
    fn layout_names_are_compact_and_specific() {
        assert_eq!(compact_layout_name("English (US)"), "US");
        assert_eq!(compact_layout_name("English (UK)"), "GB");
        assert_eq!(compact_layout_name("English"), "EN");
        assert_eq!(compact_layout_name("Russian"), "RU");
        assert_eq!(compact_layout_name("ua"), "UA");
        assert_eq!(compact_layout_name(""), "--");
    }

    #[test]
    fn niri_keyboard_state_tracks_layout_switches() {
        let mut state = KeyboardLayoutsState::default();
        let initial = Event::KeyboardLayoutsChanged {
            keyboard_layouts: KeyboardLayouts {
                names: vec!["English (US)".into(), "Russian".into()],
                current_idx: 0,
            },
        };
        let switched = Event::KeyboardLayoutSwitched { idx: 1 };

        assert!(state.apply(initial).is_none());
        assert_eq!(current_compact_layout(&state).as_deref(), Some("US"));

        assert!(state.apply(switched).is_none());
        assert_eq!(current_compact_layout(&state).as_deref(), Some("RU"));
    }
}
