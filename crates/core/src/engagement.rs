//! Own-engagement sets — port of `src/data/engagement-cache.ts` semantics.
//!
//! Your likes/reposts/replies/zapped targets fetched once into disk-backed sets
//! (`engagement.json`); glyph fill state is a pure set lookup. Zap receipts are
//! schnorr-verified per receipt with a spoof-proof anonymous fallback.

// TODO(port): M1/M2.
pub struct Engagement;
