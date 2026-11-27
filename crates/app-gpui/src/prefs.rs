//! App prefs — port of `crates/app/src/prefs.rs` (theme.ts semantics: JSON at
//! the data dir, 0600, `dark: None` follows the system) plus the
//! `content_hidden` read-modify-write helpers from `crates/app/src/settings.rs`
//! (`set_pref_bool` / `set_kind_hidden`).

use serde::{Deserialize, Serialize};

use satori_core::config;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Prefs {
    /// `None` → follow the desktop color scheme. (Both themes land in P4.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dark: Option<bool>,
    /// The undo window (theme.ts: enabled by default, 5s).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo_seconds: Option<u32>,
    /// Sidebar collapsed to icon-only?
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidebar_collapsed: Option<bool>,
}

pub fn load() -> Prefs {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(prefs: &Prefs) {
    // Merge into the raw file — Prefs doesn't carry every key the file holds
    // (content_hidden lives alongside), and a blind serialize would wipe them
    // (the GTK save had exactly that bug: toggling the undo window erased the
    // feed catalog's hidden kinds).
    let mut value = read_value(config::files::PREFS);
    if let Ok(overlay) = serde_json::to_value(prefs)
        && let (Some(dst), Some(src)) = (value.as_object_mut(), overlay.as_object())
    {
        for (k, v) in src {
            dst.insert(k.clone(), v.clone());
        }
    }
    let Ok(body) = serde_json::to_string_pretty(&value) else {
        return;
    };
    write_state(config::files::PREFS, &body);
}

/// The columns workspace layout (D68 phase 2): whether the mode is on
/// and which stream columns are pinned, in order, with their widths.
/// Local-only for now — the NIP-78 login sync rides later polish, so no
/// stamp here (a relay slot would fight the machine's own layout).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ColumnPref {
    /// `feed` | `articles` | `notifications` | `search` | `hashtag`.
    pub kind: String,
    /// The hashtag column's tag (kind `hashtag` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// Column width px; None = the flexible default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<f32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ColumnsPref {
    #[serde(default)]
    pub on: bool,
    #[serde(default)]
    pub cols: Vec<ColumnPref>,
}

/// The persisted columns layout; missing key = the default (off).
pub fn columns_layout() -> ColumnsPref {
    read_value(config::files::PREFS)
        .get("columns")
        .and_then(|c| serde_json::from_value(c.clone()).ok())
        .unwrap_or_default()
}

/// Merge-save the columns layout (the Prefs::save pattern: the file
/// holds keys this struct doesn't — clobbering them is the exact bug
/// the GTK save had).
/// The merge-save idiom every overlay key shares (the file holds keys
/// this process's struct doesn't): read the whole JSON, insert ONE key's
/// serialized overlay, write back pretty. One home (D94's audit) — the
/// write path's shape (fsync discipline, future atomic rename) changes
/// here, not in each saver.
fn save_overlay(key: &str, overlay: &impl serde::Serialize) {
    let mut value = read_value(config::files::PREFS);
    if let (Some(obj), Ok(overlay)) = (value.as_object_mut(), serde_json::to_value(overlay)) {
        obj.insert(key.into(), overlay);
    }
    if let Ok(body) = serde_json::to_string_pretty(&value) {
        write_state(config::files::PREFS, &body);
    }
}

pub fn save_columns_layout(layout: &ColumnsPref) {
    save_overlay("columns", layout)
}

/// Catalog kinds hidden from the feed (`content_hidden` in prefs.json).
pub fn content_hidden() -> Vec<u16> {
    read_value(config::files::PREFS)
        .get("content_hidden")
        .and_then(|h| serde_json::from_value(h.clone()).ok())
        .unwrap_or_default()
}

/// Flip a catalog kind's visibility (settings.rs set_kind_hidden).
pub fn set_kinds_hidden(kinds: &[u16], hidden: bool) {
    let mut value = read_value(config::files::PREFS);
    if let Some(obj) = value.as_object_mut() {
        let mut hidden_list: Vec<u16> = obj
            .get("content_hidden")
            .and_then(|h| serde_json::from_value(h.clone()).ok())
            .unwrap_or_default();
        for k in kinds {
            if hidden {
                if !hidden_list.contains(k) {
                    hidden_list.push(*k);
                }
            } else {
                hidden_list.retain(|h| !kinds.contains(h));
            }
        }
        obj.insert("content_hidden".into(), serde_json::json!(hidden_list));
    }
    if let Ok(body) = serde_json::to_string_pretty(&value) {
        write_state(config::files::PREFS, &body);
    }
}

