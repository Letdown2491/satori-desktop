//! NIP-88 polls — tag parsing slice of `src/data/polls.ts` +
//! `src/render/poll.ts`.

use nostr::prelude::*;

pub struct PollData {
    /// `(option_id, label)` in event order.
    pub options: Vec<(String, String)>,
    /// Multiple selections allowed (`polltype: multiplechoice`).
    pub multiple: bool,
    /// `endsAt` timestamp, if a voting window was set.
    pub ends_at: Option<u64>,
}

pub fn parse(ev: &Event) -> Option<PollData> {
    let mut options = Vec::new();
    let mut multiple = false;
    let mut ends_at = None;
    // Tag names per the publisher (manager.rs post_poll) and the TS
    // reference (nip88.ts parsePollOptions/parsePollType/parseEndsAt):
    // `option`, `polltype`, `endsAt`. (The first cut read "poll-option" /
    // "poll-multiple" / "poll-end" — tags nothing ever wrote, so satori's
    // own polls fell back to note cards in its own feed.)
    for tag in ev.tags.clone().to_vec() {
        let parts = tag.as_slice();
        match parts.first().map(String::as_str) {
            Some("option") if parts.len() >= 3 => {
                options.push((parts[1].clone(), parts[2..].join(" ")));
            }
            Some("polltype") => {
                multiple = parts.get(1).map(String::as_str) == Some("multiplechoice");
            }
            Some("endsAt") => {
                ends_at = parts.get(1).and_then(|v| v.parse().ok());
            }
            _ => {}
        }
    }
    (!options.is_empty()).then_some(PollData {
        options,
        multiple,
        ends_at,
    })
}

/// Tally kind:1018 votes: (response option id) → count, over one voter's
/// latest ballot (re-votes replace — last event per voter wins).
pub fn tally(votes: &[Event]) -> Vec<(String, u32)> {
    let mut latest: std::collections::HashMap<String, &Event> = std::collections::HashMap::new();
    for ev in votes {
        latest.insert(ev.pubkey.to_string(), ev);
    }
    let mut counts: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for ev in latest.values() {
        for tag in ev.tags.clone().to_vec() {
            let parts = tag.as_slice();
            if parts.first().map(String::as_str) == Some("response")
                && let Some(id) = parts.get(1)
            {
                *counts.entry(id.clone()).or_insert(0) += 1;
            }
        }
    }
    let mut out: Vec<(String, u32)> = counts.into_iter().collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A poll shaped exactly as post_poll signs it (manager.rs): `option`
    /// tags with random ids, a `polltype`, an absolute `endsAt`.
    fn poll_event(multi: bool) -> Event {
        let keys = Keys::generate();
        let unsigned = UnsignedEvent::new(
            keys.public_key(),
            Timestamp::from_secs(1),
            Kind::Custom(1068),
            vec![
                Tag::custom("option", ["opta1b2c3d".to_string(), "yes".to_string()]),
                Tag::custom("option", ["opte4f5g6h".to_string(), "no".to_string()]),
                Tag::custom(
                    "polltype",
                    [if multi {
                        "multiplechoice"
                    } else {
                        "singlechoice"
                    }
                    .to_string()],
                ),
                Tag::custom("endsAt", ["1770000000".to_string()]),
            ],
            "yes or no",
        );
        keys.sign_event(unsigned).unwrap()
    }

    #[test]
    fn parse_reads_the_publisher_tags() {
        let single = parse(&poll_event(false)).expect("a tagged poll parses");
        assert_eq!(single.options.len(), 2);
        assert_eq!(single.options[0].1, "yes");
        assert!(!single.multiple);
        assert_eq!(single.ends_at, Some(1_770_000_000));

        let multi = parse(&poll_event(true)).expect("a tagged poll parses");
        assert!(multi.multiple);
    }

    #[test]
    fn an_optionless_poll_is_not_a_poll() {
        let keys = Keys::generate();
        let unsigned = UnsignedEvent::new(
            keys.public_key(),
            Timestamp::from_secs(1),
            Kind::Custom(1068),
            Vec::<Tag>::new(),
            "not really a poll",
        );
        let ev = keys.sign_event(unsigned).unwrap();
        assert!(parse(&ev).is_none());
    }
}
