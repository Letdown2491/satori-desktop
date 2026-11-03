//! Session persistence — port of `src/session.ts` + `src/store.ts` semantics.
//!
//! The session file holds only public material (pubkey, remote signer, relays);
//! secrets go through `crate::secrets` (Secret Service, 0600 file fallback).
//! Restart resumes lazily: the bunker signer is rebuilt from the transport
//! secret without a re-approval round-trip.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;

use crate::config;
use crate::error::{LoginError, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// The user's public key (hex).
    pub pubkey: String,
    #[serde(flatten)]
    pub mode: SessionMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum SessionMode {
    /// NIP-46 remote signer. `client_secret` lives in the secret store, keyed
    /// by `secrets::BUNKER_CLIENT_SECRET`.
    Bunker {
        /// Remote signer's public key (hex).
        remote: String,
        /// The bunker's relay URLs.
        relays: Vec<String>,
    },
    /// Opt-in local key (DESIGN.md D2). The secret lives in the secret store,
    /// keyed by `secrets::NSEC`.
    Local,
}

fn path() -> std::path::PathBuf {
    config::state_file(config::files::SESSION)
}

pub fn save(session: &Session) -> Result<()> {
    let path = path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| LoginError::Store(e.to_string()))?;
    }
    let body = serde_json::to_string_pretty(session)
        .map_err(|e| LoginError::Store(e.to_string()))?;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .and_then(|mut f| f.write_all(body.as_bytes()))
        .map_err(|e| LoginError::Store(e.to_string()))?;
    Ok(())
}

/// `None` when there is no session (first run / after logout).
pub fn load() -> Option<Session> {
    serde_json::from_str(&std::fs::read_to_string(path()).ok()?).ok()
}

pub fn clear() {
    let _ = std::fs::remove_file(path());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets;
    use std::os::unix::fs::PermissionsExt;

    /// One env-mutating test (XDG_DATA_HOME is process-global): covers the
    /// session roundtrip and the secret-store 0600-file fallback (the
    /// container has no Secret Service, so store() exercises the fallback).
    #[test]
    fn session_roundtrip_and_file_fallback() {
        let _env = crate::test_env::EnvGuard::new("session");

        let session = Session {
            pubkey: "3bf0c626cb9b0e68c8280ba10023637b0425e77b29f8e2a0b7b3b6b0e0a1f2e3".into(),
            mode: SessionMode::Bunker {
                remote: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                relays: vec!["wss://relay.example".into()],
            },
        };
        save(&session).expect("save");
        let loaded = load().expect("load");
        match loaded.mode {
            SessionMode::Bunker { remote, relays } => {
                assert_eq!(relays, vec!["wss://relay.example".to_string()]);
                assert_eq!(remote.len(), 64);
            }
            _ => panic!("wrong mode"),
        }

        // Secret store: no keyring in the container -> 0600 file fallback.
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                secrets::store(secrets::NSEC, TEST_SECRET).await.expect("store fallback");
                let path = config::data_dir().join(format!("{}.secret", secrets::NSEC));
                let mode = std::fs::metadata(&path).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600, "fallback secret file must be 0600");
                assert_eq!(
                    secrets::load(secrets::NSEC).await.unwrap().as_deref(),
                    Some(TEST_SECRET)
                );
            });

        secrets::delete(secrets::NSEC);
        clear();
        assert!(load().is_none());
    }

    const TEST_SECRET: &str = "e1d4a5b33a9d2f0c8b7e6f5a4d3c2b1a0918273645f4e3d2c1b0a99887766554";
}
