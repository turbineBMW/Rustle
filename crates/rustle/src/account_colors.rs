//! Per-account colours, pushed into a CSS provider so that every widget
//! tagged with `account-<id>` picks its colour up from `--account-color`
//! without being rebound when the user changes it.

use adw::prelude::*;
use gtk::gdk;
use rustle_core::models::Account;
use std::cell::RefCell;

thread_local! {
    static PROVIDER: RefCell<Option<gtk::CssProvider>> = const { RefCell::new(None) };
}

/// The CSS class that ties a widget to an account's colour.
pub fn css_class(account_id: i64) -> String {
    format!("account-{account_id}")
}

/// Regenerate the colour rules for these accounts. Cheap; call it whenever
/// the account list is re-read.
pub fn apply(accounts: &[Account]) {
    let css: String = accounts
        .iter()
        .map(|a| {
            format!(
                ".{} {{ --account-color: {}; --account-fg-color: {}; }}\n",
                css_class(a.id),
                a.color_hex(),
                contrast_for(a.color_hex())
            )
        })
        .collect();
    PROVIDER.with(|cell| {
        let mut cell = cell.borrow_mut();
        let provider = cell.get_or_insert_with(|| {
            let provider = gtk::CssProvider::new();
            if let Some(display) = gdk::Display::default() {
                gtk::style_context_add_provider_for_display(
                    &display,
                    &provider,
                    gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
                );
            }
            provider
        });
        provider.load_from_string(&css);
    });
}

/// Swap a widget's `account-<id>` class for another (or none).
pub fn tag(widget: &impl IsA<gtk::Widget>, account_id: Option<i64>) {
    // Only the id classes: `account-dot` and friends are styling, not tags.
    for class in widget.css_classes() {
        if let Some(id) = class.strip_prefix("account-") {
            if id.bytes().all(|b| b.is_ascii_digit()) {
                widget.remove_css_class(&class);
            }
        }
    }
    if let Some(id) = account_id {
        widget.add_css_class(&css_class(id));
    }
}

pub fn parse_hex(hex: &str) -> Option<gdk::RGBA> {
    gdk::RGBA::parse(hex).ok()
}

/// The text colour that reads on a filled `#rrggbb` background: white on
/// anything but the lightest colours. The cut-off sits above the WCAG
/// break-even so mid tones like the palette green keep white text, the way
/// Adwaita's own filled buttons do.
pub fn contrast_for(hex: &str) -> &'static str {
    let channel = |at: usize| {
        let byte = u8::from_str_radix(hex.get(at..at + 2).unwrap_or("00"), 16).unwrap_or(0);
        let value = f64::from(byte) / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    let luminance = 0.2126 * channel(1) + 0.7152 * channel(3) + 0.0722 * channel(5);
    if luminance > 0.4 {
        "black"
    } else {
        "white"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contrast_picks_readable_text() {
        assert_eq!(contrast_for("#3584e4"), "white"); // palette blue
        assert_eq!(contrast_for("#3a944a"), "white"); // palette green
        assert_eq!(contrast_for("#c88800"), "white"); // palette yellow
        assert_eq!(contrast_for("#f6d32d"), "black"); // bright yellow
        assert_eq!(contrast_for("#ffffff"), "black");
        assert_eq!(contrast_for("#000000"), "white");
        assert_eq!(contrast_for("garbage"), "white");
    }
}
