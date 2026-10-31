use super::*;

#[test]
fn auto_uses_audio_activity_instead_of_a_stale_playing_flag() {
    let choices = vec![("chrome".into(), None), ("music".into(), Some(1))];
    assert_eq!(
        preferred_audio_source(&choices, Some("chrome"), false, None).as_deref(),
        Some("music")
    );
    assert_eq!(
        preferred_audio_source(&choices[..1], Some("chrome"), false, None),
        None
    );
}

#[test]
fn auto_switches_to_new_audio_and_returns_to_still_playing_source() {
    let choices = vec![
        ("chrome".into(), Some(1)),
        ("audio:telegram".into(), Some(2)),
    ];
    assert_eq!(
        preferred_audio_source(&choices, Some("chrome"), false, None).as_deref(),
        Some("audio:telegram")
    );
    assert_eq!(
        preferred_audio_source(&choices[..1], Some("audio:telegram"), false, None).as_deref(),
        Some("chrome")
    );
    let stopped = vec![("chrome".into(), None)];
    assert_eq!(
        preferred_audio_source(&stopped, Some("audio:telegram"), false, None),
        None
    );
}

#[test]
fn explicit_pause_can_resume_but_does_not_override_audible_audio() {
    let choices = vec![("paused".into(), None), ("active".into(), Some(2))];
    assert_eq!(
        preferred_audio_source(&choices[..1], Some("paused"), false, Some("paused")).as_deref(),
        Some("paused")
    );
    assert_eq!(
        preferred_audio_source(&choices, Some("paused"), false, Some("paused")).as_deref(),
        Some("active")
    );
    assert_eq!(
        preferred_audio_source(&choices, Some("paused"), true, None).as_deref(),
        Some("paused")
    );
}

#[test]
fn audio_source_pin_disappears_when_the_stream_is_removed() {
    let choices = vec![("music".into(), Some(4))];
    assert_eq!(
        preferred_audio_source(&choices, Some("audio:gone"), true, None).as_deref(),
        Some("music")
    );
    assert_eq!(
        preferred_audio_source(&[], Some("audio:gone"), true, None),
        None
    );
}

fn source(status: PlaybackStatus, selectable: bool) -> PlayerSourceState {
    PlayerSourceState { status, selectable }
}

#[test]
fn automatic_selection_always_prefers_playing_over_paused_or_stopped() {
    for status in [
        PlaybackStatus::Paused,
        PlaybackStatus::Stopped,
        PlaybackStatus::Unknown,
    ] {
        let sources = [
            ("old", source(status, true)),
            ("music", source(PlaybackStatus::Playing, true)),
        ];
        assert_eq!(
            preferred_source(&sources, Some("old"), false),
            Some("music")
        );
        assert_eq!(preferred_source(&sources, None, false), Some("music"));
    }
}

#[test]
fn automatic_selection_keeps_current_playing_source_and_paused_fallback() {
    let mut sources = [
        ("a", source(PlaybackStatus::Playing, true)),
        ("b", source(PlaybackStatus::Playing, true)),
    ];
    assert_eq!(preferred_source(&sources, Some("b"), false), Some("b"));
    sources[1].1.status = PlaybackStatus::Paused;
    assert_eq!(preferred_source(&sources, Some("b"), false), Some("a"));
    sources[0].1.status = PlaybackStatus::Paused;
    assert_eq!(preferred_source(&sources, Some("b"), false), Some("b"));
}

#[test]
fn pin_is_explicit_and_lost_when_its_source_is_unavailable() {
    let mut sources = [
        ("pin", source(PlaybackStatus::Paused, true)),
        ("music", source(PlaybackStatus::Playing, true)),
    ];
    assert_eq!(preferred_source(&sources, Some("pin"), true), Some("pin"));
    assert_eq!(
        preferred_source(&sources, Some("pin"), false),
        Some("music")
    );
    sources[0].1.selectable = false;
    assert_eq!(preferred_source(&sources, Some("pin"), true), Some("music"));
    assert_eq!(
        preferred_source(&sources[1..], Some("pin"), true),
        Some("music")
    );
}

