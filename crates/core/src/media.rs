//! Media pipeline — guarded fetch, sha256-keyed disk cache, strict decode.
//!
//! Port of `src/routes/image-proxy.ts` + `src/ssrf.ts` discipline, reduced to
//! the client surface: scheme allowlist (https only), IP-literal and
//! internal-host rejection, sha256-keyed disk cache, strict decode to pixels
//! (SVG never rendered as a trusted format — `image` decodes raster only).
//!
//! Reliability contract (polish pass): every network step is timed and
//! byte-capped; transient failures retry with backoff and report themselves
//! as such (the UI cools off instead of giving up); bytes are validated by
//! decode BEFORE they touch the disk (a poisoned cache file — an HTML error
//! page keyed as an image — used to outlive restarts); and decodes of disc-
//! sized needs ship a ≤128px PNG beside the original so an avatar never
//! costs a 48MB full-res decode inside gpui's asset thread.
//!
//! TODO(port): full SSRF set — DNS-rebinding-safe resolution on direct paths,
//! per-redirect re-checks, Tor transport, NIP-B7 healing (M2/M4).

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use sha2::{Digest, Sha256};
use url::Url;

use crate::config;

pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// The ORIGINAL encoded bytes (jpeg/png/webp/gif) — the UI hands them to
    /// gpui's image pipeline, which decodes on its own asset thread.
    pub bytes: Vec<u8>,
    /// Format of `bytes` (gpui maps it onto its own enum).
    pub format: image::ImageFormat,
    /// A ≤128px PNG re-encode for disc-sized renders (avatars). gpui decodes
    /// `bytes` at full resolution — a 4000×3000 photo costs ~48MB of decode
    /// for a 28px circle without this. None when the original is already
    /// small (the full decode is cheap) or the re-encode failed.
    pub thumb: Option<Vec<u8>>,
}

/// Why a fetch failed. The distinction is the UI's retry policy: Transient
/// cools off and tries again later; NotMedia is a verdict — the bytes
/// arrived whole and are not an image (an HTML error page, a blossom page
/// URL) — retrying can never help.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaError {
    /// Network hiccup, timeout, server error, rate limit — worth another
    /// attempt (immediately backed-off here, then again on a later look).
    Transient,
    /// Verified not-an-image (or unsafe/over-cap): terminal for this session.
    NotMedia,
}

impl std::fmt::Display for MediaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MediaError::Transient => write!(f, "media fetch failed — will retry"),
            MediaError::NotMedia => write!(f, "not an image"),
        }
    }
}

/// Download caps: media files average whole MBs, and every stream fetched
/// through here is by definition user-adjacent — 20MB covers any picture
/// worth showing (bigger is a distribution, not an image); JSON previews
/// get 2MB. Over-cap reads are verdicts, not errors: no retry helps.
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
const MAX_JSON_BYTES: usize = 2 * 1024 * 1024;

/// Bounded retry cadence for transient failures: three attempts, 0.6s →
/// 1.2s apart — enough to absorb a CDN hiccup, short enough that the UI's
/// own cool-off (media.rs) picks the next attempt up promptly.
const RETRY_ATTEMPTS: usize = 3;

/// One shared client for every fetch in this module. Two hard rules baked
/// in: a browser-style User-Agent (CDNs — Cloudflare in front of the
/// blossoms especially — 403 requests with no UA, and those 403s read as
/// verdicts, so avatars died permanently) and connect/total timeouts (a
/// hung fetch used to pin the cache entry Loading forever, joining every
/// later waiter onto a corpse — the avatar that never loads all session).
fn http() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36",
            )
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

/// GET through the guarded client with the status→verdict mapping: 429/5xx
/// and every transport-level failure are Transient; the other 4xx are
/// verdicts (a 404 will not heal by Tuesday).
async fn guarded_get(url: &str) -> Result<reqwest::Response, MediaError> {
    let res = http()
        .get(url)
        .send()
        .await
        // Timeouts, connect failures, TLS hiccups: all weather, all transient.
        .map_err(|_| MediaError::Transient)?;
    match res.status().as_u16() {
        200..=299 => Ok(res),
        429 | 500..=599 => Err(MediaError::Transient),
        _ => Err(MediaError::NotMedia),
    }
}

