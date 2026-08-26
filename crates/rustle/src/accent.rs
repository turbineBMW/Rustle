//! The system accent colour, for the parts of the UI that CSS can't reach:
//! the WebKit views rendering mail bodies and the composer's editor.
//!
//! libadwaita learns the accent from the settings portal, which only carries
//! it under GNOME's portal backend. On other compositors the portal answers
//! "not found" and libadwaita silently falls back to blue, so when it reports
//! no system support we read `org.gnome.desktop.interface accent-color`
//! ourselves and override its `--accent-*` CSS variables.

use std::cell::RefCell;

use adw::prelude::*;

const INTERFACE_SCHEMA: &str = "org.gnome.desktop.interface";

thread_local! {
    static FALLBACK: RefCell<Option<Fallback>> = const { RefCell::new(None) };
}

struct Fallback {
    settings: gio::Settings,
    provider: gtk::CssProvider,
}

/// The accent as a CSS hex colour, tracking the GNOME setting through
/// libadwaita's style manager, or through GSettings where the portal
/// doesn't relay it.
pub fn accent_hex() -> String {
    let manager = adw::StyleManager::default();
    let rgba = match fallback_accent() {
        Some(accent) => accent.to_standalone_rgba(manager.is_dark()),
        None => manager.accent_color_rgba(),
    };
    rgba_hex(&rgba)
}

pub fn rgba_hex(color: &gtk::gdk::RGBA) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        (color.red() * 255.0).round() as u8,
        (color.green() * 255.0).round() as u8,
        (color.blue() * 255.0).round() as u8
    )
}

/// Run `on_change` now and whenever the accent or the dark/light scheme flips.
pub fn watch(on_change: impl Fn() + 'static) {
    let manager = adw::StyleManager::default();
    let on_change = std::rc::Rc::new(on_change);
    for property in ["accent-color", "dark"] {
        let on_change = on_change.clone();
        manager.connect_notify_local(Some(property), move |_, _| on_change());
    }
    FALLBACK.with(|cell| {
        if let Some(fallback) = cell.borrow().as_ref() {
            let on_change = on_change.clone();
            fallback
                .settings
                .connect_changed(Some("accent-color"), move |_, _| on_change());
        }
    });
    on_change();
}

/// Install the GSettings fallback if libadwaita can't see the system accent.
/// Call once, after the display exists and before the first window.
pub fn install_fallback(display: &gtk::gdk::Display) {
    let manager = adw::StyleManager::default();
    if manager.is_system_supports_accent_colors() {
        return;
    }
    let source = gio::SettingsSchemaSource::default();
    if source.is_none_or(|s| s.lookup(INTERFACE_SCHEMA, true).is_none()) {
        log::info!("no {INTERFACE_SCHEMA} schema; keeping libadwaita's default accent");
        return;
    }
    let settings = gio::Settings::new(INTERFACE_SCHEMA);
    if !settings
        .settings_schema()
        .is_some_and(|s| s.has_key("accent-color"))
    {
        return;
    }
    log::info!("portal has no accent colour; following {INTERFACE_SCHEMA} accent-color");

    let provider = gtk::CssProvider::new();
    gtk::style_context_add_provider_for_display(
        display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
    );
    let fallback = Fallback { settings, provider };
    apply(&fallback, manager.is_dark());
    FALLBACK.with(|cell| *cell.borrow_mut() = Some(fallback));

    let refresh = || {
        let dark = adw::StyleManager::default().is_dark();
        FALLBACK.with(|cell| {
            if let Some(fallback) = cell.borrow().as_ref() {
                apply(fallback, dark);
            }
        });
    };
    FALLBACK.with(|cell| {
        if let Some(fallback) = cell.borrow().as_ref() {
            fallback
                .settings
                .connect_changed(Some("accent-color"), move |_, _| refresh());
        }
    });
    manager.connect_dark_notify(move |_| refresh());
}

fn fallback_accent() -> Option<adw::AccentColor> {
    FALLBACK.with(|cell| {
        cell.borrow()
            .as_ref()
            .map(|f| parse(&f.settings.string("accent-color")))
    })
}

fn apply(fallback: &Fallback, dark: bool) {
    let accent = parse(&fallback.settings.string("accent-color"));
    let css = format!(
        ":root {{ --accent-bg-color: {}; --accent-fg-color: #ffffff; --accent-color: {}; }}",
        rgba_hex(&accent.to_rgba()),
        rgba_hex(&accent.to_standalone_rgba(dark)),
    );
    fallback.provider.load_from_string(&css);
}

fn parse(name: &str) -> adw::AccentColor {
    use adw::AccentColor::*;
    match name {
        "teal" => Teal,
        "green" => Green,
        "yellow" => Yellow,
        "orange" => Orange,
        "red" => Red,
        "pink" => Pink,
        "purple" => Purple,
        "slate" => Slate,
        _ => Blue,
    }
}
