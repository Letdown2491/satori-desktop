//! NIP-23 long-form articles — tag parsing slice of `src/nostr/content.ts` +
//! `src/render/note.ts` articleRow/reader.

use nostr::prelude::*;

#[derive(Clone)]
pub struct ArticleData {
    pub title: String,
    pub summary: String,
    pub image: Option<String>,
    pub published_at: Option<u64>,
    pub slug: String,
}

pub fn parse(ev: &Event) -> Option<ArticleData> {
    let mut d = ArticleData {
        title: String::new(),
        summary: String::new(),
        image: None,
        published_at: None,
        slug: String::new(),
    };
    for tag in ev.tags.clone().to_vec() {
        let parts = tag.as_slice();
        match parts.first().map(String::as_str) {
            Some("title") if parts.len() >= 2 => d.title = parts[1..].join(" "),
            Some("summary") if parts.len() >= 2 => d.summary = parts[1..].join(" "),
            Some("image") if parts.len() >= 2 => d.image = Some(parts[1].clone()),
            Some("published_at") if parts.len() >= 2 => {
                d.published_at = parts.get(1).and_then(|v| v.parse().ok())
            }
            Some("d") if parts.len() >= 2 => d.slug = parts[1].clone(),
            _ => {}
        }
    }
    (!d.title.is_empty()).then_some(d)
}