/// Read the body under a hard cap (declared length rejected up-front, the
/// chunk stream cut off if it lies).
async fn read_capped(mut res: reqwest::Response, cap: usize) -> Result<Vec<u8>, MediaError> {
    if let Some(len) = res.content_length()
        && len as usize > cap
    {
        return Err(MediaError::NotMedia);
    }
    let mut body = Vec::new();
    while let Some(chunk) = res.chunk().await.map_err(|_| MediaError::Transient)? {
        if body.len() + chunk.len() > cap {
            return Err(MediaError::NotMedia);
        }
        body.extend_from_slice(chunk.as_ref());
    }
    Ok(body)
}

/// GET + byte cap + bounded transient retries. Permanent verdicts return at
/// once — nothing below this layer ever re-sees a network error.
async fn fetch_capped(url: &str, cap: usize) -> Result<Vec<u8>, MediaError> {
    let mut wait = Duration::from_millis(600);
    for attempt in 0..RETRY_ATTEMPTS {
        let outcome = match guarded_get(url).await {
            Ok(res) => return read_capped(res, cap).await,
            Err(e) => e,
        };
        if outcome == MediaError::NotMedia || attempt + 1 == RETRY_ATTEMPTS {
            return Err(outcome);
        }
        tokio::time::sleep(wait).await;
        wait *= 2;
    }
    Err(MediaError::Transient)
}

/// GET for the JSON surfaces (oEmbed, LNURL-pay): unlike the media path the
/// failure text reaches the user (the zap sheet shows it), so statuses keep
/// their own honest wording instead of media verdicts.
async fn json_get(url: &str) -> Result<reqwest::Response, String> {
    let res = http().get(url).send().await.map_err(|e| e.to_string())?;
    let status = res.status();
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    Ok(res)
}

/// Fetch a JSON document (oEmbed etc.) through the same guarded, cached
/// path. Cache read-through keeps repeat previews instant; the parse is
/// validated BEFORE the disk write (a cached non-JSON body would poison the
/// preview permanently — same discipline as fetch_image's decode).
pub async fn fetch_json(url: &str) -> Result<serde_json::Value, String> {
    if !is_safe_media_url(url) {
        return Err("unsafe URL".into());
    }
    let path = cache_path(&cache_key(url));
    if let Ok(cached) = std::fs::read(&path) {
        return serde_json::from_slice(&cached).map_err(|e| e.to_string());
    }
    let res = json_get(url).await?;
    let body = read_capped(res, MAX_JSON_BYTES)
        .await
        .map_err(|e| e.to_string())?;
    let value: serde_json::Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
    write_cache(&path, &body);
    Ok(value)
}

/// A guarded JSON fetch with NO disk cache — for answers that must never be
/// replayed (LNURL-pay callbacks return one-time bolt11 invoices; a cached
/// invoice would double-fail after its first spend). Same SSRF discipline as
/// every other fetch (D20: the wallet's LNURL path rides this).
pub async fn fetch_json_uncached(url: &str) -> Result<serde_json::Value, String> {
    if !is_safe_media_url(url) {
        return Err("unsafe URL".into());
    }
    let res = json_get(url).await?;
    let body = read_capped(res, MAX_JSON_BYTES)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::from_slice(&body).map_err(|e| e.to_string())
}

pub async fn fetch_image(url: &str) -> Result<DecodedImage, MediaError> {
    if !is_safe_media_url(url) {
        return Err(MediaError::NotMedia);
    }

    // Cache read-through, keyed by sha256(url). Only validated decodes are
    // ever on disk (below), so a cache hit cannot poison the decode.
    let key = cache_key(url);
    let path = cache_path(&key);
    match std::fs::read(&path) {
        Ok(cached) => decode(&cached),
        Err(_) => {
            let body = fetch_capped(url, MAX_IMAGE_BYTES).await?;
            // Validate BEFORE the disk write. The write used to precede the
            // decode, so an HTML error page or truncated body keyed as an
            // image stayed there forever — a broken avatar that survived
            // restarts. (The decode result doubles as the return value:
            // fresh bytes decode exactly once.)
            let decoded = decode(&body)?;
            write_cache(&path, &body);
            Ok(decoded)
        }
    }
}

/// The pre-polish cache dir could hold poisoned files (bytes hit disk before
/// decode validated them); the versioned dir starts clean and this drops the
/// legacy one. Best-effort, once per process start (manager init).
pub fn purge_legacy_cache() {
    let legacy = config::data_dir().join("media-cache");
    if legacy.exists() {
        let _ = std::fs::remove_dir_all(legacy);
    }
}

