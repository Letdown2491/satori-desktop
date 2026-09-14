//! NIP-52 calendar events — tag parsing slice of `src/data/polls.ts` +
//! `src/render/calendar.ts`. RSVPs are kind:31925 with an `a` tag and a
//! `status` tag (accepted | tentative | declined).

use nostr::prelude::*;

pub struct CalendarData {
    pub title: String,
    pub start: u64,
    pub end: Option<u64>,
    pub location: Option<String>,
    pub description: String,
    /// `d` identifier — the addressable half of the RSVP `a` tag.
    pub d: String,
    /// 31922 (date) or 31923 (time).
    pub kind: u16,
}

pub fn parse(ev: &Event) -> Option<CalendarData> {
    let mut title = String::new();
    let mut start = None;
    let mut end = None;
    let mut location = None;
    let mut d = String::new();
    for tag in ev.tags.clone().to_vec() {
        let parts = tag.as_slice();
        match parts.first().map(String::as_str) {
            Some("title") if parts.len() >= 2 => title = parts[1..].join(" "),
            Some("start") if parts.len() >= 2 => start = parts.get(1).and_then(|v| v.parse().ok()),
            Some("end") if parts.len() >= 2 => end = parts.get(1).and_then(|v| v.parse().ok()),
            Some("location") if parts.len() >= 2 => location = Some(parts[1..].join(" ")),
            Some("d") if parts.len() >= 2 => d = parts[1].clone(),
            _ => {}
        }
    }
    Some(CalendarData {
        title,
        start: start?,
        end,
        location,
        description: ev.content.clone(),
        d,
        kind: u16::from(ev.kind),
    })
}

/// The `a` tag value addressing this event: `<kind>:<pubkey>:<d>`.
pub fn a_tag(data: &CalendarData, author: &PublicKey) -> String {
    format!("{}:{}:{}", data.kind, author, data.d)
}