/// A top-level bool pref (settings.rs set_pref_bool). Missing = false.
pub fn get_bool(key: &str) -> bool {
    read_value(config::files::PREFS)
        .get(key)
        .and_then(|b| b.as_bool())
        .unwrap_or(false)
}

/// A local-edit recency stamp (`{key}_at`, unix secs): every local pref
/// change stamps its key so the NIP-78 login overlay can tell a stale
/// relay slot from a NEWER local edit. Without it, a slot that won the
/// relay's last-write-wins while the local write failed to publish (or a
/// slot from before a curation) re-applied at every login and silently
/// reverted the user's toggles — the feed's hidden-kinds list was eaten
/// exactly this way.
pub fn stamp(key: &str) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut value = read_value(config::files::PREFS);
    if let Some(obj) = value.as_object_mut() {
        obj.insert(format!("{key}_at"), serde_json::json!(now));
    }
    if let Ok(body) = serde_json::to_string_pretty(&value) {
        write_state(config::files::PREFS, &body);
    }
}

/// The NIP-78 pref dance: local recency stamp (the login overlay defers
/// to it), then the relay sync — fire and forget. The caller writes the
/// local value first. Was spelled out inline in every toggling view, and
/// the drift was real (only the theme switch guarded its publish).
pub fn sync(
    core: &std::sync::Arc<satori_core::manager::Core>,
    key: &str,
    value: serde_json::Value,
) {
    stamp(key);
    core.publish_app_data(key, value);
}

/// A top-level bool pref that defaults ON when missing (the WoT gates:
/// `notif_wot_tier`, `thread_collapse`, `profile_badge` — issue #2's
/// posture makes the tier default-on; core reads the same keys via
/// config::prefs()).
pub fn get_bool_or(key: &str, default: bool) -> bool {
    read_value(config::files::PREFS)
        .get(key)
        .and_then(|b| b.as_bool())
        .unwrap_or(default)
}

/// The Tor privacy mode (`privacy_mode` in prefs.json — privacy.ts). Only
/// strict has a face in the UI today: the Phase 3 facade degrades video to
/// links (nothing media-heavy may fetch direct). M5 wires the full toggle.
pub fn privacy_mode() -> satori_core::privacy::PrivacyMode {
    read_value(config::files::PREFS)
        .get("privacy_mode")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

/// Set a top-level bool pref (settings.rs set_pref_bool).
pub fn set_bool(key: &str, on: bool) {
    let mut value = read_value(config::files::PREFS);
    if let Some(obj) = value.as_object_mut() {
        obj.insert(key.into(), on.into());
    }
    if let Ok(body) = serde_json::to_string_pretty(&value) {
        write_state(config::files::PREFS, &body);
    }
}

/// A top-level string pref (local view chrome — the notifications strip's
/// remembered timeframe; NOT the NIP-78 sync, so no recency stamp).
pub fn get_string(key: &str) -> Option<String> {
    config::prefs().and_then(|v| v.get(key).and_then(|s| s.as_str()).map(String::from))
}

/// The feed-history retention window (D99's Settings lever; core's prune
/// reads the same key). `None` = forever (the prune is off); absent =
/// the 90-day default, so the pref file only carries an explicit choice.
pub fn retention_days() -> Option<u64> {
    match read_value(config::files::PREFS).get("retention_days") {
        Some(serde_json::Value::String(s)) if s == "forever" => None,
        // The same 7..=3650 clamp core's reader applies (five-lens audit
        // Nov 2026): a hand-edited pref can't show "0 days" while the
        // prune actually runs at 7 — display and destruction agree.
        Some(v) => Some(v.as_u64().unwrap_or(90).clamp(7, 3650)),
        None => Some(90),
    }
}

/// Set the retention window (settings.rs Storage group). `None` writes
/// the "forever" string; Some days writes the number (7..=3650 clamped —
/// the same sane range core's reader applies to hand edits).
pub fn set_retention_days(days: Option<u64>) {
    let value: serde_json::Value = match days {
        None => serde_json::Value::String("forever".into()),
        Some(d) => serde_json::Value::from(d.clamp(7, 3650)),
    };
    let mut raw = read_value(config::files::PREFS);
    if let Some(obj) = raw.as_object_mut() {
        obj.insert("retention_days".into(), value);
    }
    if let Ok(body) = serde_json::to_string_pretty(&raw) {
        write_state(config::files::PREFS, &body);
    }
}

/// Set a top-level string pref (same shape as set_bool; the media
/// servers pane's local edits).
pub fn set_string(key: &str, val: &str) {
    let mut value = read_value(config::files::PREFS);
    if let Some(obj) = value.as_object_mut() {
        obj.insert(key.into(), val.into());
    }
    if let Ok(body) = serde_json::to_string_pretty(&value) {
        write_state(config::files::PREFS, &body);
    }
}

/// The zap picker's presets (D92): amount + optional default message,
/// stored in prefs.json under `zap_presets`. Missing key = the built-in
/// strip (the 0.10.0 picker's constants). The optional message only ever
/// autofills an EMPTY message field — it never clobbers typing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZapPreset {
    pub sats: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
}

