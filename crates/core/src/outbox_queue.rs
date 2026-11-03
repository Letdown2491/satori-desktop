//! The outbox queue — pending publishes awaiting relay acceptance (D36).
//!
//! Desktop addition; semantics from `src/data/publish.ts` where they
//! overlap (write-relay target resolution), extended to full MTA-style
//! outbox behavior that the TS app never needed (it had no persistent
//! signing session). The contract:
//!
//! 1. WRITE-AHEAD: an event is enqueued BEFORE the publish attempt and
//!    removed only when ≥1 relay ACKs (OK true). A crash at any point
//!    still retries on the next login/drain — a queued note cannot die
//!    in a lost publish call.
//! 2. Decision = ≥1 relay accepted. Zero-accept publishes keep the item
//!    and surface Failed; the composer shows Committed only on first
//!    acceptance (retry successes are logged, not re-toasted).
//! 3. BACKOFF: 30s doubling, capped at 10 min, retried indefinitely —
//!    the queue outlives restarts and drains whenever a session exists.
//! 4. REPLACEABLE COORDINATE KEYS: kinds 0/3/10000-19999 and the
//!    30000-39999 range queue under `kind:pubkey[:d]`, newest version
//!    evicting older pending ones — a retried kind:10002 must never
//!    resurrect a relay list the user already superseded. Ephemeral
//!    kinds (20000-29999) are never queued (they expire in minutes).
//!
//! Divergence from the first sketch (per-item retry loops): ONE drainer
//! task owns all retries (serialized publishes — no double-fire when a
//! login drain races an in-flight backoff, no loop-per-item explosion).
//! The drainer lives in manager.rs; this module is the pure data layer.
//!
//! PERSISTED (deletions.rs discipline): OnceLock map, flusher thread,
//! temp sibling + rename at 0600. Enqueue flushes SYNCHRONOUSLY (the
//! write-ahead guarantee is only as good as the enqueue's durability);
//! removes/attempts ride the debounced flusher — a crash before those
//! flush at worst re-publishes an event, which relays dedupe by id.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use nostr::prelude::*;

use crate::config;

/// ~1000 notes at ~300 bytes ≈ 300KB ceiling; pathological backlog only.
const MAX_PENDING: usize = 500;
/// Flush cadence for removes/attempts (deletions.rs shares the cadence).
const FLUSH_SECS: u64 = 10;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Pending {
    pub event: Event,
    pub added_at: u64,
    pub attempts: u32,
    pub next_try_at: u64,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Stored {
    #[serde(default)]
    items: Vec<Pending>,
}

struct Outbox {
    map: HashMap<String, Pending>,
    order: VecDeque<String>,
}

static OUTBOX: OnceLock<Mutex<Outbox>> = OnceLock::new();
static DIRTY: AtomicBool = AtomicBool::new(false);

fn outbox() -> &'static Mutex<Outbox> {
    OUTBOX.get_or_init(|| {
        let items = std::fs::read_to_string(config::state_file(config::files::OUTBOX_PENDING))
            .ok()
            .and_then(|s| serde_json::from_str::<Stored>(&s).ok())
            .map(|s| s.items)
            .unwrap_or_default();
        let mut map = HashMap::new();
        let mut order = VecDeque::new();
        for p in items {
            let Some(key) = outbox_key(&p.event) else {
                continue;
            };
            if !map.contains_key(&key) {
                order.push_back(key.clone());
            }
            map.insert(key, p);
        }
        spawn_flusher();
        Mutex::new(Outbox { map, order })
    })
}

/// The queue key: replaceable kinds coordinate (newest version wins),
/// everything else the event id. Ephemeral kinds return None (never
/// queued — a retry would fire into the void after they expire).
fn outbox_key(event: &Event) -> Option<String> {
    let kind = u16::from(event.kind);
    if (20000..30000).contains(&kind) {
        return None;
    }
    if (10000..20000).contains(&kind) || kind == 0 || kind == 3 {
        return Some(format!("{}:{}", kind, event.pubkey));
    }
    if (30000..40000).contains(&kind) {
        let d = event
            .tags
            .iter()
            .filter_map(|t| {
                let s = t.as_slice();
                (s.first()? == "d").then(|| s.get(1).cloned().unwrap_or_default())
            })
            .next()
            .unwrap_or_default();
        return Some(format!("{}:{}:{}", kind, event.pubkey, d));
    }
    Some(event.id.to_string())
}

