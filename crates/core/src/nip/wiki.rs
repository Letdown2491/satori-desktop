//! NIP-54 wikis — tag parsing slice of `src/manifest/wiki.ts` +
//! `src/nostr/asciidoc.ts` (subset).

#[derive(Clone)]
pub struct WikiData {
    pub title: String,
    /// `d` = topic slug.
    pub topic: String,
    pub summary: Option<String>,
    pub published_at: Option<u64>,
    /// AsciiDoc body (event content).
    pub content: String,
}

use nostr::prelude::*;

pub fn parse(ev: &Event) -> Option<WikiData> {
    let mut d = WikiData {
        title: String::new(),
        topic: String::new(),
        summary: None,
        published_at: None,
        content: ev.content.clone(),
    };
    for tag in ev.tags.clone().to_vec() {
        let parts = tag.as_slice();
        match parts.first().map(String::as_str) {
            Some("title") if parts.len() >= 2 => d.title = parts[1..].join(" "),
            Some("d") if parts.len() >= 2 => d.topic = parts[1].clone(),
            Some("summary") if parts.len() >= 2 => d.summary = Some(parts[1..].join(" ")),
            Some("published_at") if parts.len() >= 2 => {
                d.published_at = parts.get(1).and_then(|v| v.parse().ok())
            }
            _ => {}
        }
    }
    (!(d.title.is_empty() && d.topic.is_empty())).then_some(d)
}
