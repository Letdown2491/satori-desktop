//! Media upload — Blossom only (NIP-96 stays rejected; matches the web app).
//!
//! Port of `src/upload.ts` semantics: your kind:10063 `server` tag list (falling
//! back to the out-of-box default when unset), the BUD-02 upload auth (kind:24242,
//! not URL-bound, so one signature serves every server in the list), PUT /upload
//! to every server with first-success-wins, and the NIP-92 `imeta` tag built from
//! the winner's response (`url`, optional `m`, optional `dim`, `x`). Image
//! dimensions come from header-only sniffing (PNG/GIF/JPEG/WebP) — no image
//! library in the upload path, mirroring the thin-deps rule.
//!
//! SSRF symmetry with `media.rs`: every server URL must pass the same
//! scheme/host guard before we connect.
//!
//! TODO(port): tor.rs — uploads run direct until M5 (M0–M4 run direct); an upload
//! is outbound traffic and must honor Privacy Mode when Tor lands.

use base64::Engine as _;
use nostr::prelude::*;
use sha2::{Digest, Sha256};

use crate::media;

/// Your media server list kind (BUD-02): `server` tags, newest event wins.
pub const KIND_BLOSSOM_LIST: u16 = 10063;
/// BUD-02 upload auth kind.
pub const KIND_BLOSSOM_AUTH: u16 = 24242;
/// Fallback when you haven't set a kind:10063 media-server list, so uploads
/// work out-of-box.
pub const DEFAULT_BLOSSOM_SERVER: &str = "https://blossom.primal.net";
/// The web app's upload cap (`upload.ts` MAX_BYTES).
pub const MAX_BYTES: u64 = 25 * 1024 * 1024;

/// One successful upload: the blob URL + the full NIP-92 `imeta` tag built for
/// it (upload.ts `Upload` — a single imeta tag, kept as a string vector).
pub struct Upload {
    pub url: String,
    /// `["imeta", "url <url>", ("m <mime>")?, ("dim <WxH>")?, "x <sha256>"]`.
    pub imeta: Vec<String>,
}

/// Unsigned Blossom auth (BUD-02): kind 24242, not URL-bound, so one signature
/// is reusable across every server in your list.
pub fn blossom_auth_template(me: PublicKey, hash: &str) -> UnsignedEvent {
    UnsignedEvent::new(
        me,
        Timestamp::now(),
        Kind::Custom(KIND_BLOSSOM_AUTH),
        vec![
            Tag::custom("t", ["upload"]),
            Tag::custom("x", [hash]),
            Tag::custom("expiration", [(now_secs() + 3600).to_string()]),
        ],
        "Upload blob".to_string(),
    )
}

/// Hash the blob before signing the auth (the `x` tag binds it).
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex(&h.finalize())
}

/// Pixel dimensions from an image byte buffer (PNG / GIF / JPEG / WebP) as a
/// NIP-92 `dim` value "WxH", or None if unrecognized. Header-only, bounded,
/// never panics (upload.ts `imageDim`). Videos return None.
pub fn image_dim(b: &[u8]) -> Option<String> {
    // PNG: 8-byte signature, then IHDR with width@16, height@20 (big-endian u32).
    if b.len() >= 24 && b[0] == 0x89 && b[1] == 0x50 && b[2] == 0x4e && b[3] == 0x47 {
        let w = u32::from_be_bytes([b[16], b[17], b[18], b[19]]);
        let h = u32::from_be_bytes([b[20], b[21], b[22], b[23]]);
        return Some(format!("{w}x{h}"));
    }
    // GIF: 'GIF8', width@6, height@8 (little-endian u16).
    if b.len() >= 10 && b[0] == 0x47 && b[1] == 0x49 && b[2] == 0x46 && b[3] == 0x38 {
        let w = u16::from_le_bytes([b[6], b[7]]);
        let h = u16::from_le_bytes([b[8], b[9]]);
        return Some(format!("{w}x{h}"));
    }
    // JPEG: FFD8, then walk segments to a Start-Of-Frame marker; height then
    // width (BE u16) follow.
    if b.len() >= 4 && b[0] == 0xff && b[1] == 0xd8 {
        let mut i = 2;
        while i + 9 < b.len() {
            if b[i] != 0xff {
                i += 1;
                continue;
            }
            let marker = b[i + 1];
            if (0xc0..=0xc3).contains(&marker)
                || (0xc5..=0xc7).contains(&marker)
                || (0xc9..=0xcb).contains(&marker)
                || (0xcd..=0xcf).contains(&marker)
            {
                let h = u16::from_be_bytes([b[i + 5], b[i + 6]]);
                let w = u16::from_be_bytes([b[i + 7], b[i + 8]]);
                return Some(format!("{w}x{h}"));
            }
            // Markers with no length payload.
            if marker == 0xd8 || marker == 0xd9 || (0xd0..=0xd7).contains(&marker) {
                i += 2;
                continue;
            }
            let len = u16::from_be_bytes([b[i + 2], b[i + 3]]);
            if len < 2 {
                return None;
            }
            i += 2 + len as usize;
        }
        return None;
    }
    // WebP (extended VP8X): 'RIFF'....'WEBP','VP8X', then (width-1),(height-1)
    // as 24-bit little-endian.
    if b.len() >= 30 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" && &b[12..16] == b"VP8X" {
        let w = 1 + (b[24] as u32 | (b[25] as u32) << 8 | (b[26] as u32) << 16);
        let h = 1 + (b[27] as u32 | (b[28] as u32) << 8 | (b[29] as u32) << 16);
        return Some(format!("{w}x{h}"));
    }
    None
}

