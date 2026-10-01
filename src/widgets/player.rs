use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::{Rc, Weak},
    time::Duration,
};

use super::dbus::variant_value;
use gio::prelude::*;
use gtk::glib;
use tracing::{debug, warn};

mod ui;
pub use ui::PlayerIndicator;
use ui::PlayerUi;

const DBUS_NAME: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const MPRIS_ROOT_INTERFACE: &str = "org.mpris.MediaPlayer2";
const MPRIS_PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";
const DBUS_TIMEOUT_MS: i32 = 1_000;
const META_CHAR_LIMIT: usize = 80;
const DBUS_RETRY_BASE_DELAY: Duration = Duration::from_secs(1);
const DBUS_RETRY_MAX_DELAY: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlaybackStatus {
    Playing,
    Paused,
    Stopped,
    Unknown,
}

struct PlayerHandle {
    bus_name: String,
    player: gio::DBusProxy,
    root: gio::DBusProxy,
}

impl PlayerHandle {
    fn from_proxies(
        bus_name: String,
        player: gio::DBusProxy,
        root: gio::DBusProxy,
        events: &async_channel::Sender<PlayerEvent>,
    ) -> Option<Self> {
        if player.name_owner().is_none() || root.name_owner().is_none() {
            return None;
        }

        let events_tx = events.clone();
        let changed_bus = bus_name.clone();
        player.connect_g_properties_changed(move |_, _, _| {
            enqueue_player_event(
                &events_tx,
                PlayerEvent::PlayerPropertiesChanged(changed_bus.clone()),
            );
        });

        let events_tx = events.clone();
        let changed_bus = bus_name.clone();
        root.connect_g_properties_changed(move |_, _, _| {
            enqueue_player_event(
                &events_tx,
                PlayerEvent::RootPropertiesChanged(changed_bus.clone()),
            );
        });

        Some(Self {
            bus_name,
            player,
            root,
        })
    }

    fn capabilities(&self, status: PlaybackStatus) -> PlayerCapabilities {
        let can_control = property_bool(&self.player, "CanControl");
        let can_play = property_bool(&self.player, "CanPlay");
        let can_pause = property_bool(&self.player, "CanPause");

        PlayerCapabilities {
            previous: can_control && property_bool(&self.player, "CanGoPrevious"),
            play_pause: can_control
                && match status {
                    PlaybackStatus::Playing => can_pause,
                    PlaybackStatus::Paused | PlaybackStatus::Stopped | PlaybackStatus::Unknown => {
                        can_play
                    }
                },
            next: can_control && property_bool(&self.player, "CanGoNext"),
            raise: property_bool(&self.root, "CanRaise"),
        }
    }

    fn playback_status(&self) -> PlaybackStatus {
        playback_status(&self.player)
    }

    fn has_owner(&self) -> bool {
        self.player.name_owner().is_some() && self.root.name_owner().is_some()
    }

    fn is_selectable(&self) -> bool {
        if !self.has_owner() || !property_bool(&self.player, "CanControl") {
            return false;
        }

        let capabilities = self.capabilities(self.playback_status());
        capabilities.previous || capabilities.play_pause || capabilities.next
    }

    fn identity(&self) -> String {
        property_string(&self.root, "Identity")
            .map(|identity| identity.trim().to_owned())
            .filter(|identity| !identity.is_empty())
            .unwrap_or_else(|| {
                self.bus_name
                    .strip_prefix(MPRIS_PREFIX)
                    .unwrap_or(&self.bus_name)
                    .to_owned()
            })
    }

    fn source_state(&self) -> PlayerSourceState {
        PlayerSourceState {
            status: self.playback_status(),
            selectable: self.is_selectable(),
        }
    }

    fn view(&self, source_count: usize, automatic: bool) -> PlayerView {
        let status = self.playback_status();
        let capabilities = self.capabilities(status);
        let metadata = metadata_text(&self.player, &self.root);
        let display_metadata = truncate_text(&metadata, META_CHAR_LIMIT);

        PlayerView {
            bus_name: self.bus_name.clone(),
            automatic,
            status,
            source_count,
            can_previous: capabilities.previous,
            can_play_pause: capabilities.play_pause,
            can_next: capabilities.next,
            can_raise: capabilities.raise,
            metadata,
            display_metadata,
        }
    }

