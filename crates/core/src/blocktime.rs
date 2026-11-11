//! Bitcoin block heights by timestamp — the third timestamp mode (Phase 2
//! §4.4). TODO(port): feather AGENTS.md — blocktime.ts (the restore-height
//! estimator, translated to a display service).
//!
//! A tap-cycled timestamp needs `#870123` for arbitrary note times. The
//! lookup ladder is Feather's, minus the wallet specifics:
//!
//! 1. LRU of exact answers (the hot path, cap 512);
//! 2. the permanent table (timestamp → height) — every fetch records its
//!    whole batch, so a region densifies and neighbors stop fetching;
//! 3. linear interpolation from the nearest known point (10-minute cadence
//!    is Bitcoin's promise; jitter inside a freshly fetched window is
//!    absorbed by taking the last block mined at-or-before the timestamp);
//! 4. one guarded HTTPS fetch to mempool.space — the SSRF-guarded
//!    `media::fetch_json_uncached` path, fixed host so the URL is a
//!    constant plus an integer — single-flight per timestamp.
//!
//! The table persists to `blocktime.json` (0600) and seeds from a
//! compiled-in anchor so interpolation works on first run, before any
//! network answer.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

/// mempool.space's REST root — the fixed allowlist host (Phase 2 §4.4).
const HOST: &str = "https://mempool.space/api";
/// Compiled-in seed: height 870_000 mined at this unix time (verified live
/// against the same API). Interpolation's foothold before any fetch.
const ANCHOR: (u64, u64) = (1_731_419_746, 870_000);
/// LRU capacity — Feather's 512-entry hot cache.
const LRU_CAP: usize = 512;
/// Ten minutes in seconds: the network's nominal cadence (the estimator's
/// slope) — and 16 blocks' worth is "dense": a freshly fetched window
/// brackets the timestamp, so its last at-or-before block IS the answer.
const DENSE_SECS: u64 = 16 * 600;

/// The answer channel (`Core::block_height`). One timestamp, one resolution:
/// the fetch failure reason on Err — waiters retry by asking again later
/// (the UI bridge forgets failed asks).
pub type BlockRx = mpsc::UnboundedReceiver<Result<u64, String>>;

pub struct Blocktime {
    rt: tokio::runtime::Handle,
    inner: Mutex<Inner>,
}

struct Inner {
    /// Permanent table, timestamp → height. Grows by ≤15 per fetch; each
    /// point makes its neighborhood interpolation-free.
    points: BTreeMap<u64, u64>,
    /// Exact answers, cap [`LRU_CAP`] (the per-frame hot path).
    lru: HashMap<u64, u64>,
    /// Eviction order for `lru` (dedup on touch).
    order: VecDeque<u64>,
    /// Single-flight: timestamp → waiters for the one fetch in flight.
    inflight: HashMap<u64, Vec<mpsc::UnboundedSender<Result<u64, String>>>>,
}

impl Blocktime {
    /// Loads the persisted table (missing file = first run) and seeds the
    /// compiled-in anchor.
    pub fn new(rt: tokio::runtime::Handle) -> Self {
        let mut points: BTreeMap<u64, u64> =
            std::fs::read_to_string(crate::config::state_file(crate::config::files::BLOCKTIME))
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
        points.entry(ANCHOR.0).or_insert(ANCHOR.1);
        Self {
            rt,
            inner: Mutex::new(Inner {
                points,
                lru: HashMap::new(),
                order: VecDeque::new(),
                inflight: HashMap::new(),
            }),
        }
    }

