//! NIP-88 polls — tag parsing slice of `src/data/polls.ts` +
//! `src/render/poll.ts`.

use nostr::prelude::*;

pub struct PollData {
    /// `(option_id, label)` in event order.
    pub options: Vec<(String, String)>,
    /// Multiple selections allowed (`poll-multiple` present).
    pub multiple: bool,
    /// `poll-end` timestamp, if closed/voting window set.
    pub ends_at: Option<u64>,
}

pub fn parse(ev: &Event) -> Option<PollData> {
    let mut options = Vec::new();
    let mut multiple = false;
    let mut ends_at = None;
    for tag in ev.tags.clone().to_vec() {
        let parts = tag.as_slice();
        match parts.first().map(String::as_str) {
            Some("poll-option") if parts.len() >= 3 => {
                options.push((parts[1].clone(), parts[2..].join(" ")));
            }
            Some("poll-multiple") => multiple = true,
            Some("poll-end") => {
                ends_at = parts.get(1).and_then(|v| v.parse().ok());
            }
            _ => {}
        }
    }
    (!options.is_empty()).then_some(PollData { options, multiple, ends_at })
}

/// Tally kind:1018 votes: (response option id) → count, over one voter's
/// latest ballot (re-votes replace — last event per voter wins).
pub fn tally(votes: &[Event]) -> Vec<(String, u32)> {
    let mut latest: std::collections::HashMap<String, &Event> =
        std::collections::HashMap::new();
    for ev in votes {
        latest.insert(ev.pubkey.to_string(), ev);
    }
    let mut counts: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for ev in latest.values() {
        for tag in ev.tags.clone().to_vec() {
            let parts = tag.as_slice();
            if parts.first().map(String::as_str) == Some("response")
                && let Some(id) = parts.get(1) {
                    *counts.entry(id.clone()).or_insert(0) += 1;
                }
        }
    }
    let mut out: Vec<(String, u32)> = counts.into_iter().collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}
