//! Note content → Pango markup; relative time. Port of `src/render/content.ts`
//! (tokenizer slice) + `timeAgo` (render/util.ts).

/// Resolves a bech32 entity (npub/nprofile) to a display name, if cached.
pub type NameResolver<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Escape for Pango markup.
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Tokenize note content into markup: escaped runs, https links, `nostr:`
/// refs and bare `@npub` mentions resolved through `resolve` (content.ts).
/// Image-looking URLs are returned separately (media-run behavior).
pub struct Rendered {
    pub markup: String,
    /// Image URLs extracted from the text (media runs).
    pub images: Vec<String>,
    /// Audio/video URLs extracted from the text (play facades — blossom
    /// blobs and any direct media link).
    pub facades: Vec<String>,
    /// YouTube/Spotify URLs (rich preview cards via oEmbed).
    pub previews: Vec<String>,
    /// note1/nevent1 bech32 references (quote/embed candidates).
    pub refs: Vec<String>,
}

pub fn render_content(content: &str, resolve: NameResolver) -> Rendered {
    let mut images = Vec::new();
    let mut refs = Vec::new();
    let mut facades = Vec::new();
    let mut previews = Vec::new();
    let markup = render_content_inner(content, &mut images, &mut refs, &mut facades, &mut previews, resolve);
    Rendered { markup, images, facades, previews, refs }
}

/// Tracking-param strip (content.ts:75-94): utm_*, click ids, referrers,
/// youtube si/feature (v= survives on watch links).
pub fn clean_url(raw: &str) -> String {
    use url::Url;
    let Ok(mut u) = Url::parse(raw) else {
        return raw.to_string();
    };
    let host = u.host_str().unwrap_or_default().replace("www.", "");
    let is_yt = host == "youtube.com" || host == "youtu.be" || host == "m.youtube.com";
    let trackers = [
        "utm_source", "utm_medium", "utm_campaign", "utm_term", "utm_content",
        "fbclid", "gclid", "dclid", "msclkid", "ref_src", "ref_url", "ref",
        "mc_cid", "mc_eid", "iga_", "spm", "scid", "si", "feature", "app",
    ];
    // No query: don't touch it (query_pairs_mut would append a bare '?').
    if u.query().is_none() {
        return u.to_string();
    }
    let keep: Vec<(String, String)> = u
        .query_pairs()
        .filter(|(k, v)| {
            let k = k.to_ascii_lowercase();
            if is_yt && (k == "si" || k == "feature" || k == "app") {
                return false;
            }
            !trackers.iter().any(|t| k == *t || k.starts_with("utm_") || k.starts_with("iga_"))
        })
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    u.query_pairs_mut().clear();
    if !keep.is_empty() {
        u.query_pairs_mut().extend_pairs(keep.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    }
    u.to_string()
}

pub fn is_youtube(url: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.replace("www.", "")))
        .map(|h| h == "youtube.com" || h == "youtu.be" || h == "m.youtube.com" || h == "music.youtube.com")
        .unwrap_or(false)
}

pub fn is_spotify(url: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.replace("www.", "").replace("open.", "")))
        .map(|h| h == "spotify.com")
        .unwrap_or(false)
}

/// Direct audio/video link (blossom blobs and friends) — play facade.
pub fn is_av_url(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or("");
    [".mov", ".mp4", ".webm", ".mkv", ".mp3", ".ogg", ".oga", ".wav", ".m4a", ".flac"]
        .iter()
        .any(|ext| path.ends_with(ext))
}

