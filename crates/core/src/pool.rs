//! Relay pool policies — port of `src/data/pool.ts` + `src/nostr/nip65.ts` semantics,
//! on nostr-sdk's relay pool primitives.
//!
//! Port list (DESIGN.md §3.4):
//! - outbox routing: NIP-65 discovery from indexers, quality-weighted greedy set-cover
//!   (≤70 sockets, ≤2 write relays/author), seen-on empirical memory folded back in
//! - deadline-not-completeness queries: `max_wait` caps, quiet-window early resolve,
//!   `page`/`adaptive` budget modes (EWMA latency profiles, read-dead detection)
//! - NIP-42 auto-auth for own relays only; NIP-09 tombstones + NIP-40 expiry enforced
//!   at the chokepoint
//! - suspend watchdog: wall-clock gap >60s recycles sockets after laptop resume

// TODO(port): incremental — start with plain nostr-sdk subscriptions (M0), add
// routing + budgets as the feed needs them (M0/M2).
pub struct Pool;
