//! NIP-99 classified listings — tag parsing slice of
//! `src/manifest/classified.ts`.

#[derive(Clone)]
pub struct ClassifiedData {
    pub title: String,
    /// Raw `price` tag: "amount currency" (NIP-99).
    pub price: Option<String>,
    pub condition: Option<String>,
    pub images: Vec<String>,
    pub location: Option<String>,
    /// "active" | "sold".
    pub status: Option<String>,
    pub summary: String,
    pub published_at: Option<u64>,
}

use nostr::prelude::*;

pub fn parse(ev: &Event) -> Option<ClassifiedData> {
    let mut d = ClassifiedData {
        title: String::new(),
        price: None,
        condition: None,
        images: Vec::new(),
        location: None,
        status: None,
        summary: ev.content.clone(),
        published_at: None,
    };
    for tag in ev.tags.clone().to_vec() {
        let parts = tag.as_slice();
        match parts.first().map(String::as_str) {
            Some("title") if parts.len() >= 2 => d.title = parts[1..].join(" "),
            Some("price") if parts.len() >= 2 => d.price = Some(parts[1..].join(" ")),
            Some("condition") if parts.len() >= 2 => d.condition = Some(parts[1].clone()),
            Some("image") if parts.len() >= 2 => d.images.push(parts[1].clone()),
            Some("location") if parts.len() >= 2 => d.location = Some(parts[1..].join(" ")),
            Some("status") if parts.len() >= 2 => d.status = Some(parts[1].clone()),
            Some("published_at") if parts.len() >= 2 => {
                d.published_at = parts.get(1).and_then(|v| v.parse().ok())
            }
            _ => {}
        }
    }
    (!d.title.is_empty()).then_some(d)
}
