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
    /// `thumb <url>` — NIP-92's smaller preview; a poster fallback when
    /// the publisher sent no `image`.
    pub thumb: Option<String>,
    /// `blurhash <hash>` — NIP-92's short placeholder hash (a compact
    /// gradient decoded client-side); the last-resort tile background
    /// when no poster URL rides the tag.
    pub blurhash: Option<String>,
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
        let mut image = None;
        let mut thumb = None;
        let mut blurhash = None;
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
                "image" => image = it.next().cloned(),
                "thumb" => thumb = it.next().cloned(),
                "blurhash" => blurhash = it.next().cloned(),
                _ => {
                    let _ = it.next(); // unknown attribute — skip its value
                }
            }
        }
        if let Some(u) = url {
            // The poster prefers NIP-92 `image` (the poster frame proper)
            // and falls back to `thumb` (the small preview): either serves
            // the tile; a publisher sending only `thumb` still gets one.
            let thumb = thumb.filter(|p| !p.is_empty());
            let poster = image
                .filter(|p| !p.is_empty())
                .or_else(|| thumb.clone());
            out.push(ImageMeta {
                url: u,
                dim,
                alt: (!alt.is_empty()).then_some(alt),
                mime,
                poster,
                thumb,
                blurhash: blurhash.filter(|h| !h.is_empty()),
            });
        }
    }
    out
}

fn parse_dim(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::prelude::*;

    fn ev(imeta: &[&str]) -> Event {
        let keys = Keys::generate();
        let mut tags = Tags::new();
        tags.push(Tag::custom("imeta", imeta.iter().copied()));
        let unsigned = UnsignedEvent::new(
            keys.public_key(),
            Timestamp::from_secs(1),
            Kind::TextNote,
            tags,
            "clip",
        );
        keys.sign_event(unsigned).unwrap()
    }

    #[test]
    fn image_attr_is_the_poster() {
        let m = &parse(&ev(&["url", "https://c/v.mp4", "image", "https://c/v.jpg"]))[0];
        assert_eq!(m.poster.as_deref(), Some("https://c/v.jpg"));
        assert_eq!(m.blurhash, None);
    }

    #[test]
    fn thumb_falls_back_to_poster() {
        let m = &parse(&ev(&["url", "https://c/v.mp4", "thumb", "https://c/v_t.jpg"]))[0];
        assert_eq!(m.poster.as_deref(), Some("https://c/v_t.jpg"));
        assert_eq!(m.thumb.as_deref(), Some("https://c/v_t.jpg"));
        // And `image` wins when both ride the tag.
        let m = &parse(&ev(&[
            "url",
            "https://c/v.mp4",
            "image",
            "https://c/v.jpg",
            "thumb",
            "https://c/v_t.jpg",
        ]))[0];
        assert_eq!(m.poster.as_deref(), Some("https://c/v.jpg"));
    }

    #[test]
    fn blurhash_and_unknown_attrs() {
        let m = &parse(&ev(&[
            "url",
            "https://c/v.mp4",
            "blurhash",
            "LEHV6nWB2yk8pyo0adR*.7kCMdnj",
            "weird",
            "skipped",
            "dim",
            "1280x720",
        ]))[0];
        assert_eq!(m.blurhash.as_deref(), Some("LEHV6nWB2yk8pyo0adR*.7kCMdnj"));
        assert_eq!(m.dim, Some((1280, 720)));
        assert_eq!(m.poster, None);
    }
}

