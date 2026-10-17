use super::*;

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
