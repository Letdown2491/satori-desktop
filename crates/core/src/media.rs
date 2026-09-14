//! Media pipeline — port of `src/routes/media.ts` + `src/routes/image-proxy.ts`
//! + `src/ssrf.ts` semantics, as an internal service.
//!
//! Port list: SSRF guards on every URL (scheme allowlist, IP-literal rejection
//! incl. IPv4-mapped forms, DNS-rebinding-safe resolution on direct paths,
//! per-redirect re-checks, byte caps, wall-clock killers); media proxy with
//! sha256-verified cache; NIP-B7 hash healing (dead Blossom URL → refetch from
//! the author's kind:10063 servers, verify sha256 before serving); SVG never
//! trusted as a rendered format; video streams direct, never proxied.

// TODO(port): M2 (avatars + proxy), M4 (healing + full pipeline).
pub struct Media;