    /// The display-ready answer: exact when known, interpolated from the
    /// nearest point otherwise. Pure read — per-frame safe, never fetches.
    pub fn cached(&self, ts: u64) -> Option<u64> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(h) = inner.lru.get(&ts).copied() {
            inner.touch(ts, h);
            return Some(h);
        }
        if let Some(h) = inner.points.get(&ts).copied() {
            return Some(h);
        }
        Some(interp_height(&inner.points, ts))
    }

    /// Is there an exact (fetched or recorded) height for this timestamp?
    /// Interpolation doesn't count — that's what a fetch refines.
    pub fn known(&self, ts: u64) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.lru.contains_key(&ts) || inner.points.contains_key(&ts)
    }

    /// Resolve a timestamp: hot answers come back immediately; otherwise
    /// the first asker spawns the one fetch and later askers join it
    /// (single-flight). Call from the UI's ensure path — the answer lands
    /// on every waiter's channel.
    pub fn get(self: &Arc<Self>, ts: u64) -> BlockRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut inner = self.inner.lock().unwrap();
        if let Some(h) = inner
            .lru
            .get(&ts)
            .copied()
            .or_else(|| inner.points.get(&ts).copied())
        {
            inner.touch(ts, h);
            let _ = tx.send(Ok(h));
            return rx;
        }
        match inner.inflight.entry(ts) {
            std::collections::hash_map::Entry::Occupied(mut e) => {
                e.get_mut().push(tx);
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(vec![tx]);
                let this = Arc::clone(self);
                self.rt.spawn(async move { this.resolve(ts).await });
            }
        }
        rx
    }

    /// The one fetch for a timestamp: estimate → guarded HTTPS → merge the
    /// batch into the table → refine → answer. Locks are short; no await
    /// under the mutex. A timestamp already bracketed densely by the
    /// table (a fetched window) answers offline — one fetch per region,
    /// not one per note.
    async fn fetch_and_refine(&self, ts: u64) -> Result<u64, String> {
        let mut points = { self.inner.lock().unwrap().points.clone() };
        if densely_bracketed(&points, ts) {
            return Ok(interp_height(&points, ts));
        }
        let est = interp_height(&points, ts);
        let batch = fetch_blocks(est).await?;
        for (bts, bh) in batch {
            points.insert(bts, bh);
        }
        let answer = interp_height(&points, ts);
        let mut inner = self.inner.lock().unwrap();
        for (bts, bh) in points {
            inner.points.entry(bts).or_insert(bh);
        }
        Ok(answer)
    }

    /// Fetch → record → wake every waiter → persist. The spawn target of
    /// `get` (first asker only).
    async fn resolve(self: Arc<Self>, ts: u64) {
        let res = self.fetch_and_refine(ts).await;
        let snapshot = {
            let mut inner = self.inner.lock().unwrap();
            if let Ok(h) = res {
                inner.record(ts, h);
            }
            if let Some(waiters) = inner.inflight.remove(&ts) {
                for tx in waiters {
                    let _ = tx.send(res.clone());
                }
            }
            inner.points.clone()
        };
        persist(&snapshot);
    }
}

impl Inner {
    /// Record an exact answer: permanent table + hot LRU (eviction on cap).
    fn record(&mut self, ts: u64, h: u64) {
        self.points.insert(ts, h);
        self.touch(ts, h);
    }

    /// LRU touch — move to the back, insert if absent, evict the front
    /// past the cap.
    fn touch(&mut self, ts: u64, h: u64) {
        self.order.retain(|t| *t != ts);
        self.order.push_back(ts);
        self.lru.insert(ts, h);
        while self.order.len() > LRU_CAP
            && let Some(old) = self.order.pop_front()
        {
            self.lru.remove(&old);
        }
    }
}

/// Is `ts` tightly bracketed by known points on both sides? A bracket
/// narrower than 16 blocks' cadence is a fetched consecutive window —
/// its exact height is a table lookup, no network.
fn densely_bracketed(points: &BTreeMap<u64, u64>, ts: u64) -> bool {
    match (
        points.range(..=ts).next_back().map(|(t, _)| *t),
        points.range(ts..).next().map(|(t, _)| *t),
    ) {
        (Some(before), Some(after)) => after - before < DENSE_SECS,
        _ => false,
    }
}

/// The height at `ts` from the nearest known point(s), 600s cadence:
/// exact hit → dense bracket (a fetched window — last at-or-before block
/// IS the answer) → linear interpolation → boundary extrapolation → the
/// compiled-in anchor. Pure, so the tests can pin each rung.
fn interp_height(points: &BTreeMap<u64, u64>, ts: u64) -> u64 {
    if let Some(h) = points.get(&ts) {
        return *h;
    }
    let before = points.range(..=ts).next_back().map(|(t, h)| (*t, *h));
    let after = points.range(ts..).next().map(|(t, h)| (*t, *h));
    match (before, after) {
        // A bracket narrower than 16 blocks' cadence is a fetched
        // consecutive window: the last block mined at-or-before ts IS
        // the answer (jitter absorbed — Feather's refine).
        (Some((t0, h0)), Some((t1, _))) if t1 - t0 < DENSE_SECS => h0,
        (Some((t0, h0)), _) => h0 + (ts - t0) / 600,
        (None, Some((t1, h1))) => h1.saturating_sub((t1 - ts) / 600),
        (None, None) => {
            let (t0, h0) = ANCHOR;
            (h0 as i64 + (ts as i64 - t0 as i64) / 600).max(0) as u64
        }
    }
}

