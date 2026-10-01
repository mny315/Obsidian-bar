use std::time::{Duration, Instant};

use gtk::{glib, prelude::*};

pub(super) fn pump(ms: u64) {
    let until = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < until {
        glib::MainContext::default().iteration(false);
        std::thread::sleep(Duration::from_millis(1));
    }
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
