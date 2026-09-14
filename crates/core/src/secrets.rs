//! Secret storage: Secret Service first, 0600 file fallback.
//!
//! Port of the web app's at-rest posture (`src/store.ts` kept the NIP-46
//! transport secret in a 0600 sessions.json): here the Secret Service is the
//! primary store (DESIGN.md D2), and the file fallback preserves the 0600
//! convention for systems without one. The user's master key never lives here
//! unless they explicitly chose nsec login (D2: opt-in).

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use crate::config;
use crate::error::{LoginError, Result};

const SERVICE: &str = "io.github.Letdown2491.Satori";
pub const BUNKER_CLIENT_SECRET: &str = "bunker-client-secret";
pub const NSEC: &str = "nsec";

fn entry(name: &str) -> Result<keyring::Entry> {
    // A unique "user" per slot keeps entries distinct in the Secret Service UI.
    keyring::Entry::new(SERVICE, name).map_err(LoginError::from)
}

fn fallback_path(name: &str) -> PathBuf {
    config::data_dir().join(format!("{name}.secret"))
}

/// Best-effort store: Secret Service, falling back to a 0600 file.
pub fn store(name: &str, secret: &str) -> Result<()> {
    let stored = entry(name).is_ok_and(|e| e.set_password(secret).is_ok());
    if !stored {
        file_store(name, secret)?;
    }
    Ok(())
}

/// Load a secret; `None` when never stored.
pub fn load(name: &str) -> Result<Option<String>> {
    if let Ok(e) = entry(name) {
        match e.get_password() {
            Ok(v) => return Ok(Some(v)),
            Err(keyring::Error::NoEntry) => return file_load(name),
            Err(e) => return Err(LoginError::from(e)),
        }
    }
    file_load(name)
}

/// Best-effort delete from both stores (logout must not leave residue).
pub fn delete(name: &str) {
    if let Ok(e) = entry(name) {
        let _ = e.delete_credential();
    }
    let _ = std::fs::remove_file(fallback_path(name));
}

fn file_store(name: &str, secret: &str) -> Result<()> {
    let path = fallback_path(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| LoginError::Store(e.to_string()))?;
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| LoginError::Store(e.to_string()))?;
    writeln!(f, "{secret}").map_err(|e| LoginError::Store(e.to_string()))?;
    Ok(())
}

fn file_load(name: &str) -> Result<Option<String>> {
    let path = fallback_path(name);
    if !path.exists() {
        return Ok(None);
    }
    let s = std::fs::read_to_string(&path).map_err(|e| LoginError::Store(e.to_string()))?;
    Ok(Some(s.trim().to_string()))
}