/// One batch of real blocks: `≤15` consecutive heights with their
/// timestamps, going backward from `est`. mempool's shape: `[{height,
/// timestamp}, …]`. An empty answer (a height beyond the tip) falls back
/// to the tip batch.
async fn fetch_blocks(est: u64) -> Result<Vec<(u64, u64)>, String> {
    let mut v = crate::media::fetch_json_uncached(&format!("{HOST}/api/blocks/{est}")).await?;
    if v.as_array().is_none_or(|a| a.is_empty()) {
        v = crate::media::fetch_json_uncached(&format!("{HOST}/api/blocks")).await?;
    }
    let arr = v.as_array().ok_or("mempool: unexpected shape")?;
    Ok(arr
        .iter()
        .filter_map(|b| {
            let h = b.get("height")?.as_u64()?;
            let ts = b.get("timestamp")?.as_u64()?;
            Some((ts, h))
        })
        .collect())
}

/// Persist the table (0600, the state-file rule). Called per resolve —
/// a fetch is rare and the file is small.
fn persist(points: &BTreeMap<u64, u64>) {
    let Ok(json) = serde_json::to_string(points) else {
        return;
    };
    let path = crate::config::state_file(crate::config::files::BLOCKTIME);
    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        return;
    }
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .and_then(|mut f| f.write_all(json.as_bytes()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_falls_back_to_the_anchor() {
        let empty = BTreeMap::new();
        let ts = ANCHOR.0 + 6_000;
        assert_eq!(interp_height(&empty, ts), ANCHOR.1 + 10);
    }

    #[test]
    fn exact_hit() {
        let mut pts = BTreeMap::new();
        pts.insert(100, 5);
        assert_eq!(interp_height(&pts, 100), 5);
    }

    #[test]
    fn dense_bracket_takes_the_last_at_or_before_block() {
        // A fetched window's jitter (500s here, cadence 600) must not
        // drift the answer off the block that actually contains ts.
        let mut pts = BTreeMap::new();
        pts.insert(1_000, 100);
        pts.insert(1_500, 101);
        assert_eq!(interp_height(&pts, 1_300), 100);
    }

    #[test]
    fn sparse_bracket_interpolates() {
        let mut pts = BTreeMap::new();
        pts.insert(0, 0);
        pts.insert(6_000_000, 10_000);
        assert_eq!(interp_height(&pts, 3_000_000), 5_000);
    }

    #[test]
    fn extrapolates_before_the_first_point() {
        let mut pts = BTreeMap::new();
        pts.insert(6_000_000, 10_000);
        assert_eq!(interp_height(&pts, 0), 0);
    }

    #[test]
    fn extrapolates_after_the_last_point() {
        let mut pts = BTreeMap::new();
        pts.insert(1_000, 100);
        assert_eq!(interp_height(&pts, 2_200), 102);
    }

    #[test]
    fn dense_bracket_is_answerable_offline() {
        let mut pts = BTreeMap::new();
        pts.insert(1_000, 100);
        pts.insert(1_500, 101);
        assert!(densely_bracketed(&pts, 1_300));
        // Sparse gaps and tail positions still need the network.
        pts.insert(6_000_000, 10_000);
        assert!(!densely_bracketed(&pts, 3_000_000));
        assert!(!densely_bracketed(&pts, 7_000_000));
    }

    #[test]
    fn lru_evicts_at_the_cap() {
        let mut inner = Inner {
            points: BTreeMap::new(),
            lru: HashMap::new(),
            order: VecDeque::new(),
            inflight: HashMap::new(),
        };
        for i in 0..LRU_CAP as u64 + 1 {
            inner.record(i, i);
        }
        assert_eq!(inner.lru.len(), LRU_CAP);
        assert!(!inner.lru.contains_key(&0)); // oldest evicted
        assert!(inner.lru.contains_key(&(LRU_CAP as u64))); // newest stays
        // The permanent table keeps everything — LRU is only the hot set.
        assert_eq!(inner.points.len(), LRU_CAP + 1);
    }
}
