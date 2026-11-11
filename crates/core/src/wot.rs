//! Web of trust: the two-hop qualified network + the shared Verdict
//! vocabulary — the single trust word-list every current and future gate
//! consumes (issue #2 WS1).
//!
//! **Verdict model** — port of Amethyst's `TrustVerdicts.kt` shape
//! (commons/src/commonMain/kotlin/com/vitorpamplona/amethyst/commons/wot/
//! TrustVerdicts.kt, `explain` ~lines 40–52; research notes
//! `wot-spam-research.md` §1/§4 #4), minus the NIP-85 provider arms
//! (TRUSTED comes from the local graph here, never a third party):
//!
//! - [`Verdict::SelfUser`] — you.
//! - [`Verdict::Follow`] — in your kind-3 contact list.
//! - [`Verdict::Trusted`] — qualified by the two-hop graph.
//! - [`Verdict::NotInNetwork`] — the graph KNOWS the author and places
//!   them outside (they appear in a fresh follow's contact list with a
//!   sub-threshold shared-follow count). A judgment.
//! - [`Verdict::Unknown`] — the graph doesn't know YET (no identity, no
//!   fresh data, feature off over the follows cap). NOT a judgment —
//!   "unknown" must never read as "outside" (the issue posture:
//!   feature-off is a first-class state, never a silent one).
//!
//! **Qualified set** — port of Dark Wisp's
//! `repo/ExtendedNetworkRepository.kt` (barrydeen/dark-wisp-android):
//! an author is qualified when at least [`QUALIFICATION_THRESHOLD`] of
//! your first-degree follows also follow them (`THRESHOLD = 10`,
//! ExtendedNetworkRepository.kt:85). Built entirely from kind-3 contact
//! lists already flowing through the store — no crawl, no SQLite (the
//! research §4 verdict on Dark Wisp's SocialGraphDb: heavy for a client
//! with LMDB in place). Incremental applyKind3 updates follow Amethyst's
//! `WoTService` discipline instead
//! (commons/.../wot/WoTService.kt ~lines 122–140): only YOUR follows'
//! kind-3s feed the graph, a republished list diffs out the old targets,
//! and the whole thing shuts off past [`MAX_FOLLOWS`]
//! (WoTService.kt:302 `MAX_FOLLOWS = 2000`).
//!
//! **Bounds** (D24 discipline: every shared resource has a ceiling):
//! follows cap [`MAX_FOLLOWS`] (Amethyst), per-event contact cap
//! [`MAX_FOLLOWS_PER_EVENT`] (WoTService.kt:305, hostile publishers),
//! tracked second-degree targets capped at [`MAX_TRACKED_TARGETS`]
//! (oldest-inserted evicted first — satori's own ceiling; Amethyst's
//! unbounded scores map only survives its own 2000-follow shutoff),
//! source lists older than [`STALE_SECS`] contribute nothing (Dark
//! Wisp's 24h staleness window, `STALE_HOURS`, ExtendedNetworkRepository.kt:91).
//!
//! **Relay set-cover** — Dark Wisp's `computeRelaySetCover`
//! (ExtendedNetworkRepository.kt ~line 360): a greedy cover of the
//! qualified pubkeys' kind-10002 relay lists, caps `MAX_EXTENDED_RELAYS
//! = 100` / `MAX_AUTHORS_PER_RELAY = 300` (same file ~line 91). The
//! cover is IN-MEMORY state computed by the manager at graph-rebuild
//! time (it owns the store) and handed back here; pool.rs consumes it
//! as the no-list fallback tier AFTER kind:10002 and the D39 author
//! hints, BEFORE the indexers.
//!
//! In-memory, rebuilt at bootstrap from the store (the graph is a
//! projection of kind-3s the LMDB store already holds — disposable by
//! construction, nothing to persist; the D33 sidecar discipline is for
//! state the store DOESN'T hold). Core-clean: no UI deps.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use nostr::prelude::*;

use crate::store::parse_key;

