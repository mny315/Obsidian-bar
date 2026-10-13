use std::time::{Duration, Instant};

use gtk::{glib, prelude::*};

pub(super) fn pump(ms: u64) {
    let until = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < until {
        glib::MainContext::default().iteration(false);
        std::thread::sleep(Duration::from_millis(1));
    }
}

pub(super) fn drag_pointer(from: (f32, f32), to: (f32, f32)) {
    drag_pointer_with_wheel(from, to, 0);
}

pub(super) fn drag_pointer_with_wheel(from: (f32, f32), to: (f32, f32), wheel: i32) {
    let mut pointer = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/scripts/test-reorder-pointer.py"
        ))
        .args([from.0, from.1, to.0, to.1].map(|value| value.to_string()))
        .arg(wheel.to_string())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(status) = pointer.try_wait().unwrap() {
            assert!(status.success(), "pointer helper failed");
            break;
        }
        if Instant::now() >= deadline {
            let _ = pointer.kill();
            let _ = pointer.wait();
            panic!("pointer helper stalled");
        }
        pump(20);
    }
    pump(300);
}

pub(super) fn capture_if_requested(window: &gtk::ApplicationWindow, name: &str) {
    let Some(directory) = std::env::var_os("OBSIDIAN_BAR_TEST_SNAPSHOTS") else {
        return;
    };
    let directory = std::path::PathBuf::from(directory);
    std::fs::create_dir_all(&directory).unwrap();
    let snapshot = gtk::Snapshot::new();
    gtk::WidgetPaintable::new(Some(window)).snapshot(
        &snapshot,
        f64::from(window.width()),
        f64::from(window.height()),
    );
    let node = snapshot.to_node().expect("mapped window must render");
    window
        .renderer()
        .unwrap()
        .render_texture(&node, None)
        .save_to_png(directory.join(format!("{name}.png")))
        .unwrap();
}