/// Image mime from a file path (extension → image/*), None for anything the
/// NIP-68 picture flow doesn't accept. Videos are rejected here — the web app
/// posts video as a note instead (note.ts postPicture).
pub fn image_mime(path: &str) -> Option<&'static str> {
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// Upload one blob to a single Blossom server (PUT /upload) with a signed auth.
async fn blossom_put(
    server: &str,
    bytes: Vec<u8>,
    content_type: &str,
    signed: &Event,
) -> Result<(String, String), String> {
    let url = format!("{}/upload", server.trim_end_matches('/'));
    let json = serde_json::to_string(signed).map_err(|e| e.to_string())?;
    let auth = format!(
        "Nostr {}",
        base64::engine::general_purpose::STANDARD.encode(json)
    );
    let res = reqwest::Client::new()
        .put(&url)
        .header("Authorization", auth)
        .header(
            "Content-Type",
            if content_type.is_empty() {
                "application/octet-stream"
            } else {
                content_type
            },
        )
        .body(bytes)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = res.status();
    if !status.is_success() {
        return Err(format!("blossom upload failed ({status})"));
    }
    let body = res.text().await.map_err(|e| e.to_string())?;
    let j: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    let url = j
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or("blossom returned no url")?
        .to_string();
    let sha = j
        .get("sha256")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok((url, sha))
}

/// Upload to all Blossom servers (first success wins, like Satori). Servers
/// failing the SSRF guard are dropped before connect (upload.ts `safe`).
pub async fn blossom_upload_all(
    servers: &[String],
    bytes: Vec<u8>,
    content_type: &str,
    hash: &str,
    signed: &Event,
) -> Result<Upload, String> {
    let safe: Vec<&String> = servers
        .iter()
        .filter(|s| {
            media::is_safe_media_url(&format!("{}/upload", s.trim_end_matches('/')))
        })
        .collect();
    if safe.is_empty() {
        return Err("no safe Blossom server to upload to".into());
    }
    // Race every server; first success wins. tokio tasks keep this parallel
    // without pulling `futures` in.
    let mut handles = Vec::new();
    for s in &safe {
        let s = (*s).clone();
        let bytes = bytes.clone();
        let content_type = content_type.to_string();
        let signed = signed.clone();
        handles.push(tokio::spawn(async move {
            blossom_put(&s, bytes, &content_type, &signed).await
        }));
    }
    let mut blob: Option<(String, String)> = None;
    let mut last_err = String::from("no Blossom server accepted the upload");
    for h in handles {
        match h.await.map_err(|e| e.to_string()).and_then(|r| r) {
            Ok(ok) => {
                blob = Some(ok);
                break;
            }
            Err(e) => last_err = e,
        }
    }
    let (url, sha) = blob.ok_or(last_err)?;
    let dim = image_dim(&bytes);
    let mut imeta = vec![
        "imeta".to_string(),
        format!("url {url}"),
    ];
    if !content_type.is_empty() {
        imeta.push(format!("m {content_type}"));
    }
    if let Some(d) = dim {
        imeta.push(format!("dim {d}"));
    }
    imeta.push(format!("x {}", if sha.is_empty() { hash.to_string() } else { sha }));
    Ok(Upload { url, imeta })
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_dims() {
        let mut png = vec![0x89, 0x50, 0x4e, 0x47, 0, 0, 0, 0];
        png.extend_from_slice(&[0; 8]);
        png.extend_from_slice(&32u32.to_be_bytes()); // width
        png.extend_from_slice(&18u32.to_be_bytes()); // height
        assert_eq!(image_dim(&png).as_deref(), Some("32x18"));
    }

    #[test]
    fn gif_dims() {
        let mut gif = b"GIF8".to_vec();
        gif.extend_from_slice(&[0, 0]);
        gif.extend_from_slice(&640u16.to_le_bytes());
        gif.extend_from_slice(&480u16.to_le_bytes());
        assert_eq!(image_dim(&gif).as_deref(), Some("640x480"));
    }

    #[test]
    fn unknown_is_none() {
        assert!(image_dim(b"hello world").is_none());
    }

    #[test]
    fn mime_from_path() {
        assert_eq!(image_mime("cat.PNG"), Some("image/png"));
        assert_eq!(image_mime("dog.jpeg"), Some("image/jpeg"));
        assert_eq!(image_mime("clip.mp4"), None);
        assert_eq!(image_mime("noext"), None);
    }
}