pub fn cache_key(url: &str) -> String {
    let mut h = Sha256::new();
    h.update(url.as_bytes());
    hex(&h.finalize())
}

fn cache_path(key: &str) -> PathBuf {
    config::media_cache_dir().join(key)
}

fn write_cache(path: &std::path::Path, body: &[u8]) {
    if std::fs::create_dir_all(config::media_cache_dir()).is_err() {
        return;
    }
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut f| f.write_all(body));
    WRITE_COUNT.fetch_add(1, Ordering::Relaxed);
    // Trim cadence vs overshoot: media files average whole MBs, so a
    // sparse cadence lets the dir balloon between trims (every 50th
    // write ≈ +150MB observed); every 10th keeps the overshoot to tens
    // of MB for a stat() pass over a few hundred files.
    if WRITE_COUNT.load(Ordering::Relaxed).is_multiple_of(10) {
        trim_cache(200 * 1024 * 1024, 150 * 1024 * 1024);
    }
}

fn decode(bytes: &[u8]) -> Result<DecodedImage, MediaError> {
    // The full decode validates the bytes (a blossom page URL is not media)
    // and yields the true dims for aspect-correct frames; the UI never
    // touches these pixels — it re-decodes from `bytes` on its own thread.
    let img = image::load_from_memory(bytes).map_err(|_| MediaError::NotMedia)?;
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    let mut format = image::guess_format(bytes).map_err(|_| MediaError::NotMedia)?;
    let mut out = bytes.to_vec();
    // gpui's image pipeline maps png/jpeg/webp/gif only — AVIF bytes would
    // die at the UI layer and read as not-media. The pixels are validated
    // here, so a one-time re-encode (on the fetch thread, disk-cached after)
    // is all the format needs to travel the rest of the pipe.
    if format == image::ImageFormat::Avif {
        let mut png = Vec::new();
        if img
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .is_ok()
        {
            out = png;
            format = image::ImageFormat::Png;
        }
    }
    // Disc thumb: only when the original dwarfs a disc — at ≤256 on the long
    // edge the full decode is already cheap. `thumbnail` is the fast path
    // (nearest sampling); a disc is 28px, quality is not the question.
    let thumb = if width.max(height) > 256 {
        let mut png = Vec::new();
        img.thumbnail(128, 128)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .ok()
            .map(|_| png)
    } else {
        None
    };
    Ok(DecodedImage {
        width,
        height,
        bytes: out,
        format,
        thumb,
    })
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

use std::sync::atomic::{AtomicU64, Ordering};
static WRITE_COUNT: AtomicU64 = AtomicU64::new(0);

/// LRU trim of the media cache by mtime (avatar-cache.ts: 200MB cap,
/// draining to 150MB). Best-effort; runs inline on the fetch path.
fn trim_cache(cap: u64, floor: u64) {
    let dir = match std::fs::read_dir(config::media_cache_dir()) {
        Ok(d) => d,
        Err(_) => return,
    };
    let mut entries: Vec<(std::path::PathBuf, u64, std::time::SystemTime)> = dir
        .flatten()
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            Some((e.path(), m.len(), m.modified().ok()?))
        })
        .collect();
    let total: u64 = entries.iter().map(|(_, s, _)| s).sum();
    if total < cap {
        return;
    }
    entries.sort_by_key(|(_, _, m)| *m);
    let mut remaining = total;
    for (path, size, _) in entries {
        if remaining < floor {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            remaining -= size;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_error_display() {
        assert_eq!(MediaError::Transient.to_string(), "media fetch failed — will retry");
        assert_eq!(MediaError::NotMedia.to_string(), "not an image");
    }

    #[test]
    fn ssrf_guard_unchanged() {
        assert!(is_safe_media_url("https://blossom.primal.net/abc.png"));
        assert!(!is_safe_media_url("http://blossom.primal.net/abc.png"));
        assert!(!is_safe_media_url("https://127.0.0.1/x.png"));
        assert!(!is_safe_media_url("https://localhost/x.png"));
        assert!(!is_safe_media_url("https://host.internal/x.png"));
        assert!(!is_safe_media_url("not a url"));
    }
}