/// The built-in strip (the hardcoded defaults, kept as the missing-key
/// default).
pub fn default_zap_presets() -> Vec<ZapPreset> {
    [21, 100, 500, 1000, 2100, 5000]
        .into_iter()
        .map(|sats| ZapPreset {
            sats,
            message: String::new(),
        })
        .collect()
}

/// The stored strip; missing/corrupt key = the built-ins. Capped at
/// READ too: the write paths enforce 8, but a hand-edited prefs.json
/// must not render unbounded rows per frame (D94's memory lens).
pub fn zap_presets() -> Vec<ZapPreset> {
    let mut presets = read_value(config::files::PREFS)
        .get("zap_presets")
        .and_then(|p| serde_json::from_value::<Vec<ZapPreset>>(p.clone()).ok())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(default_zap_presets);
    presets.truncate(8);
    presets
}

/// Merge-save the strip (the columns-layout pattern: the file holds keys
/// this struct doesn't). Hard cap 8 — the strip is a row, not a list.
pub fn save_zap_presets(presets: &[ZapPreset]) {
    // The cap lives at the write, and the READ is capped too (zap_presets)
    // — the invariant holds whichever path a prefs.json came in from.
    let capped: Vec<ZapPreset> = presets.iter().take(8).cloned().collect();
    save_overlay("zap_presets", &capped)
}

/// Does a short bolt tap send the quick amount directly (true, the D20
/// sketch) or open the sheet? The sheet's toggle row owns this pref.
pub fn zap_instant() -> bool {
    get_bool_or("zap_instant", true)
}

/// The instant zap's amount (sats) — what a bolt tap sends when
/// `zap_instant` is on. Default 21 (the D20 sketch's number). The
/// Wallet tab's field owns it.
pub fn zap_instant_sats() -> u64 {
    get_string("zap_instant_sats")
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|s| (1..=100_000).contains(s))
        .unwrap_or(21)
}

pub fn set_zap_instant_sats(sats: u64) {
    set_string("zap_instant_sats", &sats.to_string());
}

/// read_value's parse cache (one slot: PREFS is the only file it reads).
#[allow(clippy::type_complexity)]
static CACHE: parking_lot::Mutex<Option<(std::time::SystemTime, (u64, u64), serde_json::Value)>> =
    parking_lot::Mutex::new(None);

fn read_value(file: &str) -> serde_json::Value {
    // Parsed once per file change (mtime + size key — the core config.rs
    // prefs() pattern): the feed projection reads prefs per visible card
    // per frame (privacy_mode on the media path), and each read was a
    // full file read + JSON parse on the UI thread (audit perf H2).
    let path = config::state_file(file);
    let Ok(meta) = std::fs::metadata(&path) else {
        return serde_json::Value::Object(serde_json::Map::new());
    };
    let Ok(modified) = meta.modified() else {
        return serde_json::Value::Object(serde_json::Map::new());
    };
    let mut held = CACHE.lock();
    if let Some((at, len, value)) = held.as_ref()
        && *at == modified
        && *len == (meta.len(), std::os::unix::fs::MetadataExt::ino(&meta))
    {
        return value.clone();
    }
    let value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
    *held = Some((
        modified,
        (meta.len(), std::os::unix::fs::MetadataExt::ino(&meta)),
        value.clone(),
    ));
    value
}

fn write_state(file: &str, body: &str) {
    // Atomic (tmp + rename): a crash mid-write used to leave a truncated
    // prefs.json, which load() reads as defaults (every pref lost).
    let _ = satori_core::sidecar::try_write_atomic(&config::state_file(file), body.as_bytes());
    // Our own write invalidates the parse cache outright: an mtime+size
    // key can miss a same-length rewrite inside one coarse mtime tick.
    *CACHE.lock() = None;
}

fn path() -> std::path::PathBuf {
    config::state_file(config::files::PREFS)
}