    fn source_view(&self) -> PlayerSourceView {
        let status = self.playback_status();
        let capabilities = self.capabilities(status);
        PlayerSourceView {
            bus_name: self.bus_name.clone(),
            identity: self.identity(),
            metadata: metadata_text(&self.player, &self.root),
            status,
            can_play_pause: capabilities.play_pause,
            can_previous: capabilities.previous,
            can_next: capabilities.next,
        }
    }
}

struct PlayerCapabilities {
    previous: bool,
    play_pause: bool,
    next: bool,
    raise: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PlayerSourceState {
    status: PlaybackStatus,
    selectable: bool,
}

impl PlayerSourceState {
    fn is_playing(&self) -> bool {
        self.selectable && self.status == PlaybackStatus::Playing
    }
}

fn became_playing(previous: Option<PlayerSourceState>, current: PlayerSourceState) -> bool {
    current.is_playing() && !previous.is_some_and(|state| state.is_playing())
}

fn preferred_source<'a>(
    sources: &[(&'a str, PlayerSourceState)],
    current: Option<&str>,
    pinned: bool,
) -> Option<&'a str> {
    let selected = sources
        .iter()
        .find(|(bus, state)| Some(*bus) == current && state.selectable);
    if let Some((bus, state)) = selected
        && (pinned || state.is_playing())
    {
        return Some(bus);
    }
    sources
        .iter()
        .find(|(_, state)| state.is_playing())
        .or(selected)
        .or_else(|| {
            sources
                .iter()
                .find(|(_, state)| state.selectable && state.status == PlaybackStatus::Paused)
        })
        .or_else(|| sources.iter().find(|(_, state)| state.selectable))
        .map(|(bus, _)| *bus)
}

enum PlayerEvent {
    Discovered(Vec<String>),
    OwnerChanged {
        bus_name: String,
        has_owner: bool,
    },
    PlayerReady {
        bus_name: String,
        request_id: u64,
        player: Option<gio::DBusProxy>,
        root: Option<gio::DBusProxy>,
        error: Option<String>,
    },
    PlayerPropertiesChanged(String),
    RootPropertiesChanged(String),
}

fn enqueue_player_event(sender: &async_channel::Sender<PlayerEvent>, event: PlayerEvent) {
    let _ = sender.try_send(event);
}

#[derive(Clone, PartialEq, Eq)]
struct PlayerView {
    bus_name: String,
    automatic: bool,
    status: PlaybackStatus,
    source_count: usize,
    can_previous: bool,
    can_play_pause: bool,
    can_next: bool,
    can_raise: bool,
    metadata: String,
    display_metadata: String,
}

struct PlayerSourceView {
    bus_name: String,
    identity: String,
    metadata: String,
    status: PlaybackStatus,
    can_play_pause: bool,
    can_previous: bool,
    can_next: bool,
}

#[derive(Clone, Copy)]
enum PlayerAction {
    Previous,
    PlayPause,
    Next,
    Raise,
}

impl PlayerAction {
    fn method(self) -> &'static str {
        match self {
            Self::Previous => "Previous",
            Self::PlayPause => "PlayPause",
            Self::Next => "Next",
            Self::Raise => "Raise",
        }
    }
}

struct PlayerState {
    events: async_channel::Sender<PlayerEvent>,
    players: Vec<PlayerHandle>,
    pending_players: HashMap<String, u64>,
    next_request_id: u64,
    source_states: HashMap<String, PlayerSourceState>,
    active_bus: Option<String>,
    pinned: bool,
    views: Vec<Weak<PlayerUi>>,
}

impl PlayerState {
    fn handle_event(&mut self, event: PlayerEvent) {
        match event {
            PlayerEvent::Discovered(names) => self.add_discovered_players(names),
            PlayerEvent::OwnerChanged {
                bus_name,
                has_owner,
            } => self.owner_changed(&bus_name, has_owner),
            PlayerEvent::PlayerReady {
                bus_name,
                request_id,
                player,
                root,
                error,
            } => self.finish_player_connection(bus_name, request_id, player, root, error),
            PlayerEvent::PlayerPropertiesChanged(bus_name) => {
                self.player_properties_changed(&bus_name)
            }
            PlayerEvent::RootPropertiesChanged(bus_name) => self.root_properties_changed(&bus_name),
        }
    }

