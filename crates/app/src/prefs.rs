//! App prefs — the appearance cookie's successor (theme.ts semantics):
//! JSON at the app's data dir, 0600. `dark: None` follows the system.

use serde::{Deserialize, Serialize};

use satori_core::config;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Prefs {
    /// `None` → follow the desktop color scheme.
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
    // (toggling the undo window erased the feed catalog's hidden kinds).
    let raw = std::fs::read_to_string(path())
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
    let mut value = raw;
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
        .open(path())
        .and_then(|mut f| f.write_all(body.as_bytes()));
}

fn path() -> std::path::PathBuf {
    config::state_file(config::files::PREFS)
}
