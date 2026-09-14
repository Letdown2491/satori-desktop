//! Thin NIP implementations not covered by the rust-nostr workspace
//! (DESIGN.md §6.2). Mostly tag/parse/render semantics — no protocol risk.
//!
//! | module | NIP |
//! |---|---|
//! | `profile_extras` | 24 — website/banner/bot fields |
//! | `alt` | 31 — alt text fallback for unknown kinds |
//! | `repos` | 34 — git repo cards (read-only) |
//! | `draft_sync` | 37 — encrypted draft wraps (kind 31234 → 10013) |
//! | `calendar` | 52 — calendar events + RSVP + ICS export |
//! | `wiki` | 54 — wikis + AsciiDoc subset |
//! | `imeta` | 68/71/92 — picture/video/media metadata |
//! | `highlights` | 84 — highlight quote cards |
//! | `handlers` | 89 — recommended-app discovery |
//! | `classifieds` | 99 — classified listings |
//! | `podcast` | F4 — podcast episodes |
//!
//! NIP-96 stays rejected; Blossom only (matches the web app).

pub mod imeta {
    // TODO(port): src/nostr/imeta.ts — M2.
}
