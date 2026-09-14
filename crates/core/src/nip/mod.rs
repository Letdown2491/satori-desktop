//! Thin NIP implementations not covered by the rust-nostr workspace
//! (DESIGN.md §6.2). Mostly tag/parse/render semantics — no protocol risk.
//!
//! | module | NIP |
//! |---|---|
//! | `article` | 23 — long-form metadata |
//! | `imeta` | 68/71/92 — media metadata |
//! | `poll` | 88 — poll options + tally |
//! | `calendar` | 52 — calendar events + RSVP addressing |
//!
//! NIP-96 stays rejected; Blossom only (matches the web app).

pub mod article;
pub mod calendar;
pub mod imeta;
pub mod poll;
