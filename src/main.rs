mod app;
mod niri;
mod ui;
mod widgets;

use gtk::glib;
use tracing_subscriber::EnvFilter;

fn main() -> glib::ExitCode {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("obsidian_bar=info"));

    tracing_subscriber::fmt().with_env_filter(filter).init();

    app::App::new().run()
}
