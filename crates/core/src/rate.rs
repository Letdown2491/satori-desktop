//! Reply-velocity counter for Unknown authors — the anti-bot amendment to
//! the D41 trust posture (DESIGN.md §8 D43).
//!
//! The notification tier gates NotInNetwork speech (receipts, D41/D42), but
//! an Unknown author is "the graph can't say yet" — so a bot farm of fresh
//! keys replying to your notes lands in the bell forever. Bot replies are
//! the cheapest notification attack, and velocity is an objective signal
//! (same family as dedup: bounded, explained, receipted): an Unknown author
//! who replies to the user's notes `REPLY_VELOCITY_LIMIT` times within
//! `REPLY_WINDOW_SECS` is treated as a mule — further replies in the window
//! are suppressed from the bell into the receipts store.
//!
//! Discipline (D24 style, all named consts):
//! - per-author ring of delivered-reply timestamps, authors capped at
//!   `AUTHOR_CAP` (oldest-inserted evicted — the `wot::insert_target` shape);
//! - the counter is fed ONLY by delivered replies (suppressed ones never
//!   join the window — suppression must not be self-justifying);
//! - the window self-resets: timestamps older than `REPLY_WINDOW_SECS` are
//!   evicted on every check, so silence for half an hour restores a clean
//!   count;
//! - per-session only: `clear()` at teardown, like the verdicts and the
//!   receipts. Nothing here is persisted.
//!
//! NotInNetwork replies never reach this module — they are gated by the
//! tier itself (rule 1), regardless of velocity.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::LazyLock;

use nostr::prelude::*;

/// The flood window (the "30 minutes" of D43).
pub const REPLY_WINDOW_SECS: u64 = 30 * 60;
/// Replies-to-the-user within the window that trip the mule verdict: the
/// `REPLY_VELOCITY_LIMIT`-th reply in the window is suppressed (1–4 pass
/// untouched), and every further reply inside the window joins it.
pub const REPLY_VELOCITY_LIMIT: usize = 5;
/// Authors tracked (D24 ceiling): a plain ring of stamps per author, capped
/// oldest-inserted-first — far past anything a session touches.
const AUTHOR_CAP: usize = 1000;

/// The human receipt reason a velocity-suppressed reply carries (the bell's
/// "see why" surface renders it as-is).
pub fn flood_reason() -> String {
    format!(
        "{} replies in {} min",
        REPLY_VELOCITY_LIMIT,
        REPLY_WINDOW_SECS / 60
    )
}

struct Rate {
    /// Author hex → delivered-reply timestamps (in-window ring).
    stamps: HashMap<String, VecDeque<u64>>,
    /// Insertion order of the author keys (the eviction LRU's order).
    order: VecDeque<String>,
}

static RATE: LazyLock<Mutex<Rate>> =
    LazyLock::new(|| Mutex::new(Rate { stamps: HashMap::new(), order: VecDeque::new() }));

impl Rate {
    /// Drop stamps older than the window, then drop dead authors.
    fn evict(&mut self, author: &str, now: u64) {
        if let Some(ring) = self.stamps.get_mut(author) {
            while let Some(&front) = ring.front() {
                if now.saturating_sub(front) < REPLY_WINDOW_SECS {
                    break;
                }
                ring.pop_front();
            }
            if ring.is_empty() {
                self.stamps.remove(author);
                self.order.retain(|k| k != author);
            }
        }
    }

    fn entry(&mut self, author: &str) -> &mut VecDeque<u64> {
        if !self.stamps.contains_key(author) {
            // D24 ceiling: evict the oldest-inserted author first.
            while self.stamps.len() >= AUTHOR_CAP {
                match self.order.pop_front() {
                    None => {
                        self.stamps.clear();
                        break;
                    }
                    Some(old) => {
                        self.stamps.remove(&old);
                    }
                }
            }
            self.order.push_back(author.to_string());
            self.stamps.insert(author.to_string(), VecDeque::new());
        }
        self.stamps.get_mut(author).unwrap()
    }
}

