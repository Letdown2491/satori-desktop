//! Viewport-scoped engagement subscriptions — port of Dark Wisp
//! `viewmodel/FeedEngagementState.kt` semantics (notes/0.2-research.md §2.1):
//! `distantSubscriptions(nearbyIds)` + the `viewportRange(feedIds, visibleIds,
//! before, after)` helper. Per-note engagement REQs are subscribed only near
//! the viewport; distant rows fall back to the store/per-paint loads.
//!
//! Satori shape: the feed re-anchors a row cursor on every scroll event
//! (feed.rs key_nav / scroll stream); the settled cursor + visible end imply
//! an id window. THIS module is the pure half — window clamping, id
//! selection, filter chunking (outbox::chunk at the D39 e-tag ceiling) and
//! the subscribe-replacement bookkeeping (plan/commit, so a viewport move
//! always CLOSES the previous subscription before arming the next — no
//! leaked REQs, D24's no-unbounded-churn rule). manager.rs owns the live
//! half: one long-lived task that consumes the plan and rides pool.rs.

use nostr::prelude::*;
use nostr::event::Event;

use crate::outbox;

/// Rows of context on each side of the visible range (the Kotlin
/// `viewportRange(feedIds, visibleIds, before, after)` buffer): a small
/// wheel tick must not tear down a healthy subscription.
pub const VIEWPORT_BUFFER: usize = 8;

/// D24 ceiling on the live engagement set: the `#e` filter never carries
/// more than this many ids, whatever the window math implies (a huge
/// monitor at 1px rows still stops here). Well under the 150 e-tag filter
/// ceiling, so in practice ONE filter carries the whole viewport.
pub const MAX_LIVE_IDS: usize = 50;

/// The settle window (ms) the feed waits before pushing a viewport change —
/// fast scrolling must not spam subscribe/unsubscribe churn.
pub const SETTLE_MS: u64 = 400;

/// Engagement kinds the viewport subscription carries: kind 1 (replies
/// reference their parent via `e`), 6 (reposts), 7 (reactions), 9735 (zap
/// receipts), 9733/9734 (reaction-ish headers, same `e`-tag shape).
pub const ENGAGEMENT_KINDS: &[u16] = &[1, 6, 7, 9735, 9733, 9734];

/// The visible window plus buffer, clamped to the feed and capped at
/// `max` ids. `visible_start`/`visible_end` come from the scroll stream's
/// `visible_range` (or the j/k cursor for both, when the list hasn't
/// scrolled yet) and may be arbitrary — the clamps are the contract.
/// When the expanded range still exceeds `max`, the head is kept (the
/// reader's anchor row is the top of the range; trimming the tail only
/// loses context the reader is about to leave anyway).
pub fn window(
    len: usize,
    visible_start: usize,
    visible_end: usize,
    buffer: usize,
    max: usize,
) -> (usize, usize) {
    if len == 0 || max == 0 {
        return (0, 0);
    }
    let start = visible_start.min(len.saturating_sub(1));
    let end = visible_end.clamp(start, len);
    let start = start.saturating_sub(buffer);
    let end = (end + buffer).min(len);
    // Cap AFTER the buffer expansion, anchored at the expanded start.
    let end = end.min(start.saturating_add(max).max(start + 1));
    (start, end.max(start))
}

/// The ids of `notes[start..end]` (the caller's Vec order is row order).
pub fn ids_in_window(notes: &[Event], start: usize, end: usize) -> Vec<EventId> {
    let end = end.min(notes.len());
    let start = start.min(end);
    notes[start..end].iter().map(|n| n.id).collect()
}

/// The viewport filter shards: outbox::chunk at the D39 e-tag ceiling
/// (`OutboxRouter.kt` MAX_ETAGS_PER_FILTER = 150 — big relays reject
/// "total filter items too large"). With MAX_LIVE_IDS = 50 this is
/// always one shard; the chunking is the ceiling's enforcement, not a
/// second limit.
pub fn chunks_for_ids(ids: &[EventId]) -> Vec<Vec<EventId>> {
    outbox::chunk(ids, outbox::MAX_ETAGS_PER_FILTER)
}

/// Canonical live-set comparison: the SAME ids in a different order are the
/// same subscription (scroll re-anchors must not churn REQs).
fn canonical(mut ids: Vec<EventId>) -> Vec<EventId> {
    ids.sort();
    ids.dedup();
    ids.truncate(MAX_LIVE_IDS);
    ids
}

/// What the subscription owner should do on the next viewport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// The requested ids match the live set exactly — nothing to do.
    Keep,
    /// The viewport went empty (feed closed / no rows): close the current
    /// subscription, arm nothing.
    Drop,
    /// Close the current subscription, arm the given filter shards.
    Replace(Vec<Vec<EventId>>),
}

/// Subscribe-replacement bookkeeping (the "no leaked subs" half of the
/// contract). Pure state: the owner closes/arms per `Plan`, then
/// `commit`s what it armed.
#[derive(Debug, Default)]
pub struct ViewportTracker {
    /// The canonical live set the CURRENT subscription was armed for.
    live: Vec<EventId>,
}

