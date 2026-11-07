use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::{Rc, Weak},
    time::Duration,
};

use super::change_signal::{ChangeSignal, ChangeTicket};
use super::dbus::{properties_changed, unbox_variant, variant_value};
use gio::prelude::*;
use gtk::glib;
use tracing::{debug, warn};

mod audio;
mod ui;
use audio::{AudioMonitor, AudioSource};
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
    process_id: Option<u32>,
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
        let pending = ChangeSignal::default();
        player.connect_g_properties_changed(move |_, changed, invalidated| {
            if properties_changed(
                changed,
                invalidated.iter().map(|name| name.as_str()),
                &[
                    "PlaybackStatus",
                    "Metadata",
                    "CanControl",
                    "CanPlay",
                    "CanPause",
                    "CanGoPrevious",
                    "CanGoNext",
                ],
            ) && let Some(ticket) = pending.claim()
            {
                enqueue_player_event(
                    &events_tx,
                    PlayerEvent::PlayerPropertiesChanged {
                        bus_name: changed_bus.clone(),
                        ticket,
                    },
                );
            }
        });

        let events_tx = events.clone();
        let changed_bus = bus_name.clone();
        let pending = ChangeSignal::default();
        root.connect_g_properties_changed(move |_, changed, invalidated| {
            if properties_changed(
                changed,
                invalidated.iter().map(|name| name.as_str()),
                &["Identity", "CanRaise"],
            ) && let Some(ticket) = pending.claim()
            {
                enqueue_player_event(
                    &events_tx,
                    PlayerEvent::RootPropertiesChanged {
                        bus_name: changed_bus.clone(),
                        ticket,
                    },
                );
            }
        });

        Some(Self {
            bus_name,
            player,
            root,
            process_id: None,
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

fn preferred_audio_source(
    choices: &[(String, Option<u64>)],
    current: Option<&str>,
    pinned: bool,
    resume: Option<&str>,
) -> Option<String> {
    if pinned
        && let Some((bus, _)) = choices
            .iter()
            .find(|(bus, _)| Some(bus.as_str()) == current)
    {
        return Some(bus.clone());
    }
    choices
        .iter()
        .filter(|(_, rank)| rank.is_some())
        .max_by_key(|(bus, rank)| (*rank, Some(bus.as_str()) == current))
        .map(|(bus, _)| bus.clone())
        .or_else(|| {
            choices
                .iter()
                .find(|(bus, _)| Some(bus.as_str()) == resume)
                .map(|(bus, _)| bus.clone())
        })
}

enum PlayerEvent {
    AudioChanged(Option<Vec<AudioSource>>),
    ProcessReady {
        bus_name: String,
        owner: String,
        pid: u32,
    },
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
    PlayerPropertiesChanged {
        bus_name: String,
        ticket: ChangeTicket,
    },
    RootPropertiesChanged {
        bus_name: String,
        ticket: ChangeTicket,
    },
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
    audio: Option<Vec<AudioSource>>,
    audio_started: HashMap<String, u64>,
    audio_sequence: u64,
    last_audio_bus: Option<String>,
    resume_bus: RefCell<Option<String>>,
}

impl PlayerState {
    fn handle_event(&mut self, event: PlayerEvent) {
        match event {
            PlayerEvent::AudioChanged(audio) => self.audio_changed(audio),
            PlayerEvent::ProcessReady {
                bus_name,
                owner,
                pid,
            } => {
                if let Some(player) = self.players.iter_mut().find(|p| {
                    p.bus_name == bus_name
                        && p.player.name_owner().as_deref() == Some(owner.as_str())
                }) {
                    player.process_id = Some(pid);
                    self.refresh_views();
                }
            }
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
            PlayerEvent::PlayerPropertiesChanged { bus_name, ticket } => {
                drop(ticket);
                self.player_properties_changed(&bus_name)
            }
            PlayerEvent::RootPropertiesChanged { bus_name, ticket } => {
                drop(ticket);
                self.root_properties_changed(&bus_name)
            }
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
        if self.last_audio_bus.as_deref() == Some(bus_name) {
            self.last_audio_bus = None;
        }
        if self.resume_bus.borrow().as_deref() == Some(bus_name) {
            self.resume_bus.borrow_mut().take();
        }
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
            gio::DBusProxyFlags::DO_NOT_AUTO_START
                | gio::DBusProxyFlags::GET_INVALIDATED_PROPERTIES,
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
                    gio::DBusProxyFlags::DO_NOT_AUTO_START
                        | gio::DBusProxyFlags::GET_INVALIDATED_PROPERTIES,
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
        let started_playing = self.audio.is_none() && became_playing(None, state);
        self.source_states.insert(bus_name.clone(), state);
        self.resolve_process_id(&player);
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

        if started_playing && !self.pinned && self.audio.is_none() {
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
        let sources = self
            .views
            .iter()
            .filter_map(Weak::upgrade)
            .any(|ui| ui.wants_source_views())
            .then(|| self.source_views());

        self.views.retain(|weak_ui| {
            let Some(ui) = weak_ui.upgrade() else {
                return false;
            };

            match view.as_ref() {
                Some(view) => ui.render(view, sources.as_deref()),
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
            Some(view) => ui.render(view, None),
            None => ui.clear(),
        }
    }

    fn current_view(&self) -> Option<PlayerView> {
        let active_bus = self.active_bus.as_deref()?;
        let source_count = self.players.iter().filter(|p| p.is_selectable()).count()
            + self.uncontrolled_audio().len();
        if let Some(player) = self
            .players
            .iter()
            .find(|player| player.bus_name == active_bus && player.is_selectable())
        {
            return Some(player.view(source_count, !self.pinned));
        }
        self.uncontrolled_audio()
            .into_iter()
            .find(|source| source.key == active_bus)
            .map(|source| PlayerView {
                bus_name: source.key.clone(),
                automatic: !self.pinned,
                status: PlaybackStatus::Playing,
                source_count,
                can_previous: false,
                can_play_pause: false,
                can_next: false,
                can_raise: false,
                metadata: source.name.clone(),
                display_metadata: truncate_text(&source.name, META_CHAR_LIMIT),
            })
    }

    fn source_views(&self) -> Vec<PlayerSourceView> {
        let mut sources: Vec<_> = self
            .players
            .iter()
            .filter(|player| player.is_selectable())
            .map(PlayerHandle::source_view)
            .collect();
        sources.extend(
            self.uncontrolled_audio()
                .into_iter()
                .map(|source| PlayerSourceView {
                    bus_name: source.key.clone(),
                    identity: source.name.clone(),
                    metadata: source.name.clone(),
                    status: PlaybackStatus::Playing,
                    can_play_pause: false,
                    can_previous: false,
                    can_next: false,
                }),
        );
        sources
    }

    fn ensure_active_bus(&mut self) {
        if self.audio.is_some() {
            let mut choices: Vec<_> = self
                .players
                .iter()
                .filter(|p| p.is_selectable())
                .map(|player| {
                    let rank = self.audio_rank(player);
                    (player.bus_name.clone(), rank)
                })
                .collect();
            choices.extend(self.uncontrolled_audio().into_iter().map(|source| {
                (
                    source.key.clone(),
                    self.audio_started.get(&source.key).copied(),
                )
            }));
            let next = {
                // Remember the audible source across stream removal: an
                // external pause notification can arrive before or after it.
                let paused = self.last_audio_bus.as_deref().filter(|bus| {
                    self.players.iter().any(|player| {
                        player.bus_name == *bus
                            && player.playback_status() == PlaybackStatus::Paused
                    })
                });
                preferred_audio_source(
                    &choices,
                    self.active_bus.as_deref(),
                    self.pinned,
                    self.resume_bus.borrow().as_deref().or(paused),
                )
            };
            if let Some((bus, _)) = choices
                .iter()
                .find(|(bus, rank)| Some(bus.as_str()) == next.as_deref() && rank.is_some())
            {
                self.last_audio_bus = self
                    .players
                    .iter()
                    .any(|player| player.bus_name == *bus)
                    .then(|| bus.clone());
            }
            if self.pinned && next != self.active_bus {
                self.pinned = false;
            }
            if next != self.active_bus {
                debug!(source = ?next, "automatic media source follows audio activity");
            }
            self.active_bus = next;
            return;
        }
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
                .source_views()
                .iter()
                .any(|source| source.bus_name == bus_name)
            {
                return;
            }
            self.active_bus = Some(bus_name.to_owned());
        }
        self.pinned = bus_name.is_some();
        self.refresh_views();
    }

    fn cycle_source(&mut self) {
        let sources = self.source_views();
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

        let status = player.playback_status();
        let capabilities = player.capabilities(status);
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
        if matches!(action, PlayerAction::PlayPause) && self.audio.is_some() && !self.pinned {
            self.resume_bus.replace(Some(bus_name.clone()));
        }
        // Let the source toggle its actual state. Stream activity can lag or
        // belong to another tab, so it must not choose Play versus Pause.
        let method = if matches!(action, PlayerAction::PlayPause)
            && !property_bool(&player.player, "CanPause")
        {
            "Play"
        } else {
            action.method()
        };

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

    fn resolve_process_id(&self, player: &PlayerHandle) {
        let Some(owner) = player.player.name_owner() else {
            return;
        };
        let owner = owner.to_string();
        let bus_name = player.bus_name.clone();
        let events = self.events.clone();
        player.player.connection().call(
            Some(DBUS_NAME),
            DBUS_PATH,
            DBUS_NAME,
            "GetConnectionUnixProcessID",
            Some(&(owner.as_str(),).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            DBUS_TIMEOUT_MS,
            gio::Cancellable::NONE,
            move |reply| {
                if let Ok(reply) = reply
                    && let Some((pid,)) = reply.get::<(u32,)>()
                {
                    enqueue_player_event(
                        &events,
                        PlayerEvent::ProcessReady {
                            bus_name,
                            owner,
                            pid,
                        },
                    );
                }
            },
        );
    }

    fn matches_audio(player: &PlayerHandle, source: &AudioSource) -> bool {
        let desktop = property_string(&player.root, "DesktopEntry").unwrap_or_default();
        source.matches(player.process_id, &[&player.identity(), &desktop])
    }

    fn audio_rank(&self, player: &PlayerHandle) -> Option<u64> {
        self.audio
            .as_ref()?
            .iter()
            .filter(|source| Self::matches_audio(player, source))
            .filter_map(|source| self.audio_started.get(&source.key).copied())
            .max()
    }

    fn uncontrolled_audio(&self) -> Vec<&AudioSource> {
        self.audio
            .iter()
            .flatten()
            .filter(|source| {
                !self
                    .players
                    .iter()
                    .any(|player| player.is_selectable() && Self::matches_audio(player, source))
            })
            .collect()
    }

    fn audio_changed(&mut self, audio: Option<Vec<AudioSource>>) {
        if let Some(active) = &audio {
            for source in active {
                let was_active = self
                    .audio
                    .as_ref()
                    .is_some_and(|old| old.iter().any(|s| s.key == source.key));
                if !was_active {
                    self.audio_sequence = self.audio_sequence.saturating_add(1);
                    self.audio_started
                        .insert(source.key.clone(), self.audio_sequence);
                    // A newly audible application supersedes an older paused
                    // resume target, even if it has no MPRIS controls.
                    self.last_audio_bus = None;
                    self.resume_bus.borrow_mut().take();
                }
            }
            self.audio_started
                .retain(|key, _| active.iter().any(|source| &source.key == key));
        }
        self.audio = audio;
        self.refresh_views();
    }
}

pub struct PlayerController {
    manager: Rc<RefCell<Option<gio::DBusProxy>>>,
    manager_init_pending: Rc<Cell<bool>>,
    manager_retry_attempt: Rc<Cell<u32>>,
    state: Rc<RefCell<PlayerState>>,
    audio_monitor: RefCell<Option<AudioMonitor>>,
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
            audio: None,
            audio_started: HashMap::new(),
            audio_sequence: 0,
            last_audio_bus: None,
            resume_bus: RefCell::new(None),
        }));

        let weak_state = Rc::downgrade(&state);
        glib::MainContext::default().spawn_local(async move {
            let mut processed = 0;
            while let Ok(event) = events_rx.recv().await {
                let Some(state) = weak_state.upgrade() else {
                    break;
                };
                state.borrow_mut().handle_event(event);
                processed += 1;
                if processed >= 32 {
                    glib::timeout_future(Duration::ZERO).await;
                    processed = 0;
                }
            }
        });

        let manager_init_pending = Rc::new(Cell::new(false));
        let manager_retry_attempt = Rc::new(Cell::new(0));
        Self {
            manager,
            manager_init_pending,
            manager_retry_attempt,
            state,
            audio_monitor: RefCell::new(None),
        }
    }

    pub fn start(&self) {
        if self.audio_monitor.borrow().is_none() {
            match AudioMonitor::start(self.state.borrow().events.clone()) {
                Ok(monitor) => {
                    self.audio_monitor.replace(Some(monitor));
                }
                Err(error) => warn!(%error, "cannot monitor audio activity; using MPRIS status"),
            }
        }
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
    player
        .cached_property("Metadata")
        .as_ref()
        .and_then(track_metadata_text)
        .or_else(|| {
            property_string(root, "Identity")
                .map(|identity| identity.trim().to_owned())
                .filter(|identity| !identity.is_empty())
        })
        .unwrap_or_else(|| "Now playing".to_owned())
}

fn track_metadata_text(metadata: &glib::Variant) -> Option<String> {
    if metadata.type_() != glib::VariantTy::VARDICT {
        return None;
    }
    let mut title = None;
    let mut artist = None;
    // Art URLs, album data and other metadata do not need owned Rust map keys
    // or values when the bar displays only the artist and track title.
    for entry in metadata.iter() {
        let name = entry.child_value(0);
        match name.str() {
            Some("xesam:title") => {
                title = variant_value::<String>(&unbox_variant(&entry.child_value(1)))
                    .map(|value| value.trim().to_owned())
                    .filter(|value| !value.is_empty());
            }
            Some("xesam:artist") => {
                artist = variant_value::<Vec<String>>(&unbox_variant(&entry.child_value(1)))
                    .map(|artists| {
                        artists
                            .iter()
                            .map(|artist| artist.trim())
                            .filter(|artist| !artist.is_empty())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .filter(|value| !value.is_empty());
            }
            _ => {}
        }
    }

    match (artist, title) {
        (Some(artist), Some(title)) => Some(format!("{artist} — {title}")),
        (_, Some(title)) => Some(title),
        (Some(artist), None) => Some(artist),
        (None, None) => None,
    }
}

fn truncate_text(value: &str, limit: usize) -> String {
    if limit == 0 {
        return String::new();
    }

    let mut chars = value.chars();
    let mut prefix = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_none() {
        return prefix;
    }

    prefix.pop();
    prefix.push('…');
    prefix
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
