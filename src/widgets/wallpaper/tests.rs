use std::{env, thread};

#[test]
fn wallpaper_waits_for_an_announced_output_not_only_a_socket() {
    assert!(!super::awww_query_has_outputs(""));
    assert!(!super::awww_query_has_outputs(
        "obsidian-bar: DP-3: 0x0, scale: 1, currently displaying: color: 000000"
    ));
    assert!(super::awww_query_has_outputs(
        "obsidian-bar: DP-3: 2560x1440, scale: 1, currently displaying: color: 000000"
    ));
}
#[cfg(target_os = "linux")]
use std::process::{Command, Stdio};

use super::*;

#[cfg(target_os = "linux")]
const PDEATH_HELPER_FILE: &str = "OBSIDIAN_BAR_PDEATH_HELPER_FILE";

#[cfg(target_os = "linux")]
fn process_exists(pid: i32) -> bool {
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(target_os = "linux")]
#[test]
fn managed_subprocess_dies_with_parent_helper() {
    let Some(pid_file) = std::env::var_os(PDEATH_HELPER_FILE).map(PathBuf::from) else {
        return;
    };
    let process = ManagedSubprocess::spawn("pdeath-test", &[OsStr::new("sleep"), OsStr::new("30")])
        .expect("test subprocess should start");
    let pid = process
        .process
        .identifier()
        .expect("test subprocess should have a pid");
    fs::write(pid_file, pid.as_bytes()).expect("child pid should be published");
    thread::sleep(Duration::from_secs(30));
}