impl ViewportTracker {
    pub fn live(&self) -> &[EventId] {
        &self.live
    }

    pub fn plan(&self, requested: &[EventId]) -> Plan {
        let next = canonical(requested.to_vec());
        if next.is_empty() {
            return if self.live.is_empty() {
                Plan::Keep
            } else {
                Plan::Drop
            };
        }
        if next == self.live {
            return Plan::Keep;
        }
        Plan::Replace(chunks_for_ids(&next))
    }

    /// Record what was actually armed: the replacement's ids on a
    /// successful arm, empty when the arm failed or the sub was dropped —
    /// bookkeeping must match the relay's view or the next plan lies.
    pub fn commit(&mut self, ids: Vec<EventId>) {
        self.live = canonical(ids);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u64) -> EventId {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&n.to_be_bytes());
        EventId::from_byte_array(bytes)
    }

    fn notes(n: usize) -> Vec<Event> {
        (0..n)
            .map(|_| {
                let keys = nostr::prelude::Keys::generate();
                nostr::event::EventBuilder::new(Kind::TextNote, "x")
                    .finalize(&keys)
                    .unwrap()
            })
            .collect()
    }

    #[test]
    fn window_clamps_to_feed_and_caps_at_max() {
        assert_eq!(window(0, 0, 0, 8, 50), (0, 0));
        // Visible range beyond the feed clamps to the last row.
        let (s, e) = window(20, 15, 99, 8, 50);
        assert_eq!(s, 7);
        assert_eq!(e, 20);
        // A window bigger than the cap trims from the tail.
        let (s, e) = window(200, 0, 80, 8, 50);
        assert_eq!(s, 0);
        assert_eq!(e, 50);
    }

    #[test]
    fn ids_in_window_bounds_the_slice() {
        let feed = notes(10);
        let ids = ids_in_window(&feed, 4, 99);
        assert_eq!(ids.len(), 6);
        assert_eq!(ids[0], feed[4].id);
        assert_eq!(ids[5], feed[9].id);
        assert!(ids_in_window(&feed, 5, 5).is_empty());
    }

    #[test]
    fn chunks_respect_the_etag_ceiling() {
        let ids: Vec<EventId> = (0..310).map(id).collect();
        let shards = chunks_for_ids(&ids);
        assert_eq!(shards.len(), 3);
        assert!(shards[..2].iter().all(|s| s.len() == outbox::MAX_ETAGS_PER_FILTER));
        assert_eq!(shards[2].len(), 10);
        // The viewport cap keeps a whole window in one shard.
        assert_eq!(chunks_for_ids(&ids[..50]).len(), 1);
    }

    #[test]
    fn tracker_keeps_same_set_regardless_of_order() {
        let mut t = ViewportTracker::default();
        let a: Vec<EventId> = (0..10).map(id).collect();
        match t.plan(&a) {
            Plan::Replace(shards) => {
                assert_eq!(shards.len(), 1);
                t.commit(shards.into_iter().flatten().collect());
            }
            _ => panic!("first plan must replace"),
        }
        let mut b = a.clone();
        b.reverse();
        assert_eq!(t.plan(&b), Plan::Keep);
        assert_eq!(t.plan(&[]), Plan::Drop);
    }

    #[test]
    fn tracker_replace_implies_the_old_sub_closes() {
        // The bookkeeping contract: every Replace follows from `live`
        // being non-empty, and the owner closes the sub armed for `live`
        // BEFORE arming the shards. A failed arm commits empty so the
        // next plan re-replaces (the relay's view is the truth).
        let mut t = ViewportTracker::default();
        let a: Vec<EventId> = (0..5).map(id).collect();
        let b: Vec<EventId> = (5..10).map(id).collect();
        assert_eq!(t.plan(&a), Plan::Replace(chunks_for_ids(&canonical(a.clone()))));
        t.commit(a.clone());
        assert_eq!(t.plan(&b.clone()), Plan::Replace(chunks_for_ids(&b)));
        // A failed arm: the owner commits empty, the retry plans a replace.
        t.commit(Vec::new());
        assert_eq!(t.plan(&b), Plan::Replace(chunks_for_ids(&b)));
        // Empty after empty: nothing to drop, nothing to arm.
        assert_eq!(t.plan(&[]), Plan::Keep);
    }

    #[test]
    fn tracker_caps_and_dedups_the_live_set() {
        let mut t = ViewportTracker::default();
        let dup: Vec<EventId> = (0..(MAX_LIVE_IDS as u64 + 20))
            .map(|i| id(i % (MAX_LIVE_IDS as u64 + 5)))
            .collect();
        match t.plan(&dup) {
            Plan::Replace(shards) => {
                let flat: Vec<EventId> = shards.into_iter().flatten().collect();
                assert!(flat.len() <= MAX_LIVE_IDS);
                t.commit(flat);
            }
            _ => panic!("must replace"),
        }
        assert_eq!(t.live().len(), MAX_LIVE_IDS);
    }
}
