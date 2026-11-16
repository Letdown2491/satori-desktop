//! NIP-19 entity encoding — the shareable bech32 id of an event.
//!
//! The copy affordance on a note's timestamp (and anywhere else a note id
//! is worth handing off) needs the entity OTHER clients can open: nevent1
//! for regular events, naddr1 for addressable ones (the d-tag is part of
//! the coordinate), with the seen-on relays and the author riding along
//! as fetch hints (NIP-19 §Encrypted Payloads — receivers use them to
//! fetch the note without a full blast).

use nostr::prelude::*;

use crate::seen_event;

/// The note's NIP-21 URI (`nostr:nevent1…` / `nostr:naddr1…`) — the form
/// a paste lands as a clickable reference everywhere.
pub fn uri(ev: &Event) -> String {
    format!("nostr:{}", entity(ev))
}

/// The bare bech32 entity for `ev`: naddr for addressable kinds (the
/// coordinate is the identity — the d-tag, not the id), nevent otherwise.
/// Fallback to the raw hex if bech32 ever fails (it can't in practice;
/// the honest string beats a panic on the render path).
pub fn entity(ev: &Event) -> String {
    let relays: Vec<RelayUrl> = seen_event::for_event(&ev.id)
        .iter()
        .filter_map(|r| r.parse().ok())
        .collect();
    let bech = if ev.kind.is_addressable() || ev.kind.is_replaceable() {
        Nip19::Coordinate(Nip19Coordinate {
            coordinate: Coordinate {
                kind: ev.kind,
                public_key: ev.pubkey,
                identifier: ev.tags.identifier().unwrap_or_default(),
            },
            relays,
        })
        .to_bech32()
    } else {
        Nip19::Event(Nip19Event {
            event_id: ev.id,
            author: Some(ev.pubkey),
            kind: Some(ev.kind),
            relays,
        })
        .to_bech32()
    };
    bech.unwrap_or_else(|_| ev.id.to_string())
}

/// The pubkeys @-mentioned in `content`: the inline bech32 entities
/// (`nostr:npub1…`, `npub1…`, `nprofile1…`) a note body carries. This is
/// the HYDRATION-side scan — the page builders run it so mention names
/// ship with the payload and warm in the background (content.ts
/// mentionPubkeys); the app's `content.rs` owns the render-side token
/// classification and must agree with it.
///
/// Deliberately whitespace-token-shaped, like the renderer: punctuation
/// hugging a mention ("(see nostr:npub1…).") splits off, a token that is
/// anything but a pubkey entity (a link, a nevent) returns nothing.
pub fn mention_pks(content: &str) -> Vec<PublicKey> {
    let mut out: Vec<PublicKey> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for token in content.split_whitespace() {
        // The renderer's punctuation split ("(see nostr:npub1…)." keeps
        // the entity): trim the same trailing set.
        let core = token.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}', '"']);
        let bech = core.strip_prefix("nostr:").unwrap_or(core);
        let Ok(entity) = Nip19::from_bech32(bech) else {
            continue;
        };
        let pk = match entity {
            Nip19::Pubkey(pk) => pk,
            Nip19::Profile(p) => p.public_key,
            _ => continue,
        };
        if seen.insert(pk) {
            out.push(pk);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::EnvGuard;

    fn note() -> Event {
        let keys = Keys::generate();
        EventBuilder::new(Kind::TextNote, "the body")
            .finalize(&keys)
            .unwrap()
    }

    #[test]
    fn regular_note_encodes_as_nevent() {
        let _env = EnvGuard::new("nip19-nevent");
        let ev = note();
        let u = uri(&ev);
        assert!(u.starts_with("nostr:nevent1"), "{u}");
        // Round-trip: the entity decodes back to the same event id + author.
        let Nip19::Event(e) = Nip19::from_bech32(u.strip_prefix("nostr:").unwrap()).unwrap() else {
            panic!("not an nevent");
        };
        assert_eq!(e.event_id, ev.id);
        assert_eq!(e.author, Some(ev.pubkey));
    }

    #[test]
    fn article_encodes_as_naddr() {
        let _env = EnvGuard::new("nip19-naddr");
        let keys = Keys::generate();
        let ev = EventBuilder::new(Kind::LongFormTextNote, "the article")
            .tag(Tag::identifier("the-slug"))
            .finalize(&keys)
            .unwrap();
        let u = uri(&ev);
        assert!(u.starts_with("nostr:naddr1"), "{u}");
        let Nip19::Coordinate(c) = Nip19::from_bech32(u.strip_prefix("nostr:").unwrap()).unwrap()
        else {
            panic!("not an naddr");
        };
        assert_eq!(c.coordinate.identifier, "the-slug");
        assert_eq!(c.coordinate.public_key, ev.pubkey);
    }

    #[test]
    fn mention_scan_finds_pubkeys_only() {
        let alice = Keys::generate();
        let bob = Keys::generate();
        let content = format!(
            "hi {} and nostr:{}! (also {}) plus https://x.example/npub1decoy and {}",
            alice.public_key().to_bech32().unwrap(),
            bob.public_key().to_bech32().unwrap(),
            bob.public_key().to_bech32().unwrap(),
            "nevent1qqst8cujky046negxgwwm5yn3wn3lfv8xy42wyhn6pq9y4nP0ar3su2sp6dcxuz",
        );
        let pks = mention_pks(&content);
        assert_eq!(pks, vec![alice.public_key(), bob.public_key()]);
        // Punctuation hugs, `nostr:` prefixes and bare npubs all resolve;
        // links, nevents and plain words yield nothing.
        assert!(mention_pks("no mentions here").is_empty());
        assert!(mention_pks("a link https://x.example/a and a word").is_empty());
    }
}
