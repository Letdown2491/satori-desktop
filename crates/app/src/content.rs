//! Note content → Pango markup; relative time. Port of `src/render/content.ts`
//! (tokenizer slice) + `timeAgo` (render/util.ts).

/// Escape for Pango markup.
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Tokenize note content into markup: escaped runs, https links, `nostr:` refs.
/// Image-looking URLs are returned separately (rendered as media, content.ts
/// media-run behavior).
pub fn render_content(content: &str) -> (String, Vec<String>) {
    let mut images = Vec::new();
    let markup = render_content_inner(content, &mut images);
    (markup, images)
}

fn is_image_url(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or("");
    path.ends_with(".png")
        || path.ends_with(".jpg")
        || path.ends_with(".jpeg")
        || path.ends_with(".webp")
        || path.ends_with(".gif")
}

fn render_content_inner(content: &str, images: &mut Vec<String>) -> String {
    let mut out = String::with_capacity(content.len() + 64);
    let mut rest = content;
    loop {
        let http = rest.find("https://");
        let nostr = rest.find("nostr:");
        let start = match (http, nostr) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };
        match start {
            None => {
                out.push_str(&esc(rest));
                return out;
            }
            Some(i) => {
                out.push_str(&esc(&rest[..i]));
                rest = &rest[i..];
                if rest.starts_with("https://") {
                    let end = rest
                        .find(|c: char| c.is_whitespace() || c == ')' || c == '<')
                        .unwrap_or(rest.len());
                    let url = &rest[..end];
                    if is_image_url(url) {
                        if !images.contains(&url.to_string()) {
                            images.push(url.to_string());
                        }
                    } else {
                        out.push_str(&format!(
                            r#"<a href="{}">{}</a>"#,
                            esc(url),
                            esc(&display_url(url))
                        ));
                    }
                    rest = &rest[end..];
                } else {
                    // nostr:<bech32> — skip the scheme, then take bech32 chars.
                    let body_start = "nostr:".len();
                    let end = rest[body_start..]
                        .find(|c: char| !c.is_ascii_alphanumeric())
                        .map(|i| i + body_start)
                        .unwrap_or(rest.len());
                    let raw = &rest[..end];
                    out.push_str(&format!(
                        r#"<a href="{}">{}</a>"#,
                        esc(raw),
                        esc(&display_nostr_ref(raw))
                    ));
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

fn display_nostr_ref(raw: &str) -> String {
    let body = raw.strip_prefix("nostr:").unwrap_or(raw);
    if let Some(b) = body.strip_prefix("npub1") {
        return format!("@npub1{}", &b[..b.len().min(4)]);
    }
    if body.starts_with("note1") || body.starts_with("nevent1") || body.starts_with("naddr1") {
        return "↗ note".to_string();
    }
    raw.to_string()
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

    #[test]
    fn escapes_and_links() {
        let (m, _) = render_content("hi & <you> see https://example.com/x and nostr:note1abc");
        assert!(m.contains("&amp; &lt;you&gt;"));
        assert!(m.contains(r#"<a href="https://example.com/x">"#));
        assert!(m.contains(r#"<a href="nostr:note1abc">↗ note</a>"#));
    }

    #[test]
    fn images_move_out_of_text() {
        let (m, imgs) = render_content("look https://x.test/pic.png end");
        assert!(!m.contains(r#"<a href="https://x.test/pic.png">"#));
        assert_eq!(imgs, vec!["https://x.test/pic.png".to_string()]);
    }
}