    fn add_discovered_players(&mut self, names: Vec<String>) {
        let mut names = names
            .into_iter()
            .filter(|name| name.starts_with(MPRIS_PREFIX))
            .collect::<Vec<_>>();
        names.sort_unstable();
        names.dedup();

        for name in names {
            if self.players.iter().any(|player| player.bus_name == name) {
                continue;
            }
            self.connect_player(&name);
        }

        self.refresh_views();
    }

    fn owner_changed(&mut self, bus_name: &str, has_owner: bool) {
        self.players.retain(|player| player.bus_name != bus_name);
        self.source_states.remove(bus_name);
        // A replacement owner can arrive while the previous proxy is still
        // connecting. Its pending request must not suppress the new connection.
        self.pending_players.remove(bus_name);

        if has_owner {
            self.connect_player(bus_name);
        }

        self.refresh_views();
    }

    fn connect_player(&mut self, bus_name: &str) {
        if self
            .players
            .iter()
            .any(|player| player.bus_name == bus_name)
            || self.pending_players.contains_key(bus_name)
        {
            return;
        }

        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        let request_id = self.next_request_id;
        self.pending_players.insert(bus_name.to_owned(), request_id);

        let bus_name_owned = bus_name.to_owned();
        let events = self.events.clone();
        gio::DBusProxy::for_bus(
            gio::BusType::Session,
            gio::DBusProxyFlags::DO_NOT_AUTO_START,
            None,
            bus_name,
            MPRIS_PATH,
            MPRIS_PLAYER_INTERFACE,
            None::<&gio::Cancellable>,
            move |player_result| {
                let player = match player_result {
                    Ok(player) if player.name_owner().is_some() => player,
                    Ok(_) => {
                        enqueue_player_event(
                            &events,
                            PlayerEvent::PlayerReady {
                                bus_name: bus_name_owned,
                                request_id,
                                player: None,
                                root: None,
                                error: None,
                            },
                        );
                        return;
                    }
                    Err(error) => {
                        enqueue_player_event(
                            &events,
                            PlayerEvent::PlayerReady {
                                bus_name: bus_name_owned,
                                request_id,
                                player: None,
                                root: None,
                                error: Some(error.to_string()),
                            },
                        );
                        return;
                    }
                };

                let root_bus_name = bus_name_owned.clone();
                let ready_bus_name = bus_name_owned;
                let ready_events = events;
                gio::DBusProxy::for_bus(
                    gio::BusType::Session,
                    gio::DBusProxyFlags::DO_NOT_AUTO_START,
                    None,
                    &root_bus_name,
                    MPRIS_PATH,
                    MPRIS_ROOT_INTERFACE,
                    None::<&gio::Cancellable>,
                    move |root_result| {
                        let (root, error) = match root_result {
                            Ok(root) if root.name_owner().is_some() => (Some(root), None),
                            Ok(_) => (None, None),
                            Err(error) => (None, Some(error.to_string())),
                        };
                        let player = root.as_ref().map(|_| player);
                        enqueue_player_event(
                            &ready_events,
                            PlayerEvent::PlayerReady {
                                bus_name: ready_bus_name,
                                request_id,
                                player,
                                root,
                                error,
                            },
                        );
                    },
                );
            },
        );
    }

    fn finish_player_connection(
        &mut self,
        bus_name: String,
        request_id: u64,
        player: Option<gio::DBusProxy>,
        root: Option<gio::DBusProxy>,
        error: Option<String>,
    ) {
        if self.pending_players.get(&bus_name).copied() != Some(request_id) {
            return;
        }
        self.pending_players.remove(&bus_name);

        let (Some(player), Some(root)) = (player, root) else {
            if let Some(error) = error {
                debug!(%error, %bus_name, "failed to connect MPRIS player");
            }
            return;
        };
        let Some(player) = PlayerHandle::from_proxies(bus_name.clone(), player, root, &self.events)
        else {
            return;
        };

        let state = player.source_state();
        let started_playing = became_playing(None, state);
        self.source_states.insert(bus_name.clone(), state);
        self.players.push(player);
        self.players
            .sort_by(|left, right| left.bus_name.cmp(&right.bus_name));
        if started_playing && !self.pinned {
            self.active_bus = Some(bus_name);
        }
        self.refresh_views();
    }

