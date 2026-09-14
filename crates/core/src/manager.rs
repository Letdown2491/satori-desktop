//! Core: owns the tokio runtime and the active session — the seam between the
//! GTK UI and all I/O. UI code submits commands here; results flow back over
//! executor-agnostic channels (`tokio::sync`) awaited from the GLib main
//! context (DESIGN.md §3.2). The web app's server-session machinery
//! (`src/session.ts`) collapses to this single-user handle.

use std::str::FromStr;
use std::sync::{Arc, Mutex};

use nostr::prelude::*;
use nostr_connect::prelude::*;
use tokio::sync::mpsc;

use crate::config;
use crate::error::{LoginError, Result};
use crate::secrets;
use crate::session::{self, Session, SessionMode};
use crate::signer::{self, Signer};

/// How long to wait for the remote signer to approve a connect request.
const LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Progress events streamed back to the login UI.
#[derive(Debug, Clone)]
pub enum ConnectEvent {
    /// Request sent; the user must approve in their bunker app.
    Waiting,
    /// An out-of-band auth_url approval (NIP-46); open it in a browser.
    AuthUrl(String),
    Approved(PublicKey),
    Failed(String),
}

/// The channel login flows resolve through. Tokio's sync primitives are
/// executor-agnostic, so the GLib main context can await them directly.
pub type ConnectRx = mpsc::UnboundedReceiver<ConnectEvent>;

struct Active {
    session: Session,
    signer: Signer,
}

pub struct Core {
    rt: tokio::runtime::Runtime,
    active: Mutex<Option<Active>>,
    pending: Mutex<Option<Arc<NostrConnect>>>,
}

impl Default for Core {
    fn default() -> Self {
        Self::new()
    }
}

impl Core {
    pub fn new() -> Self {
        Self {
            rt: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("tokio runtime"),
            active: Mutex::new(None),
            pending: Mutex::new(None),
        }
    }

    // --- lifecycle -----------------------------------------------------------

    /// Resume a persisted session, if any. The bunker signer is rebuilt from
    /// the transport secret without user interaction (ports session resume).
    /// Failures degrade to logged-out; the dead state is not destroyed unless
    /// its secret is unrecoverable.
    pub fn init(&self) -> Option<Session> {
        let session = session::load()?;
        let built = self.rt.block_on(self.build_signer(&session));
        match built {
            Ok(signer) => {
                *self.active.lock().unwrap() =
                    Some(Active { session: session.clone(), signer });
                Some(session)
            }
            Err(e) => {
                tracing_or_log(&format!("resume failed: {e}"));
                None
            }
        }
    }

