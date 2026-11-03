//! NIP-40 expiration, READ side — port of `src/nostr/nip40.ts`: clients
//! SHOULD ignore events whose `expiration` (unix seconds) has passed.
//! Enforced at the pool — the one door every network read walks through —
//! plus the store-paint re-checks where a long-lived cache could serve an
//! event that expired mid-retention (the SDK persists everything it
//! receives, expired copies included). The write side already emits the
//! tag where the spec calls for it (the NIP-96 upload's blob expiry).

use nostr::prelude::*;

/// True when the event carries a valid `expiration` tag in the past. A
/// malformed timestamp does not expire the event (the spec ties the
/// behavior to the timestamp, not the tag). Only the tag's FIRST value
/// is consulted (nip40.ts tag1) — the tags are signature-covered, so a
/// relay can't smuggle a parseable timestamp into a later slot.
pub fn is_expired(ev: &Event) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    ev.tags.iter().any(|t| {
        let s = t.as_slice();
        s.first().is_some_and(|k| k == "expiration")
            && s.get(1)
                .and_then(|v| v.parse::<u64>().ok())
                .is_some_and(|t| t > 0 && t <= now)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev_with_tags(tags: Vec<Tag>) -> Event {
        let keys = Keys::generate();
        keys.sign_event(UnsignedEvent::new(
            keys.public_key(),
            Timestamp::now(),
            Kind::TextNote,
            tags,
            "body",
        ))
        .unwrap()
    }

    #[test]
    fn expiry_semantics() {
        // No tag: never expires.
        assert!(!is_expired(&ev_with_tags(vec![])));
        // Future stamp: not expired.
        let future = Tag::custom("expiration", [(now_secs() + 3600).to_string()]);
        assert!(!is_expired(&ev_with_tags(vec![future])));
        // Past stamp: expired.
        let past = Tag::custom("expiration", [now_secs().saturating_sub(1).to_string()]);
        assert!(is_expired(&ev_with_tags(vec![past])));
        // Malformed / zero: the spec ties behavior to the timestamp —
        // garbage and zero never expire.
        let zero = Tag::custom("expiration", ["0".to_string()]);
        assert!(!is_expired(&ev_with_tags(vec![zero])));
        let junk = Tag::custom("expiration", ["tomorrow".to_string()]);
        assert!(!is_expired(&ev_with_tags(vec![junk])));
        // tag1 semantics (nip40.ts): only the FIRST value is consulted —
        // a parseable later slot does not expire (tags are
        // signature-covered, so this shape is author-chosen, not
        // relay-injectable).
        let trailing = Tag::custom("expiration", ["nope", &(now_secs() - 1).to_string()]);
        assert!(!is_expired(&ev_with_tags(vec![trailing])));
        // The 0 prefix is honored (no leading-zero games), and an event
        // that merely CONTAINS the word is untouched.
        let other = Tag::custom("client", ["expiration".to_string()]);
        assert!(!is_expired(&ev_with_tags(vec![other])));
    }

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}