#[test]
fn automatic_selection_ignores_uncontrollable_players_and_handles_empty_list() {
    let sources = [
        ("busy", source(PlaybackStatus::Playing, false)),
        ("stopped", source(PlaybackStatus::Stopped, true)),
        ("paused", source(PlaybackStatus::Paused, true)),
    ];
    assert_eq!(preferred_source(&sources, None, false), Some("paused"));
    assert_eq!(preferred_source(&sources[..1], None, false), None);
    assert_eq!(preferred_source(&[], Some("gone"), true), None);
}

#[test]
fn activates_only_when_a_selectable_source_starts_playing() {
    assert!(became_playing(
        Some(source(PlaybackStatus::Paused, true)),
        source(PlaybackStatus::Playing, true),
    ));
    assert!(became_playing(
        Some(source(PlaybackStatus::Playing, false)),
        source(PlaybackStatus::Playing, true),
    ));

    assert!(!became_playing(
        Some(source(PlaybackStatus::Playing, true)),
        source(PlaybackStatus::Playing, true),
    ));
    assert!(!became_playing(
        Some(source(PlaybackStatus::Paused, true)),
        source(PlaybackStatus::Playing, false),
    ));
}

#[test]
fn newly_discovered_playing_source_is_an_activation_candidate() {
    assert!(became_playing(None, source(PlaybackStatus::Playing, true),));
    assert!(!became_playing(None, source(PlaybackStatus::Paused, true),));
}

#[test]
fn metadata_formatting_and_truncation_are_character_safe() {
    let metadata = HashMap::from([
        ("xesam:title", "  日本語 title  ".to_variant()),
        (
            "xesam:artist",
            vec!["  Первый ", "", " Второй  "].to_variant(),
        ),
        ("mpris:artUrl", "file:///unused/art.png".to_variant()),
    ])
    .to_variant();
    assert_eq!(
        track_metadata_text(&metadata).as_deref(),
        Some("Первый, Второй — 日本語 title")
    );
    let malformed = HashMap::from([("xesam:title", 42_u32.to_variant())]).to_variant();
    assert_eq!(track_metadata_text(&malformed), None);
    assert_eq!(track_metadata_text(&42_u32.to_variant()), None);
    assert_eq!(truncate_text("абвг", 3), "аб…");
    assert_eq!(truncate_text("abc", 3), "abc");
    assert_eq!(truncate_text("abc", 1), "…");
    assert_eq!(truncate_text("abc", 0), "");
}

#[test]
#[ignore = "requires session D-Bus; emits property notifications only on isolated proxies"]
fn repeated_property_signals_queue_one_update_and_keep_the_latest_metadata() {
    let context = glib::MainContext::new();
    context
        .with_thread_default(|| {
            let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).unwrap();
            let make_proxy = |interface| {
                gio::DBusProxy::new_sync(
                    &bus,
                    gio::DBusProxyFlags::DO_NOT_LOAD_PROPERTIES
                        | gio::DBusProxyFlags::DO_NOT_CONNECT_SIGNALS,
                    None,
                    bus.unique_name().as_deref(),
                    "/ObsidianPropertyBurstFixture",
                    interface,
                    gio::Cancellable::NONE,
                )
                .unwrap()
            };
            let player = make_proxy(MPRIS_PLAYER_INTERFACE);
            let root = make_proxy(MPRIS_ROOT_INTERFACE);
            for key in [
                "CanControl",
                "CanPlay",
                "CanPause",
                "CanGoPrevious",
                "CanGoNext",
            ] {
                player.set_cached_property(key, Some(&true.to_variant()));
            }
            player.set_cached_property("PlaybackStatus", Some(&"Paused".to_variant()));
            let (events, receiver) = async_channel::unbounded();
            let handle =
                PlayerHandle::from_proxies("fixture".into(), player.clone(), root.clone(), &events)
                    .unwrap();
            let mut state = PlayerState {
                events,
                players: vec![handle],
                pending_players: HashMap::new(),
                next_request_id: 0,
                source_states: HashMap::new(),
                active_bus: None,
                pinned: false,
                views: Vec::new(),
                audio: None,
                audio_started: HashMap::new(),
                audio_sequence: 0,
                resume_bus: RefCell::new(None),
            };
            let emit = |proxy: &gio::DBusProxy, key: &str, value: glib::Variant| {
                proxy.set_cached_property(key, Some(&value));
                let changed = HashMap::from([(key, value)]).to_variant();
                proxy.emit_by_name::<()>(
                    "g-properties-changed",
                    &[&changed, &glib::StrV::default()],
                );
            };
            for index in 0..1000 {
                emit(
                    &player,
                    "Metadata",
                    HashMap::from([("xesam:title", format!("Track {index}").to_variant())])
                        .to_variant(),
                );
                emit(&root, "Identity", format!("Source {index}").to_variant());
            }
            assert_eq!(receiver.len(), 2);
            while let Ok(event) = receiver.try_recv() {
                state.handle_event(event);
            }
            assert_eq!(state.current_view().unwrap().metadata, "Track 999");
            emit(&player, "PlaybackStatus", "Playing".to_variant());
            assert_eq!(
                receiver.len(),
                1,
                "processing a burst must allow the next notification"
            );
            state.handle_event(receiver.try_recv().unwrap());
            assert_eq!(
                state.current_view().unwrap().status,
                PlaybackStatus::Playing
            );
            emit(&player, "Position", 123_i64.to_variant());
            assert!(receiver.is_empty());
        })
        .unwrap();
}

