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
                ".{} {{ --account-color: {}; }}\n",
                css_class(a.id),
                a.color_hex()
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
