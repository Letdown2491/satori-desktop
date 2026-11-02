//! NIP-78 — arbitrary app data (kind 30078): one addressable event per
//! prefs key, the `d` tag naming prefs.json's key verbatim, content
//! NIP-44-encrypted to self (the same private-by-default semantics as the
//! NIP-51 lists).
//! TODO(port): feather AGENTS.md — Phase 2 §4.1 (satori-plan.md); the TS
//! reference never shipped NIP-78, so Feather's kind-30078 semantics are
//! the reference.
//!
//! Wire shape (NIP-78 leaves content format open — this is Satori's):
//! `kind 30078, tags [["d", "dark"]], content = nip44(payload)` where the
//! payload is the bare prefs value as JSON (bool / number / array).
//! Sync is last-write-wins by replaceable semantics: relays and the store
//! keep only the newest 30078 per (author, d); at login the remote winners
//! overlay prefs.json, and every local change publishes forward.

use nostr::prelude::*;

/// Kind 30078 — parameterized-replaceable app data.
pub const KIND: u16 = 30078;

/// The prefs.json keys Satori syncs over NIP-78. Unknown d tags (another
/// client's slots — the NIP is deliberately app-private) never touch
/// prefs.json.
pub const SYNCED_KEYS: &[&str] = &[
    "dark",
    "undo_enabled",
    "undo_seconds",
    "sidebar_collapsed",
    "content_hidden",
    "reactionNotifs",
    "machine_notes_hidden",
];

/// Is `key` one of the slots Satori syncs?
pub fn synced_key(key: &str) -> bool {
    SYNCED_KEYS.contains(&key)
}

/// The event's `d` tag value (the app-data slot name).
pub fn d_tag(ev: &Event) -> Option<String> {
    ev.tags.iter().find_map(|t| {
        let s = t.as_slice();
        if s.first().map(String::as_str) == Some("d") {
            s.get(1).cloned()
        } else {
            None
        }
    })
}

/// One app-data slot after LWW grouping: the winning (newest) event per
/// d tag. Decryption is the caller's job (the signer is async).
#[derive(Debug, Clone)]
pub struct AppData {
    /// The `d` tag — prefs.json key for synced slots.
    pub key: String,
    /// The event's content (NIP-44 ciphertext on the wire).
    pub content: String,
    /// created_at of the winning event.
    pub updated: u64,
}

/// Group kind-30078 events by d tag, newest per slot. Events without a
/// non-empty d tag are dropped (nothing addressable to sync).
pub fn group_latest(events: &[Event]) -> Vec<AppData> {
    let mut slots: Vec<AppData> = Vec::new();
    for ev in events {
        let Some(key) = d_tag(ev).filter(|k| !k.is_empty()) else {
            continue;
        };
        let updated = ev.created_at.as_secs();
        match slots.iter_mut().find(|s| s.key == key) {
            // The store dedupes per (author, kind, d) already; this is the
            // defensive merge for a mixed relay/store set.
            Some(s) if s.updated < updated => {
                s.content = ev.content.clone();
                s.updated = updated;
            }
            Some(_) => {}
            None => slots.push(AppData {
                key,
                content: ev.content.clone(),
                updated,
            }),
        }
    }
    slots.sort_by(|a, b| a.key.cmp(&b.key));
    slots
}

/// Parse a decrypted payload: the bare prefs value as JSON.
pub fn parse_payload(plain: &str) -> Option<serde_json::Value> {
    serde_json::from_str(plain).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(d: &str, at: u64) -> Event {
        let keys = Keys::generate();
        let mut tags = Tags::new();
        if !d.is_empty() {
            tags.push(Tag::custom("d", [d]));
        }
        let unsigned = UnsignedEvent::new(
            keys.public_key(),
            Timestamp::from_secs(at),
            Kind::Custom(KIND),
            tags,
            "cipher",
        );
        keys.sign_event(unsigned).unwrap()
    }

    #[test]
    fn d_tag_extracts_the_slot_name() {
        assert_eq!(d_tag(&ev("dark", 1)).as_deref(), Some("dark"));
        assert_eq!(d_tag(&ev("", 1)), None);
        // No d tag at all.
        let keys = Keys::generate();
        let unsigned = UnsignedEvent::new(
            keys.public_key(),
            Timestamp::now(),
            Kind::Custom(KIND),
            [],
            "x",
        );
        assert_eq!(d_tag(&keys.sign_event(unsigned).unwrap()), None);
    }

    #[test]
    fn newest_event_per_slot_wins() {
        let old = ev("dark", 100);
        let new = ev("dark", 200);
        let other = ev("undo_enabled", 50);
        let grouped = group_latest(&[old.clone(), new, other]);
        assert_eq!(grouped.len(), 2);
        let dark = grouped.iter().find(|s| s.key == "dark").unwrap();
        assert_eq!(dark.updated, 200);
        assert_eq!(dark.content, "cipher");
        assert!(grouped.iter().any(|s| s.key == "undo_enabled"));
    }

    #[test]
    fn events_without_d_tag_are_dropped() {
        let plain = ev("", 10);
        let dark = ev("dark", 20);
        let grouped = group_latest(&[plain, dark]);
        assert_eq!(grouped.len(), 1);
        assert_eq!(grouped[0].key, "dark");
    }

    #[test]
    fn synced_keys_are_the_prefs_names() {
        assert!(synced_key("dark"));
        assert!(synced_key("reactionNotifs"));
        assert!(!synced_key("some-other-app/prefs"));
    }

    #[test]
    fn payload_round_trips() {
        assert_eq!(parse_payload("true"), Some(serde_json::Value::Bool(true)));
        assert_eq!(
            parse_payload("[20, 30023]"),
            Some(serde_json::json!([20, 30023]))
        );
        assert_eq!(parse_payload("not json"), None);
    }
}
