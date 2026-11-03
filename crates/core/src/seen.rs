//! Empirical seen-on relay memory — port of `src/data/seen-relays.ts` (the
//! READ side of the gossip/outbox model): we remember which relays we have
//! actually RECEIVED each author's events from, not just their DECLARED
//! NIP-65. This is how an event on an UNDECLARED relay becomes findable: a
//! publisher who posts to a relay they don't advertise is invisible to a
//! pure declared-outbox client — but once that author has been seen there
//! (e.g. riding the live bell), the memory folds the relay into future
//! single-event fetches for them.
//!
//! PERSISTED (debounced ~10s like the TS json-store): a restart would
//! otherwise forget a relay you taught it, and events there would stop
//! resolving until you visited again. Bounded so the file can't grow
//! without limit (MAX_PER_AUTHOR relays per author, MAX_AUTHORS LRU).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use nostr::prelude::*;

use crate::config;
use crate::outbox;

/// Keep the most-recently-seen relays per author (seen-relays.ts: a niche
/// relay + a few others).
const MAX_PER_AUTHOR: usize = 6;
/// LRU cap so the map (and its file) can't grow without bound.
const MAX_AUTHORS: usize = 8000;
/// Flush cadence (seen-relays.ts debouncedFlush: 10s).
const FLUSH_SECS: u64 = 10;

/// pubkey hex → relay urls, recency-ordered (most recent LAST, like the
/// TS Set's insertion order); the order vec is the author LRU.
struct Seen {
    map: HashMap<String, Vec<String>>,
    order: VecDeque<String>,
}

static SEEN: OnceLock<Mutex<Seen>> = OnceLock::new();
static DIRTY: AtomicBool = AtomicBool::new(false);