#[test]
#[ignore = "requires session D-Bus; run alone"]
fn discovery_recovers_from_a_transient_list_names_failure() {
    let context = glib::MainContext::default();
    let _guard = context.acquire().unwrap();
    let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).unwrap();
    let xml = gio::DBusNodeInfo::for_xml(
        "<node><interface name='dev.obsidian.DiscoveryTest'><method name='ListNames'><arg type='as' direction='out'/></method></interface></node>",
    ).unwrap();
    let calls = Rc::new(Cell::new(0));
    let recorded = calls.clone();
    let registration = bus
        .register_object(
            "/ObsidianDiscoveryTest",
            &xml.lookup_interface("dev.obsidian.DiscoveryTest").unwrap(),
        )
        .method_call(move |_, _, _, _, _, _, invocation| {
            let count = recorded.get() + 1;
            recorded.set(count);
            if count == 1 {
                invocation.return_dbus_error(
                    "org.freedesktop.DBus.Error.Failed",
                    "transient fixture failure",
                );
            } else {
                invocation.return_value(Some(
                    &(vec!["org.mpris.MediaPlayer2.Fixture"],).to_variant(),
                ));
            }
        })
        .build()
        .unwrap();
    let proxy = gio::DBusProxy::new_sync(
        &bus,
        gio::DBusProxyFlags::DO_NOT_LOAD_PROPERTIES | gio::DBusProxyFlags::DO_NOT_CONNECT_SIGNALS,
        None,
        bus.unique_name().as_deref(),
        "/ObsidianDiscoveryTest",
        "dev.obsidian.DiscoveryTest",
        gio::Cancellable::NONE,
    )
    .unwrap();
    let (sender, receiver) = async_channel::unbounded();
    discover_players(&proxy, sender, 0);
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let event = loop {
        context.iteration(false);
        if let Ok(event) = receiver.try_recv() {
            break event;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "discovery did not recover"
        );
        std::thread::sleep(Duration::from_millis(1));
    };
    assert_eq!(calls.get(), 2);
    assert!(
        matches!(event, PlayerEvent::Discovered(names) if names == ["org.mpris.MediaPlayer2.Fixture"])
    );
    bus.unregister_object(registration).unwrap();
}

