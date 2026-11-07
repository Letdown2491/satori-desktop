//! Per-relay latency profiles, adaptive budgets and read-dead memory —
//! port of `src/data/relay-latency.ts` (phase 1 = observation): a rolling
//! latency profile per relay, built from the queries we already run many
//! times a day (no extra network). NOTHING reads it until the fan-out
//! turns profiles into per-relay deadline budgets — a relay we've LEARNED
//! is slow gets more rope without slowing the fast majority; one that
//! never delivers bails fast. Self-calibrating to the deployment: on a
//! slow-egress host every relay profiles slow (feeds come back complete);
//! on a fast host they profile fast (feeds stay snappy). Recorded from the
//! pool on each SINGLE-relay query (the outbox fan-out path) —
//! multi-relay queries can't attribute timing to one relay.
//!
//! PERSISTED (debounced ~15s, json-store like the TS) so the profile
//! survives restarts. Bounded: MAX_RELAYS LRU.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::config;

/// LRU cap so the file can't grow without bound (relay-latency.ts).
const MAX_RELAYS: usize = 500;
/// EWMA weight for a new sample (moderate smoothing over the day's queries).
const ALPHA: f64 = 0.3;
/// Adaptive-budget knobs (relay-latency.ts, tuned against real outbox-only
/// data 2026-07-02). The real signal turned out to be `ev` (events
/// delivered): slow relays deliver ~NOTHING for the feed, so the win is
/// CUTTING them, not extending them.
const N_MIN: u32 = 5; // min samples before the profile is trusted
const EV_EMPTY: f64 = 1.0; // avg events/query below this = empty FOR YOUR FEED
const EMPTY_Q: f64 = 0.15; // selection weight for a known-empty relay
const TRUNC_HI: f64 = 0.3; // a DELIVERING relay truncating above this looks starved
const RICH_EV: f64 = 20.0; // ...and only if it delivers this many events/query
/// Read-dead classification: an explicit DENIAL is trusted immediately for
/// DENIAL_TTL; the blaster PROFILE SHAPE needs DEAD_N samples, ~zero
/// events, and it (almost) never EOSEs (rides every query to the cap). A
/// merely QUIET relay EOSEs empty fast and is NOT dead — it stays on the
/// EMPTY_Q deprioritization.
const DENIAL_TTL_SECS: u64 = 12 * 3600;
const DEAD_N: u32 = 8;
const DEAD_TRUNC: f64 = 0.9;
/// Write-dead classification (D36): WRITE_DEAD_N consecutive publish
/// failures (no OK from the relay) drop it from the write fan-out; any
/// success resets the count. Only the all-dead fallback in
/// outbox_queue::targets keeps attempting dead relays, so revival always
/// remains possible.
const WRITE_DEAD_N: u32 = 3;
/// Flush cadence (relay-latency.ts debouncedFlush: 15s).
const FLUSH_SECS: u64 = 15;

/// Rolling per-relay stats (relay-latency.ts RelayStat). `ms` = EWMA of
/// time-to-last-event (relay responsiveness); `ev` = EWMA of events
/// delivered per query; `n` = samples; `trunc` = how often the query rode
/// to the hard cap while the relay was still delivering; `at` =
/// last-updated (epoch secs). `ev` disambiguates the two truncation modes
/// `ms` alone conflates: truncation with ~0 ev is a CONNECT/DELIVERY
/// problem (waiting longer is HIGH value), truncation with high ev is a
/// prolific aggregator (waiting longer just fetches older tail scroll
/// recovers anyway — so ceiling it). `denied_at` = the relay explicitly
/// REFUSED a read (CLOSED our REQ with a denial) — the read-dead signal.
/// `write_fails` = consecutive publish failures (D36) — the write-dead
/// signal; any success resets it.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct RelayStat {
    ms: u64,
    ev: f64,
    n: u32,
    trunc: u32,
    #[serde(default)]
    at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    denied_at: Option<u64>,
    #[serde(default, skip_serializing_if = "is_zero")]
    write_fails: u32,
    /// Last PROOF OF LIFE on this relay (epoch secs): an event actually
    /// delivered (record with events > 0, a live-bell arrival —
    /// mark_delivery), or a publish ACK (record_write_result true). The
    /// rail's live dot reads this (connected); a record that carries no
    /// success leaves it untouched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ok_at: Option<u64>,
}

fn is_zero(v: &u32) -> bool {
    *v == 0
}

