//! NIP-92 media metadata (`imeta` tags) — port of `src/nostr/imeta.ts`
//! (parseImeta): every tag element after the name is one `"key value"`
//! attribute — `["imeta", "url https://…", "m video/mp4", "dim 460x816",
//! "alt a cat", …]` — split at the FIRST space so values keep inner spaces.

use nostr::prelude::*;

pub struct ImageMeta {
    pub url: String,
    /// `dim WxH` — for aspect-ratio reservation.
    pub dim: Option<(u32, u32)>,
    pub alt: Option<String>,
    /// `m <mime>` — tells an extension-less blossom URL picture from video.
    pub mime: Option<String>,
    /// Preview frame for video tiles: the `thumb` URL, else `image`
    /// (parseImeta: thumb preferred; a later `image` never clobbers it).
    pub poster: Option<String>,
    /// `blurhash <hash>` — NIP-92's short placeholder hash (a compact
    /// gradient decoded client-side); the last-resort tile background
    /// when no poster URL rides the tag.
    pub blurhash: Option<String>,
}

pub fn parse(ev: &Event) -> Vec<ImageMeta> {
    let mut out: Vec<ImageMeta> = Vec::new();
    for tag in ev.tags.clone().to_vec() {
        let parts = tag.as_slice();
        if parts.first().map(String::as_str) != Some("imeta") {
            continue;
        }
        let mut url = String::new();
        let mut dim = None;
        let mut alt = None;
        let mut mime = None;
        let mut thumb = None;
        let mut blurhash = None;
        for entry in parts.iter().skip(1) {
            // One `"key value"` element, split at the first space; a bare
            // element (no space) carries nothing (imeta.ts:20-23).
            let Some((key, val)) = entry.split_once(' ') else {
                continue;
            };
            let val = val.trim();
            match key {
                "url" => url = val.to_string(),
                "alt" => alt = (!val.is_empty()).then(|| val.to_string()),
                "dim" => dim = parse_dim(val),
                "m" => mime = (!val.is_empty()).then(|| val.to_string()),
                // `thumb` is the dedicated preview; `image` is a fallback
                // that never clobbers a thumb (imeta.ts:31-33).
                "thumb" => thumb = (!val.is_empty()).then(|| val.to_string()),
                "image" if thumb.is_none() => {
                    thumb = (!val.is_empty()).then(|| val.to_string());
                }
                "blurhash" => blurhash = (!val.is_empty()).then(|| val.to_string()),
                _ => {} // unknown attribute — its value rides one element
            }
        }
        if url.is_empty() {
            continue;
        }
        let meta = ImageMeta {
            url,
            dim,
            alt,
            mime,
            poster: thumb,
            blurhash,
        };
        // The TS returns a Map keyed by URL: a later imeta tag for the same
        // URL replaces the earlier meta.
        match out.iter_mut().find(|m| m.url == meta.url) {
            Some(slot) => *slot = meta,
            None => out.push(meta),
        }
    }
    out
}

