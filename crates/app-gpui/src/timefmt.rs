//! Timestamp display modes — relative, absolute date, Bitcoin block height
//! (Phase 2 §4.4). TODO(port): feather AGENTS.md — blocktime.ts + the
//! tap-to-cycle timestamp.
//!
//! theme.rs's split, mirrored: the mode lives in a plain static (the source
//! of truth, readable from cx-less helpers), a unit gpui-global is the
//! wake-up call (`set_global` fires every observing view), and the Bridge
//! global carries the core handle plus the per-timestamp ask dedup so
//! Block mode can refine itself from item closures.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use satori_core::manager::Core;

/// The relative form (render/util.ts style) — "now", "3m", "2h", "4d", "7w".
/// Owns the shape every timestamp site used to call through feed.rs.
pub(crate) fn rel_time(secs: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let d = now.saturating_sub(secs);
    match d {
        0..60 => "now".into(),
        _ if d < 3600 => format!("{}m", d / 60),
        _ if d < 86400 => format!("{}h", d / 3600),
        _ if d < 604_800 => format!("{}d", d / 86400),
        _ => format!("{}w", d / 604_800),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Relative,
    Date,
    Block,
}

/// The mode is the source of truth (theme.rs's `CURRENT` split): cx-less
/// helpers read it directly; the [`Signal`] global is only the wake-up.
static MODE: AtomicU8 = AtomicU8::new(0);

fn mode() -> Mode {
    match MODE.load(Ordering::Relaxed) {
        1 => Mode::Date,
        2 => Mode::Block,
        _ => Mode::Relative,
    }
}

/// The wake-up global (theme.rs ThemeSignal pattern): set on a mode cycle
/// and when a block height lands — every timestamp-bearing view observes
/// it and repaints, and the item closures re-read the mode and the cache.
pub struct Signal;

impl gpui::Global for Signal {}

/// The block-lookup bridge: the core handle + per-timestamp dedup of asks.
/// Set once at startup (main.rs, before any window renders).
pub struct Bridge {
    core: Arc<Core>,
    asked: HashSet<u64>,
    /// Failed asks, with when — the retry cool-off. A failed fetch used to
    /// un-ask and wake the window, and the next render re-asked: a
    /// whole-window repaint loop for as long as mempool.space stayed
    /// unreachable (every visible Block timestamp, every cycle).
    failed: HashMap<u64, Instant>,
}

impl gpui::Global for Bridge {}

/// How long a failed blocktime ask cools off before a render may re-ask.
/// Mempool outages (offline, Tor without mempool) should cost one probe
/// per timestamp per window, not a wake storm.
const RETRY_COOL_OFF: Duration = Duration::from_secs(300);

/// Install the bridge (main.rs — one core, one bridge).
pub fn init(core: Arc<Core>, cx: &mut gpui::App) {
    cx.set_global(Bridge {
        core,
        asked: HashSet::new(),
        failed: HashMap::new(),
    });
}

/// Watch for mode/height changes (beside theme::watch, in the constructor
/// of any view that renders timestamps).
pub fn watch<V: 'static>(cx: &mut gpui::Context<V>) {
    cx.observe_global::<Signal>(|_, cx| cx.notify()).detach();
}

/// Tap on a timestamp: relative → date → block → relative.
pub fn cycle(cx: &mut gpui::App) {
    let next = match MODE.load(Ordering::Relaxed) {
        0 => 1,
        1 => 2,
        _ => 0,
    };
    MODE.store(next, Ordering::Relaxed);
    cx.set_global(Signal);
}

/// The display string for `secs` under the current mode. Block mode falls
/// back to relative until the height is known — the `ensure` call in the
/// same item closure fetches it, and the landing re-renders.
pub fn stamp(secs: u64, cx: &gpui::App) -> String {
    match mode() {
        Mode::Relative => rel_time(secs),
        Mode::Date => date(secs),
        Mode::Block => match cx
            .try_global::<Bridge>()
            .and_then(|b| b.core.block_height_cached(secs))
        {
            Some(h) => format!("#{h}"),
            None => rel_time(secs),
        },
    }
}

/// Mode-aware text for surfaces without a `cx` (article summary cards,
/// quote embeds): date mode still shows dates; block keeps relative —
/// there's no bridge on those paths to fetch heights with.
pub fn stamp_text(secs: u64) -> String {
    match mode() {
        Mode::Date => date(secs),
        _ => rel_time(secs),
    }
}

/// Ensure a block lookup is in flight for `secs` (call from the same item
/// closure that rendered `stamp`). Silent no-ops: no bridge yet, height
/// already exact, already asked, or a failed ask still cooling off (one
/// probe per RETRY_COOL_OFF per timestamp — the table converges on
/// exactness without a failure wake-storm).
pub fn ensure(secs: u64, cx: &mut gpui::App) {
    if !cx.has_global::<Bridge>() {
        return;
    }
    let rx = {
        let bridge = cx.global_mut::<Bridge>();
        if bridge.core.block_height_known(secs) {
            return;
        }
        // A failed ask cools off: no probe, no wake while the window runs.
        // Past the window this render retries — clear the gates so the
        // re-ask below re-books it.
        if bridge
            .failed
            .get(&secs)
            .is_some_and(|t| t.elapsed() < RETRY_COOL_OFF)
        {
            return;
        }
        bridge.failed.remove(&secs);
        if !bridge.asked.insert(secs) {
            return;
        }
        bridge.core.block_height(secs)
    };
    cx.spawn(async move |cx| {
        let res = { let mut rx = rx; rx.recv().await };
        cx.update(|cx| {
            if cx.has_global::<Bridge>() {
                let bridge = cx.global_mut::<Bridge>();
                if res.is_none_or(|r| r.is_err()) {
                    bridge.asked.remove(&secs);
                    bridge.failed.insert(secs, Instant::now());
                } else {
                    bridge.failed.remove(&secs);
                }
            }
            // Wake the watchers either way: a landing repaints into the
            // exact height; a failure leaves the fallback standing (and
            // the cool-off keeps that wake from re-probing in a loop).
            cx.set_global(Signal);
        });
    })
    .detach();
}

/// The absolute form — local time, ISO-ish, the mode a timestamp reader
/// actually wants when a note is weeks old.
fn date(secs: u64) -> String {
    chrono::DateTime::from_timestamp(secs as i64, 0)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}