fn seen() -> &'static Mutex<Seen> {
    SEEN.get_or_init(|| {
        let mut s = Seen { map: HashMap::new(), order: VecDeque::new() };
        for (pk, relays) in read_disk() {
            // Hydrate honoring the per-author cap (seen-relays.ts:
            // slice(-MAX_PER_AUTHOR) keeps the newest tail).
            let tail: Vec<String> = relays.iter().rev().take(MAX_PER_AUTHOR).rev().cloned().collect();
            if !tail.is_empty() {
                s.map.insert(pk.clone(), tail);
                s.order.push_back(pk);
            }
        }
        // Global LRU cap on hydrate: drop OLDEST authors first.
        while s.order.len() > MAX_AUTHORS {
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
    let Ok(raw) = std::fs::read_to_string(config::state_file(config::files::SEEN_RELAYS)) else {
        return Vec::new();
    };
    serde_json::from_str::<HashMap<String, Vec<String>>>(&raw)
        .map(|m| m.into_iter().collect())
        .unwrap_or_default()
}

fn write_disk() {
    let held = seen().lock().unwrap();
    let mut out = serde_json::Map::new();
    for (pk, relays) in &held.map {
        out.insert(pk.clone(), serde_json::to_value(relays).unwrap_or_default());
    }
    drop(held);
    // Temp sibling + rename, 0600 — the state-file discipline (read_state,
    // secrets): never rewrite in place, never follow a planted link.
    let path = config::state_file(config::files::SEEN_RELAYS);
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

/// The flusher thread: park_timeout wakeups scanning the dirty flag — no
/// runtime dependency, no timer churn (a missed window just rides the next
/// wake; the data is a learned cache, at most FLUSH_SECS stale on exit).
fn spawn_flusher() {
    std::thread::Builder::new()
        .name("seen-flush".into())
        .spawn(|| loop {
            std::thread::park_timeout(std::time::Duration::from_secs(FLUSH_SECS));
            if DIRTY.swap(false, Ordering::Relaxed) {
                write_disk();
            }
        })
        .ok();
}

/// Record that `pk`'s events arrived from `relay_urls` (seen-relays.ts
/// recordSeen). Indexers are skipped: they're queried on every fetch
/// anyway, so recording them would only crowd out the interesting niche
/// relays under the cap. Non-routable urls too: since we PERSIST + REPLAY
/// these into future fetches, a relay-hint must never teach the client a
/// loopback/LAN relay, even once (outbox::routable_relay is the same
/// screen routed queries pass).
pub fn record(pk: &PublicKey, relay_urls: &[&str]) {
    let fresh: Vec<String> = relay_urls
        .iter()
        .filter_map(|s| {
            let key = outbox::relay_key(s);
            (outbox::routable_relay(&key) && !indexer(&key)).then_some(key)
        })
        .collect();
    if fresh.is_empty() {
        return;
    }
    let key = pk.to_string();
    let mut held = seen().lock().unwrap();
    // Re-insert the author most-recently-used (TS: delete + set).
    held.order.retain(|k| k != &key);
    let set = held.map.entry(key.clone()).or_default();
    for u in fresh {
        set.retain(|x| x != &u);
        set.push(u); // most recent last
    }
    while set.len() > MAX_PER_AUTHOR {
        set.remove(0); // drop oldest relay
    }
    held.order.push_back(key);
    while held.order.len() > MAX_AUTHORS {
        let old = held.order.pop_front();
        if let Some(old) = old {
            held.map.remove(&old);
        }
    }
    drop(held);
    DIRTY.store(true, Ordering::Relaxed);
}

fn indexer(key: &str) -> bool {
    crate::pool::INDEXERS.iter().any(|i| outbox::relay_key(i) == key)
}

/// The relays we've empirically seen this author on (seen-relays.ts
/// seenRelaysFor), most recent last — to fold into an outbox/single-event
/// query for their events.
pub fn for_pubkey(pk: &PublicKey) -> Vec<String> {
    let held = seen().lock().unwrap();
    held.map.get(&pk.to_string()).cloned().unwrap_or_default()
}

/// The most-recent seen relay, as an `e`/`q` tag relay hint (actions.ts:
/// seenRelaysFor(author)[0] — the LAST element of our recency-ordered vec).
pub fn first(pk: &PublicKey) -> Option<String> {
    let held = seen().lock().unwrap();
    held.map.get(&pk.to_string()).and_then(|v| v.last().cloned())
}

/// An `e` tag carrying the author hint plus our most-recent seen relay as
/// the relay hint (actions.ts addTag: `["e", id, seenRelaysFor(author)[0]
/// ?? '', author]` — so a client that never received the note can still
/// ask the relay that actually carries it). Empty hint string when we've
/// never seen the author anywhere, exactly like the TS.
pub fn hint_tag(id: EventId, author: &PublicKey) -> Tag {
    Tag::custom(
        "e",
        [
            id.to_string(),
            first(author).unwrap_or_default(),
            author.to_string(),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::EnvGuard;

    fn pk() -> PublicKey {
        Keys::generate().public_key()
    }

    fn record_str(p: &PublicKey, urls: &[&str]) {
        record(p, urls);
    }

    /// Recency: re-seen relays move to the end; indexers and non-routable
    /// urls never record.
    #[test]
    fn record_recency_and_caps() {
        let _env = EnvGuard::new("seen-recency");
        let p = pk();
        record_str(&p, &["wss://a.example", "wss://b.example"]);
        record_str(&p, &["wss://A.example/"]); // same map key (relay_key)
        assert_eq!(for_pubkey(&p).len(), 2);
        // a was re-seen → most recent (last).
        let relays = for_pubkey(&p);
        assert_eq!(relays, vec!["wss://b.example", "wss://a.example"]);
        // Non-indexers record; indexers and non-routable urls never do.
        record_str(
            &p,
            &["wss://nos.lol/", "wss://relay.nos.social/", "ws://127.0.0.1:8080"],
        );
        let relays = for_pubkey(&p);
        assert_eq!(relays, vec!["wss://b.example", "wss://a.example", "wss://nos.lol"]);
    }

    /// MAX_PER_AUTHOR: the 7th distinct relay evicts the oldest.
    #[test]
    fn per_author_cap() {
        let _env = EnvGuard::new("seen-cap");
        let p = pk();
        for c in ['a', 'b', 'c', 'd', 'e', 'f', 'g'] {
            record_str(&p, &[&format!("wss://{c}.example")]);
        }
        let relays = for_pubkey(&p);
        assert_eq!(relays.len(), MAX_PER_AUTHOR);
        assert_eq!(relays[0], "wss://b.example"); // a dropped, oldest
        assert_eq!(relays[5], "wss://g.example"); // newest kept
    }

    /// The tag hint is the MOST RECENT seen relay.
    #[test]
    fn hint_is_most_recent() {
        let _env = EnvGuard::new("seen-hint");
        let p = pk();
        record_str(&p, &["wss://one.example"]);
        record_str(&p, &["wss://two.example"]);
        assert_eq!(first(&p).as_deref(), Some("wss://two.example"));
    }
}