/// YouTube video id (watch?v=, youtu.be/, shorts/, embed/).
pub fn youtube_id(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    let host = u.host_str()?.replace("www.", "");
    if host == "youtu.be" {
        return u.path().strip_prefix('/')?.split('/').next().map(|s| s.to_string());
    }
    if host.ends_with("youtube.com") {
        if let Some(v) = u.query_pairs().find(|(k, _)| k == "v").map(|(_, v)| v.to_string()) {
            return (!v.is_empty()).then_some(v);
        }
        let segs: Vec<&str> = u.path().split('/').filter(|s| !s.is_empty()).collect();
        if segs.len() >= 2 && (segs[0] == "shorts" || segs[0] == "embed" || segs[0] == "live") {
            return Some(segs[1].to_string());
        }
    }
    None
}

fn mention_markup(bech32: &str, resolve: NameResolver) -> String {
    let display = match resolve(bech32) {
        Some(name) => format!("@{}", name),
        None => {
            let tail: String = bech32.chars().rev().take(4).collect::<String>().chars().rev().collect();
            format!("@npub1…{tail}")
        }
    };
    format!(r#"<a href="nostr:{}">{}</a>"#, esc(bech32), esc(&display))
}

fn is_image_url(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or("");
    path.ends_with(".png")
        || path.ends_with(".jpg")
        || path.ends_with(".jpeg")
        || path.ends_with(".webp")
        || path.ends_with(".gif")
}

fn render_content_inner(
    content: &str,
    images: &mut Vec<String>,
    refs: &mut Vec<String>,
    facades: &mut Vec<String>,
    previews: &mut Vec<String>,
    resolve: NameResolver,
) -> String {
    let mut out = String::with_capacity(content.len() + 64);
    let mut rest = content;
    loop {
        let http = rest.find("https://");
        let nostr = rest.find("nostr:");
        let at = rest.find("@npub1");
        let start = [http, nostr, at].into_iter().flatten().min();
        match start {
            None => {
                out.push_str(&esc(rest));
                return out;
            }
            Some(i) => {
                out.push_str(&esc(&rest[..i]));
                rest = &rest[i..];
                if rest.starts_with("@npub1") {
                    // Scan past the @ (non-alphanumeric itself).
                    let end = rest[1..]
                        .find(|c: char| !(c.is_ascii_alphanumeric()))
                        .map(|i| i + 1)
                        .unwrap_or(rest.len());
                    let bech = &rest[1..end]; // strip @
                    out.push_str(&mention_markup(bech, resolve));
                    rest = &rest[end..];
                } else if rest.starts_with("https://") {
                    let end = rest
                        .find(|c: char| c.is_whitespace() || c == ')' || c == '<')
                        .unwrap_or(rest.len());
                    let url = clean_url(&rest[..end]);
                    rest = &rest[end..];
                    if is_youtube(&url) || is_spotify(&url) {
                        // Rich preview card replaces the plain link.
                        if !previews.contains(&url) {
                            previews.push(url);
                        }
                    } else if is_image_url(&url) {
                        if !images.contains(&url) {
                            images.push(url);
                        }
                    } else if is_av_url(&url) {
                        if !facades.contains(&url) {
                            facades.push(url);
                        }
                    } else {
                        out.push_str(&format!(
                            r#"<a href="{}">{}</a>"#,
                            esc(&url),
                            esc(&display_url(&url))
                        ));
                    }
                } else {
                    // nostr:<bech32> — skip the scheme, then take bech32 chars.
                    let body_start = "nostr:".len();
                    let end = rest[body_start..]
                        .find(|c: char| !c.is_ascii_alphanumeric())
                        .map(|i| i + body_start)
                        .unwrap_or(rest.len());
                    let raw = &rest[..end];
                    let bech = &rest[body_start..end];
                    if (bech.starts_with("note1") || bech.starts_with("nevent1"))
                        && !refs.contains(&bech.to_string()) {
                            refs.push(bech.to_string());
                        }
                    if bech.starts_with("npub1") || bech.starts_with("nprofile1") {
                        // mention_markup already carries the href.
                        out.push_str(&mention_markup(bech, resolve));
                    } else {
                        let display =
                            display_nostr_ref(bech).unwrap_or_else(|| "↗ reference".into());
                        out.push_str(&format!(r#"<a href="{}">{}</a>"#, esc(raw), esc(&display)));
                    }
                    rest = &rest[raw.len()..];
                }
            }
        }
    }
}

fn display_url(url: &str) -> String {
    let s = url.strip_prefix("https://").unwrap_or(url);
    let mut s = s.split('/').take(3).collect::<Vec<_>>().join("/");
    if s.len() > 48 {
        s.truncate(45);
        s.push('…');
    }
    s
}

/// Display text for non-profile nostr refs. NIP-19 decode first (kind-aware
/// naddr labels); prefix fallback when the checksum doesn't decode (partial
/// pastes).
fn display_nostr_ref(bech: &str) -> Option<String> {
    use nostr::prelude::*;
    if let Ok(n) = Nip19::from_bech32(bech) {
        return match n {
            Nip19::EventId(_) | Nip19::Event(_) => Some("↗ note".to_string()),
            Nip19::Coordinate(c) => match u16::from(c.kind) {
                30023 => Some("↗ article".to_string()),
                31922 | 31923 => Some("↗ event".to_string()),
                1068 => Some("↗ poll".to_string()),
                _ => None,
            },
            _ => None,
        };
    }
    if bech.starts_with("note1") || bech.starts_with("nevent1") {
        return Some("↗ note".to_string());
    }
    if bech.starts_with("naddr1") {
        return Some("↗ reference".to_string());
    }
    None
}

/// Relative time — render/util.ts timeAgo.
pub fn time_ago(secs: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let d = now.saturating_sub(secs);
    if d < 60 {
        "now".into()
    } else if d < 3600 {
        format!("{}m", d / 60)
    } else if d < 86400 {
        format!("{}h", d / 3600)
    } else if d < 7 * 86400 {
        format!("{}d", d / 86400)
    } else {
        format!("{}w", d / (7 * 86400))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_resolve(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn escapes_and_links() {
        let r = render_content(
            "hi & <you> see https://example.com/x and nostr:note1abc",
            &no_resolve,
        );
        let m = r.markup;
        assert!(m.contains("&amp; &lt;you&gt;"));
        assert!(m.contains(r#"<a href="https://example.com/x">"#));
        assert!(m.contains(r#"<a href="nostr:note1abc">↗ note</a>"#));
    }

    #[test]
    fn images_move_out_of_text() {
        let r = render_content("look https://x.test/pic.png end", &no_resolve);
        let (m, imgs) = (r.markup, r.images);
        assert!(!m.contains(r#"<a href="https://x.test/pic.png">"#));
        assert_eq!(imgs, vec!["https://x.test/pic.png".to_string()]);
    }

    #[test]
    fn tracking_params_stripped() {
        assert_eq!(
            clean_url("https://x.test/post?utm_source=rss&id=7"),
            "https://x.test/post?id=7"
        );
        assert_eq!(
            clean_url("https://youtube.com/watch?v=abc&si=trackme"),
            "https://youtube.com/watch?v=abc"
        );
    }

    #[test]
    fn youtube_ids() {
        assert_eq!(youtube_id("https://youtube.com/watch?v=abc123").as_deref(), Some("abc123"));
        assert_eq!(youtube_id("https://youtu.be/abc123").as_deref(), Some("abc123"));
        assert_eq!(youtube_id("https://youtube.com/shorts/abc123").as_deref(), Some("abc123"));
    }

    #[test]
    fn mentions_resolve_names() {
        let resolve = |b: &str| {
            (b.starts_with("npub1known")).then(|| "Matsu".to_string())
        };
        let (m, _) = {
            let r = render_content(
                "hey @npub1known and nostr:npub1unknownaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                &resolve,
            );
            (r.markup, r.refs)
        };
        assert!(m.contains(">@Matsu</a>"));
        assert!(m.contains(">@npub1…aaaa</a>"));
    }
}
