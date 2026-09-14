//! The undo window — the breath before speech. Port of `src/undo.ts` semantics.
//!
//! A signed event is held in memory + on disk for N seconds (default 5, max 30)
//! before publishing. A backstop commits 1.5s after the deadline even if the UI
//! went away. Holds are persisted and resumed across restarts (stale >1h dropped).

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub const DEFAULT_UNDO_SECONDS: u32 = 5;
pub const MAX_UNDO_SECONDS: u32 = 30;
pub const BACKSTOP_GRACE: Duration = Duration::from_millis(1500);
/// Holds older than an hour at boot are dropped, not re-armed.
pub const STALE_HOLD: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoWindow {
    pub enabled: bool,
    #[serde(default = "default_seconds")]
    seconds: u32,
}

fn default_seconds() -> u32 {
    DEFAULT_UNDO_SECONDS
}

impl Default for UndoWindow {
    fn default() -> Self {
        Self { enabled: true, seconds: DEFAULT_UNDO_SECONDS }
    }
}

impl UndoWindow {
    pub fn seconds(&self) -> u32 {
        self.seconds.min(MAX_UNDO_SECONDS)
    }

    pub fn set_seconds(&mut self, s: u32) {
        self.seconds = s.clamp(1, MAX_UNDO_SECONDS);
    }

    pub fn hold_duration(&self) -> Option<Duration> {
        self.enabled.then(|| Duration::from_secs(self.seconds() as u64))
    }

    /// When the backstop commits even if nothing asked for the tick.
    pub fn backstop_deadline(&self, started: Instant) -> Option<Instant> {
        self.hold_duration().map(|d| started + d + BACKSTOP_GRACE)
    }
}
