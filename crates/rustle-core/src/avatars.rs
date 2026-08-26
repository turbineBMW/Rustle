//! Sender pictures: a local graphmail-bridge's Outlook directory first, then
//! Gravatar, then the sender's domain favicon. Cached on disk between runs, an empty file recording that nobody has one, since
//! most senders don't and that answer otherwise costs three HTTP requests per
//! launch.

use sha2::{Digest, Sha256};
use std::fmt;
use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Shared mail hosts: a favicon here would give every sender the same logo.
const FREEMAIL_DOMAINS: &[&str] = &[
    "aol.com",
    "gmail.com",
    "googlemail.com",
    "gmx.com",
    "gmx.de",
    "hotmail.com",
    "icloud.com",
    "live.com",
    "mail.com",
    "me.com",
    "outlook.com",
    "pm.me",
    "proton.me",
    "protonmail.com",
    "yahoo.com",
    "yandex.com",
    "zoho.com",
];

const MAX_BYTES: usize = 256 * 1024;
const TIMEOUT: Duration = Duration::from_secs(5);

/// How long a cached lookup is trusted. Long, because it mostly caches the
/// answer "nobody has one", and finite so a sender who later gets a picture
/// eventually shows it.
const CACHE_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// A decoded-or-not picture: the raw bytes of an image the GTK layer turns
/// into a texture, and its width when the caller could tell.
pub type ImageBytes = Vec<u8>;

/// A graphmail-bridge photo endpoint (`GET /photo?address=`), reached with
/// the bridge's own IMAP credentials over HTTP Basic auth. Asked before the
/// public lookups because it knows the tenant directory and the user's
/// contacts, which Gravatar never will.
#[derive(Clone)]
pub struct Bridge {
    /// `http://127.0.0.1:1180/photo`
    pub url: String,
    pub user: String,
    pub password: String,
}

impl fmt::Debug for Bridge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bridge")
            .field("url", &self.url)
            .field("user", &self.user)
            .field("password", &"...")
            .finish()
    }
}

/// The photo endpoint of a bridge account: the IMAP host is loopback and
/// unencrypted, which nothing but a local bridge is. `port` is where the
/// bridge serves photos (`server.photo_port`, 1180 by default).
pub fn bridge_url(imap_host: &str, port: u16) -> Option<String> {
    let host = imap_host.trim().trim_matches(['[', ']']);
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if !loopback {
        return None;
    }
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    Some(format!("http://{host}:{port}/photo"))
}

enum BridgeAnswer {
    Found(ImageBytes),
    NotFound,
    /// Connection refused, timed out, or a 5xx: the bridge is not running or
    /// Graph is not answering. Distinct from NotFound so a bridge that is
    /// merely down does not poison the on-disk cache for a month.
    Unavailable,
}

fn hash(address: &str) -> String {
    hex::encode(Sha256::digest(address.as_bytes()))
}

pub fn gravatar_url(address: &str) -> Option<String> {
    address
        .contains('@')
        .then(|| format!("https://gravatar.com/avatar/{}?s=128&d=404", hash(address)))
}

pub fn favicon_urls(address: &str) -> Vec<String> {
    let domain = address.rsplit('@').next().unwrap_or("");
    if !domain.contains('.') || FREEMAIL_DOMAINS.contains(&domain) {
        return Vec::new();
    }
    vec![
        format!("https://icons.duckduckgo.com/ip3/{domain}.ico"),
        format!("https://www.google.com/s2/favicons?domain={domain}&sz=128"),
    ]
}

/// The first picture any lookup has for this address, or None. `bridges` are
/// asked first; `cache_dir` is where the answer is remembered; `decode` is
/// the GTK layer's image decoder, which also reports the width so the
/// largest favicon wins.
pub fn fetch(
    address: &str,
    bridges: &[Bridge],
    cache_dir: &Path,
    decode: &dyn Fn(&[u8]) -> Option<u32>,
) -> Option<ImageBytes> {
    let address = address.trim().to_lowercase();
    let path = cache_path(cache_dir, &address);
    if let Ok(metadata) = fs::metadata(&path) {
        let age = metadata
            .modified()
            .ok()
            .and_then(|m| SystemTime::now().duration_since(m).ok());
        if age.is_some_and(|age| age < CACHE_TTL) {
            return match fs::read(&path) {
                Ok(data) if !data.is_empty() => decode(&data).map(|_| data),
                _ => None,
            };
        }
    }

    let mut bridge_down = false;
    let mut image = None;
    for bridge in bridges {
        match ask_bridge(bridge, &address, decode) {
            BridgeAnswer::Found(data) => {
                image = Some(data);
                break;
            }
            BridgeAnswer::NotFound => {}
            BridgeAnswer::Unavailable => bridge_down = true,
        }
    }
    let image = image
        .or_else(|| gravatar_url(&address).and_then(|url| load(&url, decode).map(|(data, _)| data)))
        .or_else(|| favicon(&address, decode));
    if image.is_some() || !bridge_down {
        write_cache(&path, image.as_deref());
    }
    image
}

