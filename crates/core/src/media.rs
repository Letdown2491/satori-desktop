//! Media pipeline — first slice: avatar fetch for note cards.
//!
//! Port of `src/routes/image-proxy.ts` + `src/ssrf.ts` discipline, reduced to
//! the avatar surface: scheme allowlist (https only), IP-literal and
//! internal-host rejection, sha256-keyed disk cache, strict decode to pixels
//! (SVG never rendered as a trusted format — `image` decodes raster only).
//!
//! TODO(port): full SSRF set — DNS-rebinding-safe resolution on direct paths,
//! per-redirect re-checks, byte caps, Tor transport, NIP-B7 healing (M2/M4).

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use sha2::{Digest, Sha256};
use url::Url;

use crate::config;

pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// Raw RGBA8 pixels.
    pub rgba: Vec<u8>,
}

pub async fn fetch_image(url: &str) -> Result<DecodedImage, String> {
    if !is_safe_media_url(url) {
        return Err("unsafe media URL".into());
    }

    // Cache read-through, keyed by sha256(url).
    let key = cache_key(url);
    let path = cache_path(&key);
    let bytes = if let Ok(cached) = std::fs::read(&path) {
        cached
    } else {
        let res = reqwest::get(url)
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?;
        let body = res.bytes().await.map_err(|e| e.to_string())?;
        let body = body.to_vec();
        // Only cache successful decodes.
        std::fs::create_dir_all(config::media_cache_dir())
            .map_err(|e| e.to_string())?;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut f| f.write_all(&body))
            .map_err(|e| e.to_string())?;
        body
    };

    decode(&bytes)
}

pub fn cache_key(url: &str) -> String {
    let mut h = Sha256::new();
    h.update(url.as_bytes());
    hex(&h.finalize())
}

fn cache_path(key: &str) -> PathBuf {
    config::media_cache_dir().join(key)
}

fn decode(bytes: &[u8]) -> Result<DecodedImage, String> {
    let img = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    Ok(DecodedImage { width, height, rgba: rgba.into_raw() })
}

/// SSRF guard, reduced surface: https only; rejects IP literals and
/// internal/metadata hosts.
pub fn is_safe_media_url(url: &str) -> bool {
    let parsed = match Url::parse(url) {
        Ok(u) => u,
        Err(_) => return false,
    };
    if parsed.scheme() != "https" {
        return false;
    }
    match parsed.host_str() {
        Some(host) => !is_internal_host(host),
        None => false,
    }
}

fn is_internal_host(host: &str) -> bool {
    let host = host.trim_end_matches('.');
    if host.starts_with('[') {
        // IPv6 literal — reject all in this slice.
        return true;
    }
    if host == "localhost" || host.ends_with(".local") || host.ends_with(".internal") {
        return true;
    }
    // IPv4 literal forms (incl. dotted quad).
    if host.parse::<std::net::Ipv4Addr>().is_ok() {
        return true;
    }
    false
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
