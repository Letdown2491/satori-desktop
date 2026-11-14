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
    pub const SEEN_EVENT: &str = "seen-event.json";
    pub const RELAY_HINTS: &str = "relay-hints.json";
    pub const WOT_SOURCES: &str = "wot-sources.json";
    /// D48: the anti-spam ledgers — dedup's spammers + recent duplicate
    /// keys, the reply-velocity stamps + counted event ids, and the
    /// notification receipts (suppressed events + reasons). Verdicts
    /// outlive the session: boot gates from last session's knowledge.
    pub const DEDUP_STATE: &str = "dedup-state.json";
    pub const RATE_STATE: &str = "rate-state.json";
    pub const SUPPRESSED: &str = "suppressed-notifs.json";
    pub const RELAY_LATENCY: &str = "relay-latency.json";
    pub const DELETIONS: &str = "deletions.json";
    pub const OUTBOX_PENDING: &str = "outbox-pending.json";
    pub const RELAY_INFO: &str = "relay-info.json";
    pub const RELAY_FAVORITES: &str = "relay-favorites.json";
    pub const ENGAGEMENT: &str = "engagement.json";
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
    pub const DIRECT_RELAYS: &str = "direct-relays.json";
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

/// The prefs file, parsed once per file change. The feed projection reads
/// it per page, `replies()` per query, the notifications sweep twice per
/// run — each read was a full file read + JSON parse. The cache keys on
/// mtime + size, so a settings Publish lands on the next read; the cost
/// between changes is one metadata syscall. Returns the cached
/// `Arc` (audit fix: the dedup chokepoint reads per event — a deep
/// `Value` clone per read went with it).
pub fn prefs() -> Option<std::sync::Arc<serde_json::Value>> {
    static CACHE: std::sync::Mutex<
        Option<(
            std::time::SystemTime,
            u64,
            std::sync::Arc<serde_json::Value>,
        )>,
    > = std::sync::Mutex::new(None);
    let path = state_file(files::PREFS);
    let meta = std::fs::metadata(&path).ok()?; // no file = no prefs
    let modified = meta.modified().ok()?;
    let mut held = CACHE.lock().unwrap();
    if let Some((at, len, value)) = held.as_ref()
        && *at == modified
        && *len == meta.len()
    {
        return Some(std::sync::Arc::clone(value));
    }
    let value =
        serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&path).ok()?).ok()?;
    let value = std::sync::Arc::new(value);
    *held = Some((modified, meta.len(), std::sync::Arc::clone(&value)));
    Some(value)
}

/// Parent of the data dir (for mkdir before files exist).
pub fn data_dir_parent() -> Option<PathBuf> {
    data_dir().parent().map(PathBuf::from)
}
