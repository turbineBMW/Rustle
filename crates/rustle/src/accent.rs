//! The system accent colour, for the parts of the UI that CSS can't reach:
//! the WebKit views rendering mail bodies and the composer's editor.

use adw::prelude::*;

/// The accent as a CSS hex colour, tracking the GNOME setting through
/// libadwaita's style manager.
pub fn accent_hex() -> String {
    let manager = adw::StyleManager::default();
    rgba_hex(&manager.accent_color_rgba())
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
    on_change();
}