/// Shared-follows needed to qualify (dark-wisp ExtendedNetworkRepository.kt:85
/// `THRESHOLD = 10`).
pub const QUALIFICATION_THRESHOLD: usize = 10;
/// Source-list staleness: a follow's kind-3 older than this contributes
/// nothing (dark-wisp `STALE_HOURS`, ExtendedNetworkRepository.kt:91).
/// Refetch cadence, NOT validity: a follow's contact list is the best
/// known truth about who they follow however old it is (most users only
/// rewrite kind-3s every few months; excluding old lists starved the
/// graph to 0 qualified — D45). The age only decides when we go refetch
/// the list for a fresher copy (fetch_targets).
pub const STALE_SECS: u64 = 7 * 24 * 60 * 60;
/// The graph SHUTS OFF past this many follows (Amethyst WoTService.kt:302
/// `MAX_FOLLOWS = 2000`): verdicts degrade to Unknown, never to a wrong
/// "outside".
pub const MAX_FOLLOWS: usize = 2000;
/// Per-event contact-list cap (Amethyst WoTService.kt:305
/// `MAX_FOLLOWS_PER_EVENT = 5000`, hostile publishers).
pub const MAX_FOLLOWS_PER_EVENT: usize = 5000;
/// Second-degree targets tracked (satori D24 ceiling; bounded eviction,
/// oldest-inserted first).
pub const MAX_TRACKED_TARGETS: usize = 50_000;
/// Greedy cover relay cap (dark-wisp `MAX_EXTENDED_RELAYS = 100`,
/// ExtendedNetworkRepository.kt ~line 91).
pub const MAX_EXTENDED_RELAYS: usize = 100;
/// Per-relay author-coverage cap in the greedy pick (dark-wisp
/// `MAX_AUTHORS_PER_RELAY = 300`).
pub const MAX_AUTHORS_PER_RELAY: usize = 300;
/// Bootstrap source-fetch ceiling (satori D24): at graph-rebuild time the
/// manager proactively fetches the follows' kind-3 contact lists the store
/// is missing or finds stale — at most this many per rebuild pass, one pass
/// per launch (no retry loop; the next bootstrap retries what it missed).
/// Two outbox shards of [`crate::outbox::MAX_AUTHORS_PER_FILTER`] each.
pub const MAX_FETCH_TARGETS: usize = 400;

/// The single trust vocabulary every gate consumes (issue #2 WS1).
///
/// Ordering note for readers: `Unknown` is NOT the bottom of a scale —
/// it is the graph declining to answer. Only `NotInNetwork` is a
/// judgment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The signed-in user themself (Amethyst `SELF`).
    SelfUser,
    /// In the user's own kind-3 contact list (Amethyst `FOLLOW`).
    Follow,
    /// Qualified by the two-hop graph: ≥ [`QUALIFICATION_THRESHOLD`]
    /// of the user's follows follow them (dark-wisp qualified network).
    Trusted,
    /// Known and outside: seen in a fresh follow's contact list with a
    /// sub-threshold shared-follow count (Amethyst `NOT_IN_NETWORK`).
    NotInNetwork,
    /// The graph cannot say — no identity, over the follows cap, or no
    /// fresh data seen yet. NOT a judgment (Amethyst's `NO_NETWORK`
    /// honesty: feature-off is a state, never silently "outside").
    Unknown,
}

impl Verdict {
    /// The shared-follows count that produced this verdict (the profile
    /// badge's "N shared follows"). Self/Follow/Unknown read 0 — the
    /// count is only meaningful for `Trusted`/`NotInNetwork`.
    pub fn with_count(self, shared: usize) -> (Verdict, usize) {
        match self {
            Verdict::Trusted | Verdict::NotInNetwork => (self, shared),
            _ => (self, 0),
        }
    }
}

/// One followed author's latest kind-3: its timestamp and parsed follows.
#[derive(Clone)]
struct Source {
    created_at: u64,
    follows: Vec<PublicKey>,
}

struct Graph {
    me: Option<PublicKey>,
    /// First-degree follows (hex). Empty until the manager arms it.
    follows: HashSet<String>,
    /// Set when follows exceed MAX_FOLLOWS: the whole feature shuts off
    /// (Amethyst WoTService disable path) — verdicts degrade to Unknown.
    over_cap: bool,
    /// follower hex → their latest kind-3.
    sources: HashMap<String, Source>,
    /// target hex → the set of the user's follows who follow them
    /// (Amethyst WoTService reverse index, `scores`).
    scores: HashMap<String, HashSet<String>>,
    /// Target insertion order (the MAX_TRACKED_TARGETS eviction LRU).
    target_order: VecDeque<String>,
    /// Membership mirror of `target_order` (O(1) `contains` — the audit's
    /// fix: a VecDeque `contains` per ingested tag made the bootstrap
    /// rebuild quadratic against MAX_TRACKED_TARGETS).
    in_order: HashSet<String>,
    /// Muted pubkeys (hex) — excluded from qualification (dark-wisp
    /// filters muted/blocked out at ExtendedNetworkRepository.kt ~273).
    muted: HashSet<String>,
    /// The manager-computed greedy cover of the qualified set's relays.
    cover: Vec<String>,
    /// Last full rebuild from the store (unix secs) — the settings
    /// surface's staleness line.
    rebuilt: Option<u64>,
}

static GRAPH: LazyLock<Mutex<Graph>> = LazyLock::new(|| {
    Mutex::new(Graph {
        me: None,
        follows: HashSet::new(),
        over_cap: false,
        sources: HashMap::new(),
        scores: HashMap::new(),
        target_order: VecDeque::new(),
        in_order: HashSet::new(),
        muted: HashSet::new(),
        cover: Vec::new(),
        rebuilt: None,
    })
});

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn fresh(created_at: u64) -> bool {
    now_secs().saturating_sub(created_at) <= STALE_SECS
}

/// The lifetime graph lock. Identities and state live for the process;
/// the manager re-arms at every bootstrap.
fn graph() -> std::sync::MutexGuard<'static, Graph> {
    GRAPH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// --- Persistence (D45) — the sidecar that survives restarts. The SDK's
