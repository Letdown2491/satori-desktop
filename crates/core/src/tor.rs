//! Tor transport — bundled C tor child process via SOCKS (DESIGN.md D7).
//!
//! Port list (`src/data/ws-tor.ts` + `src/data/torfetch.ts`): per-host circuit
//! isolation via unique SOCKS usernames (`h-<host>` → C tor IsolateSOCKSAuth);
//! `.onion` relays always tunnel; privacy modes off/balanced/strict with
//! fail-open (5s) / fail-closed semantics and doubled budgets; Firefox UA for
//! crowd blending on HTTP fetches.
//!
//! Transport lives behind a trait so arti can replace tor later without UI
//! changes (DESIGN.md §3.5).

// TODO(port): M5 (M0-M4 run direct).
pub struct TorTransport;
