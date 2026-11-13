//! Publish routing (D59, the NIP-65 spine item — phase 1): replies reach
//! who they're for.
//!
//! Until now every publish targeted only the session's own write relays
//! (`outbox_queue::targets`): a reply to an author who shares no relay
//! with us landed on OUR relays, their client read THEIR relays, and the
//! reply vanished for exactly the person it was addressed to — silent for
//! both sides. Phase 1 routes audience-shaped events (replies, comments,
//! reactions, reposts) to, modeled on Amethyst's BroadcastRelayPlanner:
//!
//! own write relays ∪ the parent + tagged authors' INBOX relays (their
//! kind-10002 read sets, already in the LMDB cache) ∪ where the parent
//! author was recently SEEN (`seen.rs` / `hints.rs`, author-keyed).
//!
//! Phase-1 discipline — data already in hand, no publish-time network
//! fetch (a like must not wait on a relay round trip):
//! - inbox read sets come from the STORE cache only; an author whose
//!   kind-10002 isn't cached still gets the seen/hints coverage, which is
//!   exactly where their conversation lives.
//! - seen-on is author-keyed (the parent author's recent relays); the
//!   per-event provenance sidecar stays on #32 as its own schema change.
//! - every foreign relay rides `routable_relay` and the
//!   indexers are never publish targets; the total set is capped. Note
//!   the screen's honest reach (D61's watch item): for IP literals it is
//!   a full SSRF screen, for DOMAINS it is a string-level screen (the
//!   dial-time TOCTOU on resolution is D58's residual, WP2's resolved-IP
//!   pinning).
//!
//! Routing derives from the signed event's OWN tags (p-tag authors, e/q
//! presence), so the outbox drainer and the scheduled sweep route RETRIES
//! identically with no queue schema change. Personal kinds (bookmarks'
//! 10003/30001 lists — their e-tags are the bookmark SET, not a parent)
//! and everything without a parent stay own-write-only; deletions'
//! seen-on, profiles-everywhere and the personal-kind opt-out sweep are
//! phase 2.

use nostr::prelude::*;

use crate::pool::Pool;

/// The total publish target ceiling (own + foreign). Amethyst runs
/// uncapped; a desktop reader shouldn't grow its socket count without
/// bound per publish. Own relays come first and survive the cap.
const MAX_TOTAL_TARGETS: usize = 16;

/// The compute step, dropped into `Active::commit`, the outbox drainer
/// and the scheduled sweep (the three `outbox_queue::targets` call
/// sites). Cheap on the hot path: one LMDB query, two map reads.
pub(crate) async fn targets(pool: &Pool, write_relays: &[String], event: &Event) -> Vec<String> {
    let base = crate::outbox_queue::targets(write_relays);
    if !audience_event(event) {
        return base;
    }
    let authors = tagged_authors(event);
    let Some(parent_author) = authors.first().copied() else {
        return base;
    };
    // The authors' inbox read sets, LMDB-only (store-first; a cache miss
    // here stays a miss this publish — seen/hints still cover it).
    let mut inboxes: Vec<String> = Vec::new();
    if !authors.is_empty()
        && let Ok(lists) = pool.store.relay_lists(&authors).await
    {
        for ev in &lists {
            inboxes.extend(crate::outbox::parse_relay_list(ev).read);
        }
    }
    // Where the conversation's author was recently seen (the author-keyed
    // provenance in hand; the most recent first — seen's newest last, so
    // reverse before merge to prefer recency under the cap).
    let mut seen = crate::seen::for_pubkey(&parent_author);
    seen.reverse();
    seen.extend(crate::hints::for_pubkey(&parent_author));
    merge(base, inboxes, seen)
}