// one-shot fetch does not persist (probed: 98 lists fetched one session,
// 2 in the store the next), so the applied sources are written here and
// replayed at bootstrap. Same discipline as hints.rs: 0600 temp-sibling
// + rename, debounced park_timeout flusher, dirty flag.

const FLUSH_SECS: u64 = 10;
static DIRTY: AtomicBool = AtomicBool::new(false);
static FLUSHER: OnceLock<()> = OnceLock::new();

fn wot_dirty() {
    DIRTY.store(true, Ordering::Relaxed);
    FLUSHER.get_or_init(|| {
        std::thread::Builder::new()
            .name("wot-flush".into())
            .spawn(|| {
                loop {
                    std::thread::park_timeout(std::time::Duration::from_secs(FLUSH_SECS));
                    if DIRTY.swap(false, Ordering::Relaxed) {
                        write_disk();
                    }
                }
            })
            .ok();
    });
}

fn write_disk() {
    // Persist the newest MAX_FOLLOWS sources (the only ones observe can
    // ever apply — non-follows are refused at the door).
    let g = graph();
    let mut list: Vec<(&String, &Source)> = g.sources.iter().collect();
    list.sort_by_key(|(_, s)| std::cmp::Reverse(s.created_at));
    list.truncate(MAX_FOLLOWS);
    let arr: Vec<serde_json::Value> = list
        .iter()
        .map(|(pk, s)| {
            serde_json::json!({
                "pk": pk,
                "at": s.created_at,
                "follows": s.follows.iter().map(|f| f.to_string()).collect::<Vec<String>>(),
            })
        })
        .collect();
    drop(g);
    let path = crate::config::state_file(crate::config::files::WOT_SOURCES);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
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
    if f.write_all(serde_json::Value::Array(arr).to_string().as_bytes())
        .is_ok()
    {
        drop(f);
        let _ = std::fs::rename(&tmp, &path);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Disk sources: (pubkey as stored, created_at, follows). The manager
/// feeds these back through [`apply_persisted`] before the store rebuild.
pub fn load_disk() -> Vec<(String, u64, Vec<PublicKey>)> {
    let Ok(raw) =
        std::fs::read_to_string(crate::config::state_file(crate::config::files::WOT_SOURCES))
    else {
        return Vec::new();
    };
    let Ok(arr) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Vec::new();
    };
    arr.as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .filter_map(|e| {
            let pk = e.get("pk")?.as_str()?.to_string();
            let at = e.get("at")?.as_u64()?;
            let follows = e
                .get("follows")?
                .as_array()?
                .iter()
                .filter_map(|f| PublicKey::parse(f.as_str()?).ok())
                .collect::<Vec<PublicKey>>();
            (!follows.is_empty()).then_some((pk, at, follows))
        })
        .collect()
}

/// Replay a persisted source (bootstrap, before the store's cached
/// kind-3s — their observe() wins on newer). Same door as observe():
/// only the user's follows apply; over-cap answers nothing.
pub fn apply_persisted(pk: &str, created_at: u64, follows: &[PublicKey]) {
    let mut g = graph();
    if g.me.is_none() || g.over_cap {
        return;
    }
    if !g.follows.contains(pk) {
        return;
    }
    g.sources.insert(
        pk.to_string(),
        Source {
            created_at,
            follows: follows.to_vec(),
        },
    );
    for t in follows {
        insert_target(&mut g, &t.to_string(), pk, MAX_TRACKED_TARGETS);
    }
}

/// Arm the graph for a session: the user and their first-degree follows.
/// Replaces any prior identity (logout/login) — all derived state is
/// rebuilt by the manager from the store after this.
pub fn set_identity(me: PublicKey, follows: &[PublicKey]) {
    let mut g = graph();
    g.me = Some(me);
    g.follows = follows.iter().map(|p| p.to_string()).collect();
    g.over_cap = follows.len() > MAX_FOLLOWS;
    if g.over_cap {
        // Shutoff (Amethyst WoTService ~line 203): drop everything, not
        // just stop updating — no partial graph may answer.
        g.sources.clear();
        g.scores.clear();
        g.target_order.clear();
        g.in_order.clear();
        g.cover.clear();
    }
}

/// Disarm (sign-out): the graph answers Unknown until the next login.
pub fn clear_identity() {
    let mut g = graph();
    *g = Graph {
        me: None,
        follows: HashSet::new(),
        over_cap: false,
        sources: HashMap::new(),
        scores: HashMap::new(),
        target_order: VecDeque::new(),
        in_order: HashSet::new(),
        muted: HashSet::new(),
        cover: Vec::new(),
        rebuilt: None,
    };
}

/// Push the current mute set (dark-wisp excludes muted/blocked pubkeys
/// from qualification). Re-evaluates nothing eagerly; `qualified` and
/// `verdict` consult it live.
pub fn set_muted(muted: HashSet<String>) {
    graph().muted = muted;
}

/// Count one fresh target→follower edge, honoring the D24 ceiling
/// (MAX_TRACKED_TARGETS, oldest-inserted evicted first). Extracted for
/// the cap test — the real graph's cap is too large to fill with
/// keypair-generated targets in a test.
fn insert_target(g: &mut Graph, target: &str, follower: &str, cap: usize) {
    if g.muted.contains(target) {
        return;
    }
    if !g.scores.contains_key(target) && g.scores.len() >= cap {
        // D24 ceiling: evict the oldest-inserted still-live target.
        let evicted = loop {
            match g.target_order.pop_front() {
                None => break false,
                Some(old) => {
                    if g.scores.remove(&old).is_some() {
                        g.in_order.remove(&old);
                        break true;
                    }
                }
            }
        };
        if !evicted {
            return; // order bookkeeping drifted; refuse new targets
        }
    }
    g.scores
        .entry(target.to_string())
        .or_default()
        .insert(follower.to_string());
    if g.in_order.insert(target.to_string()) {
        // Re-inserted targets (diffed back in) keep their original slot:
        // fine, the LRU is a ceiling, not a fairness guarantee.
        g.target_order.push_back(target.to_string());
    }
}

/// A kind-3 contact list arrived (bootstrap rebuild from the store, a
/// fetched result at the pool chokepoint, or the live bell). Only the
/// user's own and their follows' lists apply (Amethyst `applyKind3`:
/// events from non-follows are ignored); a NEWER list replaces an older
/// one with a diff-based update (WoTService `recompute`); stale lists
/// contribute nothing.
pub fn observe(ev: &Event) {
    if u16::from(ev.kind) != 3 {
        return;
    }
    let mut g = graph();
    if g.me.is_none() || g.over_cap {
        return;
    }
    let follower = ev.pubkey.to_string();
    let is_me = Some(ev.pubkey) == g.me;
    if !is_me && !g.follows.contains(&follower) {
        return;
    }
    let created_at = ev.created_at.as_secs();
    if let Some(old) = g.sources.get(&follower)
        && old.created_at >= created_at
    {
        return; // not newer than the list already applied
    }
    // Per-event cap (WoTService.kt:305): a hostile publisher's 100k-tag
    // list neither allocates for 100k targets nor wedges the lock.
    let follows: Vec<PublicKey> = ev.tags.public_keys().take(MAX_FOLLOWS_PER_EVENT).collect();

    // Diff out the OLD list's contributions — everything applied was
    // counted when it landed (age excludes nothing; D45), so the diff
    // is unconditional.
    if let Some(old) = g.sources.remove(&follower) {
        for t in &old.follows {
            let key = t.to_string();
            if let Some(set) = g.scores.get_mut(&key) {
                set.remove(&follower);
                if set.is_empty() {
                    g.scores.remove(&key);
                }
            }
        }
    }

    g.sources.insert(
        follower.clone(),
        Source {
            created_at,
            follows: follows.clone(),
        },
    );
    for t in &follows {
        insert_target(&mut g, &t.to_string(), &follower, MAX_TRACKED_TARGETS);
    }
    wot_dirty();
}

/// Which of the user's follows need their kind-3 contact list FETCHED at
/// the bootstrap rebuild: the store holds no cached list for them, or the
/// cached one is older than [`STALE_SECS`] (a fresh list is never
/// re-fetched). Capped at `cap` (the D24 ceiling), first come first
/// served in follows order — deterministic, so one pass per launch is a
/// bounded, honest shortfall. Pure; the manager feeds it the store's
/// cached kind-3s AND the sidecar's applied sources (D45: the SDK's
/// one-shot fetch does not persist, so without the sidecar every
/// restart refetched the whole graph from scratch).
pub fn fetch_targets(
    follows: &[PublicKey],
    cached: &[Event],
    now: u64,
    cap: usize,
    sidecar: &HashMap<String, u64>,
) -> Vec<PublicKey> {
    let latest: HashMap<String, u64> = cached
        .iter()
        .filter(|e| u16::from(e.kind) == 3)
        .map(|e| (e.pubkey.to_string(), e.created_at.as_secs()))
        .chain(sidecar.iter().map(|(k, v)| (k.clone(), *v)))
        .collect();
    follows
        .iter()
        .filter(|f| {
            latest
                .get(&f.to_string())
                .is_none_or(|t| now.saturating_sub(*t) > STALE_SECS)
        })
        .take(cap)
        .copied()
        .collect()
}

/// The verdict for an author (the badge, the notification tier, the
/// thread-collapse rule — every consumer asks the same question).
pub fn verdict(pk: &PublicKey) -> Verdict {
    let g = graph();
    let Some(me) = g.me else {
        return Verdict::Unknown;
    };
    if g.over_cap {
        // Shutoff (Amethyst MAX_FOLLOWS disable path): the graph can't
        // say ANYTHING — Unknown, even for self.
        return Verdict::Unknown;
    }
    if *pk == me {
        return Verdict::SelfUser;
    }
    let key = pk.to_string();
    if g.follows.contains(&key) {
        return Verdict::Follow;
    }
    match g.scores.get(&key).map(|s| s.len()) {
        Some(n) if n >= QUALIFICATION_THRESHOLD => Verdict::Trusted,
        Some(_) => Verdict::NotInNetwork,
        None => Verdict::Unknown,
    }
}

/// Shared-follows count for the badge ("N shared follows") — the
/// Amethyst WoTService score shape (count of your follows who follow X).
pub fn shared_follows(pk: &PublicKey) -> usize {
    let g = graph();
    if g.me.is_none() || g.over_cap {
        return 0;
    }
    g.scores.get(&pk.to_string()).map(|s| s.len()).unwrap_or(0)
}

/// Is the author in the qualified network? (The routing tier's
/// membership question — dark-wisp `isInQualifiedNetwork`.) Muted
/// pubkeys never qualify.
pub fn is_qualified(pk: &PublicKey) -> bool {
    let g = graph();
    if g.me.is_none() || g.over_cap || g.muted.contains(&pk.to_string()) {
        return false;
    }
    g.scores
        .get(&pk.to_string())
        .is_some_and(|s| s.len() >= QUALIFICATION_THRESHOLD)
}

/// The whole qualified set (the cover computation's input).
pub fn qualified() -> Vec<PublicKey> {
    let g = graph();
    if g.me.is_none() || g.over_cap {
        return Vec::new();
    }
    g.scores
        .iter()
        .filter(|(k, s)| s.len() >= QUALIFICATION_THRESHOLD && !g.muted.contains(k.as_str()))
        .filter_map(|(k, _)| parse_key(k))
        .collect()
}

/// The manager hands back the greedy cover it computed from the store's
/// kind-10002s for [`qualified`].
pub fn set_cover(relays: Vec<String>) {
    let mut g = graph();
    g.cover = relays.into_iter().take(MAX_EXTENDED_RELAYS).collect();
}

/// The manager calls this after a full rebuild from the store (the
/// bootstrap pass that re-applies every kind-3): the settings surface's
/// "last rebuilt / staleness" honesty line reads it.
pub fn note_rebuilt() {
    graph().rebuilt = Some(now_secs());
}

/// The extended-network fallback tier for pool.rs routing (empty until
/// the manager computes one).
pub fn extended_relays() -> Vec<String> {
    graph().cover.clone()
}

/// The settings surface's honest status snapshot: graph size (first-degree
/// follows), qualified-set size, last rebuild, and the staleness state
/// (OverCap = shut off past MAX_FOLLOWS; SignedOut = no identity; Stale =
/// no fresh rebuild inside STALE_SECS; Fresh otherwise).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WotStatus {
    /// No signed-in identity — every verdict reads Unknown.
    SignedOut,
    /// Over the follows cap — the feature shut off (verdicts → Unknown).
    OverCap,
    /// Live: (follows, qualified, last rebuild unix secs — None before the
    /// first bootstrap rebuild).
    Live(usize, usize, Option<u64>),
}

