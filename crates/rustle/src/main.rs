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
    prefer_gl_renderer();
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

/// Draw with GTK's GL renderer unless `GSK_RENDERER` says otherwise.
///
/// WebKitGTK 2.54 hands its frames over as dmabufs in whatever layout the
/// GPU likes best; on Intel Xe2/Xe3 that's a compressed (CCS) modifier the
/// Vulkan driver won't import. GTK's default Vulkan renderer then copies
/// every frame through a side GL context, and once that context fails to
/// go current the reader and composer stay blank. The GL renderer imports
/// the same buffers directly. Runs first, before anything starts a thread.
fn prefer_gl_renderer() {
    if std::env::var_os("GSK_RENDERER").is_none() {
        std::env::set_var("GSK_RENDERER", "gl");
    }
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