#[cfg(target_os = "linux")]
#[test]
fn managed_subprocess_dies_when_parent_is_killed() {
    let pid_file = std::env::temp_dir().join(format!(
        "obsidian-pdeath-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let mut helper = Command::new(std::env::current_exe().expect("test binary should exist"))
        .args([
            "--exact",
            "widgets::wallpaper::tests::managed_subprocess_dies_with_parent_helper",
        ])
        .env(PDEATH_HELPER_FILE, &pid_file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("parent helper should start");

    let deadline = Instant::now() + Duration::from_secs(3);
    let child_pid = loop {
        if let Ok(value) = fs::read_to_string(&pid_file)
            && let Ok(pid) = value.parse::<i32>()
        {
            break pid;
        }
        assert!(
            Instant::now() < deadline,
            "parent helper did not publish its child pid"
        );
        thread::sleep(Duration::from_millis(10));
    };

    let kill_result = unsafe { libc::kill(helper.id().cast_signed(), libc::SIGKILL) };
    assert_eq!(kill_result, 0, "parent helper should be killable");
    let _ = helper.wait();

    let deadline = Instant::now() + Duration::from_secs(3);
    while process_exists(child_pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    if process_exists(child_pid) {
        unsafe {
            libc::kill(child_pid, libc::SIGKILL);
        }
        panic!("managed subprocess survived its parent");
    }
    let _ = fs::remove_file(pid_file);
}

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
fn random_transition_pool_is_varied_without_circle_bias() {
    let transitions = (0..AWWW_TRANSITIONS.len() as u64)
        .map(awww_transition_for_seed)
        .collect::<Vec<_>>();

    assert_eq!(transitions.as_slice(), AWWW_TRANSITIONS.as_slice());
    assert!(
        transitions
            .iter()
            .all(|transition| !matches!(*transition, "none" | "simple" | "fade"))
    );
    assert_eq!(
        transitions
            .into_iter()
            .filter(|transition| matches!(*transition, "grow" | "outer"))
            .count(),
        1
    );
}

#[test]
fn awww_query_requires_every_output_to_show_the_synchronized_image() {
    let target = Path::new("/tmp/new wallpaper.jpg");
    let synchronized = concat!(
        "obsidian-bar: DP-1: 1920x1080, scale: 1, currently displaying: image: ",
        "/tmp/new wallpaper.jpg\n",
        "obsidian-bar: HDMI-A-1: 2560x1440, scale: 1, currently displaying: image: ",
        "/tmp/new wallpaper.jpg\n",
    );
    let stale = concat!(
        "obsidian-bar: DP-1: 1920x1080, scale: 1, currently displaying: image: ",
        "/tmp/new wallpaper.jpg\n",
        "obsidian-bar: HDMI-A-1: 2560x1440, scale: 1, currently displaying: image: ",
        "/tmp/old.jpg\n",
    );

    assert!(awww_query_displays_image(synchronized, target));
    assert!(!awww_query_displays_image(stale, target));
    assert!(!awww_query_displays_image(
        "obsidian-bar: no outputs",
        target
    ));
}

#[test]
fn wallpaper_backends_stay_below_desktop_effects() {
    fn contains_pair(argv: &[&OsStr], flag: &str, value: &str) -> bool {
        argv.windows(2)
            .any(|pair| pair[0] == OsStr::new(flag) && pair[1] == OsStr::new(value))
    }

    let awww = awww_daemon_argv(OsStr::new("awww-daemon"), OsStr::new(AWWW_NAMESPACE));
    let transition_awww = awww_daemon_argv(
        OsStr::new("awww-daemon"),
        OsStr::new(AWWW_TRANSITION_NAMESPACE),
    );
    let mpvpaper = mpvpaper_argv(
        OsStr::new("mpvpaper"),
        OsStr::new("options"),
        OsStr::new("output"),
        OsStr::new("wallpaper"),
    );

    assert!(contains_pair(&awww, "--layer", "background"));
    assert!(contains_pair(&transition_awww, "--layer", "background"));
    assert!(contains_pair(
        &transition_awww,
        "--namespace",
        AWWW_TRANSITION_NAMESPACE
    ));
    assert!(contains_pair(&mpvpaper, "--layer", "background"));
}

#[test]
fn image_backend_matches_only_its_exact_source() {
    let backend = WallpaperBackend::Image {
        source: PathBuf::from("/tmp/current.png"),
        frame: PathBuf::from("/tmp/current.png"),
    };

    assert!(backend.matches(Path::new("/tmp/current.png")));
    assert!(!backend.matches(Path::new("/tmp/other.png")));
    assert!(!backend.matches(Path::new("/tmp/current.mp4")));
}

#[test]
fn mpvpaper_socket_owner_rejects_unrelated_names() {
    assert_eq!(
        mpvpaper_socket_owner(Path::new("/run/user/1000/obsidian-mpv-42-99.sock")),
        Some(42)
    );
    assert_eq!(
        mpvpaper_socket_owner(Path::new("/run/user/1000/mpv-42.sock")),
        None
    );
    assert_eq!(
        mpvpaper_socket_owner(Path::new("/run/user/1000/obsidian-mpv-nope-99.sock")),
        None
    );
}

#[test]
fn thumbnail_cover_dimensions_never_undershoot_card() {
    assert_eq!(cover_dimensions(1920, 1080), (150, 84));
    assert_eq!(cover_dimensions(1080, 1920), (144, 256));
    assert_eq!(cover_dimensions(144, 84), (144, 84));
    assert_eq!(cover_dimensions(0, 1080), (144, 84));
    assert_eq!(cover_dimensions(i32::MAX, 1), (i32::MAX, 84));
}

#[test]
fn mpv_request_serialization_preserves_command_arguments() {
    let mut output = Vec::new();
    let command = serde_json::json!(["get_property", "vo-configured"]);

    write_mpv_request(&mut output, MPV_REQUEST_VO_CONFIGURED, command.clone()).unwrap();

    let request: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(request["command"], command);
    assert_eq!(request["request_id"], MPV_REQUEST_VO_CONFIGURED);
}

#[test]
fn mpv_response_parser_keeps_structured_fields() {
    let response: MpvIpcMessage =
        serde_json::from_str(r#"{"request_id":1,"error":"success","data":true}"#).unwrap();

    assert_eq!(response.request_id, Some(MPV_REQUEST_VO_CONFIGURED));
    assert_eq!(response.error.as_deref(), Some("success"));
    assert_eq!(response.data, Some(serde_json::Value::Bool(true)));
}

#[cfg(target_os = "linux")]
#[test]
fn rapid_wallpaper_changes_helper() {
    let Some(directory) = env::var_os("OBSIDIAN_WALLPAPER_TEST_DIR").map(PathBuf::from) else {
        return;
    };
    let first = directory.join("first.png");
    let middle = directory.join("middle.png");
    let last = directory.join("last wallpaper.png");
    for path in [&first, &middle, &last] {
        fs::write(path, b"fixture").unwrap();
    }
    let controller = WallpaperController::new();
    controller.started.set(true);
    controller.awww_daemon.replace(Some(OwnedAwwwDaemon {
        process: ManagedSubprocess::spawn("fixture-main", &[OsStr::new("sleep"), OsStr::new("30")])
            .unwrap(),
    }));
    fs::write(directory.join("obsidian-bar.ready"), b"").unwrap();

    glib::MainContext::default().block_on(async {
        let started = Instant::now();
        controller
            .apply_animated(first.clone(), false, 0)
            .await
            .unwrap();
        controller
            .apply_animated(middle.clone(), false, 0)
            .await
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(1100),
            "image changes must not wait for either 1.2-second animation"
        );

        fs::write(directory.join("events"), b"").unwrap();
        controller.request_apply_silent(first.clone());
        controller.request_apply_silent(middle.clone());
        controller.request_apply_silent(last.clone());
        while controller.applying.get() {
            wait_local(Duration::from_millis(10)).await;
            assert!(started.elapsed() < Duration::from_secs(3));
        }
        let events = fs::read_to_string(directory.join("events")).unwrap();
        assert!(!events.contains("first.png"));
        assert!(!events.contains("middle.png"));
        assert!(events.contains("last wallpaper.png"));

        // A new click during a video handoff must settle the covering image
        // before stopping the old player, then finish on the latest selection.
        let video = OwnedMpvpaper {
            process: ManagedSubprocess::spawn(
                "fixture-video",
                &[OsStr::new("sleep"), OsStr::new("30")],
            )
            .unwrap(),
            ipc_socket: directory.join("unused.sock"),
        };
        let video_running = Rc::clone(&video.process.running);
        controller.backend.replace(Some(WallpaperBackend::Video {
            source: directory.join("old.mp4"),
            process: video,
        }));
        fs::write(directory.join("events"), b"").unwrap();
        controller.request_apply_silent(first.clone());
        let started = Instant::now();
        loop {
            let events = fs::read_to_string(directory.join("events")).unwrap();
            if events
                .lines()
                .any(|line| line.starts_with("obsidian-bar-transition|"))
            {
                assert!(
                    video_running.get(),
                    "old video must stay alive behind the transition"
                );
                break;
            }
            wait_local(Duration::from_millis(10)).await;
            assert!(started.elapsed() < Duration::from_secs(3));
        }
        controller.request_apply_silent(last.clone());
        while controller.applying.get() {
            wait_local(Duration::from_millis(10)).await;
            assert!(
                started.elapsed() < Duration::from_millis(1100),
                "a queued click must interrupt the video transition wait"
            );
        }
        assert!(!video_running.get());
        assert!(controller.backend_matches(&last));
        assert_eq!(controller.settings.borrow().current.as_ref(), Some(&last));
        let events = fs::read_to_string(directory.join("events")).unwrap();
        let cover = events.find("obsidian-bar-transition|simple|").unwrap();
        let synchronize = events.find("obsidian-bar|simple|").unwrap();
        let latest = events.rfind("last wallpaper.png").unwrap();
        assert!(cover < synchronize && synchronize < latest);

        let pending_controller = Rc::clone(&controller);
        let pending_path = last.clone();
        glib::timeout_add_local_once(Duration::from_millis(40), move || {
            pending_controller.queue_apply(PendingApply {
                path: pending_path,
                force: false,
                on_error: Rc::new(|_| {}),
            });
        });
        let started = Instant::now();
        let installed = controller
            .start_video_backend(directory.join("slow.mp4"), 0)
            .await
            .unwrap();
        assert!(!installed);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "a replaced video must not hold the queue until the readiness timeout"
        );
        assert_eq!(
            fs::read_to_string(directory.join("events")).unwrap(),
            events,
            "the covering image must remain visible when video loading is cancelled"
        );
        controller.take_pending_apply();
    });
    controller.shutdown();
}

#[cfg(target_os = "linux")]
#[test]
fn rapid_changes_replace_stale_requests_and_preserve_video_handoffs() {
    use std::os::unix::fs::PermissionsExt;

    let directory = env::temp_dir().join(format!(
        "obsidian-switch-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&directory).unwrap();
    let client = directory.join("awww");
    let daemon = directory.join("awww-daemon");
    fs::write(&client, r#"#!/bin/sh
set -eu
operation=$1
shift
namespace=obsidian-bar
transition=none
source_path=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --namespace) namespace=$2; shift 2 ;;
        --transition-type) transition=$2; shift 2 ;;
        --*) shift 2 ;;
        *) source_path=$1; shift ;;
    esac
done
case "$operation" in
    query)
        test -f "$OBSIDIAN_WALLPAPER_TEST_DIR/$namespace.ready" || exit 1
        if [ -f "$OBSIDIAN_WALLPAPER_TEST_DIR/$namespace.frame" ]; then
            printf 'TEST: 1920x1080, scale: 1, currently displaying: image: '
            cat "$OBSIDIAN_WALLPAPER_TEST_DIR/$namespace.frame"
        else
            printf 'TEST: 1920x1080, scale: 1, currently displaying: color: 000000\n'
        fi ;;
    img)
        printf '%s\n' "$source_path" > "$OBSIDIAN_WALLPAPER_TEST_DIR/$namespace.frame"
        printf '%s|%s|%s\n' "$namespace" "$transition" "$source_path" >> "$OBSIDIAN_WALLPAPER_TEST_DIR/events" ;;
    clear) printf 'clear|%s\n' "$namespace" >> "$OBSIDIAN_WALLPAPER_TEST_DIR/events" ;;
    *) exit 1 ;;
esac
"#).unwrap();
    fs::write(
        &daemon,
        r#"#!/bin/sh
set -eu
while [ "$#" -gt 0 ]; do
    case "$1" in
        --namespace) touch "$OBSIDIAN_WALLPAPER_TEST_DIR/$2.ready"; shift 2 ;;
        *) shift ;;
    esac
done
exec sleep 30
"#,
    )
    .unwrap();
    for path in [&client, &daemon] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let result = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "widgets::wallpaper::tests::rapid_wallpaper_changes_helper",
            "--nocapture",
        ])
        .env("OBSIDIAN_WALLPAPER_TEST_DIR", &directory)
        .env("OBSIDIAN_BAR_AWWW_BIN", &client)
        .env("OBSIDIAN_BAR_AWWW_DAEMON_BIN", &daemon)
        .env("OBSIDIAN_BAR_MPVPAPER_BIN", &daemon)
        .env("XDG_STATE_HOME", &directory)
        .env("XDG_RUNTIME_DIR", &directory)
        .output()
        .unwrap();
    let _ = fs::remove_dir_all(directory);
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}
