//! Secret storage: Secret Service first, 0600 file fallback.
//!
//! Port of the web app's at-rest posture (`src/store.ts` kept the NIP-46
//! transport secret in a 0600 sessions.json): here the Secret Service is the
//! primary store (DESIGN.md D2), and the file fallback preserves the 0600
//! convention for systems without one. The master key never lives here —
//! D17 removed the nsec login door; the `NSEC` slot survives only so the
//! test fixture's local mode can read old session files.

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

/// The keyring path can BLOCK for the D-Bus timeout on systems without a
/// Secret Service (niri sessions without gnome-keyring). Every keyring op
/// runs on a blocking thread with a hard 2s deadline; then we fall back.
async fn with_deadline<T: Send + 'static>(
    op: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::task::spawn_blocking(op),
    )
    .await
    {
        Ok(Ok(res)) => res,
        Ok(Err(join)) => Err(LoginError::Store(format!("{join}"))),
        Err(_) => Err(LoginError::Store("secret service timed out".into())),
    }
}

fn fallback_path(name: &str) -> PathBuf {
    config::data_dir().join(format!("{name}.secret"))
}

/// Best-effort store: Secret Service (2s deadline), falling back to a
/// 0600 file.
pub async fn store(name: &str, secret: &str) -> Result<()> {
    let name = name.to_string();
    let secret = secret.to_string();
    let stored: bool = {
        let (n, v) = (name.clone(), secret.clone());
        with_deadline(move || match entry(&n) {
            Ok(e) => e.set_password(&v).map(|_| true).map_err(LoginError::from),
            Err(e) => Err(e),
        })
        .await
        .unwrap_or(false) // no service / timeout → file fallback
    };
    if !stored {
        file_store(&name, &secret)?;
    }
    Ok(())
}

/// Load a secret; `None` when never stored.
pub async fn load(name: &str) -> Result<Option<String>> {
    let from_keyring: Option<String> = {
        let name = name.to_string();
        with_deadline(move || match entry(&name) {
            Ok(e) => match e.get_password() {
                Ok(v) => Ok(Some(v)),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(e) => Err(LoginError::from(e)),
            },
            Err(e) => Err(e),
        })
        .await
        .unwrap_or(None) // no service / timeout → file fallback
    };
    match from_keyring {
        Some(v) => Ok(Some(v)),
        None => file_load(name),
    }
}

/// Best-effort delete from both stores (logout must not leave residue).
/// Fire-and-forget thread: safe from the UI thread, immune to D-Bus hangs.
pub fn delete(name: &'static str) {
    std::thread::spawn(move || {
        if let Ok(e) = entry(name) {
            let _ = e.delete_credential();
        }
        let _ = std::fs::remove_file(fallback_path(name));
    });
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
