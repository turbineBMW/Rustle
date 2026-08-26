//! gettext setup and the sentences the core's enums turn into.

use crate::config::GETTEXT_DOMAIN;
pub use gettextrs::gettext;
use gettextrs::{bindtextdomain, ngettext, setlocale, textdomain, LocaleCategory};
use rustle_core::dates::RelativeLabel;
use rustle_core::net::errors::Failure;

pub fn init() {
    // SAFETY: called once at startup, before any other thread exists.
    unsafe {
        setlocale(LocaleCategory::LcAll, "");
    }
    // The locale directory of a system install; a checkout run simply has no
    // translations, which gettext handles by returning the source string.
    let _ = bindtextdomain(GETTEXT_DOMAIN, "/usr/share/locale");
    let _ = textdomain(GETTEXT_DOMAIN);
}

/// `gettext` with `{name}` placeholders substituted, in a form that keeps the
/// placeholders visible to translators.
pub fn format(template: &str, pairs: &[(&str, &str)]) -> String {
    let mut text = template.to_string();
    for (key, value) in pairs {
        text = text.replace(&format!("{{{key}}}"), value);
    }
    text
}

pub fn plural(singular: &str, plural: &str, n: u64, pairs: &[(&str, &str)]) -> String {
    let mut text = ngettext(singular, plural, n as u32);
    let n_text = n.to_string();
    text = text.replace("{n}", &n_text);
    format(&text, pairs)
}

/// The friendly sentence for a classified network failure.
pub fn failure_message(failure: &Failure) -> String {
    match failure {
        Failure::Auth => gettext("Sign-in failed. Check the account password."),
        Failure::Tls { host } => format(
            &gettext("Couldn't establish a secure connection to {host}."),
            &[("host", host)],
        ),
        Failure::NotFound { host } => format(
            &gettext("Can't find {host}. Check the server address or your connection."),
            &[("host", host)],
        ),
        Failure::Refused { host } => format(
            &gettext("{host} refused the connection. Check the port."),
            &[("host", host)],
        ),
        Failure::Timeout { host } => format(
            &gettext("Connecting to {host} timed out."),
            &[("host", host)],
        ),
        Failure::Unreachable => gettext("Couldn't reach the mail server. Check your connection."),
        Failure::Server(text) => text.clone(),
        Failure::NoCredential => gettext("Could not sign in to this account."),
    }
}

/// Render a stored timestamp as the short label the list and reader show.
pub fn date_label(value: &str) -> String {
    match rustle_core::dates::relative_label(value) {
        RelativeLabel::Today(time) => format(&gettext("Today {time}"), &[("time", &time)]),
        RelativeLabel::Yesterday => gettext("Yesterday"),
        RelativeLabel::Weekday(day) | RelativeLabel::Date(day) | RelativeLabel::Raw(day) => day,
    }
}
