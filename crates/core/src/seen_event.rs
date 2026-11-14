//! Per-event seen-on provenance — the NIP-65 phase-2 sidecar (#32,
//! deferred from routing.rs phase 1): which relays each EVENT (not just
//! each author, that's `seen.rs`) actually arrived from. The same two
//! pool chokepoints + the two live-drain sites that feed the author-keyed
//! memory feed this one, so attribution stays exact (one relay per
//! query).
//!
//! Consumers (this PR): the deletion sweep asks the relays the ORIGINAL
//! was seen on for its kind-5 tombstone, instead of a blanket req to
//! every read relay; per-event relay hints ride the same data later.
//!
//! The store (nostr-lmdb) can't host sidecar tables (no migration hooks,
//! no env access), so this follows the D33 sidecar discipline on the
//! `sidecar` kernel: a bounded JSON state file — OnceLock<Mutex<…>>,
//! temp-sibling + rename at 0600, park_timeout flusher, LRU cap — and
//! only non-indexer, routable relays recorded. This sidecar's recency
//! policy is FIFO (the audit's hot-path fix): the consumer (the
//! deletion sweep's targeted ask) doesn't care WHICH capped events
//! stay, so the per-ingest O(cap) MRU scan is gone; the per-event
//! relay list still reorders.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use nostr::prelude::*;

use crate::config;
use crate::sidecar;

/// Relays kept per event (recency-ordered): the deletion check needs a
/// couple of witnesses, not a census.
const MAX_PER_EVENT: usize = 3;
/// LRU cap so the map (and its file) can't grow without bound: ~2MB of
/// 64-char hex ids + short urls.
const MAX_EVENTS: usize = 10_000;

static EVENT_SEEN: OnceLock<Mutex<sidecar::RelayLru>> = OnceLock::new();
static DIRTY: AtomicBool = AtomicBool::new(false);

fn seen_event() -> &'static Mutex<sidecar::RelayLru> {
    EVENT_SEEN.get_or_init(|| {
        let mut lru = sidecar::RelayLru::new();
        lru.hydrate(
            sidecar::read_relay_pairs(config::files::SEEN_EVENT),
            MAX_PER_EVENT,
            MAX_EVENTS,
        );
        sidecar::spawn_flusher("seen-event-flush", &DIRTY, write_disk);
        Mutex::new(lru)
    })
}

fn write_disk() {
    let held = seen_event().lock().unwrap();
    let body = sidecar::relay_json(held.map.clone());
    drop(held);
    sidecar::write_atomic(config::state_file(config::files::SEEN_EVENT), &body);
}

/// Record that `id` arrived from `relay_url` (the chokepoints: pool
/// fetch_route / get_from and the live drains — every site that already
/// records the author-keyed seen-on with attribution in hand). Indexers
/// and non-routable urls never record (the kernel's screen, same
/// reason as seen.rs).
pub fn record(id: &EventId, relay_url: &str) {
    let Some(relay) = sidecar::screen_relay(relay_url) else {
        return;
    };
    let ekey = id.to_string();
    {
        let mut held = seen_event().lock().unwrap();
        // FIFO key insert (audit): no MRU retain — the per-event O(cap)
        // scan under this mutex cost a page load millions of string
        // compares, and the consumer doesn't care WHICH capped events
        // stay. The per-event relay list still reorders (push_relay).
        held.touch_fifo(&ekey, MAX_EVENTS);
        held.push_relay(&ekey, relay, MAX_PER_EVENT);
    }
    DIRTY.store(true, Ordering::Relaxed);
}

/// The relays this event was actually received from, most recent last —
/// the targeted ask for its deletion tombstone (and later, per-event
/// fetch hints).
pub fn for_event(id: &EventId) -> Vec<String> {
    let held = seen_event().lock().unwrap();
    held.map.get(&id.to_string()).cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::EnvGuard;

    fn id(seed: u8) -> EventId {
        EventId::from_byte_array([seed; 32])
    }

    /// Indexers and non-routable urls never record; a real relay does.
    #[test]
    fn record_screens_the_urls() {
        let _env = EnvGuard::new("seen-event-screen");
        let e = id(1);
        record(&e, "ws://127.0.0.1:8080");
        record(&e, "wss://relay.damus.io/"); // an indexer: asked everywhere anyway
        record(&e, "wss://nos.lol/");
        record(&e, "wss://one.example");
        assert_eq!(for_event(&e), vec!["wss://nos.lol", "wss://one.example"]);
    }

    /// MAX_PER_EVENT: the 4th distinct relay evicts the oldest.
    #[test]
    fn per_event_cap() {
        let _env = EnvGuard::new("seen-event-cap");
        let e = id(2);
        for c in ['a', 'b', 'c', 'd'] {
            record(&e, &format!("wss://{c}.example"));
        }
        let relays = for_event(&e);
        assert_eq!(relays.len(), MAX_PER_EVENT);
        assert_eq!(relays[0], "wss://b.example"); // a dropped, oldest
        assert_eq!(relays[2], "wss://d.example"); // newest kept
    }

    /// Recency: a re-seen relay moves to the end.
    #[test]
    fn recency_moves_to_the_end() {
        let _env = EnvGuard::new("seen-event-recency");
        let e = id(3);
        record(&e, "wss://a.example");
        record(&e, "wss://b.example");
        record(&e, "wss://a.example");
        assert_eq!(for_event(&e), vec!["wss://b.example", "wss://a.example"]);
    }
}
