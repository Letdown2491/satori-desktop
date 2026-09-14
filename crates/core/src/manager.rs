//! Core: owns the tokio runtime and the active session — the seam between the
//! GTK UI and all I/O. UI code submits commands here; results flow back over
//! executor-agnostic channels (`tokio::sync`) awaited from the GLib main
//! context (DESIGN.md §3.2). The web app's server-session machinery
//! (`src/session.ts`) collapses to this single-user handle.

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use nostr::prelude::*;
use nostr_connect::prelude::*;
use tokio::sync::mpsc;

use crate::error::{LoginError, Result};
use crate::pool::Pool;
use crate::secrets;
use crate::session::{self, Session, SessionMode};
use crate::signer::{self, Signer};

/// How long to wait for the remote signer to approve a connect request.
const LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Feed page size; the bootstrap fetch keeps depth for "view older".
const PAGE: usize = 40;
const BOOTSTRAP_DEPTH: usize = 200;



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

/// Stream of feed/write results for the UI.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum FeedEvent {
    Loaded(FeedPage),
    /// Older page + has-more.
    Older(FeedPage, bool),
    Replies(Vec<Event>, Vec<Profile>),
    /// Notifications (replies + mentions; reactions stay off by default).
    Notifications(Vec<Event>, Vec<Profile>),
    /// Whether the bell should light (unread past high-water).
    NotifDot(bool),
    /// NIP-50 search results (notes).
    Search(Vec<Event>, Vec<Profile>),
    /// One event by id (repost target resolution), + author profile.
    One(Option<Event>, Option<Profile>),
    /// Refresh found nothing newer — quiet feedback, no list churn.
    NothingNew,
    /// Poll tallies: (option id, count) over each voter's latest ballot.
    PollVotes(Vec<(String, u32)>),
    /// Comments on a target, oldest first + profiles.
    Comments(Vec<Event>, Vec<Profile>),
    /// Reply-presence faces: (note id, up to 3 replier pubkeys).
    Faces(Vec<(EventId, Vec<PublicKey>)>),
    /// Profiles resolved reactively (cards patch themselves).
    Profiles(Vec<Profile>),
    /// A backup export payload (raw signed events JSON).
    BackupJson(String),
    BootstrapDone,
    Failed(String),
}

pub type FeedRx = mpsc::UnboundedReceiver<FeedEvent>;

/// What the write path reports back.
#[derive(Debug, Clone)]
pub enum PublishEvent {
    /// The signed event is held for the undo window; countdown starts.
    Held { token: u64, event: Event, hold_secs: u64 },
    /// The hold was undone; nothing was published.
    Undone { token: u64 },
    /// Published (or hold expired — the backstop committed).
    Committed { token: u64 },
    /// A restore finished: n events re-signed + published.
    Restored(u32),
    Failed { token: u64, error: String },
}

pub type PublishRx = mpsc::UnboundedReceiver<PublishEvent>;

#[derive(Clone)]
struct Active {
    session: Session,
    signer: Signer,
    pool: Arc<Pool>,
    profiles: Arc<crate::profiles::ProfileService>,
    undo: crate::undo::UndoWindow,
    /// Own NIP-65 write relays.
    write_relays: Vec<String>,
    /// Own NIP-65 read relays (indexers are always on).
    read_relays: Vec<String>,
    follows: Vec<PublicKey>,
}

impl Active {
    fn me(&self) -> PublicKey {
        // Sessions always persist a valid hex pubkey.
        PublicKey::from_str(&self.session.pubkey).expect("session pubkey")
    }
}

/// A rendered-ready feed page: notes plus resolved profiles.
#[derive(Debug, Clone)]
pub struct FeedPage {
    pub notes: Vec<Event>,
    pub profiles: Vec<Profile>,
}

#[derive(Debug, Clone, Default)]
pub struct Profile {
    pub pubkey: String,
    pub name: String,
    pub picture: String,
    pub about: String,
    pub banner: String,
}

impl Profile {
    pub fn from_event(ev: &Event) -> Option<Self> {
        #[derive(serde::Deserialize)]
        struct RawMeta {
            name: Option<String>,
            display_name: Option<String>,
            picture: Option<String>,
            about: Option<String>,
            banner: Option<String>,
        }
        let raw: RawMeta = serde_json::from_str(&ev.content).ok()?;
        Some(Self {
            pubkey: ev.pubkey.to_string(),
            name: raw.display_name.or(raw.name).unwrap_or_default(),
            picture: raw.picture.unwrap_or_default(),
            about: raw.about.unwrap_or_default(),
            banner: raw.banner.unwrap_or_default(),
        })
    }

    /// Display name fallback chain: name → short npub (render/util.ts style).
    pub fn display(&self) -> String {
        if self.name.is_empty() {
            let npub = self.pubkey.clone();
            format!("npub1…{}", &npub[npub.len().saturating_sub(6)..])
        } else {
            self.name.clone()
        }
    }
}