struct Stats {
    map: HashMap<String, RelayStat>,
    order: VecDeque<String>,
}

static STATS: OnceLock<Mutex<Stats>> = OnceLock::new();
static DIRTY: AtomicBool = AtomicBool::new(false);

fn stats() -> &'static Mutex<Stats> {
    STATS.get_or_init(|| {
        let mut s = Stats { map: HashMap::new(), order: VecDeque::new() };
        for (url, stat) in read_disk() {
            s.map.insert(url.clone(), stat);
            s.order.push_back(url);
        }
        while s.order.len() > MAX_RELAYS {
            let old = s.order.pop_front();
            if let Some(old) = old {
                s.map.remove(&old);
            }
        }
        spawn_flusher();
        Mutex::new(s)
    })
}

fn read_disk() -> Vec<(String, RelayStat)> {
    let Ok(raw) = std::fs::read_to_string(config::state_file(config::files::RELAY_LATENCY)) else {
        return Vec::new();
    };
    serde_json::from_str::<HashMap<String, RelayStat>>(&raw)
        .map(|m| m.into_iter().collect())
        .unwrap_or_default()
}

fn write_disk() {
    let held = stats().lock().unwrap();
    let mut out = serde_json::Map::new();
    for (url, s) in &held.map {
        out.insert(url.clone(), serde_json::to_value(s).unwrap_or_default());
    }
    drop(held);
    let path = config::state_file(config::files::RELAY_LATENCY);
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
        .name("latency-flush".into())
        .spawn(|| loop {
            std::thread::park_timeout(std::time::Duration::from_secs(FLUSH_SECS));
            if DIRTY.swap(false, Ordering::Relaxed) {
                write_disk();
            }
        })
        .ok();
}

