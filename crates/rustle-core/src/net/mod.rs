//! Thin wrappers over the `imap` and `lettre` crates: one session type per
//! protocol, opened and torn down per operation, never pooled.

pub mod auth;
pub mod errors;
pub mod imap;
pub mod smtp;

use std::net::IpAddr;
use std::time::Duration;

/// Socket timeout for both protocols. Generous on purpose: a first sync over
/// a slow link can take a while, and a tight timeout looks like a connection
/// failure.
pub const NET_TIMEOUT: Duration = Duration::from_secs(30);

/// True for 127.0.0.1, ::1 and localhost.
///
/// Verifying certificates is the point of TLS -- except on loopback. A local
/// bridge (ProtonMail Bridge, hydroxide) terminates TLS with a self-signed
/// certificate it generated on the machine itself, and there is no path to
/// sit on between two processes on the same host.
pub fn is_loopback(host: &str) -> bool {
    let stripped = host
        .trim()
        .trim_matches(|c| c == '[' || c == ']')
        .to_lowercase();
    if stripped == "localhost" || stripped == "localhost." {
        return true;
    }
    stripped
        .parse::<IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_detection() {
        assert!(is_loopback("localhost"));
        assert!(is_loopback("127.0.0.1"));
        assert!(is_loopback("[::1]"));
        assert!(!is_loopback("imap.example.com"));
        assert!(!is_loopback("10.0.0.1"));
    }
}