    fn player_properties_changed(&mut self, bus_name: &str) {
        let Some(player) = self
            .players
            .iter()
            .find(|player| player.bus_name == bus_name)
        else {
            return;
        };

        let current = player.source_state();
        let previous = self.source_states.insert(bus_name.to_owned(), current);
        let started_playing = became_playing(previous, current);

        if started_playing && !self.pinned {
            self.active_bus = Some(bus_name.to_owned());
        }

        self.refresh_views();
    }

    fn root_properties_changed(&mut self, _bus_name: &str) {
        self.refresh_views();
    }

    fn refresh_views(&mut self) {
        self.prune_ownerless_players();
        self.ensure_active_bus();
        let view = self.current_view();
        let sources = self.source_views();

        self.views.retain(|weak_ui| {
            let Some(ui) = weak_ui.upgrade() else {
                return false;
            };

            match view.as_ref() {
                Some(view) => ui.render(view, &sources),
                None => ui.clear(),
            }
            true
        });
    }

    fn attach_view(&mut self, ui: &Rc<PlayerUi>) {
        self.views.retain(|weak_ui| weak_ui.upgrade().is_some());
        self.prune_ownerless_players();
        self.ensure_active_bus();
        self.views.push(Rc::downgrade(ui));
        let view = self.current_view();

        match view.as_ref() {
            Some(view) => ui.render(view, &self.source_views()),
            None => ui.clear(),
        }
    }

    fn current_view(&self) -> Option<PlayerView> {
        let active_bus = self.active_bus.as_deref()?;
        let source_count = self
            .players
            .iter()
            .filter(|player| player.is_selectable())
            .count();
        self.players
            .iter()
            .find(|player| player.bus_name == active_bus && player.is_selectable())
            .map(|player| player.view(source_count, !self.pinned))
    }

    fn source_views(&self) -> Vec<PlayerSourceView> {
        self.players
            .iter()
            .filter(|player| player.is_selectable())
            .map(PlayerHandle::source_view)
            .collect()
    }

    fn ensure_active_bus(&mut self) {
        let sources = self
            .players
            .iter()
            .map(|player| (player.bus_name.as_str(), player.source_state()))
            .collect::<Vec<_>>();
        let next =
            preferred_source(&sources, self.active_bus.as_deref(), self.pinned).map(str::to_owned);
        if next != self.active_bus {
            // A disappearing or uncontrollable pinned source returns to Auto.
            self.pinned = false;
        }
        self.active_bus = next;
    }

    fn select_source(&mut self, bus_name: Option<&str>) {
        if let Some(bus_name) = bus_name {
            if !self
                .players
                .iter()
                .any(|player| player.bus_name == bus_name && player.is_selectable())
            {
                return;
            }
            self.active_bus = Some(bus_name.to_owned());
        }
        self.pinned = bus_name.is_some();
        self.refresh_views();
    }

    fn cycle_source(&mut self) {
        let sources = self
            .players
            .iter()
            .filter(|player| player.is_selectable())
            .collect::<Vec<_>>();
        if sources.is_empty() {
            return;
        }
        let index = sources
            .iter()
            .position(|player| Some(player.bus_name.as_str()) == self.active_bus.as_deref())
            .map_or(0, |index| (index + 1) % sources.len());
        let next = sources[index].bus_name.clone();
        self.select_source(Some(&next));
    }

    fn prune_ownerless_players(&mut self) {
        self.players.retain(PlayerHandle::has_owner);
        let players = &self.players;
        self.source_states.retain(|bus_name, _| {
            players
                .iter()
                .any(|player| player.bus_name.as_str() == bus_name.as_str())
        });
    }

    fn call_active(&self, action: PlayerAction) {
        if let Some(bus_name) = self.active_bus.as_deref() {
            self.call_source(bus_name, action);
        }
    }

