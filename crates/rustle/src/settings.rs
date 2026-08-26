//! GSettings, with the schema found either installed or in the build tree.

use crate::config::{APP_ID, BUILT_SCHEMA_DIR};
use gtk::gio;
use gtk::prelude::*;

pub const SYNC_INTERVAL: &str = "sync-interval-minutes";
pub const NOTIFICATIONS: &str = "notifications";
pub const LOAD_REMOTE_IMAGES: &str = "load-remote-images";
pub const LOAD_SENDER_AVATARS: &str = "load-sender-avatars";
pub const RUN_IN_BACKGROUND: &str = "run-in-background";
pub const START_AT_LOGIN: &str = "start-at-login";
pub const BACKGROUND_NOTICE_SHOWN: &str = "background-notice-shown";
pub const SIGNATURE_ENABLED: &str = "signature-enabled";
pub const SIGNATURE_TEXT: &str = "signature-text";
pub const WINDOW_WIDTH: &str = "window-width";
pub const WINDOW_HEIGHT: &str = "window-height";
pub const WINDOW_MAXIMIZED: &str = "window-maximized";
pub const FOLDER_WIDTH: &str = "folder-sidebar-width";
pub const CONVERSATION_WIDTH: &str = "conversation-sidebar-width";

/// The app's settings. Prefers the installed schema; falls back to the one
/// build.rs compiled so a checkout runs without `make install`.
pub fn load() -> gio::Settings {
    let installed =
        gio::SettingsSchemaSource::default().and_then(|source| source.lookup(APP_ID, true));
    match installed {
        Some(_) => gio::Settings::new(APP_ID),
        None => {
            let source = gio::SettingsSchemaSource::from_directory(BUILT_SCHEMA_DIR, None, false)
                .expect("build.rs compiled the schema");
            let schema = source
                .lookup(APP_ID, false)
                .expect("the schema names the app id");
            gio::Settings::new_full(&schema, None::<&gio::SettingsBackend>, None)
        }
    }
}

/// The signature to append, or "" when disabled.
pub fn signature_text(settings: &gio::Settings) -> String {
    if settings.boolean(SIGNATURE_ENABLED) {
        settings.string(SIGNATURE_TEXT).trim().to_string()
    } else {
        String::new()
    }
}
