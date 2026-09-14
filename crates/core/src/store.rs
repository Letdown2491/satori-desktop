//! Event store — nostr-lmdb (LMDB) at `config::events_dir()`.
//!
//! The projection source of truth (DESIGN.md §1.2): views read from here and from
//! live subscriptions, never from widget state. Tombstones (NIP-09, own deletions
//! pinned) and NIP-40 expiry are enforced at this boundary too, mirroring the web
//! app's pool chokepoint.

// TODO(port): wire nostr-lmdb in M0; expose the query surface views need
// (kind+author+since windows, monotonic read high-waters).
pub struct Store;
