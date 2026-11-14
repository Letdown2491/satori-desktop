//! NIP-09 deletion tombstones, the read side of event deletion — port of
//! `src/data/deletions.ts`. Kind:5 events are consumed at the pool
//! chokepoint wherever they arrive and remembered here; every pool
//! read's results are then filtered against the store, so a deleted
//! event stops rendering everywhere (feeds, threads, embeds, the
//! notification union) with no per-render network cost. The spec's MUST
//! — only the author can delete their own event — is enforced
//! structurally: id tombstones are KEYED by target-id + deleter-pubkey,
//! so a lookup for an event only ever matches a deletion signed by that
//! event's own author (someone else's kind:5 for the same id lands
//! under a different key and never applies). Address (`a`) tombstones
//! validate the coordinate's pubkey against the kind:5 author at record
//! time, and apply only to versions with created_at at or before the
//! deletion's — a later re-publish supersedes the delete, per NIP-09.
//!
//! Hostile-input posture: any relay can deliver any self-signed kind:5,
//! so what one event may write is bounded (tags consumed per event, key
//! length) and YOUR OWN published deletions are PINNED in separate maps
//! that relay traffic can never evict — otherwise a stranger's
//! tag-spam kind:5 could flush your tombstones and resurrect your
//! deleted posts.
//!
//! PERSISTED (debounced ~10s, flusher thread like seen.rs): a tombstone
//! must outlive the LMDB store's retention and restarts, or deleted
//! events resurrect on reboot. Bounded like seen-relays.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use nostr::prelude::*;

use crate::config;
use crate::sidecar;

/// Per relay-fed map; a tombstone is ~100 bytes, so this stays a small file.
const MAX_TOMBS: usize = 5000;
/// Per own-deletions map; only your own publishes write here.
const MAX_PINNED: usize = 1000;
/// A real NIP-09 delete names a handful of events; tag-spam is an attack.
const MAX_TAGS_PER_EVENT: usize = 100;
/// Coordinate keys embed the `d` identifier, which is unbounded on the wire.
const MAX_KEY: usize = 256;
/// Don't re-ask relays about the same event's deletion for 30 min.
const CHECK_TTL_SECS: u64 = 30 * 60;

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
struct Stored {
    #[serde(default)]
    ids: HashMap<String, u64>,
    #[serde(default)]
    addrs: HashMap<String, u64>,
    #[serde(default)]
    pids: HashMap<String, u64>,
    #[serde(default)]
    paddrs: HashMap<String, u64>,
}

struct Deletions {
    /// "target-id:deleter-pk" → deletion created_at, and
    /// "kind:pk:d" → deletion created_at. The order deques hold the
    /// same keys in arrival order; oldest entries are dropped first at
    /// the cap (pop_front is O(1) — a Vec's remove(0) would memmove the
    /// whole tail per insert at cap, and kind:5s arrive in bursts).
    /// The pinned pair holds the same shapes but is written only from
    /// your own accepted publishes.
    maps: Stored,
    ids_order: VecDeque<String>,
    addrs_order: VecDeque<String>,
    pids_order: VecDeque<String>,
    paddrs_order: VecDeque<String>,
    /// In-memory only: ids/coords recently asked about (the background
    /// deletion sweep's TTL memory). A restart just re-checks once.
    checked: HashMap<String, u64>,
}

static DELETIONS: OnceLock<Mutex<Deletions>> = OnceLock::new();
static DIRTY: AtomicBool = AtomicBool::new(false);

fn deletions() -> &'static Mutex<Deletions> {
    DELETIONS.get_or_init(|| {
        let raw = std::fs::read_to_string(config::state_file(config::files::DELETIONS))
            .ok()
            .and_then(|s| serde_json::from_str::<Stored>(&s).ok())
            .unwrap_or_default();
        // The persisted map's entry order is our recency record; the
        // newest tail survives the cap (load keeps the newest by time).
        let ids = load(raw.ids, MAX_TOMBS);
        let addrs = load(raw.addrs, MAX_TOMBS);
        let pids = load(raw.pids, MAX_PINNED);
        let paddrs = load(raw.paddrs, MAX_PINNED);
        spawn_flusher();
        Mutex::new(Deletions {
            ids_order: ids.keys().cloned().collect(),
            addrs_order: addrs.keys().cloned().collect(),
            pids_order: pids.keys().cloned().collect(),
            paddrs_order: paddrs.keys().cloned().collect(),
            maps: Stored {
                ids,
                addrs,
                pids,
                paddrs,
            },
            checked: HashMap::new(),
        })
    })
}

/// The newest tail of a persisted map, honoring the cap (the TS kept
/// insertion order = recency; on hydrate we keep the newest by time —
/// the semantics that matter).
fn load(map: HashMap<String, u64>, cap: usize) -> HashMap<String, u64> {
    if map.len() <= cap {
        return map;
    }
    let mut all: Vec<(String, u64)> = map.into_iter().collect();
    all.sort_by_key(|(_, at)| std::cmp::Reverse(*at));
    all.into_iter().take(cap).collect()
}