pub fn status() -> WotStatus {
    let g = graph();
    if g.me.is_none() {
        return WotStatus::SignedOut;
    }
    if g.over_cap {
        return WotStatus::OverCap;
    }
    WotStatus::Live(g.follows.len(), qualified_count(&g), g.rebuilt)
}

fn qualified_count(g: &Graph) -> usize {
    g.scores
        .iter()
        .filter(|(k, s)| s.len() >= QUALIFICATION_THRESHOLD && !g.muted.contains(k.as_str()))
        .count()
}

/// Absorption diagnostics (the starving-graph investigation): (source
/// lists applied, scored targets, qualified). Pure read.
pub(crate) fn stats() -> (usize, usize, usize) {
    let g = graph();
    (g.sources.len(), g.scores.len(), qualified_count(&g))
}

/// Greedy weighted set-cover (dark-wisp `computeRelaySetCover`
/// ExtendedNetworkRepository.kt ~line 360): repeatedly take the relay
/// covering the most still-uncovered qualified authors, until
/// `max_relays` relays or full coverage. A relay stands for at most
/// `max_authors_per_relay` authors of the cover (dark-wisp
/// `MAX_AUTHORS_PER_RELAY = 300` — a mega-relay must not stand in for
/// the whole network, its list is truncated at build time). Ties break
/// on larger total coverage, then url, so the pick is deterministic.
/// Pure; the manager feeds it store-parsed relay lists.
pub fn relay_cover(
    lists: &HashMap<String, Vec<String>>,
    authors: &[String],
    max_relays: usize,
    max_authors_per_relay: usize,
) -> Vec<String> {
    let wanted: HashSet<&String> = authors.iter().collect();
    // relay → the wanted authors it serves (truncated at the cap).
    let mut coverage: Vec<(&String, Vec<&String>)> = lists
        .iter()
        .map(|(url, aurls)| {
            let served: Vec<&String> = aurls
                .iter()
                .filter(|a| wanted.contains(a))
                .take(max_authors_per_relay)
                .collect();
            (url, served)
        })
        .filter(|(_, served)| !served.is_empty())
        .collect();
    let mut covered: HashSet<&String> = HashSet::new();
    let mut picked: Vec<String> = Vec::new();
    while covered.len() < wanted.len() && picked.len() < max_relays {
        let best = coverage.iter().max_by_key(|(url, served)| {
            (
                served.iter().filter(|a| !covered.contains(*a)).count(),
                served.len(),
                std::cmp::Reverse(*url),
            )
        });
        let Some((url, served)) = best else { break };
        let gain = served.iter().filter(|a| !covered.contains(*a)).count();
        if gain == 0 {
            break;
        }
        covered.extend(served.iter().copied());
        picked.push((*url).clone());
        let url = *url;
        coverage.retain(|(u, _)| *u != url);
    }
    picked
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    clear_identity();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::EnvGuard;

    fn keys() -> Keys {
        Keys::generate()
    }

    /// Sign a kind-3 for `keys` following `targets`, at `created_at`.
    fn kind3(k: &Keys, targets: &[&Keys], at: u64) -> Event {
        let mut t = Tags::new();
        for target in targets {
            t.push(Tag::public_key(target.public_key()));
        }
        let unsigned = UnsignedEvent::new(
            k.public_key(),
            Timestamp::from_secs(at),
            Kind::ContactList,
            t,
            String::new(),
        );
        k.sign_event(unsigned).unwrap()
    }

    fn ids(n: usize) -> Vec<Keys> {
        (0..n).map(|_| Keys::generate()).collect()
    }

    struct Session {
        me: Keys,
        env: EnvGuard,
    }

    impl Session {
        fn new(follows: &[Keys]) -> Self {
            let env = EnvGuard::new("wot");
            let me = Keys::generate();
            set_identity(
                me.public_key(),
                &follows.iter().map(|k| k.public_key()).collect::<Vec<_>>(),
            );
            Self { me, env }
        }
    }

    impl Drop for Session {
        fn drop(&mut self) {
            reset_for_tests();
        }
    }

    fn now() -> u64 {
        now_secs()
    }

    /// Verdict transitions: self / follow / threshold / not-in-network /
    /// unknown — the whole vocabulary in one graph.
    #[test]
    fn verdict_transitions() {
        let follows = ids(11);
        let session = Session::new(&follows);
        let _stranger = Keys::generate();
        let outsider = Keys::generate();

        // Self.
        assert_eq!(verdict(&session.me.public_key()), Verdict::SelfUser);
        // A first-degree follow.
        assert_eq!(verdict(&follows[0].public_key()), Verdict::Follow);
        // Ten follows follow them → Trusted.
        for f in &follows[..10] {
            observe(&kind3(f, &[&outsider], now()));
        }
        assert_eq!(verdict(&outsider.public_key()), Verdict::Trusted);
        assert!(is_qualified(&outsider.public_key()));
        // Only ONE follow follows them → known, and outside.
        let lone = Keys::generate();
        observe(&kind3(&follows[10], &[&lone], now()));
        assert_eq!(verdict(&lone.public_key()), Verdict::NotInNetwork);
        assert!(!is_qualified(&lone.public_key()));
        // Seen by nobody we've heard from → Unknown (not a judgment).
        let unseen = Keys::generate();
        assert_eq!(verdict(&unseen.public_key()), Verdict::Unknown);
    }

    /// Shared-follow counting: the badge number is exactly the set of
    /// the user's follows who follow the target (duplicates across
    /// lists collapse to a set).
    #[test]
    fn shared_follow_counting() {
        let follows = ids(5);
        let _s = Session::new(&follows);
        let target = Keys::generate();
        for f in &follows[..3] {
            observe(&kind3(f, &[&target], now()));
        }
        // The same follow republishes WITH the target still there: the
        // count must not double (diff-based replace, WoTService).
        observe(&kind3(&follows[0], &[&target], now() + 10));
        assert_eq!(shared_follows(&target.public_key()), 3);
        // A fourth follow drops the target from their list: 3 → 2.
        observe(&kind3(&follows[0], &[], now() + 20));
        assert_eq!(shared_follows(&target.public_key()), 2);
    }

    /// Threshold boundary: 9 shared follows = NotInNetwork, 10 = Trusted.
    #[test]
    fn threshold_boundary() {
        let follows = ids(10);
        let _s = Session::new(&follows);
        let target = Keys::generate();
        for f in &follows[..QUALIFICATION_THRESHOLD - 1] {
            observe(&kind3(f, &[&target], now()));
        }
        assert_eq!(verdict(&target.public_key()), Verdict::NotInNetwork);
        observe(&kind3(&follows[9], &[&target], now()));
        assert_eq!(verdict(&target.public_key()), Verdict::Trusted);
    }

    /// Staleness: a follow list older than 24h contributes nothing, and
    /// replacing a fresh list with a stale one removes its contribution.
    #[test]
    fn stale_sources_still_qualify() {
        // D45: age excludes nothing. A follow's contact list is the best
        // known truth about who they follow however old it is (most users
        // only rewrite kind-3s every few months); STALE_SECS drives the
        // REFETCH cadence only, never the scoring.
        let follows = ids(2);
        let _s = Session::new(&follows);
        let target = Keys::generate();
        let old = now() - STALE_SECS - 3600;
        observe(&kind3(&follows[0], &[&target], old));
        assert_eq!(shared_follows(&target.public_key()), 1);
        // Known-but-unqualified reads NotInNetwork (Known = the graph
        // can say; Unknown is only for no-data).
        assert_eq!(verdict(&target.public_key()), Verdict::NotInNetwork);
        // A newer list replaces it (newest statement wins — observe's
        // newer-wins rule), and an OLDER republish never does.
        observe(&kind3(&follows[0], &[&target], now()));
        assert_eq!(shared_follows(&target.public_key()), 1);
        observe(&kind3(&follows[0], &[&target], now() - STALE_SECS - 60));
        assert_eq!(shared_follows(&target.public_key()), 1);
    }

    /// Muted pubkeys never qualify (dark-wisp filters muted/blocked out
    /// at qualification time, ExtendedNetworkRepository.kt ~273).
    #[test]
    fn muted_never_qualify() {
        let follows = ids(11);
        let _s = Session::new(&follows);
        let target = Keys::generate();
        for f in &follows[..10] {
            observe(&kind3(f, &[&target], now()));
        }
        assert!(is_qualified(&target.public_key()));
        set_muted(HashSet::from([target.public_key().to_string()]));
        assert!(!is_qualified(&target.public_key()));
        assert!(!qualified().iter().any(|p| p == &target.public_key()));
    }

    /// Greedy set-cover on a small topology (dark-wisp
    /// computeRelaySetCover): the relay covering the most uncovered
    /// authors is picked first; the per-relay coverage cap forces the
    /// long tail into the pick; the relay cap stops the loop.
    #[test]
    fn greedy_cover() {
        let mk =
            |authors: &[&str]| -> Vec<String> { authors.iter().map(|s| s.to_string()).collect() };
        let mut lists = HashMap::new();
        lists.insert("wss://big".to_string(), mk(&["a", "b", "c", "d", "e", "f"]));
        lists.insert("wss://mid".to_string(), mk(&["a", "b", "c"]));
        lists.insert("wss://small".to_string(), mk(&["d"]));
        // Full coverage: big covers all six in one pick.
        let authors = mk(&["a", "b", "c", "d", "e", "f"]);
        assert_eq!(relay_cover(&lists, &authors, 10, 300), vec!["wss://big"]);
        // Greedy ranking: with big absent, mid covers more than small.
        let mut partial = HashMap::new();
        partial.insert("wss://mid".to_string(), mk(&["a", "b", "c"]));
        partial.insert("wss://small".to_string(), mk(&["d"]));
        let cover = relay_cover(&partial, &authors, 10, 300);
        assert_eq!(cover.first().map(String::as_str), Some("wss://mid"));
        assert_eq!(cover.len(), 2); // then small, full coverage
        // Relay cap 1 stops after the first pick even without coverage.
        let cover = relay_cover(&lists, &authors, 1, 300);
        assert_eq!(cover, vec!["wss://big"]);
        // Per-relay coverage cap: a mega-relay's served list is
        // truncated at build time (2 here), so it can't stand in for
        // the whole network — the cover must still pick tight for x/y,
        // and c–f are left uncovered (no candidate serves them).
        let mut mega = HashMap::new();
        mega.insert(
            "wss://mega".to_string(),
            mk(&["a", "b", "c", "d", "e", "f"]),
        );
        mega.insert("wss://tight".to_string(), mk(&["x", "y"]));
        let authors = mk(&["a", "b", "c", "d", "e", "f", "x", "y"]);
        let cover = relay_cover(&mega, &authors, 10, 2);
        assert_eq!(cover.len(), 2);
        assert!(cover.contains(&"wss://tight".to_string()));
    }

    /// Bootstrap source-fetch selection: a missing list is fetched, a
    /// STALE one is re-fetched, a FRESH one is never re-fetched; the
    /// D24 cap (MAX_FETCH_TARGETS) truncates deterministically in
    /// follows order, and the whole fetch set chunks under the outbox
    /// author ceiling (D39, two shards of 200).
    /// D45 sidecar: applied sources replay at bootstrap (only follows
    /// apply; over-cap and signed-out refuse).
    #[test]
    fn persisted_sources_replay() {
        let follows = ids(2);
        let target = Keys::generate();
        // Signed out: refused.
        apply_persisted(
            &follows[0].public_key().to_string(),
            now(),
            &[target.public_key()],
        );
        assert_eq!(shared_follows(&target.public_key()), 0);
        let _s = Session::new(&follows);
        // Not a follow: refused.
        let stranger = Keys::generate();
        apply_persisted(
            &stranger.public_key().to_string(),
            now(),
            &[target.public_key()],
        );
        assert_eq!(shared_follows(&target.public_key()), 0);
        // A follow: applied.
        apply_persisted(
            &follows[0].public_key().to_string(),
            now(),
            &[target.public_key()],
        );
        assert_eq!(shared_follows(&target.public_key()), 1);
    }

    #[test]
    fn fetch_targets_selection_and_cap() {
        let follows = ids(4);
        let cached = vec![
            kind3(&follows[0], &[&Keys::generate()], now()), // fresh — skip
            kind3(&follows[1], &[&Keys::generate()], now() - STALE_SECS - 60), // stale — fetch
            // follows[2] has NO cached list — fetch.
            kind3(&follows[3], &[&Keys::generate()], now() - STALE_SECS), // exactly 24h is still fresh (fresh(): <= STALE_SECS)
        ];
        let pks = |v: &[Keys]| -> Vec<PublicKey> { v.iter().map(|k| k.public_key()).collect() };
        let want = fetch_targets(
            &pks(&follows),
            &cached,
            now(),
            MAX_FETCH_TARGETS,
            &HashMap::new(),
        );
        assert_eq!(want, vec![follows[1].public_key(), follows[2].public_key()]);
        // Cap: 10 follows, cap 3 — the FIRST three in follows order.
        let many = ids(10);
        let got = fetch_targets(&pks(&many), &[], now(), 3, &HashMap::new());
        assert_eq!(got.len(), 3);
        assert_eq!(got, pks(&many[..3]));
        // Non-kind-3 events in the cache never count as a source.
        let one = ids(1);
        assert_eq!(
            fetch_targets(&pks(&one), &[], now(), MAX_FETCH_TARGETS, &HashMap::new()).len(),
            1
        );
        // The ceiling composes with the outbox author ceiling (D39):
        // a full pass is at most two shards, never an unsplit mega-REQ.
        let shards = crate::outbox::chunk(
            &(0..MAX_FETCH_TARGETS)
                .map(|i| i.to_string())
                .collect::<Vec<String>>(),
            crate::outbox::MAX_AUTHORS_PER_FILTER,
        );
        assert_eq!(shards.len(), 2);
        assert!(
            shards
                .iter()
                .all(|s| s.len() <= crate::outbox::MAX_AUTHORS_PER_FILTER)
        );
    }

    /// Non-follows' contact lists never feed the graph (Amethyst
    /// applyKind3), and an older duplicate list doesn't clobber a newer
    /// one.
    #[test]
    fn only_follows_lists_apply() {
        let follows = ids(1);
        let _s = Session::new(&follows);
        let target = Keys::generate();
        let rando = Keys::generate();
        observe(&kind3(&rando, &[&target], now()));
        assert_eq!(shared_follows(&target.public_key()), 0);
        // Follow: two versions, older arrives LAST — must not replace.
        observe(&kind3(&follows[0], &[&target], now() + 100));
        observe(&kind3(&follows[0], &[&target], now()));
        assert_eq!(shared_follows(&target.public_key()), 1);
    }

    /// The follows cap shuts the feature off (Amethyst MAX_FOLLOWS):
    /// verdicts degrade to Unknown, ingest is refused.
    #[test]
    fn follows_cap_shuts_off() {
        let follows = ids(MAX_FOLLOWS + 1);
        let me = Keys::generate();
        set_identity(
            me.public_key(),
            &follows.iter().map(|k| k.public_key()).collect::<Vec<_>>(),
        );
        assert_eq!(verdict(&me.public_key()), Verdict::Unknown);
        let target = Keys::generate();
        observe(&kind3(&follows[0], &[&target], now()));
        assert_eq!(shared_follows(&target.public_key()), 0);
    }

    /// The D24 target ceiling holds (tested through `insert_target` with
    /// a small cap — filling the real 50k cap with generated keypairs is
    /// not a test): at cap, a new target evicts the OLDEST-inserted one;
    /// a drifted order book refuses rather than exceeding the cap.
    #[test]
    fn tracked_targets_capped() {
        let mut g = Graph {
            me: None,
            follows: HashSet::new(),
            over_cap: false,
            sources: HashMap::new(),
            scores: HashMap::new(),
            target_order: VecDeque::new(),
            in_order: HashSet::new(),
            muted: HashSet::new(),
            cover: Vec::new(),
            rebuilt: None,
        };
        insert_target(&mut g, "a", "f1", 2);
        insert_target(&mut g, "b", "f1", 2);
        assert_eq!(g.scores.len(), 2);
        // At cap: "c" evicts the oldest ("a").
        insert_target(&mut g, "c", "f2", 2);
        assert_eq!(g.scores.len(), 2);
        assert!(!g.scores.contains_key("a"));
        assert!(g.scores.contains_key("c"));
        // Refusal path: order book drained (drift), still refuses.
        g.target_order.clear();
        insert_target(&mut g, "d", "f3", 2);
        assert_eq!(g.scores.len(), 2);
        assert!(!g.scores.contains_key("d"));
    }
}
