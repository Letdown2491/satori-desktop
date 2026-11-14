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
//! no env access), so this follows the D33 sidecar discipline seen.rs /
//! deletions.rs / hints.rs share: a bounded JSON state file —
//! OnceLock<Mutex<…>>, temp-sibling + rename at 0600, park_timeout
//! flusher, LRU cap — and only non-indexer, routable relays recorded
//! (indexers are asked on every fetch anyway; a persisted hint must
//! never teach the client a loopback/LAN relay).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use nostr::prelude::*;

use crate::config;
use crate::outbox;

/// Relays kept per event (recency-ordered): the deletion check needs a
/// couple of witnesses, not a census.
const MAX_PER_EVENT: usize = 3;
/// LRU cap so the map (and its file) can't grow without bound: ~2MB of
/// 64-char hex ids + short urls.
const MAX_EVENTS: usize = 10_000;
/// Flush cadence (the D33 discipline's 10s).
const FLUSH_SECS: u64 = 10;

/// event id hex → relay urls, recency-ordered (most recent LAST); the
/// order vec is the event LRU.
struct EventSeen {
    map: HashMap<String, Vec<String>>,
    order: VecDeque<String>,
}

static EVENT_SEEN: OnceLock<Mutex<EventSeen>> = OnceLock::new();
static DIRTY: AtomicBool = AtomicBool::new(false);

fn seen_event() -> &'static Mutex<EventSeen> {
    EVENT_SEEN.get_or_init(|| {
        let mut s = EventSeen {
            map: HashMap::new(),
            order: VecDeque::new(),
        };
        for (id, relays) in read_disk() {
            let tail: Vec<String> = relays
                .iter()
                .rev()
                .take(MAX_PER_EVENT)
                .rev()
                .cloned()
                .collect();
            if !tail.is_empty() {
                s.map.insert(id.clone(), tail);
                s.order.push_back(id);
            }
        }
        while s.order.len() > MAX_EVENTS {
            let old = s.order.pop_front();
            if let Some(old) = old {
                s.map.remove(&old);
            }
        }
        spawn_flusher();
        Mutex::new(s)
    })
}

fn read_disk() -> Vec<(String, Vec<String>)> {
    let Ok(raw) = std::fs::read_to_string(config::state_file(config::files::SEEN_EVENT)) else {
        return Vec::new();
    };
    serde_json::from_str::<HashMap<String, Vec<String>>>(&raw)
        .map(|m| m.into_iter().collect())
        .unwrap_or_default()
}

fn write_disk() {
    let held = seen_event().lock().unwrap();
    let mut out = serde_json::Map::new();
    for (id, relays) in &held.map {
        out.insert(id.clone(), serde_json::to_value(relays).unwrap_or_default());
    }
    drop(held);
    let path = config::state_file(config::files::SEEN_EVENT);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let tmp = path.with_extension(format!("tmp-{}-{nanos}", std::process::id()));
    let body = serde_json::Value::Object(out).to_string();
    use std::os::unix::fs::OpenOptionsExt;
    let Ok(mut f) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
    else {
        return;
    };
    use std::io::Write as _;
    if f.write_all(body.as_bytes()).is_ok() {
        drop(f);
        let _ = std::fs::rename(&tmp, &path);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

fn spawn_flusher() {
    std::thread::Builder::new()
        .name("seen-event-flush".into())
        .spawn(|| {
            loop {
                std::thread::park_timeout(std::time::Duration::from_secs(FLUSH_SECS));
                if DIRTY.swap(false, Ordering::Relaxed) {
                    write_disk();
                }
            }
        })
        .ok();
}

/// Record that `id` arrived from `relay_url` (the chokepoints: pool
/// fetch_route / get_from and the live drains — every site that already
/// records the author-keyed seen-on with attribution in hand). Indexers
/// and non-routable urls never record (the seen.rs screen, same reason).
pub fn record(id: &EventId, relay_url: &str) {
    let relay = outbox::relay_key(relay_url);
    if !outbox::routable_relay(&relay)
        || crate::pool::INDEXERS
            .iter()
            .any(|i| outbox::relay_key(i) == relay)
    {
        return;
    }
    let ekey = id.to_string();
    let mut held = seen_event().lock().unwrap();
    // Re-insert the event most-recently-used.
    held.order.retain(|k| k != &ekey);
    let set = held.map.entry(ekey.clone()).or_default();
    set.retain(|x| x != &relay);
    set.push(relay); // most recent last
    while set.len() > MAX_PER_EVENT {
        set.remove(0); // drop oldest relay
    }
    held.order.push_back(ekey);
    while held.order.len() > MAX_EVENTS {
        let old = held.order.pop_front();
        if let Some(old) = old {
            held.map.remove(&old);
        }
    }
    drop(held);
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
