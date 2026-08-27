//! Rustle: a GTK 4 / libadwaita mail client.

mod accent;
mod account_colors;
mod application;
mod avatar_loader;
mod composer;
mod config;
mod dialogs;
mod editor;
mod i18n;
mod objects;
mod settings;
mod sound;
mod widgets;
mod window;
mod workers;

use gtk::prelude::*;
use gtk::{gio, glib};

fn main() -> glib::ExitCode {
    configure_logging();
    i18n::init();

    gio::resources_register_include!("rustle.gresource")
        .expect("the resource bundle is compiled in");
    glib::set_application_name("Rustle");

    let app = application::RustleApplication::new();
    app.run()
}

/// Log to stderr. WARNING by default so a normal run stays quiet;
/// `RUSTLE_LOG=debug` (or any level name) turns it up without a rebuild.
/// This is separate from `G_MESSAGES_DEBUG`, which only affects GLib.
///
/// Never log a worker's credential: `Credential`'s Debug hides the secret,
/// but nothing stops a `{:?}` of a whole request struct that carries one.
fn configure_logging() {
    let level = std::env::var("RUSTLE_LOG").unwrap_or_else(|_| "warn".to_string());
    env_logger::Builder::new()
        .parse_filters(&level)
        .format_timestamp_secs()
        .init();
}
