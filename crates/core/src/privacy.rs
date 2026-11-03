//! Tor privacy modes — port of `src/privacy.ts`.
//!
//! `off`      direct connections
//! `balanced` 5s Tor attempt, then fail-open direct
//! `strict`   fail-closed: Tor-only, never leaks (doubled timeouts apply)
//!
//! HONESTY NOTE (audit security H3, Nov 2026): the Tor TRANSPORT is still
//! the M5 stub — nothing here proxies sockets yet. Today the mode only
//! gates what the UI does with media (strict keeps poster sniffs off and
//! inline video out); every network path (relays, media fetches, NIP-05,
//! LNURL) still connects DIRECTLY, and this module's "never leaks" claim
//! is not yet true. The claims above describe the port's target behavior;
//! wire the SOCKS5/Tor transport before trusting them.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrivacyMode {
    #[default]
    Off,
    Balanced,
    Strict,
}

impl PrivacyMode {
    /// Tor gets larger read budgets everywhere (pool.ts doubles them).
    pub fn budget_multiplier(self) -> f64 {
        match self {
            Self::Off => 1.0,
            Self::Balanced | Self::Strict => 2.0,
        }
    }

    /// strict never falls back to direct on a failed Tor attempt.
    pub fn fail_closed(self) -> bool {
        matches!(self, Self::Strict)
    }
}
