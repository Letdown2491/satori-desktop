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
use crate::pool::{Pool, INDEXERS};
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
pub enum FeedEvent {
    Loaded(FeedPage),
    /// Older page + has-more.
    Older(FeedPage, bool),
    Replies(Vec<Event>, Vec<Profile>),
    /// Notifications (replies + mentions; reactions stay off by default).
    Notifications(Vec<Event>, Vec<Profile>),
    /// Whether the bell should light (unread past high-water).
    NotifDot(bool),
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
    Failed { token: u64, error: String },
}

pub type PublishRx = mpsc::UnboundedReceiver<PublishEvent>;

#[derive(Clone)]
struct Active {
    session: Session,
    signer: Signer,
    pool: Arc<Pool>,
    undo: crate::undo::UndoWindow,
    /// Own NIP-65 write relays.
    write_relays: Vec<String>,
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
}

impl Profile {
    fn from_event(ev: &Event) -> Option<Self> {
        #[derive(serde::Deserialize)]
        struct RawMeta {
            name: Option<String>,
            display_name: Option<String>,
            picture: Option<String>,
        }
        let raw: RawMeta = serde_json::from_str(&ev.content).ok()?;
        Some(Self {
            pubkey: ev.pubkey.to_string(),
            name: raw.display_name.or(raw.name).unwrap_or_default(),
            picture: raw.picture.unwrap_or_default(),
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
            holds: Mutex::new(std::collections::HashMap::new()),
        }
    }

    // --- lifecycle -----------------------------------------------------------

    /// Resume a persisted session, if any. The bunker signer is rebuilt from
    /// the transport secret without user interaction (ports session resume).
    pub fn init(&self) -> Option<Session> {
        let session = session::load()?;
        match self.rt.block_on(self.build_active(&session)) {
            Ok(active) => {
                *self.active.lock().unwrap() = Some(active);
                Some(session)
            }
            Err(e) => {
                eprintln!("[core] resume failed: {e}");
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

    /// Engagement lookups for card rendering (pure set membership, render is a
    /// projection — DESIGN.md §1.2).
    pub fn is_liked(&self, event_id: &EventId) -> bool {
        self.own_liked.lock().unwrap().contains(&event_id.to_string())
    }

    pub fn is_bookmarked(&self, event_id: &EventId) -> bool {
        self.own_bookmarked.lock().unwrap().contains(&event_id.to_string())
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

        let active = self.rt.block_on(self.build_active(&session))?;
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

    async fn build_active(&self, session: &Session) -> Result<Active> {
        let signer = self.build_signer(session).await?;
        let pool = Arc::new(Pool::open().await.map_err(|e| LoginError::Resume(e.to_string()))?);
        Ok(Active {
            session: session.clone(),
            signer,
            pool,
            undo: crate::undo::UndoWindow::default(),
            write_relays: Vec::new(),
            follows: Vec::new(),
        })
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
        let (read, write) = active
            .pool
            .nip65(me)
            .await
            .unwrap_or_else(|_| (Vec::new(), Vec::new()));
        active.write_relays = write.clone();
        if let Some(a) = self.active.lock().unwrap().as_mut() {
            a.write_relays = write;
        }
        active.pool.connect_reads(read).await.ok();

        let follows = active.pool.follows(me).await.unwrap_or_default();
        active.follows = follows.clone();
        if let Some(a) = self.active.lock().unwrap().as_mut() {
            a.follows = follows.clone();
        }

        // Deep feed fetch (deadline-capped).
        if !follows.is_empty() {
            let filter = Filter::new()
                .kind(Kind::TextNote)
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
        // TODO(port): bookmarks need the NIP-44-decrypted private list too (M3).
        if let Ok(Some(ev)) = active
            .pool
            .store
            .latest_by_kind(me, Kind::Custom(10003))
            .await
        {
            let mut b = self.own_bookmarked.lock().unwrap();
            for id in ev.tags.clone().event_ids() {
                b.insert(id.to_string());
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
            .kind(Kind::TextNote)
            .authors(follows.clone())
            .since(Timestamp::from(since))
            .limit(100);
        for ev in active.pool.get(filter).await.unwrap_or_default() {
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

    async fn page_from_store(&self, active: &Active, until: Option<u64>) -> Result<FeedPage> {
        let follows = active.follows.clone();
        let notes = active
            .pool
            .store
            .feed_page(&follows, until, PAGE)
            .await
            .map_err(|e| LoginError::Resume(e.to_string()))?;
        let profiles = Self::profiles_for(active, &notes).await;
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

    /// Bookmark (NIP-51 kind:10003, public slice).
    pub fn bookmark(self: &Arc<Self>, target: &Event) -> PublishRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        let target = target.clone();
        self.rt.spawn(async move {
            let res: Result<()> = async {
                let active = this.active().await?;
                let me = active.me();
                // Merge into the existing list event (read-modify-write).
                let prior = active
                    .pool
                    .store
                    .latest_by_kind(me, Kind::Custom(10003))
                    .await
                    .ok()
                    .flatten();
                let mut tags: Vec<Tag> = prior
                    .as_ref()
                    .map(|ev| ev.tags.clone().to_vec())
                    .unwrap_or_default();
                tags.push(Tag::event(target.id));
                let unsigned = UnsignedEvent::new(me, Timestamp::now(), Kind::Custom(10003), tags, "");
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

fn _silence_indexers() {
    let _ = INDEXERS.len();
}
