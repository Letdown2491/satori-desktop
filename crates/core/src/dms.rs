//! NIP-17 private DMs — port of `src/data/dms.ts` + `src/nostr/nip17.ts` semantics.
//!
//! Port list: gift-wrap query (#p=me, self-wraps), unwrap 1059→seal(13)→rumor with
//! signature verification and triage (mutes at one decrypt, kind 14 vs private
//! replies), strict routing to kind:10050 DM relays (fallback NIP-65 read, never
//! indexers), wrap-pair publish (peer + self), request/inbox triage, read
//! high-waters (`dm-read.json`), ±2d timestamp fuzzing on wrap.
//!
//! Decision D5: decrypted rumor cache is memory-only (LRU ~8000); nothing
//! decrypts to disk.

// TODO(port): M3.
pub struct Dms;
