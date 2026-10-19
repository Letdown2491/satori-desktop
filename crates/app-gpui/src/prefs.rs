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

fn read_value(file: &str) -> serde_json::Value {
    std::fs::read_to_string(config::state_file(file))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()))
}

fn write_state(file: &str, body: &str) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = config::data_dir_parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(config::state_file(file))
        .and_then(|mut f| f.write_all(body.as_bytes()));
}

fn path() -> std::path::PathBuf {
    config::state_file(config::files::PREFS)
}
