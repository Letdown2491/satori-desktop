//! NIP-84 highlights — tag parsing slice of `src/manifest/highlight.ts`.

#[derive(Clone)]
pub struct HighlightData {
    /// The quoted text (event content).
    pub content: String,
    /// Surrounding context, if the highlighter included it.
    pub context: Option<String>,
    /// Source URL.
    pub source: Option<String>,
    /// Attributed author (npub or display name via `author` tag).
    pub author: Option<String>,
}

use nostr::prelude::*;

pub fn parse(ev: &Event) -> HighlightData {
    let mut d = HighlightData {
        content: ev.content.clone(),
        context: None,
        source: None,
        author: None,
    };
    for tag in ev.tags.clone().to_vec() {
        let parts = tag.as_slice();
        match parts.first().map(String::as_str) {
            Some("context") if parts.len() >= 2 => d.context = Some(parts[1..].join(" ")),
            Some("source") if parts.len() >= 2 => d.source = Some(parts[1].clone()),
            Some("author") if parts.len() >= 2 => d.author = Some(parts[1..].join(" ")),
            _ => {}
        }
    }
    d
}