pub struct Core {
    rt: tokio::runtime::Runtime,
    active: Mutex<Option<Active>>,
    pending: Mutex<Option<Arc<NostrConnect>>>,
    hold_seq: AtomicU64,
    /// Own reactions: target event id (hex) → engaged.
    own_liked: Mutex<HashSet<String>>,
    own_bookmarked: Mutex<HashSet<String>>,
    /// Muted authors (pubkey hex) — NIP-51 kind:10000, content encrypted.
    muted: Mutex<HashSet<String>>,
    /// Resolved profiles (mentions/name display) — absorbed as they stream in.
    profiles_mem: Mutex<std::collections::HashMap<String, Profile>>,
    /// Live undo holds: token → cancel signal.
    holds: Mutex<std::collections::HashMap<u64, tokio::sync::oneshot::Sender<()>>>,
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
            hold_seq: AtomicU64::new(1),
            own_liked: Mutex::new(HashSet::new()),
            own_bookmarked: Mutex::new(HashSet::new()),
            muted: Mutex::new(HashSet::new()),
            profiles_mem: Mutex::new(std::collections::HashMap::new()),
            holds: Mutex::new(std::collections::HashMap::new()),
        }
    }

    // --- lifecycle -----------------------------------------------------------

    /// Resume a persisted session, if any. Returns the session plus the
    /// failure reason (shown on the login page — silent resume failures are
    /// how users get lost). The bunker signer is rebuilt from the transport
    /// secret without user interaction.
    pub fn init(&self) -> (Option<Session>, Option<String>) {
        let Some(session) = session::load() else {
            return (None, None); // first run — not an error
        };
        match self.rt.block_on(self.build_active(&session)) {
            Ok(active) => {
                *self.active.lock().unwrap() = Some(active);
                (Some(session), None)
            }
            Err(e) => {
                eprintln!("[core] resume failed: {e}");
                (None, Some(e.to_string()))
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
        self.own_liked.lock().unwrap().clear();
        self.own_bookmarked.lock().unwrap().clear();
        self.muted.lock().unwrap().clear();
    }

    /// Follow-state lookup for profile pages (from the cached contact list).
    pub fn is_following(&self, who: &PublicKey) -> bool {
        self.active
            .lock()
            .unwrap()
            .as_ref()
            .map(|a| a.follows.iter().any(|f| f == who))
            .unwrap_or(false)
    }

    pub fn is_muted(&self, who: &PublicKey) -> bool {
        self.muted.lock().unwrap().contains(&who.to_string())
    }

    /// Engagement lookups for card rendering (pure set membership, render is a
    /// projection — DESIGN.md §1.2).
    pub fn is_liked(&self, event_id: &EventId) -> bool {
        self.own_liked.lock().unwrap().contains(&event_id.to_string())
    }

    pub fn is_bookmarked(&self, event_id: &EventId) -> bool {
        self.own_bookmarked.lock().unwrap().contains(&event_id.to_string())
    }

    /// Fetch a JSON document (oEmbed previews) through the guarded pipeline.
    pub fn fetch_json(
        self: &Arc<Self>,
        url: String,
    ) -> mpsc::UnboundedReceiver<Option<serde_json::Value>> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.rt.spawn(async move {
            let res = crate::media::fetch_json(&url).await.ok();
            let _ = tx.send(res);
        });
        rx
    }

    /// Fetch (and cache) an image — SSRF-guarded, sha256-keyed (media.rs).
    /// Resolves with raw RGBA8 pixels + dimensions, or None on failure.
    pub fn fetch_image(
        self: &Arc<Self>,
        url: String,
    ) -> mpsc::UnboundedReceiver<Option<crate::media::DecodedImage>> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.rt.spawn(async move {
            let res = crate::media::fetch_image(&url).await.ok();
            let _ = tx.send(res);
        });
        rx
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

        secrets::store(
            secrets::BUNKER_CLIENT_SECRET,
            &client_keys.secret_key().display_secret().to_string(),
        )?;
        let session = Session {
            pubkey: pk.to_string(),
            mode: SessionMode::Bunker { remote, relays },
        };
        session::save(&session)?;

        // Already on the runtime (spawned task): await, never block_on.
        let active = self.build_active(&session).await?;
        *self.active.lock().unwrap() = Some(active);
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

        let active = self.build_active(&session).await?;
        *self.active.lock().unwrap() = Some(active);
        Ok(pk)
    }

    // --- resume machinery ------------------------------------------------------

    async fn build_signer(&self, session: &Session) -> Result<Signer> {
        match &session.mode {
            SessionMode::Bunker { remote, relays } => {
                let secret = secrets::load(secrets::BUNKER_CLIENT_SECRET)?
                    .ok_or_else(|| LoginError::Resume("transport secret missing".into()))?;
                let client_keys = signer::client_keys_from_secret(&secret)?;
                // Repeated relays join with & (query params, not extra ?s).
                let mut uri = format!("bunker://{remote}");
                if !relays.is_empty() {
                    uri.push_str("?relay=");
                    uri.push_str(&relays.join("&relay="));
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

    async fn build_active(&self, session: &Session) -> Result<Active> {
        let signer = self.build_signer(session).await?;
        let pool = Arc::new(Pool::open().await.map_err(|e| LoginError::Resume(e.to_string()))?);
        let profiles = crate::profiles::ProfileService::new(Arc::clone(&pool));
        // Undo-window prefs live in prefs.json (theme.ts semantics); core
        // reads its slice at session build — TODO(M1): cleaner pref split.
        let mut undo = crate::undo::UndoWindow::default();
        if let Ok(raw) = std::fs::read_to_string(crate::config::state_file(
            crate::config::files::PREFS,
        ))
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                if let Some(b) = v.get("undo_enabled").and_then(|b| b.as_bool()) {
                    undo.enabled = b;
                }
                if let Some(sec) = v.get("undo_seconds").and_then(|s| s.as_u64()) {
                    undo.set_seconds(sec as u32);
                }
            }
        Ok(Active {
            session: session.clone(),
            signer,
            pool,
            profiles,
            undo,
            write_relays: Vec::new(),
            read_relays: Vec::new(),
            follows: Vec::new(),
        })
    }

    /// Live undo-window update (settings): active state + prefs.json.
    pub fn set_undo(&self, enabled: bool, seconds: u32) {
        {
            let mut a = self.active.lock().unwrap();
            if let Some(a) = a.as_mut() {
                a.undo.enabled = enabled;
                a.undo.set_seconds(seconds);
            }
        }
        // Read-modify-write prefs.json, preserving app-owned keys.
        let path =
            crate::config::state_file(crate::config::files::PREFS);
        let mut value = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
        if let Some(obj) = value.as_object_mut() {
            obj.insert("undo_enabled".into(), enabled.into());
            obj.insert("undo_seconds".into(), seconds.into());
        }
        if let Ok(body) = serde_json::to_string_pretty(&value) {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let _ = std::fs::create_dir_all(crate::config::data_dir());
            let _ = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&path)
                .and_then(|mut f| f.write_all(body.as_bytes()));
        }
    }

    // --- feed ------------------------------------------------------------------

    /// Bootstrap: NIP-65 relay lists, follows, connect reads, first page
    /// (deep fetch for older pagination), own engagement sets, profiles.
    pub fn bootstrap(self: &Arc<Self>) -> FeedRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            if let Err(e) = this.bootstrap_inner(&tx).await {
                let _ = tx.send(FeedEvent::Failed(e.to_string()));
            }
        });
        rx
    }

    async fn bootstrap_inner(&self, tx: &mpsc::UnboundedSender<FeedEvent>) -> Result<()> {
        let mut active = self.active().await?;

        let me = active.me();
        // The NIP-65 fetch itself needs a connected pool: indexers first,
        // then the discovered read set (nip65.ts bootstrap order).
        active.pool.connect_reads(Vec::new()).await.ok();
        let (read, write) = active
            .pool
            .nip65(me)
            .await
            .unwrap_or_else(|_| {
                eprintln!("[core] nip65 fetch failed or timed out");
                (Vec::new(), Vec::new())
            });
        eprintln!(
            "[core] bootstrap: {} read relay(s), {} write relay(s)",
            read.len(),
            write.len()
        );
        active.read_relays = read.clone();
        active.write_relays = write.clone();
        if let Some(a) = self.active.lock().unwrap().as_mut() {
            a.read_relays = read.clone();
            a.write_relays = write;
        }
        active.pool.connect_reads(read).await.ok();

        let follows = active.pool.follows(me).await.unwrap_or_default();
        active.follows = follows.clone();
        if let Some(a) = self.active.lock().unwrap().as_mut() {
            a.follows = follows.clone();
        }
        // Eager warm: every follow's profile before the first paint
        // (profile-cache.ts warm semantics — the feed resolves names).
        let mut warm_set = follows.clone();
        warm_set.push(me);
        active.profiles.warm_await(warm_set).await;

        // Deep feed fetch — skipped when warm (feed-cache.ts: the store
        // persists across runs; a recent bootstrap already filled depth).
        let warm = crate::read_state::load().feed_warm_at
            > now_secs().saturating_sub(600);
        if warm {
            eprintln!("[core] bootstrap: store is warm, skipping deep fetch");
        }
        if !follows.is_empty() && !warm {
            let filter = Filter::new()
                .kinds(crate::store::visible_kinds())
                .authors(follows.clone())
                .limit(BOOTSTRAP_DEPTH);
            for ev in active.pool.get(filter).await.unwrap_or_default() {
                let _ = active.pool.store.save(&ev).await;
            }
        }

        // Own engagement sets.
        let mine = Filter::new()
            .author(me)
            .kinds([Kind::Reaction, Kind::Custom(10003)])
            .limit(2000);
        for ev in active.pool.get(mine).await.unwrap_or_default() {
            let _ = active.pool.store.save(&ev).await;
        }
        self.load_own_sets(&active).await;

        crate::read_state::advance(|st| st.feed_warm_at = now_secs());
        let page = self.page_from_store(&active, None).await?;
        let _ = tx.send(FeedEvent::Loaded(page));
        let _ = tx.send(FeedEvent::BootstrapDone);
        Ok(())
    }



    async fn load_own_sets(&self, active: &Active) {
        let me = active.me();
        if let Ok(reactions) = active.pool.store.own_reactions(me).await {
            let mut liked = self.own_liked.lock().unwrap();
            for ev in reactions {
                if let Some(target) = ev.tags.clone().event_ids().next() {
                    liked.insert(target.to_string());
                }
            }
        }

        // Private lists (NIP-51, content = nip44-encrypted tag JSON — the web
        // app's private-by-default semantics): bookmarks 10003, mutes 10000.
        if let Ok(Some(ev)) = active
            .pool
            .store
            .latest_by_kind(me, Kind::Custom(10003))
            .await
        {
            if !ev.content.is_empty()
                && let Ok(plain) = active.signer.nip44_decrypt_self(&me, ev.content.as_str()).await {
                    let mut b = self.own_bookmarked.lock().unwrap();
                    for id in parse_list_event(&plain) {
                        b.insert(id);
                    }
                }
            // Public tags still count (merge public+private — list semantics).
            let mut b = self.own_bookmarked.lock().unwrap();
            for id in ev.tags.clone().event_ids() {
                b.insert(id.to_string());
            }
        }

        if let Ok(Some(ev)) = active
            .pool
            .store
            .latest_by_kind(me, Kind::Custom(10000))
            .await
        {
            let decrypted: Vec<String> = if ev.content.is_empty() {
                Vec::new()
            } else {
                active
                    .signer
                    .nip44_decrypt_self(&me, ev.content.as_str())
                    .await
                    .map(|plain| parse_list_event(&plain))
                    .unwrap_or_default()
            };
            let mut m = self.muted.lock().unwrap();
            for pk in decrypted {
                m.insert(pk);
            }
            for pk in ev.tags.clone().public_keys() {
                m.insert(pk.to_string());
            }
        }
    }

    /// Newest page (home button): relay fetch since the newest known note.
    pub fn refresh(self: &Arc<Self>) -> FeedRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            if let Err(e) = this.refresh_inner(&tx).await {
                let _ = tx.send(FeedEvent::Failed(e.to_string()));
            }
        });
        rx
    }

    async fn refresh_inner(&self, tx: &mpsc::UnboundedSender<FeedEvent>) -> Result<()> {
        let active = self.active().await?;
        let follows = active.follows.clone();
        if follows.is_empty() {
            return self.bootstrap_inner(tx).await;
        }
        let newest = active
            .pool
            .store
            .feed_page(&follows, None, 1)
            .await
            .map_err(|e| LoginError::Resume(e.to_string()))?;
        let since = newest
            .first()
            .map(|e| e.created_at.as_secs())
            .unwrap_or(0);
        let filter = Filter::new()
            .kinds(crate::store::visible_kinds())
            .authors(follows.clone())
            .since(Timestamp::from(since))
            .limit(100);
        let fetched = active.pool.get(filter).await.unwrap_or_default();
        if fetched.is_empty() {
            let _ = tx.send(FeedEvent::NothingNew);
            return Ok(());
        }
        for ev in fetched {
            let _ = active.pool.store.save(&ev).await;
        }
        let page = self.page_from_store(&active, None).await?;
        let _ = tx.send(FeedEvent::Loaded(page));
        Ok(())
    }

    /// Older page from the store (pure projection; relay top-up in M2).
    pub fn older(self: &Arc<Self>, until: u64) -> FeedRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res: Result<FeedEvent> = async {
                let active = this.active().await?;
                let mut notes = active
                    .pool
                    .store
                    .feed_page(&active.follows, Some(until), PAGE + 1)
                    .await
                    .map_err(|e| LoginError::Resume(e.to_string()))?;
                {
                    let muted = this.muted.lock().unwrap();
                    notes.retain(|ev| !muted.contains(&ev.pubkey.to_string()));
                }
                let has_more = notes.len() > PAGE;
                notes.truncate(PAGE);
                let profiles = Self::profiles_for(&active, &notes).await;
                Ok(FeedEvent::Older(FeedPage { notes, profiles }, has_more))
            }
            .await;
            match res {
                Ok(ev) => {
                    let _ = tx.send(ev);
                }
                Err(e) => {
                    let _ = tx.send(FeedEvent::Failed(e.to_string()));
                }
            }
        });
        rx
    }

    fn absorb_profiles(&self, profiles: &[Profile]) {
        let mut m = self.profiles_mem.lock().unwrap();
        for p in profiles {
            m.insert(p.pubkey.clone(), p.clone());
        }
    }

    /// Resolve profiles reactively: cards call this with whatever pks they
    /// need; the service coalesces + fetches, the UI patches on arrival.
    pub fn ensure_profiles(self: &Arc<Self>, pks: Vec<PublicKey>) -> FeedRx {
        let (tx, rx) = mpsc::unbounded_channel();
        match &*self.active.lock().unwrap() {
            Some(active) => {
                let service = Arc::clone(&active.profiles);
                let service2 = Arc::clone(&active.profiles);
                self.rt.spawn(async move {
                    let mut inner = service.ensure_many(pks);
                    while let Some(profs) = inner.recv().await {
                        let _ = tx.send(FeedEvent::Profiles(profs));
                    }
                    let _ = service2;
                });
            }
            None => {
                let _ = tx.send(FeedEvent::Profiles(Vec::new()));
            }
        }
        rx
    }

    /// Display resolution for inline mentions (content.ts: @names from the
    /// profile cache; misses render as short npubs).
    pub fn profile_of(&self, pk: &PublicKey) -> Option<Profile> {
        self.profiles_mem.lock().unwrap().get(&pk.to_string()).cloned()
    }

    async fn page_from_store(&self, active: &Active, until: Option<u64>) -> Result<FeedPage> {
        let follows = active.follows.clone();
        let mut notes = active
            .pool
            .store
            .feed_page(&follows, until, PAGE)
            .await
            .map_err(|e| LoginError::Resume(e.to_string()))?;
        // Mutes gate here — every feed page passes this chokepoint
        // (pool.ts: NIP-09/NIP-40 consume-at-the-chokepoint role).
        {
            let muted = self.muted.lock().unwrap();
            notes.retain(|ev| !muted.contains(&ev.pubkey.to_string()));
        }
        let profiles = Self::profiles_for(active, &notes).await;
        self.absorb_profiles(&profiles);
        for p in &profiles {
            active.profiles.absorb(p.clone());
        }
        Ok(FeedPage { notes, profiles })
    }

    async fn profiles_for(active: &Active, notes: &[Event]) -> Vec<Profile> {
        let authors: Vec<PublicKey> = notes.iter().map(|e| e.pubkey).collect();
        let cached = active
            .pool
            .store
            .profiles(&authors)
            .await
            .unwrap_or_default();
        let mut profiles: Vec<Profile> = cached
            .iter()
            .filter_map(Profile::from_event)
            .collect();
        // Fresh metadata for authors missing from the cache.
        let mut seen = std::collections::HashSet::new();
        let missing: Vec<PublicKey> = authors
            .iter()
            .filter(|a| !profiles.iter().any(|p| p.pubkey == a.to_string()))
            .filter(|a| seen.insert(a.to_string()))
            .cloned()
            .collect();
        if !missing.is_empty() {
            let filter = Filter::new().kind(Kind::Metadata).authors(missing.clone()).limit(500);
            for ev in active.pool.get(filter).await.unwrap_or_default() {
                let _ = active.pool.store.save(&ev).await;
            }
            let fresh = active
                .pool
                .store
                .profiles(&missing)
                .await
                .unwrap_or_default();
            for p in fresh.iter().filter_map(Profile::from_event) {
                if !profiles.iter().any(|q| q.pubkey == p.pubkey) {
                    profiles.push(p);
                }
            }
        }
        profiles
    }

    // --- notifications ------------------------------------------------------------

    /// The notification stream (port of `src/data/notifications.ts`, slice 1):
    /// replies to my notes + mentions (#p=me), newest first, own events
    /// dropped, reactions/poll-votes/zap classes land with their milestones.
    pub fn notifications(self: &Arc<Self>) -> FeedRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res: Result<(Vec<Event>, Vec<Profile>, bool)> = async {
                let active = this.active().await?;
                let me = active.me();

                // My recent notes give the reply surface (#e targets).
                let my_notes = active
                    .pool
                    .store
                    .feed_page(&[me], None, 100)
                    .await
                    .unwrap_or_default();
                let ids: Vec<EventId> = my_notes.iter().map(|e| e.id).collect();

                let since = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
                    .saturating_sub(7 * 86400);

                let mut filters = vec![Filter::new()
                    .kind(Kind::TextNote)
                    .pubkeys([me]) // mentions (#p)
                    .since(Timestamp::from(since))
                    .limit(200)];
                if !ids.is_empty() {
                    filters.push(
                        Filter::new()
                            .kind(Kind::TextNote)
                            .events(ids)
                            .since(Timestamp::from(since))
                            .limit(200),
                    );
                }
                let mut mine_events: Vec<Event> = Vec::new();
                for f in filters {
                    for ev in active.pool.get(f).await.unwrap_or_default() {
                        mine_events.push(ev);
                    }
                }
                for ev in &mine_events {
                    let _ = active.pool.store.save(ev).await;
                }

                // Dedup + drop own + sort desc.
                let mut seen = std::collections::HashSet::new();
                let mut notifs: Vec<Event> = mine_events
                    .into_iter()
                    .filter(|ev| ev.pubkey != me && seen.insert(ev.id))
                    .collect();
                notifs.sort_by_key(|e| std::cmp::Reverse(e.created_at.as_secs()));

                let authors: Vec<PublicKey> = notifs.iter().map(|e| e.pubkey).collect();
                let cached = active
                    .pool
                    .store
                    .profiles(&authors)
                    .await
                    .unwrap_or_default();
                let profiles: Vec<Profile> =
                    cached.iter().filter_map(Profile::from_event).collect();

                // Dot state: anything newer than the high-water?
                let high_water = crate::read_state::load().notif;
                let unread = notifs
                    .first()
                    .map(|e| e.created_at.as_secs() > high_water)
                    .unwrap_or(false);

                Ok((notifs, profiles, unread))
            }
            .await;
            match res {
                Ok((notifs, profiles, unread)) => {
                    let _ = tx.send(FeedEvent::Notifications(notifs, profiles));
                    let _ = tx.send(FeedEvent::NotifDot(unread));
                }
                Err(e) => {
                    let _ = tx.send(FeedEvent::Failed(e.to_string()));
                }
            }
        });
        rx
    }

    /// Opening the notifications page advances the high-water (monotonic).
    pub fn mark_notifications_seen(self: &Arc<Self>) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        crate::read_state::advance(|s| s.notif = s.notif.max(now));
    }

    /// Reply-presence: per note, who replied (up to 3, earliest first) —
    /// the web app's "faces" hydration (routes/feed.ts getFaces).
    pub fn reply_faces(self: &Arc<Self>, ids: Vec<EventId>) -> FeedRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res: Result<Vec<(EventId, Vec<PublicKey>)>> = async {
                let active = this.active().await?;
                let filter = Filter::new()
                    .kind(Kind::TextNote)
                    .events(ids.clone())
                    .limit(600);
                let mut out: Vec<(EventId, Vec<PublicKey>)> = Vec::new();
                let mut per_note: std::collections::HashMap<EventId, Vec<PublicKey>> =
                    std::collections::HashMap::new();
                let id_set: std::collections::HashSet<EventId> = ids.iter().copied().collect();
                let mut replies: Vec<Event> = active
                    .pool
                    .get(filter)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
                replies.sort_by_key(|e| e.created_at.as_secs());
                for r in replies {
                    // Parent = last e tag within the requested set.
                    if let Some(pid) = r
                        .tags
                        .clone()
                        .event_ids()
                        .last()
                        .filter(|pid| id_set.contains(pid))
                    {
                        let names = per_note.entry(pid).or_default();
                        if names.len() < 3 && !names.contains(&r.pubkey) {
                            names.push(r.pubkey);
                        }
                    }
                }
                for (id, names) in per_note {
                    out.push((id, names));
                }
                Ok(out)
            }
            .await;
            match res {
                Ok(faces) => {
                    let _ = tx.send(FeedEvent::Faces(faces));
                }
                Err(e) => {
                    let _ = tx.send(FeedEvent::Failed(e.to_string()));
                }
            }
        });
        rx
    }

    /// Does the following feed hold notes newer than the last home view?
    /// (Quiet-dot poll — the store answers, no relays.)
    pub fn feed_dot(self: &Arc<Self>) -> FeedRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res: Result<bool> = async {
                let active = this.active().await?;
                let newest = active
                    .pool
                    .store
                    .feed_page(&active.follows, None, 1)
                    .await
                    .map_err(|e| LoginError::Resume(e.to_string()))?;
                Ok(newest
                    .first()
                    .map(|e| e.created_at.as_secs() > crate::read_state::load().feed_seen)
                    .unwrap_or(false))
            }
            .await;
            match res {
                Ok(dot) => {
                    let _ = tx.send(FeedEvent::NotifDot(dot));
                }
                Err(e) => {
                    let _ = tx.send(FeedEvent::Failed(e.to_string()));
                }
            }
        });
        rx
    }

    /// Home opened: the feed is seen up to now (monotonic).
    pub fn mark_feed_seen(self: &Arc<Self>) {
        crate::read_state::advance(|s| s.feed_seen = s.feed_seen.max(now_secs()));
    }

    /// Export the user's list events (kinds 3/10000/10002/10003) as raw
    /// signed JSON — the list-backup.ts export.
    pub fn export_lists(self: &Arc<Self>) -> FeedRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res: Result<String> = async {
                let active = this.active().await?;
                let me = active.me();
                let filter = Filter::new().author(me).kinds([
                    Kind::ContactList,
                    Kind::Custom(10000),
                    Kind::Custom(10002),
                    Kind::Custom(10003),
                ]);
                let raw = active.pool.get(filter).await.unwrap_or_default();
                // Keep the newest per kind.
                let mut by_kind: std::collections::HashMap<u16, Event> =
                    std::collections::HashMap::new();
                for ev in raw {
                    by_kind.insert(u16::from(ev.kind), ev);
                }
                let events: Vec<&Event> = by_kind.values().collect();
                serde_json::to_string_pretty(&events)
                    .map_err(|e| LoginError::Resume(e.to_string()))
            }
            .await;
            match res {
                Ok(json) => {
                    let _ = tx.send(FeedEvent::BackupJson(json));
                }
                Err(e) => {
                    let _ = tx.send(FeedEvent::Failed(e.to_string()));
                }
            }
        });
        rx
    }

    /// Restore from a backup: re-sign each event fresh (list-backup.ts:
    /// replace = same tags/content, new created_at) and publish.
    pub fn import_lists(self: &Arc<Self>, json: String) -> PublishRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res: Result<u32> = async {
                let active = this.active().await?;
                let me = active.me();
                let events: Vec<Event> = serde_json::from_str(&json)
                    .map_err(|e| LoginError::Resume(format!("bad backup: {e}")))?;
                let mut n = 0;
                for ev in events {
                    if ev.pubkey != me {
                        continue; // only restore own lists
                    }
                    let unsigned = UnsignedEvent::new(
                        me,
                        Timestamp::now(),
                        ev.kind,
                        ev.tags.clone().to_vec(),
                        ev.content.clone(),
                    );
                    let signed = active.signer.sign_event(unsigned).await?;
                    Self::commit(&active, &signed, &tx).await?;
                    let _ = active.pool.store.save(&signed).await;
                    n += 1;
                }
                Ok(n)
            }
            .await;
            match res {
                Ok(n) => {
                    let _ = tx.send(PublishEvent::Restored(n));
                }
                Err(e) => {
                    let _ = tx.send(PublishEvent::Failed { token: 0, error: e.to_string() });
                }
            }
        });
        rx
    }

    /// Comments on a target (NIP-22 kind:1111), oldest first.
    pub fn comments(self: &Arc<Self>, target: crate::nip::comment::CommentTarget) -> FeedRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res: Result<(Vec<Event>, Vec<Profile>)> = async {
                let active = this.active().await?;
                let mut comments: Vec<Event> = Vec::new();
                for filter in target.fetch_filters() {
                    for ev in active.pool.get(filter).await.unwrap_or_default() {
                        comments.push(ev);
                    }
                }
                for ev in &comments {
                    let _ = active.pool.store.save(ev).await;
                }
                // Dedup + oldest first.
                let mut seen = std::collections::HashSet::new();
                comments.retain(|ev| seen.insert(ev.id));
                comments.sort_by_key(|e| e.created_at.as_secs());
                let _authors: Vec<PublicKey> = comments.iter().map(|e| e.pubkey).collect();
                let profiles = Self::profiles_for(&active, &comments).await;
                this.absorb_profiles(&profiles);
                // Include target-author profile (fetch may be empty).

                Ok((comments, profiles))
            }
            .await;
            match res {
                Ok((comments, profiles)) => {
                    let _ = tx.send(FeedEvent::Comments(comments, profiles));
                }
                Err(e) => {
                    let _ = tx.send(FeedEvent::Failed(e.to_string()));
                }
            }
        });
        rx
    }

    /// Publish a comment (top-level when `parent` is None, else a reply).
    pub fn comment(
        self: &Arc<Self>,
        content: String,
        target: crate::nip::comment::CommentTarget,
        parent: Option<Event>,
    ) -> PublishRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res: Result<()> = async {
                let active = this.active().await?;
                let mut tags = match &parent {
                    Some(p) => crate::nip::comment::reply_tags(p, &target),
                    None => target.root_tags(),
                };
                tags.push(Tag::custom("client", ["satori-desktop"]));
                let unsigned =
                    UnsignedEvent::new(active.me(), Timestamp::now(), Kind::Custom(1111), tags, content);
                let signed = active.signer.sign_event(unsigned).await?;
                Self::commit(&active, &signed, &tx).await?;
                let _ = active.pool.store.save(&signed).await;
                Ok(())
            }
            .await;
            if let Err(e) = res {
                let _ = tx.send(PublishEvent::Failed { token: 0, error: e.to_string() });
            } else {
                let _ = tx.send(PublishEvent::Committed { token: 0 });
            }
        });
        rx
    }

    /// Tallies for a poll: kind:1018 ballots #e=poll id.
    pub fn poll_votes(self: &Arc<Self>, poll_id: EventId) -> FeedRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res: Result<Vec<(String, u32)>> = async {
                let active = this.active().await?;
                let filter = Filter::new()
                    .kind(Kind::Custom(1018))
                    .event(poll_id)
                    .limit(1000);
                let mut votes: Vec<Event> = Vec::new();
                for ev in active.pool.get(filter).await.unwrap_or_default() {
                    votes.push(ev);
                }
                for ev in &votes {
                    let _ = active.pool.store.save(ev).await;
                }
                Ok(crate::nip::poll::tally(&votes))
            }
            .await;
            match res {
                Ok(t) => {
                    let _ = tx.send(FeedEvent::PollVotes(t));
                }
                Err(e) => {
                    let _ = tx.send(FeedEvent::Failed(e.to_string()));
                }
            }
        });
        rx
    }

    /// Vote in a poll (NIP-88 kind:1018; re-voting replaces your ballot).
    pub fn poll_vote(self: &Arc<Self>, poll: &Event, option_ids: Vec<String>) -> PublishRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        let poll = poll.clone();
        self.rt.spawn(async move {
            let res: Result<()> = async {
                let active = this.active().await?;
                let mut tags = vec![Tag::event(poll.id), Tag::public_key(poll.pubkey)];
                for opt in option_ids {
                    tags.push(Tag::custom("response", [opt]));
                }
                let unsigned =
                    UnsignedEvent::new(active.me(), Timestamp::now(), Kind::Custom(1018), tags, "");
                let signed = active.signer.sign_event(unsigned).await?;
                Self::commit(&active, &signed, &tx).await?;
                let _ = active.pool.store.save(&signed).await;
                Ok(())
            }
            .await;
            if let Err(e) = res {
                let _ = tx.send(PublishEvent::Failed { token: 0, error: e.to_string() });
            } else {
                let _ = tx.send(PublishEvent::Committed { token: 0 });
            }
        });
        rx
    }

    /// RSVP to a calendar event (NIP-52 kind:31925).
    pub fn rsvp(self: &Arc<Self>, calendar: &Event, status: &str) -> PublishRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        let calendar = calendar.clone();
        let status = status.to_string();
        self.rt.spawn(async move {
            let res: Result<()> = async {
                let active = this.active().await?;
                let data = crate::nip::calendar::parse(&calendar)
                    .ok_or_else(|| LoginError::Resume("bad calendar event".into()))?;
                let a = crate::nip::calendar::a_tag(&data, &calendar.pubkey);
                let unsigned = UnsignedEvent::new(
                    active.me(),
                    Timestamp::now(),
                    Kind::Custom(31925),
                    [
                        Tag::custom("a", [a]),
                        Tag::event(calendar.id),
                        Tag::public_key(calendar.pubkey),
                        Tag::custom("status", [status]),
                    ],
                    "",
                );
                let signed = active.signer.sign_event(unsigned).await?;
                Self::commit(&active, &signed, &tx).await?;
                let _ = active.pool.store.save(&signed).await;
                Ok(())
            }
            .await;
            if let Err(e) = res {
                let _ = tx.send(PublishEvent::Failed { token: 0, error: e.to_string() });
            } else {
                let _ = tx.send(PublishEvent::Committed { token: 0 });
            }
        });
        rx
    }

    /// Repost (NIP-18 kind:6).
    pub fn repost(self: &Arc<Self>, target: &Event) -> PublishRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        let target = target.clone();
        self.rt.spawn(async move {
            let res: Result<()> = async {
                let active = this.active().await?;
                let unsigned = UnsignedEvent::new(
                    active.me(),
                    Timestamp::now(),
                    Kind::Repost,
                    [Tag::event(target.id), Tag::public_key(target.pubkey)],
                    format!("nostr:{}", target.id.to_bech32().unwrap_or_default()),
                );
                let signed = active.signer.sign_event(unsigned).await?;
                Self::commit(&active, &signed, &tx).await?;
                let _ = active.pool.store.save(&signed).await;
                Ok(())
            }
            .await;
            if let Err(e) = res {
                let _ = tx.send(PublishEvent::Failed { token: 0, error: e.to_string() });
            } else {
                let _ = tx.send(PublishEvent::Committed { token: 0 });
            }
        });
        rx
    }

    /// Resolve a note/nevent bech32 to its event (embed/quote previews).
    pub fn event_by_id_dech32(self: &Arc<Self>, bech32: &str) -> FeedRx {
        use nostr::prelude::*;
        let id = match Nip19::from_bech32(bech32) {
            Ok(Nip19::EventId(id)) => Some(id),
            Ok(Nip19::Event(e)) => Some(e.event_id),
            _ => None,
        };
        match id {
            Some(id) => self.event_by_id(id),
            None => {
                let (tx, rx) = mpsc::unbounded_channel();
                let _ = tx.send(FeedEvent::One(None, None));
                rx
            }
        }
    }

    /// Resolve one event by id (repost targets); None if unknown everywhere.
    pub fn event_by_id(self: &Arc<Self>, id: EventId) -> FeedRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res: Result<(Option<Event>, Option<Profile>)> = async {
                let active = this.active().await?;
                let cached = active.pool.store.event_by_id(id).await.ok().flatten();
                let ev = match cached {
                    Some(ev) => Some(ev),
                    None => {
                        let fetched = active
                            .pool
                            .get(Filter::new().id(id).limit(1))
                            .await
                            .unwrap_or_default();
                        let ev = fetched.into_iter().next();
                        if let Some(ev) = &ev {
                            let _ = active.pool.store.save(ev).await;
                        }
                        ev
                    }
                };
                let profile = match &ev {
                    Some(ev) => {
                        let authors = [ev.pubkey];
                        let cached = active.pool.store.profiles(&authors).await.unwrap_or_default();
                        let mut p: Option<Profile> =
                            cached.iter().filter_map(Profile::from_event).next();
                        if p.is_none() {
                            let filter = Filter::new()
                                .kind(Kind::Metadata)
                                .author(ev.pubkey)
                                .limit(1);
                            for meta in active.pool.get(filter).await.unwrap_or_default() {
                                let _ = active.pool.store.save(&meta).await;
                                if p.is_none() {
                                    p = Profile::from_event(&meta);
                                }
                            }
                        }
                        p
                    }
                    None => None,
                };
                Ok((ev, profile))
            }
            .await;
            match res {
                Ok((ev, profile)) => {
                    let _ = tx.send(FeedEvent::One(ev, profile));
                }
                Err(e) => {
                    let _ = tx.send(FeedEvent::Failed(e.to_string()));
                }
            }
        });
        rx
    }

    /// One addressable event by (kind, author, d) — naddr resolution.
    pub fn event_by_address(
        self: &Arc<Self>,
        kind: u16,
        author: PublicKey,
        identifier: String,
    ) -> FeedRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res: Result<(Option<Event>, Option<Profile>)> = async {
                let active = this.active().await?;
                let filter = Filter::new()
                    .author(author)
                    .kind(Kind::from_u16(kind))
                    .identifier(&identifier)
                    .limit(1);
                let ev = active.pool.get(filter).await.unwrap_or_default();
                // Addressable events: newest replaces (NIP-01); take the
                // greatest (id-ordered BTreeSet ≈ latest seen).
                let ev = ev.into_iter().next_back();
                if let Some(ev) = &ev {
                    let _ = active.pool.store.save(ev).await;
                }
                let profile = match &ev {
                    Some(ev) => {
                        let authors = [ev.pubkey];
                        let cached = active
                            .pool
                            .store
                            .profiles(&authors)
                            .await
                            .unwrap_or_default();
                        let mut p: Option<Profile> =
                            cached.iter().filter_map(Profile::from_event).next();
                        if p.is_none() {
                            let f = Filter::new()
                                .kind(Kind::Metadata)
                                .author(ev.pubkey)
                                .limit(1);
                            for meta in active.pool.get(f).await.unwrap_or_default() {
                                let _ = active.pool.store.save(&meta).await;
                                if p.is_none() {
                                    p = Profile::from_event(&meta);
                                }
                            }
                        }
                        p
                    }
                    None => None,
                };
                if let Some(p) = &profile {
                    this.absorb_profiles(std::slice::from_ref(p));
                }
                Ok((ev, profile))
            }
            .await;
            match res {
                Ok((ev, profile)) => {
                    let _ = tx.send(FeedEvent::One(ev, profile));
                }
                Err(e) => {
                    let _ = tx.send(FeedEvent::Failed(e.to_string()));
                }
            }
        });
        rx
    }

    // --- search (NIP-50) ----------------------------------------------------------

    /// Note search across the read set. TODO(port): NIP-11 capability gating
    /// (only ask relays that say they can search — relay-info.ts:126).
    pub fn search(self: &Arc<Self>, query: String) -> FeedRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res: Result<FeedPage> = async {
                let active = this.active().await?;
                let filter = Filter::new()
                    .kind(Kind::TextNote)
                    .search(&query)
                    .limit(50);
                let mut results: Vec<Event> = Vec::new();
                for ev in active.pool.get(filter).await.unwrap_or_default() {
                    results.push(ev);
                }
                for ev in &results {
                    let _ = active.pool.store.save(ev).await;
                }
                results.sort_by_key(|e| std::cmp::Reverse(e.created_at.as_secs()));
                let profiles = Self::profiles_for(&active, &results).await;
                Ok(FeedPage { notes: results, profiles })
            }
            .await;
            match res {
                Ok(page) => {
                    let _ = tx.send(FeedEvent::Search(page.notes, page.profiles));
                }
                Err(e) => {
                    let _ = tx.send(FeedEvent::Failed(e.to_string()));
                }
            }
        });
        rx
    }

    // --- profiles ---------------------------------------------------------------

    /// A profile page's projection: their recent notes + their metadata.
    pub fn profile(self: &Arc<Self>, who: PublicKey) -> FeedRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res: Result<FeedPage> = async {
                let active = this.active().await?;
                // Their notes: relay fetch into the store, then project.
                let filter = Filter::new()
                    .kinds(crate::store::visible_kinds())
                    .author(who)
                    .limit(50);
                for ev in active.pool.get(filter).await.unwrap_or_default() {
                    let _ = active.pool.store.save(&ev).await;
                }
                let notes = active
                    .pool
                    .store
                    .feed_page(&[who], None, PAGE)
                    .await
                    .unwrap_or_default();
                let profiles = Self::profiles_for(&active, &notes).await;
                Ok(FeedPage { notes, profiles })
            }
            .await;
            match res {
                Ok(page) => {
                    let _ = tx.send(FeedEvent::Loaded(page));
                }
                Err(e) => {
                    let _ = tx.send(FeedEvent::Failed(e.to_string()));
                }
            }
        });
        rx
    }

    // --- threads ----------------------------------------------------------------

    /// Replies to a note (store first, relay top-up).
    pub fn thread(self: &Arc<Self>, id: EventId) -> FeedRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res: Result<FeedEvent> = async {
                let active = this.active().await?;
                let filter = Filter::new().kind(Kind::TextNote).event(id).limit(200);
                for ev in active.pool.get(filter).await.unwrap_or_default() {
                    let _ = active.pool.store.save(&ev).await;
                }
                let replies = active
                    .pool
                    .store
                    .replies(id)
                    .await
                    .map_err(|e| LoginError::Resume(e.to_string()))?;
                let authors: Vec<PublicKey> = replies.iter().map(|e| e.pubkey).collect();
                let cached = active
                    .pool
                    .store
                    .profiles(&authors)
                    .await
                    .unwrap_or_default();
                let profiles: Vec<Profile> =
                    cached.iter().filter_map(Profile::from_event).collect();
                Ok(FeedEvent::Replies(replies, profiles))
            }
            .await;
            match res {
                Ok(ev) => {
                    let _ = tx.send(ev);
                }
                Err(e) => {
                    let _ = tx.send(FeedEvent::Failed(e.to_string()));
                }
            }
        });
        rx
    }

    // --- write path ---------------------------------------------------------------

    /// Post a note (optionally a NIP-10 reply). Signed, then held for the undo
    /// window with a backstop commit; events stream on the channel.
    pub fn post_note(self: &Arc<Self>, content: String, reply_to: Option<Event>) -> PublishRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res = this.post_note_inner(&tx, content, reply_to).await;
            if let Err(e) = res {
                let _ = tx.send(PublishEvent::Failed { token: 0, error: e.to_string() });
            }
        });
        rx
    }

    async fn post_note_inner(
        self: &Arc<Self>,
        tx: &mpsc::UnboundedSender<PublishEvent>,
        content: String,
        reply_to: Option<Event>,
    ) -> Result<()> {
        let active = self.active().await?;
        let me = active.me();
        let mut tags: Vec<Tag> = Vec::new();
        if let Some(parent) = &reply_to {
            // NIP-10 marked tags; root = parent's root if marked, else parent itself.
            let mut root_id: Option<EventId> = None;
            for tag in parent.tags.clone().to_vec() {
                let parts = tag.as_slice();
                if parts.len() >= 2 && parts[0] == "e" {
                    if parts.len() >= 4 && parts[3] == "root" {
                        root_id = Some(EventId::from_str(&parts[1]).ok()).flatten();
                    }
                    break;
                }
            }
            let root = root_id.unwrap_or(parent.id);
            let empty = "";
            tags.push(Tag::custom(
                "e",
                [root.to_string(), empty.to_string(), "root".to_string()],
            ));
            tags.push(Tag::custom(
                "e",
                [parent.id.to_string(), empty.to_string(), "reply".to_string()],
            ));
            tags.push(Tag::public_key(parent.pubkey));
            if root != parent.id {
                // Root author belongs in p too when different (NIP-10).
                tags.push(Tag::custom("p", [root.to_string()]));
            }
        }
        let unsigned = UnsignedEvent::new(
            me,
            Timestamp::now(),
            Kind::TextNote,
            tags,
            content,
        );
        let signed = active.signer.sign_event(unsigned).await?;

        match active.undo.hold_duration() {
            Some(hold) => {
                let token = self.hold_seq.fetch_add(1, Ordering::SeqCst);
                let _ = tx.send(PublishEvent::Held {
                    token,
                    event: signed.clone(),
                    hold_secs: hold.as_secs(),
                });
                self.hold_and_commit(Arc::new(active.clone()), token, signed, hold, tx.clone()).await;
            }
            None => Self::commit(&active, &signed, tx).await?,
        }
        Ok(())
    }

    /// The undo window: sleep-or-cancel, then the backstop commit (ports
    /// undo.ts semantics; disk-persistence of holds lands in M1).
    async fn hold_and_commit(
        self: &Arc<Self>,
        active: Arc<Active>,
        token: u64,
        event: Event,
        hold: std::time::Duration,
        tx: mpsc::UnboundedSender<PublishEvent>,
    ) {
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
        self.holds.lock().unwrap().insert(token, cancel_tx);
        tokio::spawn({
            let core = Arc::clone(self);
            async move {
                tokio::select! {
                    _ = tokio::time::sleep(hold + crate::undo::BACKSTOP_GRACE) => {
                        if let Err(e) = Self::commit(&active, &event, &tx).await {
                            let _ = tx.send(PublishEvent::Failed { token, error: e.to_string() });
                        } else {
                            let _ = tx.send(PublishEvent::Committed { token });
                        }
                    }
                    _ = cancel_rx => {
                        let _ = tx.send(PublishEvent::Undone { token });
                    }
                }
                core.holds.lock().unwrap().remove(&token);
            }
        });
    }

    /// Cancel a held publish.
    pub fn undo(&self, token: u64) {
        if let Some(cancel) = self.holds.lock().unwrap().remove(&token) {
            let _ = cancel.send(());
        }
    }

    async fn commit(active: &Active, event: &Event, _tx: &mpsc::UnboundedSender<PublishEvent>) -> Result<()> {
        active.pool.publish(event, &active.write_relays).await
            .map_err(|e| LoginError::Connect(e.to_string()))?;
        let _ = active.pool.store.save(event).await;
        Ok(())
    }

    /// Like (NIP-25). No undo window on reactions.
    pub fn react(self: &Arc<Self>, target: &Event) -> PublishRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        let target = target.clone();
        self.rt.spawn(async move {
            let res: Result<()> = async {
                let active = this.active().await?;
                let unsigned = UnsignedEvent::new(
                    active.me(),
                    Timestamp::now(),
                    Kind::Reaction,
                    [Tag::event(target.id), Tag::public_key(target.pubkey)],
                    "+",
                );
                let signed = active.signer.sign_event(unsigned).await?;
                Self::commit(&active, &signed, &tx).await?;
                this.own_liked.lock().unwrap().insert(target.id.to_string());
                Ok(())
            }
            .await;
            if let Err(e) = res {
                let _ = tx.send(PublishEvent::Failed { token: 0, error: e.to_string() });
            } else {
                let _ = tx.send(PublishEvent::Committed { token: 0 });
            }
        });
        rx
    }

    /// Bookmark (NIP-51 kind:10003, private by default: content is a
    /// nip44-encrypted tag list — the web app's private-default semantics).
    pub fn bookmark(self: &Arc<Self>, target: &Event) -> PublishRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        let target = target.clone();
        self.rt.spawn(async move {
            let res: Result<()> = async {
                let active = this.active().await?;
                let me = active.me();
                let mut ids: Vec<String> = this.own_bookmarked.lock().unwrap().clone().into_iter().collect();
                ids.push(target.id.to_string());
                let plain = list_json(&ids, "e");
                let content = active.signer.nip44_encrypt_self(&me, &plain).await?;
                let unsigned = UnsignedEvent::new(me, Timestamp::now(), Kind::Custom(10003), [], content);
                let signed = active.signer.sign_event(unsigned).await?;
                Self::commit(&active, &signed, &tx).await?;
                this.own_bookmarked.lock().unwrap().insert(target.id.to_string());
                Ok(())
            }
            .await;
            if let Err(e) = res {
                let _ = tx.send(PublishEvent::Failed { token: 0, error: e.to_string() });
            } else {
                let _ = tx.send(PublishEvent::Committed { token: 0 });
            }
        });
        rx
    }

    /// Current NIP-65 relay split (settings pane projection).
    pub fn current_relays(&self) -> (Vec<String>, Vec<String>) {
        self.active
            .lock()
            .unwrap()
            .as_ref()
            .map(|a| (a.read_relays.clone(), a.write_relays.clone()))
            .unwrap_or_default()
    }

    /// Publish the NIP-65 relay list (kind:10002 RMW) and swap it in live:
    /// new reads join the pool, removed writes stop receiving publishes.
    pub fn set_relays(self: &Arc<Self>, read: Vec<String>, write: Vec<String>) -> PublishRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res: Result<()> = async {
                let active = this.active().await?;
                let me = active.me();
                let mut tags: Vec<Tag> = Vec::new();
                for url in &read {
                    let marker = if write.contains(url) { None } else { Some("read") };
                    tags.push(relay_tag(url, marker));
                }
                for url in &write {
                    if read.contains(url) {
                        continue; // both-capability tags carry no marker
                    }
                    tags.push(relay_tag(url, Some("write")));
                }
                let unsigned =
                    UnsignedEvent::new(me, Timestamp::now(), Kind::RelayList, tags, "");
                let signed = active.signer.sign_event(unsigned).await?;
                Self::commit(&active, &signed, &tx).await?;
                {
                    let mut a = this.active.lock().unwrap();
                    if let Some(a) = a.as_mut() {
                        a.read_relays = read.clone();
                        a.write_relays = write.clone();
                    }
                }
                // New reads join the pool (removed reads drain on restart; a
                // live force_remove port lands with the pool rewrite — M2).
                for url in &read {
                    let _ = active.pool.client().add_relay(url.clone()).await;
                }
                Ok(())
            }
            .await;
            if let Err(e) = res {
                let _ = tx.send(PublishEvent::Failed { token: 0, error: e.to_string() });
            } else {
                let _ = tx.send(PublishEvent::Committed { token: 0 });
            }
        });
        rx
    }

    /// Follow / unfollow (NIP-02 kind:3 read-modify-write).
    pub fn set_following(self: &Arc<Self>, who: PublicKey, follow: bool) -> PublishRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res: Result<()> = async {
                let active = this.active().await?;
                let me = active.me();
                let mut follows = active.follows.clone();
                if follow {
                    if !follows.contains(&who) {
                        follows.push(who);
                    }
                } else {
                    follows.retain(|f| f != &who);
                }
                let tags: Vec<Tag> = follows.iter().map(|pk| Tag::public_key(*pk)).collect();
                let unsigned = UnsignedEvent::new(me, Timestamp::now(), Kind::ContactList, tags, "");
                let signed = active.signer.sign_event(unsigned).await?;
                Self::commit(&active, &signed, &tx).await?;
                {
                    let mut a = this.active.lock().unwrap();
                    if let Some(a) = a.as_mut() {
                        a.follows = follows;
                    }
                }
                Ok(())
            }
            .await;
            if let Err(e) = res {
                let _ = tx.send(PublishEvent::Failed { token: 0, error: e.to_string() });
            } else {
                let _ = tx.send(PublishEvent::Committed { token: 0 });
            }
        });
        rx
    }

    /// Mute / unmute (NIP-51 kind:10000, content encrypted to self).
    pub fn set_muted(self: &Arc<Self>, who: PublicKey, mute: bool) -> PublishRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res: Result<()> = async {
                let active = this.active().await?;
                let me = active.me();
                let mut mutes = this.muted.lock().unwrap().clone();
                let hex = who.to_string();
                if mute {
                    mutes.insert(hex.clone());
                } else {
                    mutes.remove(&hex);
                }
                let plain = list_json(&mutes.into_iter().collect::<Vec<_>>(), "p");
                let content = active.signer.nip44_encrypt_self(&me, &plain).await?;
                let unsigned = UnsignedEvent::new(me, Timestamp::now(), Kind::Custom(10000), [], content);
                let signed = active.signer.sign_event(unsigned).await?;
                Self::commit(&active, &signed, &tx).await?;
                Ok(())
            }
            .await;
            if let Err(e) = res {
                let _ = tx.send(PublishEvent::Failed { token: 0, error: e.to_string() });
            } else {
                let _ = tx.send(PublishEvent::Committed { token: 0 });
            }
        });
        rx
    }

    /// Delete own post (NIP-09 kind:5) + local tombstone.
    pub fn delete(self: &Arc<Self>, target: &Event) -> PublishRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        let target = target.clone();
        self.rt.spawn(async move {
            let res: Result<()> = async {
                let active = this.active().await?;
                let unsigned = UnsignedEvent::new(
                    active.me(),
                    Timestamp::now(),
                    Kind::EventDeletion,
                    [Tag::event(target.id)],
                    "",
                );
                let signed = active.signer.sign_event(unsigned).await?;
                Self::commit(&active, &signed, &tx).await?;
                let _ = active.pool.store.delete_event(target.id).await;
                Ok(())
            }
            .await;
            if let Err(e) = res {
                let _ = tx.send(PublishEvent::Failed { token: 0, error: e.to_string() });
            } else {
                let _ = tx.send(PublishEvent::Committed { token: 0 });
            }
        });
        rx
    }

    async fn active(&self) -> Result<Active> {
        self.active
            .lock()
            .unwrap()
            .clone()
            .ok_or(LoginError::Resume("no active session".into()))
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// NIP-65 `r` tag with optional read/write marker.
fn relay_tag(url: &str, marker: Option<&str>) -> Tag {
    match marker {
        Some(m) => Tag::custom("r", [url.to_string(), m.to_string()]),
        None => Tag::custom("r", [url.to_string()]),
    }
}

/// NIP-51 encrypted list content: [["p", hex], ...] / [["e", hex], ...].
fn list_json(items: &[String], letter: &str) -> String {
    let arr: Vec<[String; 2]> = items.iter().map(|i| [letter.to_string(), i.clone()]).collect();
    serde_json::to_string(&arr).unwrap_or_default()
}

/// Parse an encrypted list's plaintext back into item ids.
fn parse_list_event(plain: &str) -> Vec<String> {
    let arr: Vec<[String; 2]> = serde_json::from_str(plain).unwrap_or_default();
    arr.into_iter().filter_map(|pair| pair.get(1).cloned()).collect()
}
