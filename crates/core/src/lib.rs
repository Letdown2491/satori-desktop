//! satori-core: Satori's Nostr domain layer.
//!
//! The web app reference lives in `src/` at the repo root (frozen). Each module's
//! doc comment names the TypeScript file(s) whose *semantics* it ports. This crate
//! must never depend on GTK: the UI (`crates/app`) is a projection of the state
//! managed here (DESIGN.md §1.2 — HATEOAS, natively).
//!
//! Signing is local (bunker via NIP-46, or an opt-in local key in the Secret
//! Service). The web app's entire NIP-07 sign-and-resubmit seam is deliberately
//! absent (DESIGN.md §2).

#![allow(dead_code)]

pub mod blocktime;
pub mod config;
pub mod error;
pub mod manager;
pub mod privacy;
pub mod profiles;
pub mod read_state;
pub mod secrets;
pub mod session;
pub mod signer;
pub mod undo;

pub mod dms;
pub mod engagement;
pub mod kinds;
pub mod media;
pub mod nip;
pub mod notifications;
pub mod pool;
pub mod publish;
pub mod store;
pub mod tor;
pub mod upload;
pub mod wallet;

#[cfg(test)]
pub(crate) mod test_env {
    /// Tests that touch prefs/secrets on disk serialize here (the env is
    /// process-global; parallel runs delete each other's state).
    pub static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