/// Retry delay after the k-th failed queue attempt: 30s doubling, 10 min
/// ceiling (D36). Indefinite total retries.
pub(crate) fn backoff(attempts: u32) -> u64 {
    let shift = attempts.saturating_sub(1).min(5);
    (30u64 << shift).min(600)
}

pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Enqueue for delivery (write-ahead). Returns false when the event is
/// not queueable (ephemeral) — the caller still publishes it, it just
/// can't be retried. Flushes to disk SYNCHRONOUSLY: the durability point.
/// A newer version of a replaceable evicts older pending versions.
pub(crate) fn enqueue(event: &Event) -> bool {
    let Some(key) = outbox_key(event) else {
        return false;
    };
    let now = now_secs();
    {
        let mut held = outbox().lock().unwrap();
        // Replaceable coordinate: drop older pending versions so a
        // superseded relay list can't resurrect on retry.
        if (10000..20000).contains(&u16::from(event.kind)) || u16::from(event.kind) == 0 || u16::from(event.kind) == 3 || (30000..40000).contains(&u16::from(event.kind)) {
            let coord = key.clone();
            let stale: Vec<String> = held
                .map
                .iter()
                .filter(|(_, p)| outbox_key(&p.event).as_deref() == Some(coord.as_str()))
                .map(|(k, _)| k.clone())
                .collect();
            for k in stale {
                if held.map.remove(&k).is_some() {
                    held.order.retain(|o| o != &k);
                }
            }
        }
        if !held.map.contains_key(&key) {
            held.order.push_back(key.clone());
        } else {
            held.order.retain(|o| o != &key);
            held.order.push_back(key.clone());
        }
        held.map.insert(
            key,
            Pending {
                event: event.clone(),
                added_at: now,
                attempts: 0,
                // First queue attempt comes one backoff step after the
                // failed commit — the commit's own attempt just happened.
                next_try_at: now + backoff(1),
            },
        );
        while held.order.len() > MAX_PENDING {
            let Some(old) = held.order.pop_front() else {
                break;
            };
            if held.map.remove(&old).is_some() {
                eprintln!("[core] outbox: backlog over {} — dropping oldest pending", MAX_PENDING);
            }
        }
    }
    write_disk();
    true
}

/// The item was accepted somewhere (or superseded) — stop retrying it.
pub(crate) fn remove(event: &Event) {
    let Some(key) = outbox_key(event) else {
        return;
    };
    let mut held = outbox().lock().unwrap();
    if held.map.remove(&key).is_some() {
        held.order.retain(|o| o != &key);
        DIRTY.store(true, Ordering::Relaxed);
    }
}

/// Record a failed queue attempt: attempts+1, next try one backoff later.
pub(crate) fn mark_attempt(event: &Event) {
    let Some(key) = outbox_key(event) else {
        return;
    };
    let now = now_secs();
    let mut held = outbox().lock().unwrap();
    if let Some(p) = held.map.get_mut(&key) {
        p.attempts += 1;
        p.next_try_at = now + backoff(p.attempts);
        DIRTY.store(true, Ordering::Relaxed);
    }
}

/// Push an item's next try out (logged out, relays unreachable): just
/// move the due time; attempts unchanged.
pub(crate) fn reschedule(event: &Event, at: u64) {
    let Some(key) = outbox_key(event) else {
        return;
    };
    let mut held = outbox().lock().unwrap();
    if let Some(p) = held.map.get_mut(&key)
        && p.next_try_at < at
    {
        p.next_try_at = at;
        DIRTY.store(true, Ordering::Relaxed);
    }
}

/// The oldest due item, if any (the drainer processes one at a time).
pub(crate) fn next_due(now: u64) -> Option<Pending> {
    let held = outbox().lock().unwrap();
    held.order
        .iter()
        .filter_map(|k| held.map.get(k))
        .filter(|p| p.next_try_at <= now)
        .min_by_key(|p| p.next_try_at)
        .cloned()
}

/// Queue depth (bootstrap log line).
pub(crate) fn len() -> usize {
    outbox().lock().unwrap().map.len()
}