fn ask_bridge(
    bridge: &Bridge,
    address: &str,
    decode: &dyn Fn(&[u8]) -> Option<u32>,
) -> BridgeAnswer {
    let url = format!(
        "{}?address={}",
        bridge.url,
        percent_encoding::utf8_percent_encode(address, percent_encoding::NON_ALPHANUMERIC)
    );
    let credentials = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        format!("{}:{}", bridge.user, bridge.password),
    );
    let response = agent()
        .get(&url)
        .header("Authorization", &format!("Basic {credentials}"))
        .call();
    let response = match response {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(404)) => return BridgeAnswer::NotFound,
        Err(ureq::Error::StatusCode(status)) => {
            log::debug!("bridge {} answered {status} for a photo", bridge.url);
            return BridgeAnswer::Unavailable;
        }
        Err(error) => {
            log::debug!("bridge {} unreachable for a photo: {error}", bridge.url);
            return BridgeAnswer::Unavailable;
        }
    };
    match read_image(response, decode) {
        Some((data, _)) => BridgeAnswer::Found(data),
        None => BridgeAnswer::NotFound,
    }
}

/// The largest icon on offer, stopping early once one is big enough.
fn favicon(address: &str, decode: &dyn Fn(&[u8]) -> Option<u32>) -> Option<ImageBytes> {
    const MIN_ICON_PX: u32 = 64;
    let mut best: Option<(ImageBytes, u32)> = None;
    for url in favicon_urls(address) {
        if let Some((data, width)) = load(&url, decode) {
            if best
                .as_ref()
                .is_none_or(|(_, best_width)| width > *best_width)
            {
                best = Some((data, width));
            }
        }
        if best
            .as_ref()
            .is_some_and(|(_, width)| *width >= MIN_ICON_PX)
        {
            break;
        }
    }
    best.map(|(data, _)| data)
}

fn cache_path(cache_dir: &Path, address: &str) -> PathBuf {
    cache_dir.join("avatars").join(hash(address))
}

fn write_cache(path: &Path, image: Option<&[u8]>) {
    let result = path
        .parent()
        .map(fs::create_dir_all)
        .unwrap_or(Ok(()))
        .and_then(|_| fs::write(path, image.unwrap_or(&[])));
    if let Err(error) = result {
        // A full or read-only cache directory costs a lookup next launch,
        // which is not worth failing an avatar over.
        log::debug!("could not cache the avatar at {}: {error}", path.display());
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .build(),
        )
        .timeout_global(Some(TIMEOUT))
        .build()
        .new_agent()
}

/// Download and decode, or None. Remote input, so nothing here fails loudly.
fn load(url: &str, decode: &dyn Fn(&[u8]) -> Option<u32>) -> Option<(ImageBytes, u32)> {
    let response = agent().get(url).call().ok()?;
    read_image(response, decode)
}

fn read_image(
    response: ureq::http::Response<ureq::Body>,
    decode: &dyn Fn(&[u8]) -> Option<u32>,
) -> Option<(ImageBytes, u32)> {
    let mut body = response.into_body();
    let data = body
        .with_config()
        .limit(MAX_BYTES as u64 + 1)
        .read_to_vec()
        .ok()?;
    if data.is_empty() || data.len() > MAX_BYTES {
        return None;
    }
    let width = decode(&data)?;
    Some((data, width))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert!(gravatar_url("ada@example.com")
            .unwrap()
            .starts_with("https://gravatar.com/avatar/"));
        assert!(gravatar_url("nope").is_none());
        assert!(favicon_urls("ada@gmail.com").is_empty());
        assert_eq!(favicon_urls("ada@example.com").len(), 2);
    }

    #[test]
    fn cache_records_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(dir.path(), "x@y.z");
        write_cache(&path, None);
        assert_eq!(fs::read(&path).unwrap().len(), 0);
        let decode = |_: &[u8]| Some(1);
        assert!(fetch("x@y.z", &[], dir.path(), &decode).is_none());
    }

    #[test]
    fn bridge_urls_only_for_loopback_plain_hosts() {
        assert_eq!(
            bridge_url("127.0.0.1", 1180).as_deref(),
            Some("http://127.0.0.1:1180/photo")
        );
        assert_eq!(
            bridge_url("localhost", 2000).as_deref(),
            Some("http://localhost:2000/photo")
        );
        assert_eq!(
            bridge_url("::1", 1180).as_deref(),
            Some("http://[::1]:1180/photo")
        );
        assert!(bridge_url("imap.example.com", 1180).is_none());
    }

    #[test]
    fn a_bridge_that_is_down_does_not_poison_the_cache() {
        // Nothing listens on this port; the lookup must not record a miss.
        let dir = tempfile::tempdir().unwrap();
        let bridge = Bridge {
            url: "http://127.0.0.1:9/photo".into(),
            user: "u".into(),
            password: "p".into(),
        };
        let decode = |_: &[u8]| Some(1);
        assert!(fetch("nobody@nowhere.invalid", &[bridge], dir.path(), &decode).is_none());
        assert!(!cache_path(dir.path(), "nobody@nowhere.invalid").exists());
    }
}
