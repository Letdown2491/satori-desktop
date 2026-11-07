//! Author-keyed relay hints — port of Dark Wisp's `repo/RelayHintStore.kt`
//! (barrydeen/dark-wisp-android): a per-AUTHOR relay index built from two
//! sources, filling the no-kind-10002 fallback gap (an author who publishes
//! no NIP-65 list is invisible to a pure declared-outbox router):
//!
//! (a) **Provenance** (`RelayHintStore.kt addAuthorRelay`, ~line 50): which
//!     relay actually DELIVERED an author's event — the same drain
//!     `seen.rs` rides, but keyed for author-level routing, not
//!     single-event resolution.
//! (b) **Tag hints** (`RelayHintStore.kt extractHintsFromTags`, ~line 56):
//!     relay URLs at index 2 of `e`/`p` tags — publishers point at the
//!     relay carrying the referenced event / the mentioned author's home.
//!
//! Persisted like `seen.rs` (the D33 sidecar discipline): 0600 JSON,
//! temp-sibling + rename, debounced flusher thread on `park_timeout` + a
//! dirty flag. Bounded: `MAX_HINTS_PER_AUTHOR` relays per author (the
//! Kotlin store's 5-hint cap), `MAX_AUTHORS` LRU in memory, the newest
//! `PERSISTED_AUTHORS` written out on flush. Core-clean: no UI deps.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use std::str::FromStr;

use nostr::prelude::*;

use crate::config;
use crate::outbox;

/// Relays kept per author (RelayHintStore.kt: capped at 5 hints/pubkey).
const MAX_HINTS_PER_AUTHOR: usize = 5;
/// LRU cap so the in-memory map can't grow without bound.
const MAX_AUTHORS: usize = 5000;
/// The persisted file keeps the newest slice of the LRU
/// (RelayHintStore.kt: 2000 persisted).
const PERSISTED_AUTHORS: usize = 2000;
/// Flush cadence (the seen.rs discipline: 10s debounce).
const FLUSH_SECS: u64 = 10;

/// pubkey hex → relay urls, recency-ordered (most recent LAST, matching
/// seen.rs's insertion-order convention); the order vec is the author LRU.
struct Hints {
    map: HashMap<String, Vec<String>>,
    order: VecDeque<String>,
}

static HINTS: OnceLock<Mutex<Hints>> = OnceLock::new();
static DIRTY: AtomicBool = AtomicBool::new(false);

fn hints() -> &'static Mutex<Hints> {
    HINTS.get_or_init(|| {
        let mut h = Hints { map: HashMap::new(), order: VecDeque::new() };
        let disk = read_disk();
        // Hydrate honoring the per-author cap (newest tail wins), then the
        // global LRU cap: drop OLDEST authors first.
        for (pk, relays) in disk {
            let tail: Vec<String> =
                relays.iter().rev().take(MAX_HINTS_PER_AUTHOR).rev().cloned().collect();
            if !tail.is_empty() {
                h.map.insert(pk.clone(), tail);
                h.order.push_back(pk);
            }
        }
        while h.order.len() > MAX_AUTHORS {
            if let Some(old) = h.order.pop_front() {
                h.map.remove(&old);
            }
        }
        spawn_flusher();
        Mutex::new(h)
    })
}

fn read_disk() -> Vec<(String, Vec<String>)> {
    let Ok(raw) = std::fs::read_to_string(config::state_file(config::files::RELAY_HINTS)) else {
        return Vec::new();
    };
    serde_json::from_str::<HashMap<String, Vec<String>>>(&raw)
        .map(|m| m.into_iter().collect())
        .unwrap_or_default()
}

