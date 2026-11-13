//! The diagnostic log: stderr plus a small file mirror.
//!
//! The daily-driver launch nohups the binary with stderr discarded, so a
//! plain eprintln is lost exactly when it's needed — diagnosing a login
//! handshake that stalled, or a resume that never recovered. Everything
//! on the login and suspend-watchdog paths writes through here: stderr
//! for a foreground run, the mirror for the packaged one. The mirror is
//! a plain append under the data dir, rotated at startup past 1 MiB.

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;

use crate::config;

/// Rotate threshold — a diagnostic log past this is recreated on boot.
const ROTATE_BYTES: u64 = 1024 * 1024;

fn path() -> std::path::PathBuf {
    config::data_dir().join("satori.log")
}

/// Called once at startup (Core::new): keep the mirror bounded.
pub fn rotate() {
    let _ = std::fs::create_dir_all(config::data_dir());
    let Ok(meta) = std::fs::metadata(path()) else {
        return;
    };
    if meta.len() > ROTATE_BYTES {
        let _ = std::fs::remove_file(path());
    }
}

/// One diagnostic line: stderr for a foreground run, the mirror for the
/// nohup launch, each prefixed with epoch seconds so gaps (suspend!) show.
/// A failing write is silent — a log never takes the app down. The mirror
/// rotates IN PLACE once past the threshold (audit memory L-2: rotation
/// used to be startup-only, so a long session's log grew unbounded).
pub fn log(args: std::fmt::Arguments) {
    eprintln!("[satori] {args}");
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(meta) = std::fs::metadata(path())
        && meta.len() > ROTATE_BYTES
    {
        let _ = std::fs::remove_file(path());
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        // 0600 at creation (audit security: the only state file that
        // wasn't). The mirror carries no secrets by policy now — probe
        // logs are method-name-only — but 0644 by umask default is one
        // future omission away from being wrong.
        .mode(0o600)
        .open(path())
    {
        let _ = writeln!(f, "{secs} {args}");
    }
}

/// The one-liner: `dlog!("stage: {detail}")`.
#[macro_export]
macro_rules! dlog {
    ($($t:tt)*) => { $crate::diag::log(format_args!($($t)*)) };
}