/// An event with an audience beyond our own followers: the reply-shaped
/// engagement kinds. Kind 1 counts only as a reply (an e-tag present) —
/// a top-level note's mentions are phase 2.
fn audience_event(ev: &Event) -> bool {
    match u16::from(ev.kind) {
        1 => ev
            .tags
            .iter()
            .any(|t| t.as_slice().first().map(String::as_str) == Some("e")),
        6 | 7 | 1111 => true,
        _ => false,
    }
}

/// The p-tagged authors in routing order: NIP-10 writes the PARENT author
/// first (then the root, then mentions), so `first` is who the event is
/// for. Self is skipped — our own relays are already the base set.
fn tagged_authors(ev: &Event) -> Vec<PublicKey> {
    let mut out: Vec<PublicKey> = Vec::new();
    for t in ev.tags.iter() {
        let tag = t.as_slice();
        if tag.first().map(String::as_str) != Some("p") {
            continue;
        }
        if let Some(pk) = tag.get(1).and_then(|s| PublicKey::parse(s).ok())
            && pk != ev.pubkey
            && !out.contains(&pk)
        {
            out.push(pk);
        }
    }
    out
}

/// The merge: base (already normalized, write-dead filtered with the
/// revive rule) first, then inboxes, then seen/hints — deduped by relay
/// key, screened (routable, non-indexer, write-alive), capped. Extras
/// fill in even when the own-write set is empty: a reply reaching the
/// parent's inbox despite a broken write list is exactly the point.
fn merge(base: Vec<String>, inboxes: Vec<String>, seen: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut acc = Vec::new();
    acc.extend(base);
    acc.extend(inboxes);
    acc.extend(seen);
    for u in acc {
        let k = crate::outbox::relay_key(&u);
        if k.is_empty()
            || out.contains(&k)
            || crate::pool::INDEXERS
                .iter()
                .any(|i| crate::outbox::relay_key(i) == k)
            || !crate::net_guard::routable_relay(&k)
            || crate::relay_latency::is_write_dead(&k)
        {
            continue;
        }
        out.push(k);
        if out.len() >= MAX_TOTAL_TARGETS {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> Keys {
        Keys::generate()
    }

    fn reply(keys: &Keys, parent: &Event, mention: Option<&PublicKey>) -> Event {
        let mut t = Tags::new();
        t.push(Tag::event(parent.id));
        t.push(Tag::public_key(parent.pubkey));
        if let Some(m) = mention {
            t.push(Tag::public_key(*m));
        }
        keys.sign_event(UnsignedEvent::new(
            keys.public_key(),
            Timestamp::now(),
            Kind::TextNote,
            t,
            "a reply body".to_string(),
        ))
        .unwrap()
    }

    #[test]
    fn audience_kinds() {
        let k = keys();
        let root = note(&k, None);
        // A reply (e-tag) routes; a top-level note doesn't.
        assert!(audience_event(&reply(&k, &root, None)));
        assert!(!audience_event(&note(&k, None)));
        // Reactions, reposts and comments route unconditionally.
        for kind in [Kind::Reaction, Kind::Repost, Kind::Custom(1111)] {
            let ev = k
                .sign_event(UnsignedEvent::new(
                    k.public_key(),
                    Timestamp::now(),
                    kind,
                    Tags::new(),
                    String::new(),
                ))
                .unwrap();
            assert!(audience_event(&ev), "kind {kind:?} routes");
        }
        // A bookmark list's e-tags are the SET, not a parent — never routed.
        let bm = k
            .sign_event(UnsignedEvent::new(
                k.public_key(),
                Timestamp::now(),
                Kind::Custom(10003),
                root.tags,
                String::new(),
            ))
            .unwrap();
        assert!(!audience_event(&bm));
    }

    fn note(keys: &Keys, parent: Option<&Event>) -> Event {
        let mut t = Tags::new();
        if let Some(p) = parent {
            t.push(Tag::event(p.id));
        }
        keys.sign_event(UnsignedEvent::new(
            keys.public_key(),
            Timestamp::now(),
            Kind::TextNote,
            t,
            "a note body".to_string(),
        ))
        .unwrap()
    }

    #[test]
    fn tagged_authors_parent_first_self_skipped() {
        let me = keys();
        let parent = note(&keys(), None);
        let other = keys().public_key();
        let ev = reply(&me, &parent, Some(&other));
        let authors = tagged_authors(&ev);
        assert_eq!(authors[0], parent.pubkey, "NIP-10: parent author first");
        assert!(authors.contains(&other), "mentions ride too");
        assert!(!authors.contains(&me.public_key()), "self skipped");
    }

    #[test]
    fn merge_prefers_base_then_inbox_then_seen_capped() {
        let base = vec!["wss://mine.example/".to_string()]; // normalized below
        let inboxes = vec![
            "wss://inbox.example".to_string(),
            "ws://insecure.example".to_string(), // dropped: not wss
            "wss://relay.damus.io".to_string(),  // dropped: indexer
            "wss://mine.example".to_string(),    // dropped: dup of base
        ];
        let seen = vec![
            "wss://seen.example".to_string(),
            "wss://dead.example".to_string(), // dropped: write-dead below
            "wss://inbox.example".to_string(), // dropped: dup
        ];
        crate::relay_latency::record_write_result("wss://dead.example", false);
        crate::relay_latency::record_write_result("wss://dead.example", false);
        crate::relay_latency::record_write_result("wss://dead.example", false);
        crate::relay_latency::record_write_result("wss://dead.example", false);
        crate::relay_latency::record_write_result("wss://dead.example", false);
        let out = merge(base, inboxes, seen);
        assert_eq!(
            out,
            vec![
                "wss://mine.example",
                "wss://inbox.example",
                "wss://seen.example"
            ]
        );
    }

    #[test]
    fn merge_caps_at_max_total() {
        let base = vec!["wss://mine.example".to_string()];
        let inboxes: Vec<String> = (0..40).map(|i| format!("wss://r{i}.example")).collect();
        let out = merge(base, inboxes, vec![]);
        assert_eq!(out.len(), MAX_TOTAL_TARGETS);
        assert_eq!(out[0], "wss://mine.example", "own relays survive the cap");
    }

    /// Fixture: a kind-10002 the way real clients publish it (Amethyst's
    /// writer emits r=read / r=write / bare-r=both). The INBOX is the read
    /// side only — a write-only relay must not become a publish target.
    #[test]
    fn nip65_inbox_is_the_read_side() {
        let author = keys();
        let mut t = Tags::new();
        t.push(Tag::custom("r", ["wss://nostr.mom", "read"]));
        t.push(Tag::custom("r", ["wss://inbox.example", "read"]));
        t.push(Tag::custom("r", ["wss://outbox.example", "write"]));
        t.push(Tag::custom("r", ["wss://both.example"]));
        let ev = author
            .sign_event(UnsignedEvent::new(
                author.public_key(),
                Timestamp::now(),
                Kind::RelayList,
                t,
                String::new(),
            ))
            .unwrap();
        let list = crate::outbox::parse_relay_list(&ev);
        // A bare r (no marker) is BOTH per NIP-65 — read includes it.
        let mut write = list.write.clone();
        write.sort();
        assert_eq!(write, vec!["wss://both.example", "wss://outbox.example"]);
        let mut read = list.read.clone();
        read.sort();
        assert_eq!(
            read,
            vec![
                "wss://both.example",
                "wss://inbox.example",
                "wss://nostr.mom"
            ]
        );
        // The merge uses only the read side as publish targets.
        let out = merge(vec!["wss://mine.example".to_string()], list.read, vec![]);
        for inbox in [
            "wss://nostr.mom",
            "wss://inbox.example",
            "wss://both.example",
        ] {
            assert!(out.contains(&inbox.to_string()), "{inbox} is an inbox");
        }
        assert!(!out.contains(&"wss://outbox.example".to_string()));
    }
}