    pub fn pubkey(&self) -> Option<PublicKey> {
        self.active
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|a| PublicKey::from_str(&a.session.pubkey).ok())
    }

    pub fn logout(&self) {
        *self.active.lock().unwrap() = None;
        session::clear();
        secrets::delete(secrets::BUNKER_CLIENT_SECRET);
        secrets::delete(secrets::NSEC);
    }

    // --- bunker login ---------------------------------------------------------

    /// Connect to a `bunker://` URI. Events stream on the returned channel:
    /// Waiting → (AuthUrl?) → Approved | Failed. Cancel with `cancel_login`.
    pub fn connect_bunker(self: &Arc<Self>, uri: String) -> ConnectRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let _ = tx.send(ConnectEvent::Waiting);
            match this.connect_bunker_inner(&uri).await {
                Ok(pk) => {
                    let _ = tx.send(ConnectEvent::Approved(pk));
                }
                Err(e) => {
                    let _ = tx.send(ConnectEvent::Failed(e.to_string()));
                }
            }
        });
        rx
    }

    /// Abandon an in-flight bunker connect (best-effort shutdown).
    pub fn cancel_login(&self) {
        if let Some(pending) = self.pending.lock().unwrap().take() {
            self.rt
                .spawn(async move { Arc::unwrap_or_clone(pending).shutdown().await });
        }
    }

    async fn connect_bunker_inner(&self, uri: &str) -> Result<PublicKey> {
        let parsed = NostrConnectUri::parse(uri.trim())
            .map_err(|e| LoginError::InvalidBunkerUri(e.to_string()))?;

        // bunker://<remote>?relay=... (client:// initiation is out of scope).
        let remote = match &parsed {
            NostrConnectUri::Bunker { remote_signer_public_key, .. } => {
                remote_signer_public_key.to_string()
            }
            _ => return Err(LoginError::InvalidBunkerUri("expected bunker:// URI".into())),
        };

        let client_keys = Keys::generate();
        let connect = Arc::new(
            NostrConnect::new(parsed, client_keys.clone(), LOGIN_TIMEOUT, None)
                .map_err(|e| LoginError::Connect(e.to_string()))?,
        );
        let relays: Vec<String> = connect.relays().iter().map(|r| r.to_string()).collect();
        *self.pending.lock().unwrap() = Some(Arc::clone(&connect));

        let pk = connect
            .get_public_key_async()
            .await
            .map_err(|e| LoginError::Connect(e.to_string()))?;

        secrets::store(secrets::BUNKER_CLIENT_SECRET, &client_keys.secret_key().display_secret().to_string())?;
        let session = Session {
            pubkey: pk.to_string(),
            mode: SessionMode::Bunker { remote, relays },
        };
        session::save(&session)?;

        *self.active.lock().unwrap() =
            Some(Active { session: session.clone(), signer: Signer::Bunker(connect) });
        *self.pending.lock().unwrap() = None;
        Ok(pk)
    }

    // --- nsec login (opt-in local key) ----------------------------------------

    /// Import an nsec/hex secret. One-shot resolution through the same event
    /// stream as the bunker flow (Approved | Failed).
    pub fn login_nsec(self: &Arc<Self>, secret: String) -> ConnectRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            match this.login_nsec_inner(&secret).await {
                Ok(pk) => {
                    let _ = tx.send(ConnectEvent::Approved(pk));
                }
                Err(e) => {
                    let _ = tx.send(ConnectEvent::Failed(e.to_string()));
                }
            }
        });
        rx
    }

    async fn login_nsec_inner(&self, secret: &str) -> Result<PublicKey> {
        let keys = signer::parse_local_secret(secret)?;
        let pk = keys.public_key();

        secrets::store(secrets::NSEC, &keys.secret_key().display_secret().to_string())?;
        let session = Session { pubkey: pk.to_string(), mode: SessionMode::Local };
        session::save(&session)?;

        *self.active.lock().unwrap() =
            Some(Active { session: session.clone(), signer: Signer::Local(keys) });
        Ok(pk)
    }

    // --- resume machinery ------------------------------------------------------

    async fn build_signer(&self, session: &Session) -> Result<Signer> {
        match &session.mode {
            SessionMode::Bunker { remote, relays } => {
                let secret = secrets::load(secrets::BUNKER_CLIENT_SECRET)?
                    .ok_or_else(|| LoginError::Resume("transport secret missing".into()))?;
                let client_keys = signer::client_keys_from_secret(&secret)?;
                let mut uri = format!("bunker://{remote}");
                for r in relays {
                    uri.push_str(&format!("?relay={r}"));
                }
                let parsed = NostrConnectUri::parse(&uri)
                    .map_err(|e| LoginError::Resume(e.to_string()))?;
                let connect = NostrConnect::new(parsed, client_keys, LOGIN_TIMEOUT, None)
                    .map_err(|e| LoginError::Resume(e.to_string()))?;
                Ok(Signer::Bunker(Arc::new(connect)))
            }
            SessionMode::Local => {
                let secret = secrets::load(secrets::NSEC)?
                    .ok_or_else(|| LoginError::Resume("local key missing".into()))?;
                Ok(Signer::Local(signer::parse_local_secret(&secret)?))
            }
        }
    }
}

fn tracing_or_log(msg: &str) {
    eprintln!("[core] {msg}");
}

// Re-export for the UI crate's convenience.
pub use config::APP_ID;