/// `dim WxH` — TS parseDim (content.ts:147) tolerates the decimal dims some
/// clients send ("464.0x848.0"): parse as f64, round, reject non-positive.
fn parse_dim(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once('x')?;
    let w = w.trim().parse::<f64>().ok()?;
    let h = h.trim().parse::<f64>().ok()?;
    let (w, h) = (w.round() as u32, h.round() as u32);
    (w > 0 && h > 0).then_some((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A note with one imeta tag per element list (NIP-92 element form).
    fn ev_tags(tags: &[&[&str]]) -> Event {
        let keys = Keys::generate();
        let mut t = Tags::new();
        for elements in tags {
            t.push(Tag::custom("imeta", elements.iter().copied()));
        }
        let unsigned = UnsignedEvent::new(
            keys.public_key(),
            Timestamp::from_secs(1),
            Kind::TextNote,
            t,
            "clip",
        );
        keys.sign_event(unsigned).unwrap()
    }

    fn ev(elements: &[&str]) -> Event {
        ev_tags(&[elements])
    }

    /// Regression: the live tag on Vitor's "Me, using the new Amethyst."
    /// note — real-world NIP-92 element form, video with a blurhash and no
    /// poster. The old alternating-token parser read every element as a
    /// bare key and emitted nothing.
    #[test]
    fn real_world_video_tag_parses() {
        let m = &parse(&ev(&[
            "url https://cdn.satellite.earth/1aa5cbda3bfa7ee875418062e5e8cf4078d573c480b7bab9cedb564a00307da1.mp4",
            "x 1aa5cbda3bfa7ee875418062e5e8cf4078d573c480b7bab9cedb564a00307da1",
            "size 1585738",
            "m video/mp4",
            "dim 460x816",
            "blurhash ]B8ZD.rUb^$+axv0kXs;bwjY7$O]tTJ,of-Wg1T0kEWA-Q%LXTwJjYPqjvwHXRs.e7XAo$NdX9pymiRiohV?x^TzbXr=sn",
            "ox 1aa5cbda3bfa7ee875418062e5e8cf4078d573c480b7bab9cedb564a00307da1",
        ]))[0];
        assert_eq!(
            m.url,
            "https://cdn.satellite.earth/1aa5cbda3bfa7ee875418062e5e8cf4078d573c480b7bab9cedb564a00307da1.mp4"
        );
        assert_eq!(m.mime.as_deref(), Some("video/mp4"));
        assert_eq!(m.dim, Some((460, 816)));
        assert!(m.poster.is_none());
        assert!(m.blurhash.is_some());
    }

    #[test]
    fn dim_tolerates_decimals_and_rejects_nonpositive() {
        let m = &parse(&ev(&["url https://c/v.mp4", "dim 464.0x848.0"]))[0];
        assert_eq!(m.dim, Some((464, 848)));
        assert_eq!(
            parse(&ev(&["url https://c/v.mp4", "dim 0x10"]))[0].dim,
            None
        );
    }

    #[test]
    fn alt_value_keeps_inner_spaces() {
        let m = &parse(&ev(&["url https://c/v.mp4", "alt a cat asleep"]))[0];
        assert_eq!(m.alt.as_deref(), Some("a cat asleep"));
    }

    #[test]
    fn thumb_wins_over_image_regardless_of_order() {
        let image_then_thumb = &parse(&ev(&[
            "url https://c/v.mp4",
            "image https://c/v.jpg",
            "thumb https://c/v_t.jpg",
        ]))[0];
        assert_eq!(
            image_then_thumb.poster.as_deref(),
            Some("https://c/v_t.jpg")
        );
        let thumb_then_image = &parse(&ev(&[
            "url https://c/v.mp4",
            "thumb https://c/v_t.jpg",
            "image https://c/v.jpg",
        ]))[0];
        assert_eq!(
            thumb_then_image.poster.as_deref(),
            Some("https://c/v_t.jpg")
        );
        let image_only = &parse(&ev(&["url https://c/v.mp4", "image https://c/v.jpg"]))[0];
        assert_eq!(image_only.poster.as_deref(), Some("https://c/v.jpg"));
    }

    #[test]
    fn bare_elements_and_unknown_keys_are_skipped() {
        let metas = parse(&ev(&[
            "nonsense",
            "weird skipme",
            "url https://c/v.mp4",
            "x abcd",
        ]));
        assert_eq!(metas.len(), 1);
        assert!(metas[0].poster.is_none());
        assert!(metas[0].blurhash.is_none());
    }

    #[test]
    fn tag_without_url_is_dropped() {
        assert!(parse(&ev(&["m video/mp4", "dim 460x816"])).is_empty());
    }

    #[test]
    fn later_imeta_tag_wins_for_the_same_url() {
        let metas = parse(&ev_tags(&[
            &["url https://c/v.mp4", "dim 100x100"],
            &["url https://c/v.mp4", "dim 200x200"],
        ]));
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].dim, Some((200, 200)));
    }
}