#[test]
#[ignore = "requires session D-Bus; uses only an isolated fixture player"]
fn audio_activity_drives_selection_and_transport_commands() {
    let context = glib::MainContext::new();
    context.with_thread_default(|| {
        let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).unwrap();
        let xml = gio::DBusNodeInfo::for_xml(
            "<node><interface name='org.mpris.MediaPlayer2.Player'><method name='Pause'/><method name='Play'/><method name='PlayPause'/></interface></node>",
        ).unwrap();
        let calls = Rc::new(RefCell::new(Vec::new()));
        let recorded = calls.clone();
        let registration = bus.register_object(MPRIS_PATH, &xml.lookup_interface(MPRIS_PLAYER_INTERFACE).unwrap())
            .method_call(move |_, _, _, _, method, _, invocation| {
                recorded.borrow_mut().push(method.to_owned());
                invocation.return_value(None);
            }).build().unwrap();
        let (events, _) = async_channel::unbounded();
        let make_handle = |name: &str, pid| {
            let proxy = |interface| gio::DBusProxy::new_sync(&bus,
                gio::DBusProxyFlags::DO_NOT_LOAD_PROPERTIES | gio::DBusProxyFlags::DO_NOT_CONNECT_SIGNALS,
                None, bus.unique_name().as_deref(), MPRIS_PATH, interface, gio::Cancellable::NONE).unwrap();
            let player = proxy(MPRIS_PLAYER_INTERFACE);
            for key in ["CanControl", "CanPlay", "CanPause"] {
                player.set_cached_property(key, Some(&true.to_variant()));
            }
            player.set_cached_property("PlaybackStatus", Some(&"Playing".to_variant()));
            let root = proxy(MPRIS_ROOT_INTERFACE);
            root.set_cached_property("Identity", Some(&name.to_variant()));
            let mut handle = PlayerHandle::from_proxies(name.into(), player, root, &events).unwrap();
            handle.process_id = Some(pid);
            handle
        };
        let browser = make_handle("browser", 1001);
        let browser_proxy = browser.player.clone();
        let music = make_handle("music", 1002);
        let mut state = PlayerState {
            events, players: vec![browser, music], pending_players: HashMap::new(),
            next_request_id: 0, source_states: HashMap::new(), active_bus: None,
            pinned: false, views: Vec::new(), audio: None, audio_started: HashMap::new(),
            audio_sequence: 0, resume_bus: RefCell::new(None),
        };
        let audio = |name: &str, pid| AudioSource {
            key: format!("audio:{pid}"), name: name.into(), app_id: String::new(),
            binary: String::new(), process_ids: vec![pid],
        };
        let browser_audio = audio("browser", 1001);
        let telegram_audio = audio("Telegram", 2000);
        state.audio_changed(Some(vec![]));
        assert!(state.current_view().is_none(), "stale Playing alone must not show a source");
        state.audio_changed(Some(vec![browser_audio.clone()]));
        assert_eq!(state.active_bus.as_deref(), Some("browser"));
        state.audio_changed(Some(vec![browser_audio.clone(), telegram_audio]));
        let view = state.current_view().unwrap();
        assert_eq!(view.metadata, "Telegram");
        assert!(!view.can_play_pause, "audio-only apps must not control another player");
        state.player_properties_changed("browser");
        assert_eq!(state.active_bus.as_deref(), Some("audio:2000"));
        state.audio_changed(Some(vec![browser_audio]));
        assert_eq!(state.active_bus.as_deref(), Some("browser"));

        let expect_call = |method: &str| {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while calls.borrow().is_empty() {
                context.iteration(false);
                assert!(std::time::Instant::now() < deadline, "missing {method} call");
                std::thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(calls.borrow_mut().remove(0), method);
        };
        state.call_active(PlayerAction::PlayPause);
        expect_call("Pause");
        // A pause acknowledgement can arrive before PipeWire drains its stream.
        browser_proxy.set_cached_property("PlaybackStatus", Some(&"Paused".to_variant()));
        state.player_properties_changed("browser");
        assert_eq!(state.current_view().unwrap().status, PlaybackStatus::Paused);
        state.audio_changed(Some(vec![]));
        assert_eq!(state.active_bus.as_deref(), Some("browser"));
        state.call_active(PlayerAction::PlayPause);
        expect_call("Play");
        // Even a stale Playing flag must not turn resume into another Pause.
        browser_proxy.set_cached_property("PlaybackStatus", Some(&"Playing".to_variant()));
        state.player_properties_changed("browser");
        assert_eq!(state.current_view().unwrap().status, PlaybackStatus::Paused);

        state.select_source(Some("browser"));
        state.audio_changed(Some(vec![audio("music", 1002)]));
        assert_eq!(state.active_bus.as_deref(), Some("browser"));
        state.call_active(PlayerAction::PlayPause);
        expect_call("PlayPause");
        state.select_source(None);
        assert_eq!(state.active_bus.as_deref(), Some("music"));
        state.audio_changed(Some(vec![]));
        assert!(state.current_view().is_none());
        state.audio_changed(None);
        assert!(state.current_view().is_some(), "monitor failure must retain MPRIS fallback");
        bus.unregister_object(registration).unwrap();
        // Drain asynchronous replies before dropping this thread's main context.
        while context.pending() { context.iteration(false); }
    }).unwrap();
}