    fn call_source(&self, bus_name: &str, action: PlayerAction) {
        let Some(player) = self
            .players
            .iter()
            .find(|player| player.bus_name == bus_name && player.has_owner())
        else {
            return;
        };

        let capabilities = player.capabilities(player.playback_status());
        let allowed = match action {
            PlayerAction::Previous => capabilities.previous,
            PlayerAction::PlayPause => capabilities.play_pause,
            PlayerAction::Next => capabilities.next,
            PlayerAction::Raise => capabilities.raise,
        };
        if !allowed {
            return;
        }

        let proxy = match action {
            PlayerAction::Raise => player.root.clone(),
            PlayerAction::Previous | PlayerAction::PlayPause | PlayerAction::Next => {
                player.player.clone()
            }
        };
        let bus_name = player.bus_name.clone();
        let method = action.method();

        proxy.call(
            method,
            None,
            gio::DBusCallFlags::NONE,
            DBUS_TIMEOUT_MS,
            None::<&gio::Cancellable>,
            move |result| {
                if let Err(error) = result {
                    warn!(%error, %bus_name, %method, "MPRIS method failed");
                }
            },
        );
    }
}

pub struct PlayerController {
    manager: Rc<RefCell<Option<gio::DBusProxy>>>,
    manager_init_pending: Rc<Cell<bool>>,
    manager_retry_attempt: Rc<Cell<u32>>,
    state: Rc<RefCell<PlayerState>>,
}

impl PlayerController {
    pub fn new() -> Self {
        let (events_tx, events_rx) = async_channel::unbounded::<PlayerEvent>();
        let manager = Rc::new(RefCell::new(None));

        let state = Rc::new(RefCell::new(PlayerState {
            events: events_tx,
            players: Vec::new(),
            pending_players: HashMap::new(),
            next_request_id: 0,
            source_states: HashMap::new(),
            active_bus: None,
            pinned: false,
            views: Vec::new(),
        }));

        let weak_state = Rc::downgrade(&state);
        glib::MainContext::default().spawn_local(async move {
            while let Ok(event) = events_rx.recv().await {
                let Some(state) = weak_state.upgrade() else {
                    break;
                };
                state.borrow_mut().handle_event(event);
            }
        });

        let manager_init_pending = Rc::new(Cell::new(false));
        let manager_retry_attempt = Rc::new(Cell::new(0));
        Self {
            manager,
            manager_init_pending,
            manager_retry_attempt,
            state,
        }
    }

    pub fn start(&self) {
        initialize_player_manager(
            &self.manager,
            &self.state,
            &self.manager_init_pending,
            &self.manager_retry_attempt,
        );
    }
}

fn initialize_player_manager(
    manager: &Rc<RefCell<Option<gio::DBusProxy>>>,
    state: &Rc<RefCell<PlayerState>>,
    init_pending: &Rc<Cell<bool>>,
    retry_attempt: &Rc<Cell<u32>>,
) {
    if manager.borrow().is_some() || init_pending.replace(true) {
        return;
    }

    let weak_manager = Rc::downgrade(manager);
    let weak_state = Rc::downgrade(state);
    let init_pending = Rc::clone(init_pending);
    let retry_attempt = Rc::clone(retry_attempt);
    gio::DBusProxy::for_bus(
        gio::BusType::Session,
        gio::DBusProxyFlags::DO_NOT_LOAD_PROPERTIES,
        None,
        DBUS_NAME,
        DBUS_PATH,
        DBUS_NAME,
        None::<&gio::Cancellable>,
        move |result| {
            init_pending.set(false);
            let (Some(manager_slot), Some(state)) = (weak_manager.upgrade(), weak_state.upgrade())
            else {
                return;
            };

            let manager_proxy = match result {
                Ok(proxy) => proxy,
                Err(error) => {
                    warn!(%error, "session D-Bus is unavailable; retrying player controls");
                    schedule_player_manager_retry(
                        Rc::downgrade(&manager_slot),
                        Rc::downgrade(&state),
                        init_pending,
                        retry_attempt,
                    );
                    return;
                }
            };

            retry_attempt.set(0);
            let events_tx = state.borrow().events.clone();
            let signal_events = events_tx.clone();
            manager_proxy.connect_g_signal(Some("NameOwnerChanged"), move |_, _, _, parameters| {
                let Some((name, _old_owner, new_owner)) =
                    parameters.get::<(String, String, String)>()
                else {
                    return;
                };
                if !name.starts_with(MPRIS_PREFIX) {
                    return;
                }

                enqueue_player_event(
                    &signal_events,
                    PlayerEvent::OwnerChanged {
                        bus_name: name,
                        has_owner: !new_owner.is_empty(),
                    },
                );
            });

            discover_players(&manager_proxy, events_tx, 0);
            manager_slot.replace(Some(manager_proxy));
        },
    );
}

