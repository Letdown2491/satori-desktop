//! Tor privacy modes — port of `src/privacy.ts`.
//!
//! `off`      direct connections
//! `balanced` 5s Tor attempt, then fail-open direct
//! `strict`   fail-closed: Tor-only, never leaks (doubled timeouts apply)

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
