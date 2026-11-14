//! The D33 bounded-JSON sidecar kernel — the one home for the plumbing
//! every sidecar (seen, seen_event, hints, deletions) had copy-pasted:
//! the 0600 temp-sibling write, the park_timeout flusher thread, and
//! the bounded relay-list LRU with its relay screen. The audit's
//! third-drifting-copy rule: the copies had already drifted once
//! (deletions serializes outside its lock; the other three didn't), so
//! the invariant lives here and each sidecar keeps only its OWN
//! semantics (keying, caps, what its values mean).
//!
//! What the kernel does NOT own: the empty/absent-value policy per
//! sidecar, deletions' newest-by-time hydrate (its values are times,
//! not recency lists — and its serialize-outside-lock stays: that
//! mutex gates every is_deleted render lookup).

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config;
use crate::outbox;

/// Flush cadence (the D33 discipline's 10s debounce, all sidecars).
pub(crate) const FLUSH_SECS: u64 = 10;

/// Write `body` to `path` via a temp sibling + rename, 0600 — the
/// state-file discipline (read_state, secrets): never rewrite in
/// place, never follow a planted link. A failed open or write removes
/// the temp and leaves the previous file intact.
pub(crate) fn write_atomic(path: PathBuf, body: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let tmp = path.with_extension(format!("tmp-{}-{nanos}", std::process::id()));
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    let Ok(mut f) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
    else {
        return;
    };
    if f.write_all(body.as_bytes()).is_ok() {
        drop(f);
        let _ = std::fs::rename(&tmp, &path);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// The flusher thread: park_timeout wakeups scanning the module's dirty
/// flag — no runtime dependency, no timer churn (a missed window just
/// rides the next wake; the data is a learned cache, at most FLUSH_SECS
/// stale on exit). One thread per sidecar, named for the census.
pub(crate) fn spawn_flusher(thread_name: &str, dirty: &'static AtomicBool, write: fn()) {
    std::thread::Builder::new()
        .name(thread_name.into())
        .spawn(move || {
            loop {
                std::thread::park_timeout(std::time::Duration::from_secs(FLUSH_SECS));
                if dirty.swap(false, Ordering::Relaxed) {
                    write();
                }
            }
        })
        .ok();
}

/// A relay sidecar's url screen (all of them share it): non-routable
/// urls never persist — a persisted hint must never teach the client a
/// loopback/LAN relay, even once (outbox::routable_relay is the same
/// screen routed queries pass) — and indexers never record: they're
/// queried on every fetch anyway, so recording them would only crowd
/// out the interesting niche relays under the cap. Yields the
/// normalized relay key when the url passes.
pub(crate) fn screen_relay(url: &str) -> Option<String> {
    let key = outbox::relay_key(url);
    (outbox::routable_relay(&key) && !indexer_keys().contains(&key)).then_some(key)
}

/// The indexer keys, parsed once (the screen runs on every ingested
/// event; re-lowercasing five constants per event was the audit's
/// arithmetic waste).
fn indexer_keys() -> &'static std::collections::HashSet<String> {
    static KEYS: std::sync::OnceLock<std::collections::HashSet<String>> =
        std::sync::OnceLock::new();
    KEYS.get_or_init(|| {
        crate::pool::INDEXERS
            .iter()
            .map(|i| outbox::relay_key(i))
            .collect()
    })
}

/// Read a persisted relay map (key → relay list) as insertion-ordered
/// pairs; a missing or corrupt file hydrates to nothing.
pub(crate) fn read_relay_pairs(file: &str) -> Vec<(String, Vec<String>)> {
    let Ok(raw) = std::fs::read_to_string(config::state_file(file)) else {
        return Vec::new();
    };
    serde_json::from_str::<HashMap<String, Vec<String>>>(&raw)
        .map(|m| m.into_iter().collect())
        .unwrap_or_default()
}

/// Serialize key → relay-list entries as the sidecar file's body (an
/// object of string arrays). Built under the caller's lock hold, as
/// before; the disk write itself stays outside it.
pub(crate) fn relay_json(entries: impl IntoIterator<Item = (String, Vec<String>)>) -> String {
    let mut out = serde_json::Map::new();
    for (key, relays) in entries {
        out.insert(key, serde_json::to_value(&relays).unwrap_or_default());
    }
    serde_json::Value::Object(out).to_string()
}

/// The bounded LRU every relay-list sidecar shares: key → recency-ordered
/// relay list (most recent LAST, the TS Set's insertion order), `order`
/// holding the keys in use order (front = oldest). Recency policy is the
/// CALLER's per record call — `touch` re-inserts MRU (seen.rs / hints.rs,
/// the TS delete + set), `touch_fifo` never scans (seen_event's audit
/// policy: the consumer doesn't care which capped keys stay, and the
/// per-event O(cap) scan under the mutex was millions of compares per
/// page load). The recency-across-restarts cost of the FIFO variant is a
/// recorded audit watch item.
pub(crate) struct RelayLru {
    pub map: HashMap<String, Vec<String>>,
    pub order: VecDeque<String>,
}

impl RelayLru {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// Hydrate from disk pairs: keep each entry's newest tail under the
    /// per-entry cap (TS slice(-cap)), skip empty entries, then drop the
    /// OLDEST keys over the global cap.
    pub fn hydrate(&mut self, pairs: Vec<(String, Vec<String>)>, per: usize, global: usize) {
        for (key, relays) in pairs {
            let tail: Vec<String> = relays.iter().rev().take(per).rev().cloned().collect();
            if !tail.is_empty() {
                self.map.insert(key.clone(), tail);
                self.order.push_back(key);
            }
        }
        self.evict_to(global);
    }

    /// Re-insert the key most-recently-used (TS: delete + set), then
    /// evict over the global cap.
    pub fn touch(&mut self, key: &str, global: usize) {
        self.order.retain(|k| k != key);
        self.order.push_back(key.to_string());
        self.evict_to(global);
    }

    /// The FIFO variant (seen_event's audit policy): a NEW key joins the
    /// back of the order deque; an existing key's position never moves —
    /// no O(cap) scan per ingest.
    pub fn touch_fifo(&mut self, key: &str, global: usize) {
        if !self.map.contains_key(key) {
            self.order.push_back(key.to_string());
        }
        self.evict_to(global);
    }

    /// Push a relay onto an entry's recency-ordered list (most recent
    /// last; a re-seen relay moves to the end), dropping the oldest over
    /// the per-entry cap.
    pub fn push_relay(&mut self, key: &str, relay: String, per: usize) {
        let set = self.map.entry(key.to_string()).or_default();
        set.retain(|x| x != &relay);
        set.push(relay); // most recent last
        while set.len() > per {
            set.remove(0); // drop oldest relay
        }
    }

    /// Evict keys from the FRONT (oldest) until the order deque is at
    /// most `cap` long.
    pub fn evict_to(&mut self, cap: usize) {
        while self.order.len() > cap {
            let old = self.order.pop_front();
            if let Some(old) = old {
                self.map.remove(&old);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Kernel semantics the sidecars lean on: hydrate keeps tails and
    /// drops oldest keys; touch re-inserts MRU; touch_fifo never scans
    /// (existing position frozen); push_relay recency-orders and caps.
    #[test]
    fn lru_shapes() {
        let mut lru = RelayLru::new();
        let pairs = vec![
            ("k1".into(), vec!["a".into(), "b".into(), "c".into()]),
            ("k2".into(), vec![]),
            ("k3".into(), vec!["x".into()]),
        ];
        lru.hydrate(pairs, 2, 3);
        // k2 empty: skipped; k1 tail keeps the newest 2.
        assert_eq!(
            lru.map.get("k1").unwrap(),
            &["b".to_string(), "c".to_string()]
        );
        assert_eq!(lru.order, ["k1", "k3"]);

        // MRU touch moves the key to the back.
        lru.touch("k1", 3);
        assert_eq!(lru.order, ["k3", "k1"]);
        // Global cap: the oldest key falls off.
        lru.touch("k4", 2);
        assert!(lru.map.get("k3").is_none());
        assert_eq!(lru.order, ["k1", "k4"]);

        // FIFO touch: an existing key's position never moves.
        let mut fifo = RelayLru::new();
        fifo.hydrate(vec![("a".into(), vec!["x".into()])], 2, 10);
        fifo.touch_fifo("a", 10);
        assert_eq!(fifo.order, ["a"]);
        fifo.touch_fifo("b", 10);
        assert_eq!(fifo.order, ["a", "b"]);

        // push_relay: re-seen moves to the end, cap drops the oldest.
        lru.push_relay("k4", "r1".into(), 2);
        lru.push_relay("k4", "r2".into(), 2);
        lru.push_relay("k4", "r1".into(), 2);
        assert_eq!(
            lru.map.get("k4").unwrap(),
            &["r2".to_string(), "r1".to_string()]
        );
        lru.push_relay("k4", "r3".into(), 2);
        assert_eq!(
            lru.map.get("k4").unwrap(),
            &["r1".to_string(), "r3".to_string()]
        );
    }

    /// The screen: non-routable and indexer urls never pass; a real url
    /// yields its normalized key.
    #[test]
    fn screen_screens() {
        assert_eq!(screen_relay("ws://127.0.0.1:8080"), None);
        assert_eq!(screen_relay("wss://relay.damus.io/"), None);
        assert_eq!(
            screen_relay("wss://A.example/"),
            Some("wss://a.example".into())
        );
    }
}