fn discover_players(
    manager: &gio::DBusProxy,
    events: async_channel::Sender<PlayerEvent>,
    attempt: u32,
) {
    let weak_manager = manager.downgrade();
    manager.call(
        "ListNames",
        None,
        gio::DBusCallFlags::NONE,
        DBUS_TIMEOUT_MS,
        gio::Cancellable::NONE,
        move |result| match result {
            Ok(reply) => {
                if let Some((names,)) = reply.get::<(Vec<String>,)>() {
                    enqueue_player_event(&events, PlayerEvent::Discovered(names));
                }
            }
            Err(error) => {
                warn!(%error, "failed to enumerate MPRIS players; retrying");
                glib::timeout_add_local_once(retry_delay(attempt), move || {
                    if !events.is_closed()
                        && let Some(manager) = weak_manager.upgrade()
                    {
                        discover_players(&manager, events, attempt.saturating_add(1));
                    }
                });
            }
        },
    );
}

fn schedule_player_manager_retry(
    weak_manager: Weak<RefCell<Option<gio::DBusProxy>>>,
    weak_state: Weak<RefCell<PlayerState>>,
    init_pending: Rc<Cell<bool>>,
    retry_attempt: Rc<Cell<u32>>,
) {
    let delay = retry_delay(retry_attempt.get());
    retry_attempt.set(retry_attempt.get().saturating_add(1));

    glib::timeout_add_local_once(delay, move || {
        let (Some(manager), Some(state)) = (weak_manager.upgrade(), weak_state.upgrade()) else {
            return;
        };
        initialize_player_manager(&manager, &state, &init_pending, &retry_attempt);
    });
}

fn retry_delay(attempt: u32) -> Duration {
    let multiplier = 1_u32 << attempt.min(5);
    DBUS_RETRY_BASE_DELAY
        .saturating_mul(multiplier)
        .min(DBUS_RETRY_MAX_DELAY)
}

fn playback_status(proxy: &gio::DBusProxy) -> PlaybackStatus {
    match property_string(proxy, "PlaybackStatus").as_deref() {
        Some("Playing") => PlaybackStatus::Playing,
        Some("Paused") => PlaybackStatus::Paused,
        Some("Stopped") => PlaybackStatus::Stopped,
        _ => PlaybackStatus::Unknown,
    }
}

fn metadata_text(player: &gio::DBusProxy, root: &gio::DBusProxy) -> String {
    let metadata = player
        .cached_property("Metadata")
        .and_then(|variant| variant.get::<HashMap<String, glib::Variant>>());

    let title = metadata
        .as_ref()
        .and_then(|map| map.get("xesam:title"))
        .and_then(variant_value::<String>)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());

    let artist = metadata
        .as_ref()
        .and_then(|map| map.get("xesam:artist"))
        .and_then(variant_value::<Vec<String>>)
        .map(|artists| {
            artists
                .into_iter()
                .map(|artist| artist.trim().to_owned())
                .filter(|artist| !artist.is_empty())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|value| !value.is_empty());

    match (artist, title) {
        (Some(artist), Some(title)) => format!("{artist} — {title}"),
        (_, Some(title)) => title,
        (Some(artist), None) => artist,
        (None, None) => property_string(root, "Identity")
            .map(|identity| identity.trim().to_owned())
            .filter(|identity| !identity.is_empty())
            .unwrap_or_else(|| "Now playing".to_owned()),
    }
}

fn truncate_text(value: &str, limit: usize) -> String {
    if limit == 0 {
        return String::new();
    }

    let mut chars = value.chars();
    let prefix = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_none() {
        return prefix;
    }

    if limit == 1 {
        return "…".to_owned();
    }

    let mut shortened = prefix.chars().take(limit - 1).collect::<String>();
    shortened.push('…');
    shortened
}

fn property_string(proxy: &gio::DBusProxy, name: &str) -> Option<String> {
    proxy
        .cached_property(name)
        .and_then(|value| variant_value(&value))
}

fn property_bool(proxy: &gio::DBusProxy, name: &str) -> bool {
    proxy
        .cached_property(name)
        .and_then(|value| variant_value(&value))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests;
