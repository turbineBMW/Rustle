//! Sender pictures: Gravatar first, then the sender's domain favicon. Cached
//! on disk between runs, an empty file recording that nobody has one, since
//! most senders don't and that answer otherwise costs three HTTP requests per
//! launch.

use sha2::{Digest, Sha256};
use std::fs;
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

/// The first picture any lookup has for this address, or None. `cache_dir`
/// is where the answer is remembered; `decode` is the GTK layer's image
/// decoder, which also reports the width so the largest favicon wins.
pub fn fetch(
    address: &str,
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

    let image = gravatar_url(&address)
        .and_then(|url| load(&url, decode).map(|(data, _)| data))
        .or_else(|| favicon(&address, decode));
    write_cache(&path, image.as_deref());
    image
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

/// Download and decode, or None. Remote input, so nothing here fails loudly.
fn load(url: &str, decode: &dyn Fn(&[u8]) -> Option<u32>) -> Option<(ImageBytes, u32)> {
    let agent = ureq::Agent::config_builder()
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .build(),
        )
        .timeout_global(Some(TIMEOUT))
        .build()
        .new_agent();
    let response = agent.get(url).call().ok()?;
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
        assert!(fetch("x@y.z", dir.path(), &decode).is_none());
    }
}