fn write_disk() {
    // Snapshot under the lock, serialize OUTSIDE it: this same mutex
    // gates every is_deleted render lookup, and JSON-formatting ~10k
    // entries must not stall a paint (audit perf).
    let snap = deletions().lock().unwrap().maps.clone();
    let body = serde_json::to_string(&snap).unwrap_or_default();
    // Temp sibling + rename, 0600 — the sidecar kernel's discipline.
    let path = config::state_file(config::files::DELETIONS);
    sidecar::write_atomic(path, &body);
}

fn spawn_flusher() {
    sidecar::spawn_flusher("deletions-flush", &DIRTY, write_disk);
}

/// Keep the NEWEST deletion time per key; oldest evicted over the cap.
fn put(
    map: &mut HashMap<String, u64>,
    order: &mut VecDeque<String>,
    key: String,
    at: u64,
    cap: usize,
) {
    if let Some(cur) = map.get(&key)
        && *cur >= at
    {
        return;
    }
    if !map.contains_key(&key) {
        order.push_back(key.clone());
    }
    map.insert(key, at);
    while order.len() > cap {
        let old = order.pop_front();
        if let Some(old) = old {
            map.remove(&old);
        }
    }
}

/// Record a kind:5's targets as tombstones (deletions.ts recordDeletion).
/// `e` targets are keyed with the deleter's pubkey (pubkey-matching
/// happens at lookup); `a` targets are only honored when the
/// coordinate's pubkey IS the kind:5 author (the NIP-09 MUST — anyone
/// can e-tag, but an address names its owner, so a mismatch is a
/// cross-author deletion attempt and is dropped). `pin` = this is the
/// logged-in user's own accepted deletion: stored in the pinned maps,
/// immune to relay-fed eviction. Both key halves are lowercased — hex
/// is lowercase per NIP-01, but tag values from the wire aren't
/// guaranteed to be.
pub fn record(ev: &Event, pin: bool) {
    if u16::from(ev.kind) != 5 {
        return;
    }
    let pk = ev.pubkey.to_string().to_lowercase();
    let at = ev.created_at.as_secs();
    let mut touched = false;
    let mut consumed = 0usize;
    let mut held = deletions().lock().unwrap();
    let d = &mut *held;
    for t in ev.tags.iter() {
        let s = t.as_slice();
        let (Some(k), Some(v)) = (s.first(), s.get(1)) else {
            continue;
        };
        if k != "e" && k != "a" {
            continue;
        }
        consumed += 1;
        if consumed > MAX_TAGS_PER_EVENT {
            break; // tag-spam bound: one event can't flush the store
        }
        if k == "e" && v.len() == 64 && v.chars().all(|c| c.is_ascii_hexdigit()) {
            let key = format!("{}:{pk}", v.to_lowercase());
            if pin {
                put(&mut d.maps.pids, &mut d.pids_order, key, at, MAX_PINNED);
            } else {
                put(&mut d.maps.ids, &mut d.ids_order, key, at, MAX_TOMBS);
            }
            touched = true;
        } else if k == "a"
            && v.len() <= MAX_KEY
            && let Some((_, cpk, _)) = coord_parts(v)
            && cpk.to_string().to_lowercase() == pk
        {
            let key = v.to_lowercase();
            if pin {
                put(&mut d.maps.paddrs, &mut d.paddrs_order, key, at, MAX_PINNED);
            } else {
                put(&mut d.maps.addrs, &mut d.addrs_order, key, at, MAX_TOMBS);
            }
            touched = true;
        }
    }
    if touched {
        DIRTY.store(true, Ordering::Relaxed);
    }
}

/// "kind:pubkey[:d]" → (kind, pubkey, d) — tags.ts coordParts. One parser
/// for every site that would otherwise hand-split with ':' indices.
fn coord_parts(coord: &str) -> Option<(u32, PublicKey, String)> {
    let mut it = coord.splitn(3, ':');
    let kind = it.next()?.parse::<u32>().ok()?;
    let pk = PublicKey::parse(it.next()?).ok()?;
    Some((kind, pk, it.next().unwrap_or_default().to_string()))
}

