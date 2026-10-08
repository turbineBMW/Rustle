//! Rustle: a GTK 4 / libadwaita mail client.

mod accent;
mod account_colors;
mod account_pictures;
mod application;
mod autostart;
mod avatar_loader;
#[cfg(feature = "graph")]
mod bridge;
mod composer;
mod config;
mod dialogs;
mod editor;
mod i18n;
mod media_activity;
mod objects;
mod omarchy;
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
    // A phone build is the phone's Mail app; Rustle is its codename.
    glib::set_application_name(if cfg!(feature = "phone") {
        "Mail"
    } else {
        "Rustle"
    });

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
    // The D-Bus library's internals reach the log through tracing; they're
    // noise unless asked for by name (RUSTLE_LOG=zbus=debug still works).
    env_logger::Builder::new()
        .parse_filters(&format!("zbus=warn,tracing=warn,{level}"))
        .format_timestamp_secs()
        .init();
}