#[cfg(test)]
pub(crate) fn contains(event: &Event) -> bool {
    outbox_key(event)
        .map(|k| outbox().lock().unwrap().map.contains_key(&k))
        .unwrap_or(false)
}

/// The event actually stored under `event`'s queue key — for replaceable
/// coordinates, which version survived.
#[cfg(test)]
fn stored_event(event: &Event) -> Option<Event> {
    let key = outbox_key(event)?;
    outbox().lock().unwrap().map.get(&key).map(|p| p.event.clone())
}

fn write_disk() {
    // Snapshot under the lock, serialize OUTSIDE it (audit perf: the
    // same mutex must never gate a JSON-format behind a render lookup).
    let snap: Vec<Pending> = {
        let held = outbox().lock().unwrap();
        held.order.iter().filter_map(|k| held.map.get(k)).cloned().collect()
    };
    let body = serde_json::to_string(&Stored { items: snap }).unwrap_or_default();
    let path = config::state_file(config::files::OUTBOX_PENDING);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let tmp = path.with_extension(format!("tmp-{}-{nanos}", std::process::id()));
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
        DIRTY.store(false, Ordering::Relaxed);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

fn spawn_flusher() {
    std::thread::Builder::new()
        .name("outbox-flush".into())
        .spawn(|| loop {
            std::thread::park_timeout(std::time::Duration::from_secs(FLUSH_SECS));
            if DIRTY.swap(false, Ordering::Relaxed) {
                write_disk();
            }
        })
        .ok();
}

/// The publish target set (publish.ts relay resolution, extended):
/// own write relays, normalized (outbox::relay_key — lowercase, no
/// trailing slash; nostr.build arrives tagged `wss://relay.nostr.build/`),
/// deduped, write-dead filtered — unless ALL are dead, in which case keep
/// them all: a relay revives only by a real attempt, and an empty set
/// would starve the outbox forever. An empty input stays empty (the pool
/// falls back to indexers, its existing contract).
pub(crate) fn targets(write_relays: &[String]) -> Vec<String> {
    let mut norm: Vec<String> = Vec::new();
    for r in write_relays {
        let k = crate::outbox::relay_key(r);
        if !k.is_empty() && !norm.contains(&k) {
            norm.push(k);
        }
    }
    let live: Vec<String> = norm
        .iter()
        .filter(|r| !crate::relay_latency::is_write_dead(r))
        .cloned()
        .collect();
    if live.is_empty() {
        norm
    } else {
        live
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::EnvGuard;

    fn note(keys: &Keys, content: &str) -> Event {
        keys.sign_event(UnsignedEvent::new(
            keys.public_key(),
            Timestamp::now(),
            Kind::TextNote,
            vec![],
            content,
        ))
        .unwrap()
    }

    fn relay_list(keys: &Keys, at: Timestamp) -> Event {
        keys.sign_event(UnsignedEvent::new(
            keys.public_key(),
            at,
            Kind::RelayList,
            vec![Tag::custom("r", ["wss://a.example"])],
            "",
        ))
        .unwrap()
    }

    /// Regular events queue by id, dedupe, and leave on remove.
    #[test]
    fn enqueue_dedupe_remove() {
        let _env = EnvGuard::new("outbox-basic");
        let keys = Keys::generate();
        let ev = note(&keys, "one");
        assert!(enqueue(&ev));
        assert!(enqueue(&ev)); // dedupe by id
        assert!(contains(&ev));
        remove(&ev);
        assert!(!contains(&ev));
    }

    /// Replaceable kinds queue under their coordinate and a newer
    /// version evicts the older pending one (a retried relay list must
    /// never resurrect a superseded set).
    #[test]
    fn replaceable_newest_wins() {
        let _env = EnvGuard::new("outbox-repl");
        let keys = Keys::generate();
        let v1 = relay_list(&keys, Timestamp::from(now_secs() - 10));
        let v2 = relay_list(&keys, Timestamp::now());
        assert!(enqueue(&v1));
        assert!(enqueue(&v2));
        // Same coordinate key → one entry, and it IS the newer version
        // (the older pending one was evicted, never to resurrect).
        assert_eq!(stored_event(&v2).map(|e| e.id), Some(v2.id));
        assert_ne!(stored_event(&v2).map(|e| e.id), Some(v1.id));
        // Removing by either version clears the coordinate (same key).
        remove(&v1);
        assert!(!contains(&v2));
    }

    /// Ephemeral kinds are never queued.
    #[test]
    fn ephemeral_not_queued() {
        let _env = EnvGuard::new("outbox-eph");
        let keys = Keys::generate();
        let ev = keys
            .sign_event(UnsignedEvent::new(
                keys.public_key(),
                Timestamp::now(),
                Kind::Custom(20001),
                vec![],
                " ephemeral",
            ))
            .unwrap();
        assert!(!enqueue(&ev));
        assert!(!contains(&ev));
    }

    /// 30s doubling, 10 min ceiling, indefinitely.
    #[test]
    fn backoff_growth() {
        assert_eq!(backoff(1), 30);
        assert_eq!(backoff(2), 60);
        assert_eq!(backoff(3), 120);
        assert_eq!(backoff(4), 240);
        assert_eq!(backoff(5), 480);
        assert_eq!(backoff(6), 600); // capped
        assert_eq!(backoff(500), 600);
    }

    /// A failed attempt schedules the next try one backoff step out;
    /// the item only comes due after it.
    #[test]
    fn attempts_schedule() {
        let _env = EnvGuard::new("outbox-sched");
        let keys = Keys::generate();
        let ev = note(&keys, "sched");
        assert!(enqueue(&ev));
        // Freshly enqueued: due one backoff(1) out, not now.
        assert!(next_due(now_secs()).is_none_or(|p| p.event.id != ev.id));
        mark_attempt(&ev);
        let held = outbox().lock().unwrap();
        let key = outbox_key(&ev).unwrap();
        let p = held.map.get(&key).unwrap();
        assert_eq!(p.attempts, 1);
        assert!(p.next_try_at >= now_secs() + 30);
        drop(held);
        remove(&ev);
        assert!(!contains(&ev));
    }

    /// The persisted file round-trips (serde shape + 0600 discipline).
    #[test]
    fn disk_round_trip() {
        let _env = EnvGuard::new("outbox-disk");
        let keys = Keys::generate();
        let ev = note(&keys, "persist");
        assert!(enqueue(&ev)); // synchronous flush inside
        let path = config::state_file(config::files::OUTBOX_PENDING);
        let raw = std::fs::read_to_string(&path).expect("outbox file written");
        let parsed: Stored = serde_json::from_str(&raw).expect("valid json");
        let mine = parsed
            .items
            .iter()
            .find(|p| p.event.id == ev.id)
            .expect("our item persisted");
        assert_eq!(mine.event.content, "persist");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// Write-set resolution: normalize (trailing slash), dedupe, drop
    /// write-dead relays — but keep everything when ALL are dead (a
    /// relay revives only by a real attempt; the outbox must not starve).
    #[test]
    fn targets_filter_and_fallback() {
        let _env = EnvGuard::new("outbox-targets");
        let a = "wss://wt-a.example";
        let b = "wss://wt-b.example";
        // Trailing slash normalizes onto the same key; dedupe holds.
        let set = vec![format!("{a}/"), a.to_string(), b.to_string()];
        let t = targets(&set);
        assert_eq!(t, vec![a, b]);
        // Two failures is not enough to learn dead.
        crate::relay_latency::record_write_result(a, false);
        crate::relay_latency::record_write_result(a, false);
        assert!(!crate::relay_latency::is_write_dead(a));
        assert_eq!(targets(&set), vec![a, b]);
        // The third consecutive failure is.
        crate::relay_latency::record_write_result(a, false);
        assert!(crate::relay_latency::is_write_dead(a));
        assert_eq!(targets(&set), vec![b]);
        // All dead → full set (revival attempt, no starvation).
        crate::relay_latency::record_write_result(b, false);
        crate::relay_latency::record_write_result(b, false);
        crate::relay_latency::record_write_result(b, false);
        assert_eq!(targets(&set), vec![a, b]);
        // A success clears the learned deadness.
        crate::relay_latency::record_write_result(a, true);
        assert!(!crate::relay_latency::is_write_dead(a));
    }
}