fn write_disk() {
    let held = hints().lock().unwrap();
    // Persist only the newest PERSISTED_AUTHORS (LRU order: the front is
    // the oldest — skip it).
    let skip = held.order.len().saturating_sub(PERSISTED_AUTHORS);
    let mut out = serde_json::Map::new();
    for pk in held.order.iter().skip(skip) {
        if let Some(relays) = held.map.get(pk) {
            out.insert(pk.clone(), serde_json::to_value(relays).unwrap_or_default());
        }
    }
    drop(held);
    // Temp sibling + rename, 0600 — the state-file discipline (seen.rs,
    // read_state, secrets): never rewrite in place, never follow a
    // planted link.
    let path = config::state_file(config::files::RELAY_HINTS);
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

/// The flusher thread: park_timeout wakeups scanning the dirty flag —
/// identical to seen.rs's (no runtime dependency, no timer churn).
fn spawn_flusher() {
    std::thread::Builder::new()
        .name("hints-flush".into())
        .spawn(|| loop {
            std::thread::park_timeout(std::time::Duration::from_secs(FLUSH_SECS));
            if DIRTY.swap(false, Ordering::Relaxed) {
                write_disk();
            }
        })
        .ok();
}

/// The common recording path (RelayHintStore.kt addHint): indexers are
/// skipped (they're queried on every fetch anyway — recording them would
/// only crowd out the interesting relays under the cap), and so are
/// non-routable urls: since hints PERSIST + REPLAY into future routed
/// fetches, one must never teach the client a loopback/LAN relay
/// (outbox::routable_relay is the same screen routed queries pass).
fn add(pk: &str, url: &str) {
    let key = outbox::relay_key(url);
    if !outbox::routable_relay(&key) || indexer(&key) {
        return;
    }
    let mut held = hints().lock().unwrap();
    held.order.retain(|k| k != pk);
    let set = held.map.entry(pk.to_string()).or_default();
    set.retain(|x| x != &key);
    set.push(key); // most recent last
    while set.len() > MAX_HINTS_PER_AUTHOR {
        set.remove(0); // drop oldest hint
    }
    held.order.push_back(pk.to_string());
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

/// Provenance (RelayHintStore.kt addAuthorRelay): `relay_url` delivered an
/// event by `pk`. Call at every network ingestion point, next to
/// `seen::record` (which keys by EVENT; this keys by AUTHOR).
pub fn record_provenance(pk: &PublicKey, relay_url: &str) {
    add(&pk.to_string(), relay_url);
}

/// Tag hints (RelayHintStore.kt extractHintsFromTags): relay URLs at index
/// 2 of `e`/`p` tags, attributed to the REFERENCED pubkey (tag index 1) —
/// a publisher's hint about where that event/author lives. Malformed tags
/// (wrong arity, non-routable/indexer urls) are skipped; the tag's
/// referenced key must be a valid pubkey hex.
pub fn record_tag_hints(ev: &Event) {
    for tag in ev.tags.clone().to_vec() {
        let parts = tag.as_slice();
        if parts.first().map(String::as_str) != Some("e")
            && parts.first().map(String::as_str) != Some("p")
        {
            continue;
        }
        let (Some(target), Some(url)) = (parts.get(1), parts.get(2)) else {
            continue;
        };
        if target.is_empty() || url.is_empty() {
            continue;
        }
        // Only a well-formed pubkey gets indexed — the hint is keyed by
        // the referenced author, and a junk key would poison the map.
        let Ok(target) = PublicKey::from_str(target) else {
            continue;
        };
        add(&target.to_string(), url);
    }
}

/// The one-call ingestion hook: provenance + tag hints for an event off
/// `relay_url`. Every place `seen::record` runs for delivery attribution
/// runs this too.
pub fn observe(ev: &Event, relay_url: &str) {
    record_provenance(&ev.pubkey, relay_url);
    record_tag_hints(ev);
}

/// The author's hint relays (RelayHintStore.kt getHints), recency-ordered
/// most-recent LAST — to seed routing for an author with no kind:10002.
pub fn for_pubkey(pk: &PublicKey) -> Vec<String> {
    let held = hints().lock().unwrap();
    held.map.get(&pk.to_string()).cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::EnvGuard;

    fn pk() -> PublicKey {
        Keys::generate().public_key()
    }

    fn ev(keys: &Keys, tags: Vec<Vec<String>>) -> Event {
        let mut t = Tags::new();
        for v in tags {
            t.push(Tag::custom(&v[0], v[1..].to_vec()));
        }
        let unsigned = UnsignedEvent::new(
            keys.public_key(),
            Timestamp::from_secs(1),
            Kind::TextNote,
            t,
            String::new(),
        );
        keys.sign_event(unsigned).unwrap()
    }

    /// Provenance records, normalizes (relay_key), skips indexers and
    /// non-routable urls, and caps at MAX_HINTS_PER_AUTHOR.
    #[test]
    fn provenance_records_and_caps() {
        let _env = EnvGuard::new("hints-prov");
        let p = pk();
        record_provenance(&p, "wss://A.example/");
        record_provenance(&p, "wss://relay.damus.io/"); // indexer: skipped
        record_provenance(&p, "ws://127.0.0.1:8080"); // non-routable: skipped
        assert_eq!(for_pubkey(&p), vec!["wss://a.example"]);
        for c in ['a', 'b', 'c', 'd', 'e', 'f'] {
            record_provenance(&p, &format!("wss://{c}.example"));
        }
        let relays = for_pubkey(&p);
        assert_eq!(relays.len(), MAX_HINTS_PER_AUTHOR);
        assert_eq!(relays[0], "wss://b.example"); // a evicted, oldest
        assert_eq!(relays[4], "wss://f.example"); // newest kept
    }

    /// e/p-tag relay urls at index 2 index the REFERENCED pubkey; junk
    /// keys, missing urls, and other tag names are skipped.
    #[test]
    fn tag_hints_index_the_referenced_pubkey() {
        let _env = EnvGuard::new("hints-tags");
        let author = Keys::generate();
        let target = pk();
        let e = ev(
            &author,
            vec![
                vec!["e".into(), "abc123".into(), "wss://carries-the-note.example".into()],
                vec!["p".into(), target.to_string(), "wss://their-home.example/".into()],
                vec!["p".into(), target.to_string(), "wss://relay.primal.net/".into()], // indexer: skip
                vec!["p".into(), "not-a-pubkey".into(), "wss://junk.example".into()], // junk key: skip
                vec!["p".into(), target.to_string()], // no url: skip
                vec!["t".into(), "hashtag".into(), "wss://not-a-hint.example".into()],
            ],
        );
        record_tag_hints(&e);
        // The e tag's key is junk hex: only the p tag's target indexed.
        assert_eq!(for_pubkey(&target), vec!["wss://their-home.example"]);
        assert!(for_pubkey(&author.public_key()).is_empty());
    }

    /// observe() is the pair; the author of the event gets provenance.
    #[test]
    fn observe_records_both() {
        let _env = EnvGuard::new("hints-observe");
        let keys = Keys::generate();
        let p = keys.public_key();
        let other = pk();
        let e = ev(
            &keys,
            vec![
                vec![
                    "p".into(),
                    other.to_string(),
                    "wss://other.example".into(),
                ],
            ],
        );
        observe(&e, "wss://delivered-here.example");
        assert_eq!(for_pubkey(&p), vec!["wss://delivered-here.example"]);
        assert_eq!(for_pubkey(&other), vec!["wss://other.example"]);
    }

    /// The LRU: past MAX_AUTHORS authors, the oldest author falls off.
    #[test]
    fn author_lru() {
        let _env = EnvGuard::new("hints-lru");
        let first = pk();
        record_provenance(&first, "wss://first.example");
        for _ in 0..MAX_AUTHORS {
            let p = Keys::generate().public_key();
            record_provenance(&p, "wss://filler.example");
        }
        assert!(for_pubkey(&first).is_empty());
    }
}