fn key(url: &str) -> String {
    crate::outbox::relay_key(url)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Record one relay's observed query time (ms), events delivered, and
/// whether it rode to the hard cap (relay-latency.ts recordLatency). Cheap
/// (a map update + a debounced write); side-effect only — it never changes
/// what the query returns.
pub fn record(relay: &str, ms: u64, truncated: bool, events: usize) {
    let url = key(relay);
    if url.is_empty() {
        return;
    }
    let mut held = stats().lock().unwrap();
    held.order.retain(|u| u != &url);
    let prev = held.map.get(&url).cloned();
    // EWMA with the alpha fold-in; seed ev from the current sample when
    // absent (first sample) so the EWMA can't NaN.
    let ev = match prev.as_ref() {
        Some(p) => p.ev * (1.0 - ALPHA) + events as f64 * ALPHA,
        None => events as f64,
    };
    let ms2 = match prev.as_ref() {
        Some(p) => (p.ms as f64 * (1.0 - ALPHA) + ms as f64 * ALPHA) as u64,
        None => ms,
    };
    let stat = RelayStat {
        ms: ms2,
        ev,
        n: prev.as_ref().map(|p| p.n + 1).unwrap_or(1),
        trunc: prev.as_ref().map(|p| p.trunc + u32::from(truncated)).unwrap_or(u32::from(truncated)),
        at: now_secs(),
        denied_at: prev.as_ref().and_then(|p| p.denied_at),
        write_fails: prev.as_ref().map(|p| p.write_fails).unwrap_or(0),
        ok_at: if events > 0 {
            Some(now_secs())
        } else {
            prev.as_ref().and_then(|p| p.ok_at)
        },
    };
    held.map.insert(url.clone(), stat);
    held.order.push_back(url);
    while held.order.len() > MAX_RELAYS {
        let old = held.order.pop_front();
        if let Some(old) = old {
            held.map.remove(&old);
        }
    }
    drop(held);
    DIRTY.store(true, Ordering::Relaxed);
}

/// Adaptive hard-cap for a single relay from its latency profile
/// (relay-latency.ts relayBudget):
/// - EMPTY (delivers ~nothing across many queries) → `floor`: bail fast.
///   It returns nothing for your feed, so waiting the full cap is pure
///   wasted latency with no completeness cost. The main win.
/// - a real BACKLOG (delivers a lot AND still truncates at the cap) →
///   extend toward `ceiling`, proportional to its truncation rate. (Rare
///   on the outbox path — most slow relays are empty, not rich.)
/// - otherwise (delivers and finishes in time) or too little evidence →
///   `base`. Pure profile read; cheap per query.
pub fn budget(relay: &str, floor: Duration, base: Duration, ceiling: Duration) -> Duration {
    let held = stats().lock().unwrap();
    let Some(s) = held.map.get(&key(relay)) else {
        return base;
    };
    if s.n < N_MIN {
        return base; // not enough evidence yet → default
    }
    if s.ev < EV_EMPTY {
        return floor; // empty for your feed → bail fast
    }
    // Lifetime-average truncation rate (lags recent behavior; only gates
    // the rare rich-but-truncating branch — TS's own note).
    let trunc_rate = s.trunc as f64 / s.n as f64;
    if trunc_rate >= TRUNC_HI && s.ev >= RICH_EV {
        let ext = base.as_millis() as f64 + trunc_rate * (ceiling.as_millis() as f64 - base.as_millis() as f64);
        return Duration::from_millis(ext.min(ceiling.as_millis() as f64) as u64);
    }
    base
}

/// Note a relay explicitly refused to serve a read (relay-latency.ts
/// recordReadDenial: CLOSED our REQ with a refusal — a write-only blaster,
/// an auth wall, a paywall).
pub fn record_read_denial(relay: &str) {
    let url = key(relay);
    if url.is_empty() {
        return;
    }
    let mut held = stats().lock().unwrap();
    held.order.retain(|u| u != &url);
    let prev = held.map.get(&url).cloned();
    held.map.insert(
        url.clone(),
        RelayStat {
            denied_at: Some(now_secs()),
            at: now_secs(),
            ..prev.unwrap_or_default()
        },
    );
    held.order.push_back(url);
    while held.order.len() > MAX_RELAYS {
        let old = held.order.pop_front();
        if let Some(old) = old {
            held.map.remove(&old);
        }
    }
    drop(held);
    DIRTY.store(true, Ordering::Relaxed);
}

/// The read-dead shape test on one stat (shared by [`is_read_dead`] and
/// [`snapshot`], which must not take the lock twice).
fn stat_read_dead(s: &RelayStat) -> bool {
    if let Some(at) = s.denied_at
        && now_secs().saturating_sub(at) < DENIAL_TTL_SECS
    {
        return true;
    }
    s.n >= DEAD_N && s.ev < EV_EMPTY && s.trunc as f64 / s.n as f64 >= DEAD_TRUNC
}

/// True when reading from this relay is known-futile (relay-latency.ts
/// isReadDead): it denied us recently, or its profile is the
/// write-only-blaster shape. Routing treats read-dead as score 0 (never
/// picked) and shard skips drop it at query time.
pub fn is_read_dead(relay: &str) -> bool {
    let held = stats().lock().unwrap();
    held.map
        .get(&key(relay))
        .is_some_and(stat_read_dead)
}

/// Note a publish outcome for this relay (D36 write-dead learning — the
/// write-side twin of record_read_denial): a success resets the
/// consecutive-failure count, a failure increments it. WRITE_DEAD_N
/// consecutive failures make the relay write-dead.
pub fn record_write_result(relay: &str, ok: bool) {
    let url = key(relay);
    if url.is_empty() {
        return;
    }
    let mut held = stats().lock().unwrap();
    held.order.retain(|u| u != &url);
    let prev = held.map.get(&url).cloned();
    let fails = if ok {
        0
    } else {
        prev.as_ref().map(|p| p.write_fails).unwrap_or(0) + 1
    };
    let now = now_secs();
    held.map.insert(
        url.clone(),
        RelayStat {
            write_fails: fails,
            at: now,
            ok_at: if ok { Some(now) } else { prev.as_ref().and_then(|p| p.ok_at) },
            ..prev.unwrap_or_default()
        },
    );
    held.order.push_back(url);
    while held.order.len() > MAX_RELAYS {
        let old = held.order.pop_front();
        if let Some(old) = old {
            held.map.remove(&old);
        }
    }
    drop(held);
    DIRTY.store(true, Ordering::Relaxed);
}

/// True when publishing to this relay failed WRITE_DEAD_N times in a row
/// (D36): it leaves the write fan-out (outbox_queue::targets) until a
/// success — which only the all-dead fallback can produce — resets it.
pub fn is_write_dead(relay: &str) -> bool {
    let held = stats().lock().unwrap();
    held.map
        .get(&key(relay))
        .is_some_and(|s| s.write_fails >= WRITE_DEAD_N)
}

/// Proof-of-life mark for the LIVE stream path (manager.rs's bell): an
/// event arrived over this relay's socket. The query paths stamp the same
/// field inside record(); the bell delivers between queries, so it needs
/// its own (cheap) door.
pub fn mark_delivery(relay: &str) {
    let url = key(relay);
    if url.is_empty() {
        return;
    }
    let mut held = stats().lock().unwrap();
    match held.map.get_mut(&url) {
        Some(s) => s.ok_at = Some(now_secs()),
        // First contact on an unprofiled relay: a one-field seed. (No
        // `order` touch — the LRU discipline treats this as metadata, and
        // the next real query sample re-arrants it.)
        None => {
            held.map.insert(
                url,
                RelayStat { ok_at: Some(now_secs()), ..RelayStat::default() },
            );
        }
    }
}

/// The window the rail's live dot reads (0.2-design-spec §4.4): a relay
/// that delivered within the last five minutes counts as pool-liveness.
/// Deliberately minutes-wide — an idle-but-connected pool goes quiet
/// between the feed's queries, and the dot must not flicker on that.
const CONNECTED_WINDOW_SECS: u64 = 300;

/// Pool connectivity, honestly stated: did ANY relay deliver an event or
/// ACK a publish inside the window? This is NOT socket state (nostr-sdk
/// doesn't surface per-relay connection status we could trust) — it is
/// "the pool recently did something real". False therefore covers startup
/// (nothing proven yet) and full disconnection alike, which is exactly the
/// one quiet state §4.4 wants the rail to show: dot grey, nothing moves.
pub fn connected() -> bool {
    let now = now_secs();
    let held = stats().lock().unwrap();
    held.map
        .values()
        .any(|s| s.ok_at.is_some_and(|at| now.saturating_sub(at) < CONNECTED_WINDOW_SECS))
}

/// One relay's learned health, for the settings pane (WS4): the EWMA
/// latency, sample count, and the two dead-markers (D33 read-dead /
/// D36 write-dead). A copy — the pane renders it, the lock is never
/// held across a paint.
#[derive(Debug, Clone)]
pub struct RelayHealth {
    pub url: String,
    /// EWMA of time-to-last-event in ms (relay responsiveness).
    pub ms: u64,
    /// Samples behind the profile (low n = the numbers aren't trusted yet).
    pub n: u32,
    pub read_dead: bool,
    pub write_dead: bool,
}

/// Snapshot of every profiled relay, unordered (settings.rs relay-health
/// pane sorts and merges it with the configured list).
pub fn snapshot() -> Vec<RelayHealth> {
    let held = stats().lock().unwrap();
    held.map
        .iter()
        .map(|(url, s)| RelayHealth {
            url: url.clone(),
            ms: s.ms,
            n: s.n,
            read_dead: stat_read_dead(s),
            write_dead: s.write_fails >= WRITE_DEAD_N,
        })
        .collect()
}

/// Selection weight in (0,1] for outbox routing (relay-latency.ts
/// relayQuality — feeds routeAuthorsToRelays' relay_score). A relay we've
/// LEARNED delivers ~nothing for your feed is a wasted slot — its authors'
/// notes come from their other relays, or not at all — so weight it DOWN,
/// but never to zero: it's still picked when it's an author's only relay.
/// Unknown or low-confidence relays are neutral (1.0): don't penalize one
/// we haven't profiled — it may be great.
pub fn quality(relay: &str) -> f64 {
    let held = stats().lock().unwrap();
    let Some(s) = held.map.get(&key(relay)) else {
        return 1.0;
    };
    if s.n < N_MIN {
        return 1.0; // not enough evidence yet → neutral
    }
    if s.ev < EV_EMPTY {
        return EMPTY_Q; // empty for your feed → deprioritize (still pickable)
    }
    1.0
}

/// The read-denial refusal matcher (pool.ts READ_DENIAL_RE, de-regexed):
/// write-only blasters ("does not accept REQs"), auth walls we won't sign
/// for, paywalls. Matched against the relay's CLOSED reason only — our own
/// close reasons never look like this.
pub fn is_read_denial(reason: &str) -> bool {
    let lower = reason.to_lowercase();
    ["denied", "not accept", "restricted", "blocked", "forbidden", "auth-required", "payment"]
        .iter()
        .any(|m| lower.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::EnvGuard;

    #[test]
    fn ewma_and_caps() {
        let _env = EnvGuard::new("lat-ewma");
        record("wss://r.example", 1000, false, 10);
        record("wss://r.example", 3000, true, 0);
        let held = stats().lock().unwrap();
        let s = held.map.get("wss://r.example").unwrap();
        // ms: 1000*(0.7) + 3000*(0.3) = 1600
        assert_eq!(s.ms, 1600);
        // ev: 10*(0.7) + 0*(0.3) = 7
        assert!((s.ev - 7.0).abs() < 1e-9);
        assert_eq!(s.n, 2);
        assert_eq!(s.trunc, 1);
    }

    #[test]
    fn budget_branches() {
        let _env = EnvGuard::new("lat-budget");
        let (floor, base, ceiling) =
            (Duration::from_secs(2), Duration::from_secs(4), Duration::from_secs(8));
        // Unknown / low confidence → base.
        assert_eq!(budget("wss://x.example", floor, base, ceiling), base);
        // Fewer than N_MIN samples → base regardless of shape.
        for _ in 0..N_MIN - 1 {
            record("wss://empty.example", 4000, true, 0);
        }
        assert_eq!(budget("wss://empty.example", floor, base, ceiling), base);
        // EMPTY: many samples, ~zero events → floor (bail fast).
        for _ in 0..N_MIN {
            record("wss://empty.example", 4000, true, 0);
        }
        assert_eq!(budget("wss://empty.example", floor, base, ceiling), floor);
        // RICH but truncating → extended toward the ceiling.
        for _ in 0..N_MIN + 5 {
            record("wss://rich.example", 4000, true, 30);
        }
        let b = budget("wss://rich.example", floor, base, ceiling);
        assert!(b > base && b <= ceiling, "{b:?}");
        // Delivering and finishing in time → base.
        for _ in 0..N_MIN + 5 {
            record("wss://ok.example", 500, false, 10);
        }
        assert_eq!(budget("wss://ok.example", floor, base, ceiling), base);
    }

    #[test]
    fn read_dead_paths() {
        let _env = EnvGuard::new("lat-dead");
        // The blaster shape: many samples, ~zero events, always truncating.
        for _ in 0..DEAD_N + 1 {
            record("wss://blaster.example", 4000, true, 0);
        }
        assert!(is_read_dead("wss://blaster.example"));
        assert_eq!(quality("wss://blaster.example"), EMPTY_Q);
        // A quiet relay (EOSEs empty FAST, never truncates) is NOT dead.
        for _ in 0..DEAD_N + 1 {
            record("wss://quiet.example", 300, false, 0);
        }
        assert!(!is_read_dead("wss://quiet.example"));
        assert_eq!(quality("wss://quiet.example"), EMPTY_Q); // deprioritized, not banned
        // Unknown → neutral, not dead.
        assert!(!is_read_dead("wss://unknown.example"));
        assert_eq!(quality("wss://unknown.example"), 1.0);
        // An explicit denial is trusted immediately.
        record_read_denial("wss://wall.example");
        assert!(is_read_denial("CLOSED: auth-required: please sign in"));
        assert!(!is_read_denial("CLOSED by relay: connection closed cleanly"));
    }

    #[test]
    fn connected_reads_recent_proof_of_life() {
        let _env = EnvGuard::new("lat-connected");
        // NOTE: connected() scans the whole process-wide profile map, and
        // the other tests in this file record deliveries concurrently —
        // so the pool-level verdict is asserted only in the positive
        // direction; the negative case (grey) is asserted per-relay.
        // A query that delivered events proves life.
        record("wss://alive.example", 200, false, 5);
        assert!(connected());
        // A zero-delivery sample does NOT (a denial or an empty EOSE is
        // not proof the pool answers with content).
        record("wss://alive.example", 200, false, 0);
        assert!(connected()); // the earlier proof still stands (window)
        // A live-bell arrival on an unprofiled relay seeds the field.
        mark_delivery("wss://bell.example");
        assert!(connected());
        // A publish ACK proves life; a failure keeps whatever stood.
        record_write_result("wss://ack.example", true);
        assert!(connected());
        record_write_result("wss://ack.example", false);
        assert!(connected());
        // Per-relay honesty: a failure mark doesn't erase the success
        // stamp, and an unprofiled relay has none.
        let held = stats().lock().unwrap();
        let ack = held.map.get("wss://ack.example").unwrap();
        assert_eq!(ack.write_fails, 1);
        assert!(ack.ok_at.is_some());
        assert!(!held.map.contains_key("wss://never.example"));
    }
}
