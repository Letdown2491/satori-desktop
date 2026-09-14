//! NIP-F4 podcast episodes — tag parsing slice of `src/manifest/podcast.ts`.

#[derive(Clone)]
pub struct PodcastData {
    pub title: String,
    pub image: Option<String>,
    /// Audio enclosure.
    pub url: Option<String>,
    pub published_at: Option<u64>,
    /// Show notes (event content).
    pub notes: String,
}

use nostr::prelude::*;

pub fn parse(ev: &Event) -> Option<PodcastData> {
    let mut d = PodcastData {
        title: String::new(),
        image: None,
        url: None,
        published_at: None,
        notes: ev.content.clone(),
    };
    for tag in ev.tags.clone().to_vec() {
        let parts = tag.as_slice();
        match parts.first().map(String::as_str) {
            Some("title") if parts.len() >= 2 => d.title = parts[1..].join(" "),
            Some("image") if parts.len() >= 2 => d.image = Some(parts[1].clone()),
            // NIP-F4 audio lives in `url` (fallback `source`/`enclosure`).
            Some("url") | Some("source") | Some("enclosure") if d.url.is_none() => {
                d.url = parts.get(1).cloned()
            }
            Some("published_at") if parts.len() >= 2 => {
                d.published_at = parts.get(1).and_then(|v| v.parse().ok())
            }
            _ => {}
        }
    }
    (!d.title.is_empty() || d.url.is_some()).then_some(d)
}
