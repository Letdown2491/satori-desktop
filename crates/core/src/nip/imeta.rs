//! NIP-92 media metadata (`imeta` tags) — port of `src/nostr/imeta.ts`
//! (inline form slice): `imeta <url> dim WxH m <mime> alt <text> ...`

use nostr::prelude::*;

pub struct ImageMeta {
    pub url: String,
    /// `dim WxH` — for aspect-ratio reservation.
    pub dim: Option<(u32, u32)>,
    pub alt: Option<String>,
    pub mime: Option<String>,
    /// `image <url>` — the poster frame video clients may attach (NIP-92);
    /// None on plain picture tags.
    pub poster: Option<String>,
}

pub fn parse(ev: &Event) -> Vec<ImageMeta> {
    let mut out = Vec::new();
    for tag in ev.tags.clone().to_vec() {
        let parts = tag.as_slice();
        if parts.first().map(String::as_str) != Some("imeta") || parts.len() < 2 {
            continue;
        }
        // Inline form: imeta url <url> dim WxH m <mime> alt <text...>
        let mut url = None;
        let mut dim = None;
        let mut alt = String::new();
        let mut mime = None;
        let mut poster = None;
        let mut it = parts.iter().skip(1);
        while let Some(key) = it.next() {
            match key.as_str() {
                "url" => url = it.next().cloned(),
                "dim" => {
                    if let Some(d) = it.next().and_then(|s| parse_dim(s)) {
                        dim = Some(d);
                    }
                }
                "m" => mime = it.next().cloned(),
                "alt" => alt = it.next().cloned().unwrap_or_default(),
                "image" => poster = it.next().cloned(),
                _ => {
                    let _ = it.next(); // unknown attribute — skip its value
                }
            }
        }
        if let Some(u) = url {
            out.push(ImageMeta {
                url: u,
                dim,
                alt: (!alt.is_empty()).then_some(alt),
                mime,
                poster: poster.filter(|p| !p.is_empty()),
            });
        }
    }
    out
}

fn parse_dim(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}
