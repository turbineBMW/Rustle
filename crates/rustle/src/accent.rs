//! The system accent colour, for the parts of the UI that CSS can't reach:
//! the WebKit views rendering mail bodies and the composer's editor.
//!
//! libadwaita learns the accent from the settings portal, which only carries
//! it under GNOME's portal backend. On other compositors the portal answers
//! "not found" and libadwaita silently falls back to blue, so when it reports
//! no system support we read `org.gnome.desktop.interface accent-color`
//! ourselves and override its `--accent-*` CSS variables.
//!
//! While Rustle follows an Omarchy theme (`omarchy.rs`), the theme's accent
//! outranks both: the CSS side through its own provider, this side through
//! [`set_override`].

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

const INTERFACE_SCHEMA: &str = "org.gnome.desktop.interface";

thread_local! {
    static FALLBACK: RefCell<Option<Fallback>> = const { RefCell::new(None) };
    static OVERRIDE: RefCell<Option<String>> = const { RefCell::new(None) };
    static SCHEME: RefCell<Option<rustle_core::darkmode::Scheme>> = const { RefCell::new(None) };
    static WATCHERS: RefCell<Vec<Rc<dyn Fn()>>> = const { RefCell::new(Vec::new()) };
}

struct Fallback {
    settings: gio::Settings,
    provider: gtk::CssProvider,
}

/// The accent as a CSS hex colour, tracking the GNOME setting through
/// libadwaita's style manager, or through GSettings where the portal
/// doesn't relay it -- unless a followed Omarchy theme states its own.
pub fn accent_hex() -> String {
    if let Some(accent) = OVERRIDE.with(|cell| cell.borrow().clone()) {
        return accent;
    }
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
    let on_change: Rc<dyn Fn()> = Rc::new(on_change);
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
    WATCHERS.with(|cell| cell.borrow_mut().push(on_change.clone()));
    on_change();
}

/// Replace the accent with a theme's own (`None` hands it back to the
/// system). Reports whether it changed and leaves [`notify`] to the caller,
/// who knows if a dark/light flip is about to run the watchers anyway.
pub fn set_override(accent: Option<String>) -> bool {
    OVERRIDE.with(|cell| {
        let changed = *cell.borrow() != accent;
        cell.replace(accent);
        changed
    })
}

/// Run every [`watch`] callback.
/// The colours dark mail is adapted to: the followed Omarchy theme's,
/// else the reader's own.
pub fn reader_scheme() -> rustle_core::darkmode::Scheme {
    SCHEME.with(|scheme| scheme.borrow().unwrap_or_default())
}

/// Set the theme's reader colours. True when that changed what the reader
/// paints, so the caller can re-render.
pub fn set_reader_scheme(colours: Option<(String, String)>) -> bool {
    let scheme =
        colours.and_then(|(canvas, text)| rustle_core::darkmode::Scheme::new(&canvas, &text));
    SCHEME.with(|current| current.replace(scheme) != scheme)
}

pub fn notify() {
    let watchers = WATCHERS.with(|cell| cell.borrow().clone());
    for on_change in watchers {
        on_change();
    }
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