/// Would this reply (the author's `REPLY_VELOCITY_LIMIT`-th inside the
/// window) be suppressed? Pure check — records nothing. The tier's
/// cached-phase split uses it so both phases wear the same gate (the
/// phase-equality rule) without double-feeding the counter.
pub fn reply_flooded(author: &PublicKey, at: u64) -> bool {
    let mut r = RATE.lock().unwrap();
    let key = author.to_string();
    r.evict(&key, at);
    r.stamps.get(&key).is_some_and(|ring| ring.len() + 1 >= REPLY_VELOCITY_LIMIT)
}

/// Record one DELIVERED reply (the caller already checked `reply_flooded`
/// and is delivering — suppressed replies must not land here). Returns
/// false if the reply is the one that trips the velocity verdict (the
/// caller suppresses it instead); true if admitted and counted.
pub fn admit_reply(author: &PublicKey, at: u64) -> bool {
    let mut r = RATE.lock().unwrap();
    let key = author.to_string();
    r.evict(&key, at);
    {
        let ring = r.entry(&key);
        if ring.len() + 1 >= REPLY_VELOCITY_LIMIT {
            return false;
        }
        ring.push_back(at);
    }
    true
}

/// Session teardown: the counter is per-session identity state (D43) —
/// a different account must never inherit it.
pub fn clear() {
    RATE.lock().unwrap().stamps.clear();
    RATE.lock().unwrap().order.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::EnvGuard;

    fn t() -> u64 {
        1_700_000_000
    }

    #[test]
    fn four_pass_fifth_trips_then_window_resets() {
        let _env = EnvGuard::new("rate-trip");
        clear();
        let bot = Keys::generate();
        for i in 0..4u64 {
            assert!(
                admit_reply(&bot.public_key(), t() + i * 60),
                "reply {i} must pass"
            );
        }
        // The 5th reply inside the window trips the verdict…
        assert!(!admit_reply(&bot.public_key(), t() + 300));
        // …and every further reply in the window is suppressed too — the
        // counter was NOT fed by the suppressed one, so the ring is still
        // the 4 delivered stamps, each of which again trips.
        assert!(reply_flooded(&bot.public_key(), t() + 600));
        // Window reset: half an hour of silence and the author passes
        // untouched again (1–4 fresh replies start a new window).
        assert!(!reply_flooded(&bot.public_key(), t() + REPLY_WINDOW_SECS + 1));
        assert!(admit_reply(&bot.public_key(), t() + REPLY_WINDOW_SECS + 2));
        clear();
    }

    #[test]
    fn suppressed_replies_never_feed_the_counter() {
        let _env = EnvGuard::new("rate-nofeed");
        clear();
        let bot = Keys::generate();
        for i in 0..4u64 {
            admit_reply(&bot.public_key(), t() + i);
        }
        // Flood: check-only (the cached phase's gate) must not count —
        // otherwise every phase re-arm shortens the fuse.
        for i in 5..20u64 {
            assert!(reply_flooded(&bot.public_key(), t() + i));
        }
        // admit_reply at the 5th slot returns false (suppressed, uncounted);
        // the ring still holds exactly 4 stamps, so the window-reset test
        // below proves nothing leaked in.
        assert!(!admit_reply(&bot.public_key(), t() + 20));
        assert!(!reply_flooded(&bot.public_key(), t() + REPLY_WINDOW_SECS + 1));
        clear();
    }

    #[test]
    fn authors_are_capped_oldest_evicted() {
        let _env = EnvGuard::new("rate-cap");
        clear();
        let now = t();
        for _ in 0..(AUTHOR_CAP + 50) {
            let k = Keys::generate();
            admit_reply(&k.public_key(), now);
        }
        let r = RATE.lock().unwrap();
        assert_eq!(r.stamps.len(), AUTHOR_CAP, "cap holds");
        assert_eq!(r.order.len(), AUTHOR_CAP, "order mirrors the map");
        drop(r);
        clear();
    }
}
