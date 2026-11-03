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

// Renamed with the app ID (D12). Entries under the old capitalized service
// are NOT orphaned anymore: load() migrates them on first miss (below).
const SERVICE: &str = "io.github.Letdown2491.satori";
/// The pre-D12 service string. Read-only — migrate_legacy_service clears
/// each slot as it copies it across.
const LEGACY_SERVICE: &str = "io.github.Letdown2491.Satori";
pub const BUNKER_CLIENT_SECRET: &str = "bunker-client-secret";
pub const NSEC: &str = "nsec";
/// The NWC wallet's connection secret (NIP-47). D20: a separate, revocable
/// wallet credential — NOT identity material (D17 is untouched).
pub const NWC_SECRET: &str = "nwc-connection-secret";

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

/// Load a secret; `None` when never stored. A keyring miss first tries the
/// pre-rename (D12) service — one-time migration, invisible to callers.
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
    let from_keyring = match from_keyring {
        Some(v) => Some(v),
        None => migrate_legacy_service(name).await?,
    };
    match from_keyring {
        Some(v) => Ok(Some(v)),
        None => file_load(name),
    }
}

/// One-time migration for the D12 app-id rename: the keyring service was
/// `io.github.Letdown2491.Satori` (capital S) before it, so a miss under the
/// current service reads the legacy entry, copies the secret across, and
/// clears the orphan. Best-effort in both directions: any legacy-read
/// trouble is a clean miss (the file fallback still runs), and a failed
/// copy keeps the legacy entry for the next attempt.
async fn migrate_legacy_service(name: &str) -> Result<Option<String>> {
    let name = name.to_string();
    let migrated: Option<String> = with_deadline(move || {
        let legacy = match keyring::Entry::new(LEGACY_SERVICE, &name) {
            Ok(e) => e,
            Err(_) => return Ok(None),
        };
        match legacy.get_password() {
            Ok(secret) => {
                let copied = entry(&name)
                    .ok()
                    .and_then(|cur| cur.set_password(&secret).ok());
                if copied.is_some() {
                    let _ = legacy.delete_credential();
                } else {
                    eprintln!("[secrets] legacy migration: copy failed, orphan kept");
                }
                // Resume with the legacy secret either way — this launch
                // works, and a failed copy migrates on the next one.
                Ok(Some(secret))
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => {
                eprintln!("[secrets] legacy migration read failed: {e}");
                Ok(None)
            }
        }
    })
    .await
    .unwrap_or(None); // no service / timeout → treat as no legacy entry
    Ok(migrated)
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
    // Write to a fresh 0600 temp sibling and rename over (audit security
    // L3): plain create() FOLLOWS a pre-planted symlink, and the mode flag
    // is a no-op on an existing file. The rename replaces whatever sits at
    // the path - link or file - without following it.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let tmp = path.with_extension(format!("tmp-{}-{nanos}", std::process::id()));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| LoginError::Store(e.to_string()))?;
    writeln!(f, "{secret}").map_err(|e| LoginError::Store(e.to_string()))?;
    drop(f);
    std::fs::rename(&tmp, &path).map_err(|e| LoginError::Store(e.to_string()))?;
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