/// Is this event deleted by its own author? (deletions.ts isDeletedEvent.)
/// Id tombstones match only when the deleter IS the event's author (the
/// key embeds both). Address tombstones suppress versions up to the
/// deletion time, so a re-published (newer) version renders again.
/// Pinned (own) tombstones are checked alongside the relay-fed maps.
pub fn is_deleted(ev: &Event) -> bool {
    let held = deletions().lock().unwrap();
    let id_key = format!("{}:{}", ev.id, ev.pubkey).to_lowercase();
    if held.maps.ids.contains_key(&id_key) || held.maps.pids.contains_key(&id_key) {
        return true;
    }
    // Address tombstones apply to parameterized-replaceable kinds only.
    let kind = u16::from(ev.kind);
    if !(30000..40000).contains(&kind) {
        return false;
    }
    let d: String = ev
        .tags
        .iter()
        .filter_map(|t| {
            let s = t.as_slice();
            (s.first()? == "d").then(|| s.get(1).cloned().unwrap_or_default())
        })
        .next()
        .unwrap_or_default();
    let coord = format!("{kind}:{}:{d}", ev.pubkey).to_lowercase();
    if coord.len() > MAX_KEY {
        return false;
    }
    let at = held
        .maps
        .addrs
        .get(&coord)
        .or_else(|| held.maps.paddrs.get(&coord))
        .copied();
    at.is_some_and(|at| ev.created_at.as_secs() <= at)
}

/// Should the sweep ask relays about this target (deletions.ts
/// shouldCheckDeletion)? True at most once per TTL per target; marks the
/// target as checked when it says yes. Oversized keys (an unbounded `d`
/// identifier) are never checked — they can't be tombstoned either.
pub fn should_check(key: &str) -> bool {
    if key.len() > MAX_KEY {
        return false;
    }
    let now = now_secs();
    let mut held = deletions().lock().unwrap();
    if let Some(last) = held.checked.get(key)
        && now.saturating_sub(*last) < CHECK_TTL_SECS
    {
        return false;
    }
    if held.checked.len() >= 20_000 {
        held.checked.clear(); // cheap bound; worst case is one early re-check
    }
    held.checked.insert(key.to_string(), now);
    true
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::EnvGuard;

    fn kind5(targets: Vec<Tag>, deleter: &Keys) -> Event {
        deleter
            .sign_event(UnsignedEvent::new(
                deleter.public_key(),
                Timestamp::now(),
                Kind::EventDeletion,
                targets,
                "",
            ))
            .unwrap()
    }

    fn note(author: &Keys, tags: Vec<Tag>) -> Event {
        author
            .sign_event(UnsignedEvent::new(
                author.public_key(),
                Timestamp::now(),
                Kind::TextNote,
                tags,
                "body",
            ))
            .unwrap()
    }

    /// The NIP-09 MUST, structurally: a stranger's kind:5 naming your
    /// event never deletes it (the tombstone key embeds the deleter);
    /// the author's own does.
    #[test]
    fn only_the_author_deletes() {
        let _env = EnvGuard::new("del-author");
        let author = Keys::generate();
        let stranger = Keys::generate();
        let note = note(&author, vec![]);
        // Stranger's tombstone for the author's event: recorded, but the
        // lookup key embeds the deleter — never matches the note.
        record(&kind5(vec![Tag::event(note.id)], &stranger), false);
        assert!(!is_deleted(&note));
        // The author's own tombstone does.
        record(&kind5(vec![Tag::event(note.id)], &author), false);
        assert!(is_deleted(&note));
    }

    /// Address tombstones: honored only when the coordinate's pubkey IS
    /// the kind:5 author, and only for versions at or before the
    /// deletion time (a re-publish supersedes the delete).
    #[test]
    fn address_tombstones_scoped() {
        let _env = EnvGuard::new("del-addr");
        let author = Keys::generate();
        let stranger = Keys::generate();
        let d = "some-article";
        let mk = |keys: &Keys, at: Timestamp| {
            keys.sign_event(UnsignedEvent::new(
                keys.public_key(),
                at,
                Kind::Custom(30023),
                vec![Tag::custom("d", [d])],
                "v1",
            ))
            .unwrap()
        };
        // A stranger's a-tombstone for the author's coordinate: dropped at
        // record (cross-author attempt).
        let cross = format!("30023:{}:{d}", author.public_key());
        let stranger_del = stranger
            .sign_event(UnsignedEvent::new(
                stranger.public_key(),
                Timestamp::now(),
                Kind::EventDeletion,
                vec![Tag::custom("a", [cross.clone()])],
                "",
            ))
            .unwrap();
        record(&stranger_del, false);
        assert!(!is_deleted(&mk(&author, Timestamp::now())));
        // The author's own tombstone suppresses versions UP TO its time.
        let older = mk(&author, Timestamp::from(now_secs() - 100));
        let newer = mk(&author, Timestamp::from(now_secs() + 100));
        let del = kind5(vec![Tag::custom("a", [cross])], &author);
        record(&del, false);
        assert!(is_deleted(&older));
        assert!(!is_deleted(&newer)); // re-publish supersedes
    }

    /// The sweep's TTL: should_check fires once per key per window.
    #[test]
    fn check_ttl() {
        let _env = EnvGuard::new("del-ttl");
        assert!(should_check("abc123"));
        assert!(!should_check("abc123"));
        assert!(should_check("def456"));
        // Oversized keys are never checked (can't be tombstoned either).
        assert!(!should_check(&"x".repeat(MAX_KEY + 1)));
    }
}
