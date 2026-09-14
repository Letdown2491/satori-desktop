//! Read state — port of `src/read-state.ts`: monotonic high-waters per
//! account, persisted 0600. The web app kept these in a client cookie; the
//! desktop keeps them on disk (same advance-only semantics).

use serde::{Deserialize, Serialize};

use crate::config;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReadState {
    /// Newest notification seen (UNIX secs).
    #[serde(default)]
    pub notif: u64,
    /// Newest following-feed note marked seen (UNIX secs).
    #[serde(default)]
    pub feed: u64,
    /// Last time the feed store was deep-filled (bootstrap skip marker).
    #[serde(default)]
    pub feed_warm_at: u64,
    /// Newest note seen at the last home view (home-dot marker).
    #[serde(default)]
    pub feed_seen: u64,
}

pub fn load() -> ReadState {
    std::fs::read_to_string(config::state_file(config::files::READ_STATE))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Advance-only write (read-state.ts: monotonic).
pub fn advance(update: impl FnOnce(&mut ReadState)) {
    let mut state = load();
    update(&mut state);
    if let Ok(body) = serde_json::to_string(&state) {
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(parent) = config::data_dir_parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(config::state_file(config::files::READ_STATE))
            .and_then(|mut f| std::io::Write::write_all(&mut f, body.as_bytes()));
    }
}
