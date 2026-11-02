//! Filesystem layout and identity constants.
//!
//! Flatpak-safe: everything lives under the app's XDG data dir, which the
//! sandbox maps to a per-app location automatically (DESIGN.md §3.3).

use std::path::PathBuf;

/// The application identity (D12): reverse-DNS, lowercase app name — the
/// house style (io.github.Letdown2491.gertie / .riddim). Must stay
/// byte-identical across the .desktop file's ID, the Icon= name, and the
/// window's Wayland app_id (WindowOptions.app_id) so GNOME matches them.
pub const APP_ID: &str = "io.github.Letdown2491.satori";

/// App-state JSON files (0600, debounced flush, flush-on-exit — the port of
/// `src/data/json-store.ts` conventions). Event cache is separate: LMDB.
pub mod files {
    pub const SESSION: &str = "session.json";
    pub const PREFS: &str = "prefs.json";
    pub const DRAFTS: &str = "drafts.json";
    pub const HOLDS: &str = "holds.json";
    pub const SCHEDULED: &str = "scheduled.json";
    pub const FILTERS: &str = "filters.json";
    pub const READ_STATE: &str = "read-state.json";
    pub const DM_READ: &str = "dm-read.json";
    pub const RELAYS_CACHE: &str = "relays-cache.json";
    pub const SEEN_RELAYS: &str = "seen-relays.json";
    pub const RELAY_LATENCY: &str = "relay-latency.json";
    pub const RELAY_INFO: &str = "relay-info.json";
    pub const RELAY_FAVORITES: &str = "relay-favorites.json";
    pub const ENGAGEMENT: &str = "engagement.json";
    pub const DELETIONS: &str = "deletions.json";
    pub const PROFILES: &str = "profiles.json";
    /// NWC wallet connection — secret-FREE (the secret rides in the secret
    /// store under `secrets::NWC_SECRET`); only pubkey + relays live here so
    /// a restart can rebuild the client. D20.
    pub const WALLET: &str = "wallet.json";
    /// Signed kind-9734 zap requests, stored BEFORE paying (the Feather
    /// rule: an explicit retry after a crash reuses the same event ID —
    /// no duplicate zap events).
    pub const ZAPS: &str = "zaps.json";
    /// Bitcoin timestamp→height table (blocktime, Phase 2 §4.4) — permanent,
    /// tiny, seeded from a compiled-in anchor.
    pub const BLOCKTIME: &str = "blocktime.json";
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// `$XDG_DATA_HOME/satori` (default `~/.local/share/satori`).
pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("satori")
}

/// LMDB event store directory (the projection source of truth).
pub fn events_dir() -> PathBuf {
    data_dir().join("events")
}

/// Media blob cache (LRU, size-capped, keyed by sha256).
pub fn media_cache_dir() -> PathBuf {
    data_dir().join("media-cache")
}

pub fn state_file(name: &str) -> PathBuf {
    data_dir().join(name)
}

/// Parent of the data dir (for mkdir before files exist).
pub fn data_dir_parent() -> Option<PathBuf> {
    data_dir().parent().map(PathBuf::from)
}
