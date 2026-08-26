//! Turning a raw failure into the one distinction the UI acts on -- was it
//! the password? -- and a friendly description the GTK layer translates.

use std::error::Error as StdError;
use std::io;
use thiserror::Error;

/// Everything a network operation can fail with.
#[derive(Debug, Error)]
pub enum NetError {
    #[error("IMAP: {0}")]
    Imap(#[from] ::imap::Error),
    #[error("SMTP: {0}")]
    Smtp(#[from] lettre::transport::smtp::Error),
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("TLS: {0}")]
    Tls(#[from] native_tls::Error),
    /// A protocol-level problem in our own handling ("not connected",
    /// "no message body returned"), named after the resource involved.
    #[error("{0}")]
    Protocol(String),
    /// The keyring or GNOME Online Accounts had nothing to sign in with.
    #[error("no credential for {0}")]
    NoCredential(String),
}

/// What went wrong, in terms the user can act on. The GTK layer turns each
/// variant into a translated sentence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The server rejected the credentials -- the one failure no Retry fixes.
    Auth,
    /// TLS could not be established with `host`.
    Tls { host: String },
    /// `host` does not resolve.
    NotFound { host: String },
    /// `host` refused the connection (probably the port).
    Refused { host: String },
    /// Connecting to `host` timed out.
    Timeout { host: String },
    /// Some other socket failure: no route, connection dropped.
    Unreachable,
    /// The server said no, in its own words.
    Server(String),
    /// Nothing to sign in with.
    NoCredential,
}

impl Failure {
    pub fn is_auth(&self) -> bool {
        matches!(self, Failure::Auth)
    }
}

/// Substrings (lowercased) that mean the server rejected the credentials.
const AUTH_HINTS: &[&str] = &[
    "authenticationfailed",
    "authentication failed",
    "invalid credentials",
    "username and password not accepted",
    "login failed",
    "5.7.8",
    "authentication unsuccessful",
];

fn is_auth_text(text: &str) -> bool {
    let lower = text.to_lowercase();
    AUTH_HINTS.iter().any(|hint| lower.contains(hint))
}

fn classify_io(error: &io::Error, host: &str) -> Failure {
    let host = host.to_string();
    match error.kind() {
        io::ErrorKind::ConnectionRefused => Failure::Refused { host },
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => Failure::Timeout { host },
        _ => {
            let text = error.to_string().to_lowercase();
            if text.contains("lookup")
                || text.contains("name or service")
                || text.contains("resolve")
            {
                Failure::NotFound { host }
            } else {
                Failure::Unreachable
            }
        }
    }
}

/// Walk the source chain looking for the socket error underneath.
fn io_source<'a>(error: &'a (dyn StdError + 'static)) -> Option<&'a io::Error> {
    let mut current: Option<&(dyn StdError + 'static)> = Some(error);
    while let Some(err) = current {
        if let Some(io) = err.downcast_ref::<io::Error>() {
            return Some(io);
        }
        current = err.source();
    }
    None
}

/// Classify a failure against the host it was talking to.
pub fn classify(error: &NetError, host: &str) -> Failure {
    match error {
        NetError::Io(io) => classify_io(io, host),
        NetError::Tls(_) => Failure::Tls {
            host: host.to_string(),
        },
        NetError::NoCredential(_) => Failure::NoCredential,
        NetError::Protocol(text) => Failure::Server(text.clone()),
        NetError::Imap(imap) => match imap {
            ::imap::Error::Io(io) => classify_io(io, host),
            ::imap::Error::TlsHandshake(_) | ::imap::Error::Tls(_) => Failure::Tls {
                host: host.to_string(),
            },
            ::imap::Error::No(no) => {
                let text = no.to_string();
                if is_auth_text(&text) {
                    Failure::Auth
                } else {
                    Failure::Server(no.information.clone())
                }
            }
            ::imap::Error::Bad(bad) => {
                let text = bad.to_string();
                if is_auth_text(&text) {
                    Failure::Auth
                } else {
                    Failure::Server(bad.information.clone())
                }
            }
            ::imap::Error::Bye(bye) => Failure::Server(bye.information.clone()),
            ::imap::Error::ConnectionLost => Failure::Unreachable,
            other => Failure::Server(other.to_string()),
        },
        NetError::Smtp(smtp) => {
            let text = smtp.to_string();
            // 535 is "authentication credentials invalid" (RFC 4954).
            if smtp
                .status()
                .is_some_and(|code| code.to_string().starts_with("535"))
                || is_auth_text(&text)
            {
                return Failure::Auth;
            }
            if smtp.is_tls() {
                return Failure::Tls {
                    host: host.to_string(),
                };
            }
            if smtp.is_timeout() {
                return Failure::Timeout {
                    host: host.to_string(),
                };
            }
            if let Some(io) = io_source(smtp) {
                return classify_io(io, host);
            }
            Failure::Server(text)
        }
    }
}

/// Server messages often carry a help URL ("Application-specific password
/// required: https://support.google.com/..."). The banner renders Pango
/// markup, so escape the text first, then turn bare URLs into links. Trailing
/// punctuation is sentence, not URL: "see https://x/y." keeps the dot out.
pub fn linkify(text: &str) -> String {
    use regex::Regex;
    use std::sync::LazyLock;
    static URL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"https?://[^\s<>]*[^\s<>.,;:)\]]").unwrap());
    let escaped = glib::markup_escape_text(text).to_string();
    URL.replace_all(&escaped, |captures: &regex::Captures| {
        let url = &captures[0];
        format!("<a href=\"{url}\">{url}</a>")
    })
    .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_socket_errors() {
        let refused = NetError::Io(io::Error::new(io::ErrorKind::ConnectionRefused, "x"));
        assert_eq!(
            classify(&refused, "h"),
            Failure::Refused { host: "h".into() }
        );
        let lookup = NetError::Io(io::Error::other("failed to lookup address information"));
        assert_eq!(
            classify(&lookup, "h"),
            Failure::NotFound { host: "h".into() }
        );
        let timeout = NetError::Io(io::Error::new(io::ErrorKind::TimedOut, "x"));
        assert_eq!(
            classify(&timeout, "h"),
            Failure::Timeout { host: "h".into() }
        );
        assert_eq!(
            classify(&NetError::Protocol("nope".into()), "h"),
            Failure::Server("nope".into())
        );
    }

    #[test]
    fn spots_auth_failures() {
        assert!(is_auth_text(
            "NO [AUTHENTICATIONFAILED] Invalid credentials"
        ));
        assert!(is_auth_text("535 5.7.8 Username and Password not accepted"));
        assert!(!is_auth_text("NO Mailbox does not exist"));
    }

    #[test]
    fn linkifies_urls() {
        assert_eq!(
            linkify("see https://x.y/z. <b>"),
            "see <a href=\"https://x.y/z\">https://x.y/z</a>. &lt;b&gt;"
        );
    }
}
