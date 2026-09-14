//! Publishing — port of `src/data/publish.ts` + `src/nostr/nip10.ts` semantics.
//!
//! Port list: tag building for replies/quotes/comments (NIP-10 marked-e, NIP-22
//! uppercase-root/lowercase-parent), relay target resolution (write relays +
//! recipient inbox caps, de-duped delivery reporting), lowercase-hex hygiene.

// TODO(port): M1.
pub struct Publish;
