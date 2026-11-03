//! Core: owns the tokio runtime and the active session — the seam between the
//! GTK UI and all I/O. UI code submits commands here; results flow back over
//! executor-agnostic channels (`tokio::sync`) awaited from the GLib main
//! context (DESIGN.md §3.2). The web app's server-session machinery
//! (`src/session.ts`) collapses to this single-user handle.

use std::collections::{HashMap, HashSet, VecDeque};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use nostr::prelude::*;
use nostr_connect::prelude::*;
use tokio::sync::mpsc;

use crate::blocktime::Blocktime;
use crate::error::{LoginError, Result};
use crate::nip::nip78;
use crate::pool::Pool;
use crate::secrets;
use crate::session::{self, Session, SessionMode};
use crate::signer::{self, Signer};
use crate::wallet::{self, Wallet, WalletEvent};

/// How long to wait for the remote signer to approve a connect request.
const LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Feed page size; the bootstrap fetch keeps depth for "view older".
const PAGE: usize = 40;
/// How long the cached NIP-09 deletion set stands (drop_deleted).
const DELETED_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// The follows' deletion pairs (author, deleted event id) — shared out of
/// the cache as an Arc so the per-page hit path costs one bump.
type DeletedSet = Arc<HashSet<(PublicKey, EventId)>>;
/// How deep one older-page top-up reaches into the relays (the web app's
/// fetchRoutedPage asked limit*2 per author shard — same spirit: each round
/// trip should feed several pages before the next one).
const TOPUP_DEPTH: usize = 200;
const BOOTSTRAP_DEPTH: usize = 200;



/// Progress events streamed back to the login UI.
#[derive(Debug, Clone)]
pub enum ConnectEvent {
    /// Request sent; the user must approve in their bunker app.
    Waiting,
    /// An out-of-band auth_url approval (NIP-46); open it in a browser.
    AuthUrl(String),
    /// The nostrconnect:// URI to advertise (QR in the UI) — client-initiated
    /// NIP-46; the signer scans it and approves.
    Uri(String),
    Approved(PublicKey),
    Failed(String),
}

/// The channel login flows resolve through. Tokio's sync primitives are
/// executor-agnostic, so the GLib main context can await them directly.
pub type ConnectRx = mpsc::UnboundedReceiver<ConnectEvent>;

/// One zap's result stream: the events for the view that started (or
/// retried) the payment. The same events also flow on the wallet broadcast
/// (`subscribe_wallet`) — a view's channel dies with the view, the feed's
/// bolt glyphs must not.
pub type WalletResultRx = mpsc::UnboundedReceiver<WalletEvent>;

/// Zap-flow outcome: the error IS the event (a failed zap is a user-facing
/// message, not a crash).
type ZapFlow = std::result::Result<(), WalletEvent>;

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
    /// Resolved reply parents for the timeline's context whisper: parent
    /// event id → author + snippet. Misses resolve on a later ask.
    Parents(std::collections::HashMap<String, ParentNote>),
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
    /// Your kind:10063 Blossom media-server list (settings pane).
    MediaServers(Vec<String>),
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
    /// A Blossom upload finished (upload.rs): blob URL + the NIP-92 imeta tag.
    Uploaded { url: String, imeta: Vec<String> },
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
    /// Own pubkey, parsed once at session build. me() hands it out; it
    /// used to re-parse the session hex per call (26 call sites, some on
    /// hot paths) and expect-panicked if session.json held a corrupt
    /// pubkey — build_active now refuses the session instead.
    me: PublicKey,
}

impl Active {
    fn me(&self) -> PublicKey {
        self.me
    }
}

/// A rendered-ready feed page: notes plus resolved profiles.
#[derive(Debug, Clone)]
pub struct FeedPage {
    pub notes: Vec<Event>,
    pub profiles: Vec<Profile>,
}

/// A push signal off the live subscription (one persistent sub per session,
/// three filters). Live signaling, not the live timeline — nothing renders
/// until a view asks. Also carries a few out-of-band manager signals (the
/// NIP-78 login sync) that ride the same broadcast for want of a second one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Live {
    /// A reply or mention tagging me (#p) — light the bell.
    Mention,
    /// A fresh post from a follow — bump the timeline's new-notes pill.
    Post,
    /// NIP-78 app-data sync landed (login): remote prefs won — re-read.
    PrefsSynced,
}

/// A reply's parent, for the timeline context whisper: author + a plain
/// one-liner from the parent's content.
#[derive(Debug, Clone)]
pub struct ParentNote {
    pub author: String,
    pub snippet: String,
}

/// Plain-text one-liner for previews: whitespace collapsed, `nostr:` refs
/// ellipsized, URLs dropped entirely (they never read as text — mangled
/// “https//…” tails in a whisper or quote preview were pure noise; links get
/// their own row/embed), capped at `cap` chars. Shared by the reply whisper
/// (load_parents) and quote embeds (content.rs embed_card).
pub fn plain_snippet(content: &str, cap: usize) -> String {
    let mut words: Vec<&str> = Vec::new();
    for w in content.split_whitespace() {
        if w.starts_with("nostr:") {
            words.push("…");
        } else if w.starts_with("https://") || w.starts_with("http://") {
            continue;
        } else {
            words.push(w);
        }
    }
    let mut s = words.join(" ");
    if s.chars().count() > cap {
        s = s.chars().take(cap).collect::<String>() + "…";
    }
    s
}

#[derive(Debug, Clone, Default)]
pub struct Profile {
    pub pubkey: String,
    pub name: String,
    pub picture: String,
    pub about: String,
    pub banner: String,
    /// NIP-05 address as self-asserted ("name@domain") — the profile page
    /// shows it, then verifies it out-of-band (verify_nip05).
    pub nip05: String,
    /// The lightning pointer, lud16 before lud06 (NIP-57's order) — the
    /// profile page's zap row (same field the zap request reads).
    pub lud: String,
    /// The banner's blurhash, from a kind-0 imeta tag whose url IS the
    /// banner (NIP-92, Oct 2026) — the header's placeholder while the
    /// bytes are in flight.
    pub banner_blurhash: Option<String>,
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
            nip05: Option<String>,
            lud16: Option<String>,
            lud06: Option<String>,
        }
        let raw: RawMeta = serde_json::from_str(&ev.content).ok()?;
        // NIP-92 (Oct 2026): a kind-0 MAY carry imeta describing its
        // picture and banner — a tag describes a field only when its url
        // matches exactly. We adopt the banner's blurhash (the header
        // placeholder); `dim` is moot under the cover-fit strip.
        let banner_blurhash = crate::nip::imeta::parse(ev)
            .into_iter()
            .find(|m| Some(m.url.as_str()) == raw.banner.as_deref())
            .and_then(|m| m.blurhash);
        Some(Self {
            pubkey: ev.pubkey.to_string(),
            name: raw.display_name.or(raw.name).unwrap_or_default(),
            picture: raw.picture.unwrap_or_default(),
            about: raw.about.unwrap_or_default(),
            banner: raw.banner.unwrap_or_default(),
            nip05: raw.nip05.unwrap_or_default(),
            lud: raw.lud16.or(raw.lud06).unwrap_or_default(),
            banner_blurhash,
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

/// In-memory cap for the cross-session profile mirror (`profiles_mem`):
/// display data for mentions/search only — the durable copy is the
/// profile service's profiles.json (10k there). 5000 hydrated profiles
/// is far past anything a long session touches.
const PROFILES_MEM_CAP: usize = 5000;

/// The bounded profile mirror. A plain HashMap is insert-only, so every
/// unique pubkey seen (each feed page absorbs ~40) stayed for the process
/// lifetime — and it survived logout, too. Insertion-order eviction: the
/// oldest-absorbed profile goes first, which is exactly the stale half
/// of the cache.
struct ProfileMem {
    map: HashMap<String, Profile>,
    order: VecDeque<String>,
}

impl ProfileMem {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// Absorb one profile. Overwrites don't re-push the order key (the
    /// entry keeps its place; re-pushing would turn the deque into a
    /// duplicate-riddled mess for zero benefit).
    fn insert(&mut self, p: Profile) {
        let key = p.pubkey.clone();
        if !self.map.contains_key(&key)
            && self.map.len() >= PROFILES_MEM_CAP
            && let Some(oldest) = self.order.pop_front()
        {
            self.map.remove(&oldest);
        }
        if !self.map.contains_key(&key) {
            self.order.push_back(key.clone());
        }
        self.map.insert(key, p);
    }

    fn get(&self, pk: &str) -> Option<&Profile> {
        self.map.get(pk)
    }

    fn values(&self) -> impl Iterator<Item = &Profile> {
        self.map.values()
    }

    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }
}

pub struct Core {
    rt: tokio::runtime::Runtime,
    /// Hold-state mirror (Held/Committed/Undone/Failed) for the shell's
    /// undo toast — the composer pops on Held, so the toast must outlive
    /// it. subscribe_hold().
    hold_tx: tokio::sync::broadcast::Sender<PublishEvent>,
    active: Mutex<Option<Active>>,
    pending: Mutex<Option<Arc<NostrConnect>>>,
    hold_seq: AtomicU64,
    /// Login epoch: bumped on every connect attempt and cancel; a
    /// handshake whose epoch has moved on drops itself instead of saving
    /// a session the UI (or a newer attempt) no longer expects — the
    /// core-side twin of the login view's generation guard.
    login_seq: AtomicU64,
    /// ONE pool for the process: the LMDB write lock must not survive a
    /// logout (a second env open on the same path deadlocks). The profile
    /// service's debouncer is session-scoped (Weak handle — it exits when
    /// the service drops), so each login builds its own.
    pool: Mutex<Option<Arc<Pool>>>,
    /// Own reactions: target event id (hex) → engaged.
    own_liked: Mutex<HashSet<String>>,
    /// Own reposts (NIP-18 kind 6/16) — same shape as own_liked.
    own_reposted: Mutex<HashSet<String>>,
    own_bookmarked: Mutex<HashSet<String>>,
    /// Muted authors (pubkey hex) — NIP-51 kind:10000, content encrypted.
    muted: Mutex<HashSet<String>>,
    /// Own zaps: note id (hex) → zapped. The ledger (zaps.json) is the
    /// durable authority; this mirrors it for render, like own_liked.
    own_zapped: Mutex<HashSet<String>>,
    /// The follows' NIP-09 deletion pairs, cached for DELETED_TTL. The
    /// feed projection drops deleted notes per page — twice per scroll
    /// past the store's depth (the top-up re-projects) — and each miss
    /// was a full kind-5 query + hash rebuild. A fresh deletion lands
    /// via the live sub or a refresh save and is honored at the next
    /// projection, worst case a TTL later.
    deleted_cache: Mutex<Option<(std::time::Instant, DeletedSet)>>,
    /// The NIP-47 wallet handle (D20) — one per process, like the pool.
    wallet: Arc<Wallet>,
    /// Bitcoin timestamp→height service (blocktime, Phase 2 §4.4) — one
    /// per process; its table outlives sessions like the ledger.
    blocktime: Arc<Blocktime>,
    /// Resolved profiles (mentions/name display) — absorbed as they stream
    /// in. Bounded (`ProfileMem`): insert-only maps grow for the process
    /// lifetime otherwise, and this one survives logouts by design.
    profiles_mem: Mutex<ProfileMem>,
    /// Live undo holds: token → cancel signal.
    holds: Mutex<std::collections::HashMap<u64, tokio::sync::oneshot::Sender<()>>>,
    /// Push signals off the live subscription (shell bell, feed pills).
    live: tokio::sync::broadcast::Sender<Live>,
    /// The live sub starts once per process (bootstrap runs per surface).
    /// Whether the live subscription is armed (start_live). An Arc so the
    /// spawned task can re-arm it when the subscribe fails.
    live_on: Arc<std::sync::atomic::AtomicBool>,
    /// Identity epoch for the live sub: bumped on sign-out. A running
    /// live loop compares its captured epoch per event and tears down
    /// (unsubscribe + exit) on mismatch — the loop used to outlive its
    /// session, still filtering by the old account's pubkey and follows,
    /// while live_on true blocked the next login from arming its own.
    live_epoch: Arc<std::sync::atomic::AtomicU64>,
}

impl Default for Core {
    fn default() -> Self {
        Self::new()
    }
}

impl Core {
    pub fn new() -> Self {
        // Keep the diagnostic mirror bounded (diag.rs) before anything logs.
        crate::diag::rotate();
        // The runtime is built first — the blocktime service needs its
        // handle for the single-flight spawns (Handle::current would panic
        // outside a runtime).
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");
        // One-time media-cache migration: the pre-polish cache could hold
        // poisoned files (bytes written before decode validated them); the
        // versioned dir starts clean, the legacy dir goes (media.rs).
        rt.spawn(async { crate::media::purge_legacy_cache(); });
        let blocktime = Arc::new(Blocktime::new(rt.handle().clone()));
        Self {
            rt,
            blocktime,
            active: Mutex::new(None),
            pending: Mutex::new(None),
            hold_seq: AtomicU64::new(1),
            login_seq: AtomicU64::new(1),
            pool: Mutex::new(None),
            own_liked: Mutex::new(HashSet::new()),
            own_reposted: Mutex::new(HashSet::new()),
            own_bookmarked: Mutex::new(HashSet::new()),
            muted: Mutex::new(HashSet::new()),
            own_zapped: Mutex::new(HashSet::new()),
            deleted_cache: Mutex::new(None),
            wallet: Arc::new(Wallet::new()),
            profiles_mem: Mutex::new(ProfileMem::new()),
            holds: Mutex::new(std::collections::HashMap::new()),
            live: tokio::sync::broadcast::Sender::new(64),
            hold_tx: tokio::sync::broadcast::Sender::new(16),
            live_on: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            live_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// Suspend watchdog (DESIGN.md §3.4): after sleep/hibernate every relay
    /// socket is dead. The SDK's 55s ping notices, but its adaptive backoff
    /// (10s doubling) can take minutes to reconnect — until then every read
    /// burns its whole budget against dead relays. A wall-clock jump far
    /// larger than the monotonic one means the machine slept: poke every
    /// relay (`connect()` is idempotent) so reads recover in seconds, not
    /// minutes. One probe per 30s for the process lifetime; skipped while
    /// logged out. Call once, from main, on the Arc'd core.
    pub fn spawn_suspend_watchdog(self: &Arc<Self>) {
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let mut last_wall = std::time::SystemTime::now();
            let mut last_mono = std::time::Instant::now();
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                let wall = std::time::SystemTime::now();
                let mono = std::time::Instant::now();
                let wall_d = wall.duration_since(last_wall).unwrap_or_default();
                let mono_d = mono.duration_since(last_mono);
                last_wall = wall;
                last_mono = mono;
                // CLOCK_MONOTONIC freezes in sleep; the wall clock doesn't.
                if wall_d.saturating_sub(mono_d) <= std::time::Duration::from_secs(90) {
                    continue;
                }
                // The pool() getter would BUILD a pool while logged out —
                // read the raw field and skip when there's nothing to poke.
                let Some(pool) = this.pool.lock().unwrap().clone() else {
                    continue;
                };
                crate::dlog!(
                    "watchdog: wall jumped {:?} vs monotonic {:?} — machine slept; reconnecting relays",
                    wall_d, mono_d
                );
                pool.client().connect().await;
            }
        });
    }

    // --- lifecycle -----------------------------------------------------------

    /// Resume a persisted session, if any. Returns the session plus the
    /// failure reason (shown on the login page — silent resume failures are
    /// how users get lost). The bunker signer is rebuilt from the transport
    /// secret without user interaction.
    /// Sign out: forget the session file + both secret-store entries
    /// (logout must not leave residue) and drop the in-memory Active — the
    /// next login rebuilds it exactly as a fresh process would.
    pub fn sign_out(&self) {
        crate::session::clear();
        crate::secrets::delete(crate::secrets::NSEC);
        crate::secrets::delete(crate::secrets::BUNKER_CLIENT_SECRET);
        *self.active.lock().unwrap() = None;
        // The deletion cache was built from the OLD session's follow set.
        *self.deleted_cache.lock().unwrap() = None;
        // The live sub is armed per session: the epoch bump makes the old
        // loop tear down at its next event, and live_on false lets the
        // next login's bootstrap arm a fresh one with the new identity.
        self.live_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.live_on
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn init(&self) -> (Option<Session>, Option<String>) {
        let Some(session) = session::load() else {
            return (None, None); // first run — not an error
        };
        match self.rt.block_on(self.build_active(&session)) {
            Ok(active) => {
                *self.active.lock().unwrap() = Some(active);
                self.resume_wallet();
                (Some(session), None)
            }
            Err(e) => {
                eprintln!("[core] resume failed: {e}");
                // The notice copy is human on purpose (the doors show it
                // verbatim); the technical detail is the stderr line above.
                match e {
                    // Definitive — retrying can never succeed: forget the
                    // session so the notice shows once, not every launch.
                    LoginError::CredentialGone(_) => {
                        session::clear();
                        (
                            None,
                            Some("Couldn't unlock your saved session — sign in again \
                                  below. Your keys stay with your signer."
                                .into()),
                        )
                    }
                    // Transient (relay down at launch, store timeout): keep
                    // the session — the next launch retries the resume.
                    _ => (
                        None,
                        Some("Couldn't reach your signer just now — Satori retries \
                              on next launch, or sign in below."
                            .into()),
                    ),
                }
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
        // Shut the bunker transport down FIRST (zombie kind-24133
        // subscriptions = phantom approval prompts in the user's bunker app
        // after logout).
        let old = self.active.lock().unwrap().take();
        if let Some(a) = old
            && let Signer::Bunker(connect) = &a.signer {
                let connect = Arc::clone(connect);
                self.rt
                    .spawn(async move { Arc::unwrap_or_clone(connect).shutdown().await });
            }
        session::clear();
        secrets::delete(secrets::BUNKER_CLIENT_SECRET);
        secrets::delete(secrets::NSEC);
        // The wallet is a session-scoped capability too: shut the NWC
        // transport down (phantom subscriptions are the bunker bug's
        // cousin) and forget the render mirror. The LEDGER stays — it is
        // history, not state.
        let w = Arc::clone(&self.wallet);
        self.rt.spawn(async move { w.disconnect().await });
        self.own_liked.lock().unwrap().clear();
        self.own_reposted.lock().unwrap().clear();
        self.own_bookmarked.lock().unwrap().clear();
        self.own_zapped.lock().unwrap().clear();
        self.muted.lock().unwrap().clear();
        // The profile mirror is display state for the logged-in session's
        // reading; a next login re-hydrates it from its own feed pages.
        self.profiles_mem.lock().unwrap().clear();
        eprintln!("[core] logout");
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

    /// The engagement sets, for a view's session snapshot (feed.rs seeds its
    /// optimistic sets from these at construction — after a restart the
    /// glyphs still read engaged).
    pub fn liked_set(&self) -> HashSet<String> {
        self.own_liked.lock().unwrap().clone()
    }

    pub fn bookmarked_set(&self) -> HashSet<String> {
        self.own_bookmarked.lock().unwrap().clone()
    }

    pub fn reposted_set(&self) -> HashSet<String> {
        self.own_reposted.lock().unwrap().clone()
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

    /// NIP-05 verification: does `domain`'s /.well-known/nostr.json vouch
    /// that `name` maps to this pubkey? One SSRF-guarded JSON fetch (the
    /// same path the zap invoice rides). `None` = unproven (fetch failed,
    /// malformed answer, mismatch beyond doubt is `Some(false)`); the UI
    /// treats None as "stay quiet", never red — verification is an
    /// affordance, not an accusation.
    pub fn verify_nip05(
        self: &Arc<Self>,
        nip05: String,
        pubkey: String,
    ) -> mpsc::UnboundedReceiver<Option<bool>> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.rt.spawn(async move {
            let verdict = verify_nip05_inner(&nip05, &pubkey).await;
            let _ = tx.send(verdict);
        });
        rx
    }

    /// Fetch (and cache) an image — SSRF-guarded, sha256-keyed (media.rs).
    /// The outcome carries the failure KIND: Transient failures cool off and
    /// get another look in the UI; NotMedia is a verdict (the bytes arrived
    /// whole and are not an image).
    pub fn fetch_image(
        self: &Arc<Self>,
        url: String,
    ) -> mpsc::UnboundedReceiver<
        std::result::Result<crate::media::DecodedImage, crate::media::MediaError>,
    > {
        let (tx, rx) = mpsc::unbounded_channel();
        self.rt.spawn(async move {
            let res = crate::media::fetch_image(&url).await;
            let _ = tx.send(res);
        });
        rx
    }

    /// The poster job (D32): everything tokio-flavored stays on this
    /// runtime — the UI's executor has no tokio context, and awaiting
    /// these futures from it panicked thread 'main (the crash the day
    /// D32 shipped). The sniffer is injected by the UI (gstreamer lives
    /// there) and runs on this pool's blocking threads; the channel is
    /// fetch_image's exact seam.
    pub fn fetch_poster(
        self: &Arc<Self>,
        url: String,
        cap: usize,
        sniff: impl FnOnce(&[u8]) -> std::result::Result<(u32, u32, Vec<u8>), ()> + Send + 'static,
    ) -> mpsc::UnboundedReceiver<
        std::result::Result<crate::media::DecodedImage, crate::media::MediaError>,
    > {
        let (tx, rx) = mpsc::unbounded_channel();
        self.rt.spawn(async move {
            let res = crate::media::fetch_poster(url, cap, sniff).await;
            let _ = tx.send(res);
        });
        rx
    }

    // --- wallet / zaps (D20) ---------------------------------------------------

    /// The wallet handle (the settings pane reads status/balance directly).
    pub fn wallet(&self) -> Arc<Wallet> {
        Arc::clone(&self.wallet)
    }

    // --- blocktime (Phase 2 §4.4) ----------------------------------------------

    /// Resolve a timestamp to a Bitcoin block height (Feather's blocktime
    /// spec): hot answers immediately, otherwise one guarded single-flight
    /// fetch to mempool.space. The answer lands on every waiter.
    pub fn block_height(self: &Arc<Self>, ts: u64) -> crate::blocktime::BlockRx {
        self.blocktime.get(ts)
    }

    /// The display-ready answer: exact when known, interpolated from the
    /// nearest point otherwise. Pure read — per-frame safe, never fetches.
    pub fn block_height_cached(&self, ts: u64) -> Option<u64> {
        self.blocktime.cached(ts)
    }

    /// Is there an exact (fetched or recorded) height for this timestamp?
    /// The ensure path's skip test — interpolation doesn't count.
    pub fn block_height_known(&self, ts: u64) -> bool {
        self.blocktime.known(ts)
    }

    /// Wallet event stream: status changes, pay outcomes, verified
    /// receipts. The global pump every surface keeps.
    pub fn subscribe_wallet(&self) -> wallet::WalletRx {
        self.wallet.subscribe()
    }

    /// Connect a `nostr+walletconnect://` URI. Status streams on
    /// subscribe_wallet (Connecting → Ready | ConnectFailed).
    pub fn connect_wallet(&self, uri: String) {
        let w = Arc::clone(&self.wallet);
        self.rt.spawn(async move { w.connect(uri).await });
    }

    /// Disconnect and forget (wallet.json + the secret store entry).
    pub fn disconnect_wallet(&self) {
        let w = Arc::clone(&self.wallet);
        self.rt.spawn(async move { w.disconnect().await });
    }

    /// Reconnect a persisted wallet and seed the own-zapped mirror from
    /// the ledger. Best-effort: a dead wallet backend is a settings
    /// concern, never a login failure.
    fn resume_wallet(&self) {
        // Paid/Confirmed notes render settled without waiting on the
        // wallet; Pending/Uncertain still owe the wallet an answer.
        let mut zapped = self.own_zapped.lock().unwrap();
        for z in wallet::ledger_history() {
            if matches!(z.state, wallet::ZapState::Paid | wallet::ZapState::Confirmed) {
                zapped.insert(z.note);
            }
        }
        drop(zapped);
        let w = Arc::clone(&self.wallet);
        self.rt.spawn(async move { w.resume().await });
    }

    /// Zap-state lookup for render (the settled bolt).
    pub fn is_zapped(&self, note: &EventId) -> bool {
        self.own_zapped.lock().unwrap().contains(&note.to_string())
    }

    /// The whole own-zapped set (a view seeds its snapshot from this).
    pub fn zapped_set(&self) -> HashSet<String> {
        self.own_zapped.lock().unwrap().clone()
    }

    /// Zap a note: a PUBLIC NIP-57 zap through the connected NWC wallet
    /// (the tap = 21 sats default). Events stream on the returned channel
    /// AND on subscribe_wallet — see [`WalletResultRx`].
    pub fn zap(self: &Arc<Self>, note: Event, msat: u64) -> WalletResultRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        let w = Arc::clone(&self.wallet);
        self.rt.spawn(async move {
            if let Err(e) = this.zap_inner(&note, msat, &tx).await {
                // Early failures never reach pay_stored's own emit — the
                // feed's whisper still needs to hear them.
                w.emit(e.clone());
                let _ = tx.send(e);
            }
        });
        rx
    }

    /// Retry a stored zap (Pending/Uncertain only). The SAME signed request
    /// goes back out — same event id, no duplicate zap events; that is the
    /// ledger's whole reason for keeping the full signed 9734.
    pub fn retry_zap(self: &Arc<Self>, note: String) -> WalletResultRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        let w = Arc::clone(&self.wallet);
        self.rt.spawn(async move {
            if let Err(e) = this.retry_inner(note, &tx).await {
                w.emit(e.clone());
                let _ = tx.send(e);
            }
        });
        rx
    }

    async fn retry_inner(
        self: &Arc<Self>,
        note: String,
        tx: &mpsc::UnboundedSender<WalletEvent>,
    ) -> ZapFlow {
        let Some(record) = wallet::ledger_for_note(&note).filter(|z| {
            matches!(z.state, wallet::ZapState::Pending | wallet::ZapState::Uncertain)
        }) else {
            return Err(WalletEvent::ZapFailed {
                note,
                reason: "nothing to retry for this note".into(),
            });
        };
        // The stored request replays as-is — re-signing would mint a new
        // event id and risk a duplicate zap event (the Feather rule).
        let signed = serde_json::from_str::<Event>(&record.event).map_err(|_| {
            WalletEvent::ZapFailed {
                note: note.clone(),
                reason: "stored zap request is corrupt".into(),
            }
        })?;
        // One in-flight payment per note — the retry reclaims the slot.
        if !self.wallet.claim_flight(&note) {
            return Err(WalletEvent::ZapFailed {
                note,
                reason: "a zap on this note is already in flight".into(),
            });
        }
        let _guard = FlightGuard(&self.wallet, &note);
        self.pay_stored(&record, &signed, tx).await
    }

    async fn zap_inner(
        self: &Arc<Self>,
        note: &Event,
        msat: u64,
        tx: &mpsc::UnboundedSender<WalletEvent>,
    ) -> ZapFlow {
        let note_hex = note.id.to_string();
        // One in-flight payment per note — the Feather rule (the wallet
        // owns the set; fresh zaps and retries both come through it).
        if !self.wallet.claim_flight(&note_hex) {
            return Err(WalletEvent::ZapFailed {
                note: note_hex,
                reason: "a zap on this note is already in flight".into(),
            });
        }
        let _guard = FlightGuard(&self.wallet, &note_hex);

        if !self.wallet.is_ready() {
            return Err(WalletEvent::ZapFailed {
                note: note_hex.clone(),
                reason: "no wallet connected".into(),
            });
        }
        let active = self.active().await.map_err(|e| WalletEvent::ZapFailed {
            note: note_hex.clone(),
            reason: e.to_string(),
        })?;
        let me = active.me();
        let recipient = note.pubkey;

        // The recipient's lightning pointer (lud16 before lud06). No
        // address, no zap — an honest quiet failure, not a dialog.
        let lud = match active
            .pool
            .store
            .latest_by_kind(recipient, Kind::Metadata)
            .await
        {
            Ok(Some(meta)) => lightning_field(&meta.content),
            _ => None,
        }
        .ok_or_else(|| WalletEvent::ZapFailed {
            note: note_hex.clone(),
            reason: "this profile has no lightning address".into(),
        })?;
        let lnurl = wallet::lnurlp_url(&lud).map_err(|e| WalletEvent::ZapFailed {
            note: note_hex.clone(),
            reason: e,
        })?;

        // Sign FIRST (through the owner's signer — the bunker), STORE the
        // signed request, then pay. A crash between sign and pay leaves a
        // Pending record a retry can finish — same event id.
        let unsigned = wallet::zap_request(
            me,
            recipient,
            note.id,
            msat,
            &lnurl,
            active.write_relays.clone(),
        );
        let signed = active
            .signer
            .sign_event(unsigned)
            .await
            .map_err(|e| WalletEvent::ZapFailed {
                note: note_hex.clone(),
                reason: e.to_string(),
            })?;
        let record = wallet::ZapRecord {
            req: signed.id.to_string(),
            note: note_hex.clone(),
            recipient: recipient.to_string(),
            msat,
            state: wallet::ZapState::Pending,
            created_at: now_secs(),
            event: serde_json::to_string(&signed).unwrap_or_default(),
        };
        wallet::ledger_update(record.clone());

        self.pay_stored(&record, &signed, tx).await
    }

    /// The shared pay path for a STORED, signed request (fresh or retry):
    /// fetch the invoice from the recipient's LNURL-pay endpoint, hand the
    /// bolt11 to the NWC wallet, classify the answer per the Feather rule.
    async fn pay_stored(
        self: &Arc<Self>,
        record: &wallet::ZapRecord,
        signed: &Event,
        tx: &mpsc::UnboundedSender<WalletEvent>,
    ) -> ZapFlow {
        let note = record.note.clone();
        let req = record.req.clone();

        // The lnurl tag rides the signed request (bech32) — lnurlp_url
        // accepts it as-is (the lud06 branch). Reading it from the event
        // (not the caller) is what lets retries share this path.
        let lnurl_tag = signed
            .tags
            .iter()
            .find(|t| t.as_slice().first().map(String::as_str) == Some("lnurl"))
            .and_then(|t| t.as_slice().get(1).cloned())
            .ok_or_else(|| WalletEvent::ZapFailed {
                note: note.clone(),
                reason: "zap request has no lnurl tag".into(),
            })?;
        let endpoint =
            wallet::lnurlp_url(&lnurl_tag).map_err(|e| WalletEvent::ZapFailed {
                note: note.clone(),
                reason: e,
            })?;
        // No invoice yet → nothing CAN have been paid: a definite LNURL
        // error erases the record, the user just taps again.
        let callback = match wallet::fetch_lnurlp(&endpoint).await {
            Ok(cb) => cb,
            Err(e) => {
                wallet::ledger_remove(&req);
                return Err(WalletEvent::ZapFailed { note, reason: e });
            }
        };
        let bolt11 = match wallet::fetch_zap_invoice(&callback, record.msat, signed).await {
            Ok(pr) => pr,
            Err(e) => {
                wallet::ledger_remove(&req);
                return Err(WalletEvent::ZapFailed { note, reason: e });
            }
        };

        // The money moment. The wallet SPOKE and did not pay → retrying is
        // safe (erase + quiet error). Anything else → Uncertain, terminal
        // until the user (or a receipt) decides — never auto-retried.
        match self.wallet.pay(&bolt11).await {
            Ok(_preimage) => {
                wallet::ledger_set_state(&req, wallet::ZapState::Paid);
                let ev = WalletEvent::ZapPaid {
                    note: note.clone(),
                    req: req.clone(),
                    msat: record.msat,
                };
                self.wallet.emit(ev.clone());
                let _ = tx.send(ev);
                self.watch_receipt(&note, &req, tx);
                Ok(())
            }
            Err(wallet::PayError::Uncertain) => {
                wallet::ledger_set_state(&req, wallet::ZapState::Uncertain);
                let ev = WalletEvent::ZapUncertain {
                    note: note.clone(),
                    req: req.clone(),
                    msat: record.msat,
                };
                self.wallet.emit(ev.clone());
                let _ = tx.send(ev);
                Ok(())
            }
            Err(e) => {
                // Code(_) or NoWallet: the wallet never paid.
                wallet::ledger_remove(&req);
                let ev = WalletEvent::ZapFailed {
                    note,
                    reason: e.to_string(),
                };
                self.wallet.emit(ev.clone());
                let _ = tx.send(ev);
                Ok(())
            }
        }
    }

    /// Watch for the wallet service's kind-9735 receipt (it publishes to
    /// OUR relays — the request's relays tag) and reconcile the ledger to
    /// Confirmed when OUR verified zap lands. Best-effort: Paid is already
    /// truthful (the wallet returned a preimage); the receipt is the
    /// stronger proof and recovers nothing else.
    fn watch_receipt(
        self: &Arc<Self>,
        note_hex: &str,
        req: &str,
        tx: &mpsc::UnboundedSender<WalletEvent>,
    ) {
        let this = Arc::clone(self);
        let note = note_hex.to_string();
        let req = req.to_string();
        let tx = tx.clone(); // the spawned task outlives the borrow
        self.rt.spawn(async move {
            let Ok(active) = this.active().await else { return };
            let me = active.me();
            let Ok(note_id) = EventId::from_hex(&note) else { return };
            for _ in 0..6 {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                let events = match active
                    .pool
                    .get(Filter::new().kind(Kind::Custom(9735)).event(note_id))
                    .await
                {
                    Ok(set) => set,
                    Err(_) => continue, // relay budget blown — look again
                };
                // parse_zap_receipt verifies the embedded 9734 (signature +
                // p/e/a tags); the sender being ME is the reconciliation.
                if events
                    .iter()
                    .any(|ev| crate::notifications::parse_zap_receipt(ev).sender == Some(me))
                {
                    wallet::ledger_set_state(&req, wallet::ZapState::Confirmed);
                    let ev = WalletEvent::ZapReceipt {
                        note: note.clone(),
                        req: req.clone(),
                    };
                    this.wallet.emit(ev.clone());
                    let _ = tx.send(ev);
                    return;
                }
            }
        });
    }

    // --- bunker login ---------------------------------------------------------

    /// Connect to a `bunker://` URI. Events stream on the returned channel:
    /// Waiting → (AuthUrl?) → Approved | Failed. Cancel with `cancel_login`.
    pub fn connect_bunker(self: &Arc<Self>, uri: String) -> ConnectRx {
        // This flow's epoch = the counter value after the bump; any later
        // bump (cancel, a newer attempt) invalidates it.
        let seq = self.login_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let _ = tx.send(ConnectEvent::Waiting);
            crate::dlog!("bunker: flow started (seq {seq})");
            match this.connect_bunker_inner(&uri, seq).await {
                Ok(pk) => {
                    crate::dlog!("bunker: approved for {pk}");
                    let _ = tx.send(ConnectEvent::Approved(pk));
                }
                Err(e) => {
                    crate::dlog!("bunker: failed: {e}");
                    let _ = tx.send(ConnectEvent::Failed(e.to_string()));
                }
            }
        });
        rx
    }

    /// Abandon an in-flight bunker connect (best-effort shutdown).
    pub fn cancel_login(&self) {
        self.login_seq.fetch_add(1, Ordering::SeqCst);
        if let Some(pending) = self.pending.lock().unwrap().take() {
            self.rt
                .spawn(async move { Arc::unwrap_or_clone(pending).shutdown().await });
        }
    }

    async fn connect_bunker_inner(&self, uri: &str, seq: u64) -> Result<PublicKey> {
        let parsed = NostrConnectUri::parse(uri.trim())
            .map_err(|e| LoginError::InvalidBunkerUri(e.to_string()))?;

        // bunker://<remote>?relay=... (client:// initiation is out of scope).
        let remote = match &parsed {
            NostrConnectUri::Bunker { remote_signer_public_key, .. } => {
                remote_signer_public_key.to_string()
            }
            _ => return Err(LoginError::InvalidBunkerUri("expected bunker:// URI".into())),
        };
        crate::dlog!("bunker: parsed, remote signer {remote}");

        let client_keys = Keys::generate();
        let connect = Arc::new(
            NostrConnect::new(parsed, client_keys.clone(), LOGIN_TIMEOUT, None)
                .map_err(|e| LoginError::Connect(e.to_string()))?,
        );
        let relays: Vec<String> = connect.relays().iter().map(|r| r.to_string()).collect();
        crate::dlog!("bunker: client built over {} relay(s); awaiting approval", relays.len());
        *self.pending.lock().unwrap() = Some(Arc::clone(&connect));

        let pk = match connect.get_public_key_async().await {
            Ok(pk) => pk,
            Err(e) => {
                crate::dlog!("bunker: get_public_key failed: {e}");
                return Err(LoginError::Connect(e.to_string()));
            }
        };
        crate::dlog!("bunker: approval received from signer");

        if self.login_seq.load(Ordering::SeqCst) != seq {
            crate::dlog!("bunker: login superseded mid-handshake; dropping");
            return Err(LoginError::Connect("superseded".into()));
        }

        secrets::store(
            secrets::BUNKER_CLIENT_SECRET,
            &client_keys.secret_key().display_secret().to_string(),
        )
        .await?;
        let session = Session {
            pubkey: pk.to_string(),
            mode: SessionMode::Bunker { remote, relays },
        };
        session::save(&session)?;
        crate::dlog!("bunker: session saved");

        // Already on the runtime (spawned task): await, never block_on.
        let active = self.build_active(&session).await?;
        *self.active.lock().unwrap() = Some(active);
        // Clear pending only if it still points at THIS handshake — a newer
        // attempt may have replaced it.
        let mut pending = self.pending.lock().unwrap();
        if pending.as_ref().is_some_and(|p| Arc::ptr_eq(p, &connect)) {
            *pending = None;
        }
        drop(pending);
        crate::dlog!("bunker: active session built — login complete");
        Ok(pk)
    }

    /// nostrconnect login (NIP-46, client-initiated): generate a fresh
    /// transport keypair and advertise it as a QR'd nostrconnect:// URI on
    /// the shared NIP-46 relay — the signer scans and approves. The URI goes
    /// down the same stream first (ConnectEvent::Uri); from approval on,
    /// this is the bunker flow (transport key in BUNKER_CLIENT_SECRET,
    /// session mode Bunker — resume rebuilds a bunker:// URI).
    pub fn connect_nostrconnect(self: &Arc<Self>, relay: String) -> ConnectRx {
        let seq = self.login_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let _ = tx.send(ConnectEvent::Waiting);
            crate::dlog!("nostrconnect: flow started (seq {seq}, relay {relay})");
            match this.connect_nostrconnect_inner(&relay, seq, &tx).await {
                Ok(pk) => {
                    crate::dlog!("nostrconnect: approved for {pk}");
                    let _ = tx.send(ConnectEvent::Approved(pk));
                }
                Err(e) => {
                    crate::dlog!("nostrconnect: first window failed: {e}");
                    // One automatic retry on the SDK's 60s timeout: a fast
                    // signer (kumaOS's built-in panel approves in ~1s) can
                    // publish its response before the SDK's subscribe is
                    // live, and kind-24133 filters don't replay. A fresh
                    // URI re-arms the handshake — the Uri event repaints
                    // the QR. Superseded or cancelled flows don't retry.
                    if !e.to_string().contains("timeout")
                        || this.login_seq.load(Ordering::SeqCst) != seq
                    {
                        let _ = tx.send(ConnectEvent::Failed(e.to_string()));
                        return;
                    }
                    // (No lock across the shutdown await — take first.)
                    let pending = this.pending.lock().unwrap().take();
                    if let Some(pending) = pending {
                        Arc::unwrap_or_clone(pending).shutdown().await;
                    }
                    crate::dlog!("nostrconnect: retrying with a fresh URI");
                    match this.connect_nostrconnect_inner(&relay, seq, &tx).await {
                        Ok(pk) => {
                            crate::dlog!("nostrconnect: approved for {pk} (retry)");
                            let _ = tx.send(ConnectEvent::Approved(pk));
                        }
                        Err(e2) => {
                            crate::dlog!("nostrconnect: retry failed: {e2}");
                            let _ = tx.send(ConnectEvent::Failed(e2.to_string()));
                        }
                    }
                }
            }
        });
        rx
    }

    async fn connect_nostrconnect_inner(
        &self,
        relay: &str,
        seq: u64,
        tx: &mpsc::UnboundedSender<ConnectEvent>,
    ) -> Result<PublicKey> {
        let client_keys = Keys::generate();
        // The metadata the signer shows at approval time. url/icons stay
        // unset until the app has hosted art — both optional per NIP-46.
        let mut uri = NostrConnectUri::client(
            client_keys.public_key(),
            [RelayUrl::parse(relay)
                .map_err(|e| LoginError::InvalidBunkerUri(e.to_string()))?],
            "Satori",
        );
        if let NostrConnectUri::Client { metadata, .. } = &mut uri {
            metadata.description = Some("A quiet Nostr reader.".to_string());
        }
        // The SDK emits the client identity as a metadata= JSON blob (the
        // pre-2025 NIP-46 draft); the current spec spells flat query params
        // (name/url/image). Append both — old signers read the blob, current
        // ones (kumaOS) the flat param. The handshake itself validates the
        // secret against the parsed URI, so the extras ride along inert.
        let mut uri_str = uri.to_string();
        uri_str.push_str("&name=Satori");
        crate::dlog!("nostrconnect: uri ready ({} chars)", uri_str.len());
        let _ = tx.send(ConnectEvent::Uri(uri_str));

        let connect = Arc::new(
            NostrConnect::new(uri, client_keys.clone(), LOGIN_TIMEOUT, None)
                .map_err(|e| LoginError::Connect(e.to_string()))?,
        );
        crate::dlog!("nostrconnect: client built, awaiting signer…");
        *self.pending.lock().unwrap() = Some(Arc::clone(&connect));

        // The probe: a bare diagnostic client riding the same relay,
        // subscribed to this key's response kind. The SDK's own pump skips
        // bad responses with a tracing::warn that vanishes (no subscriber),
        // and a fast approval can land before its subscribe — the probe
        // turns both into lines we can actually read. Stands down when the
        // handshake resolves.
        let (probe_done_tx, probe_done_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(Self::nostrconnect_probe(
            client_keys.clone(),
            relay.to_string(),
            probe_done_rx,
        ));

        let user_pk = match connect.get_public_key_async().await {
            Ok(pk) => pk,
            Err(e) => {
                let _ = probe_done_tx.send(());
                crate::dlog!("nostrconnect: get_public_key failed: {e}");
                return Err(LoginError::Connect(e.to_string()));
            }
        };
        let _ = probe_done_tx.send(());
        crate::dlog!("nostrconnect: approval received from signer");

        if self.login_seq.load(Ordering::SeqCst) != seq {
            crate::dlog!("nostrconnect: login superseded mid-handshake; dropping");
            return Err(LoginError::Connect("superseded".into()));
        }

        secrets::store(
            secrets::BUNKER_CLIENT_SECRET,
            &client_keys.secret_key().display_secret().to_string(),
        )
        .await?;
        let remote = match connect
            .bunker_uri()
            .await
            .map_err(|e| LoginError::Connect(e.to_string()))?
        {
            NostrConnectUri::Bunker { remote_signer_public_key, .. } => {
                remote_signer_public_key.to_string()
            }
            _ => unreachable!("bunker_uri returns the Bunker variant"),
        };
        let relays: Vec<String> = connect.relays().iter().map(|r| r.to_string()).collect();

        let session = Session {
            pubkey: user_pk.to_string(),
            mode: SessionMode::Bunker { remote, relays },
        };
        session::save(&session)?;
        crate::dlog!("nostrconnect: session saved");

        let active = self.build_active(&session).await?;
        *self.active.lock().unwrap() = Some(active);
        // Clear pending only if it still points at THIS handshake — a newer
        // attempt may have replaced it.
        let mut pending = self.pending.lock().unwrap();
        if pending.as_ref().is_some_and(|p| Arc::ptr_eq(p, &connect)) {
            *pending = None;
        }
        drop(pending);
        crate::dlog!("nostrconnect: active session built — login complete");
        Ok(user_pk)
    }

    /// The nostrconnect diagnostic sub (see connect_nostrconnect_inner):
    /// one bare client on the same relay, subscribed to the transport
    /// key's response kind. Every signer response it sees is logged with
    /// its decrypt outcome — "the relay delivered it and the SDK pump
    /// missed it" becomes provable from the log instead of a guess.
    async fn nostrconnect_probe(client_keys: Keys, relay: String, done: tokio::sync::oneshot::Receiver<()>) {
        use futures_util::StreamExt as _;
        let probe = nostr_sdk::client::ClientBuilder::new().build();
        if let Err(e) = probe.add_relay(&relay).await {
            crate::dlog!("nostrconnect probe: relay add failed: {e}");
            return;
        }
        probe.connect().await;
        crate::dlog!("nostrconnect probe: relay connected");
        let filter = Filter::new()
            .pubkey(client_keys.public_key())
            .kind(Kind::NostrConnect)
            .limit(0);
        let Ok(output) = probe.subscribe(vec![filter]).await else {
            crate::dlog!("nostrconnect probe: subscribe failed");
            return;
        };
        crate::dlog!("nostrconnect probe: subscribed for responses");
        let mut done = done;
        let mut notifications = probe.notifications();
        loop {
            tokio::select! {
                _ = &mut done => {
                    crate::dlog!("nostrconnect probe: handshake resolved, standing down");
                    let _ = probe.shutdown().await;
                    return;
                }
                n = notifications.next() => {
                    let Some(n) = n else {
                        crate::dlog!("nostrconnect probe: notification stream ended");
                        return;
                    };
                    match n {
                        nostr_sdk::client::ClientNotification::Event { subscription_id, event, .. }
                            if output.id() == &subscription_id =>
                        {
                            let event = *event;
                            crate::dlog!(
                                "nostrconnect probe: response event from {} — the relay DID deliver",
                                event.pubkey
                            );
                            match nostr::nips::nip44::decrypt(
                                client_keys.secret_key(),
                                &event.pubkey,
                                &event.content,
                            ) {
                                Ok(json) => {
                                    match serde_json::from_str::<serde_json::Value>(&json) {
                                        Ok(v) => crate::dlog!(
                                            "nostrconnect probe: response type={:?} result={:?} error={:?}",
                                            v.get("type"),
                                            v.get("result"),
                                            v.get("error"),
                                        ),
                                        Err(_) => crate::dlog!(
                                            "nostrconnect probe: undecodable response: {}…",
                                            &json[..json.len().min(120)]
                                        ),
                                    }
                                }
                                Err(e) => {
                                    crate::dlog!("nostrconnect probe: decrypt failed: {e}");
                                }
                            }
                        }
                        nostr_sdk::client::ClientNotification::Shutdown => {
                            crate::dlog!("nostrconnect probe: client shutdown");
                            return;
                        }
                        _ => continue,
                    }
                }
            }
        }
    }



    // --- resume machinery ------------------------------------------------------

    async fn build_signer(&self, session: &Session) -> Result<Signer> {
        match &session.mode {
            SessionMode::Bunker { remote, relays } => {
                let secret = secrets::load(secrets::BUNKER_CLIENT_SECRET)
                    .await?
                    .ok_or(LoginError::CredentialGone("transport secret"))?;
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
                let secret = secrets::load(secrets::NSEC)
                    .await?
                    .ok_or(LoginError::CredentialGone("local key"))?;
                Ok(Signer::Local(signer::parse_local_secret(&secret)?))
            }
        }
    }

    /// The process pool (create-or-get).
    async fn pool(&self) -> Result<Arc<Pool>> {
        if let Some(p) = self.pool.lock().unwrap().as_ref() {
            return Ok(Arc::clone(p));
        }
        let p = Arc::new(Pool::open().await.map_err(|e| LoginError::Resume(e.to_string()))?);
        *self.pool.lock().unwrap() = Some(Arc::clone(&p));
        Ok(p)
    }

    async fn build_active(&self, session: &Session) -> Result<Active> {
        // The session pubkey, parsed once. A corrupt session.json fails
        // the login here, with a message on the login pane, instead of
        // panicking at the first me() call somewhere deep in a feed task.
        let me = PublicKey::from_str(&session.pubkey)
            .map_err(|e| LoginError::Resume(format!("session pubkey: {e}")))?;
        let signer = self.build_signer(session).await?;
        let pool = self.pool().await?;
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
        // NIP-78 app-data sync (Phase 2 §4.1): our kind-30078 slots →
        // prefs.json. Remote wins at login (Feather's semantics): every
        // local change publishes forward, so only offline edits lose.
        // PrefsSynced wakes the shell to re-read what it renders live.
        {
            let pool = Arc::clone(&pool);
            let signer = signer.clone();
            let live = self.live.clone();
            self.rt.spawn(async move {
                sync_app_data(&pool, &me, &signer, &live).await;
            });
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
            me,
        })
    }

    /// Live undo-window update (settings): active state + prefs.json,
    /// then NIP-78 sync of both undo keys (Phase 2 §4.1).
    pub fn set_undo(self: &Arc<Self>, enabled: bool, seconds: u32) {
        {
            let mut a = self.active.lock().unwrap();
            if let Some(a) = a.as_mut() {
                a.undo.enabled = enabled;
                a.undo.set_seconds(seconds);
            }
        }
        write_prefs_overlay(&[
            ("undo_enabled".to_string(), enabled.into()),
            ("undo_seconds".to_string(), seconds.into()),
        ]);
        // Fire and forget — sync needs no feedback surface.
        self.publish_app_data("undo_enabled", serde_json::Value::Bool(enabled));
        self.publish_app_data("undo_seconds", serde_json::Value::from(seconds));
    }

    /// NIP-78 app-data publish (Phase 2 §4.1): one kind-30078 per prefs
    /// key, `d` = prefs.json key verbatim, content NIP-44-encrypted to
    /// self — the same private-by-default semantics as the NIP-51 lists.
    pub fn publish_app_data(self: &Arc<Self>, key: &str, value: serde_json::Value) -> PublishRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        let key = key.to_string();
        self.rt.spawn(async move {
            let res: Result<()> = async {
                let active = this.active().await?;
                let me = active.me();
                let content = active.signer.nip44_encrypt_self(&me, &value.to_string()).await?;
                let unsigned = UnsignedEvent::new(
                    me,
                    Timestamp::now(),
                    Kind::Custom(nip78::KIND),
                    [Tag::custom("d", [key.as_str()])],
                    content,
                );
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

    // --- feed ------------------------------------------------------------------

    /// Bootstrap: NIP-65 relay lists, follows, connect reads, first page
    /// (deep fetch for older pagination), own engagement sets, profiles.
    /// Bootstrap the given surface: `visible_kinds()` for the timeline, a
    /// single kind for a narrow one (articles).
    pub fn bootstrap(self: &Arc<Self>, kinds: Vec<Kind>) -> FeedRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            if let Err(e) = this.bootstrap_inner(&tx, kinds).await {
                let _ = tx.send(FeedEvent::Failed(e.to_string()));
            }
        });
        rx
    }

    async fn bootstrap_inner(
        &self,
        tx: &mpsc::UnboundedSender<FeedEvent>,
        kinds: Vec<Kind>,
    ) -> Result<()> {
        let mut active = self.active().await?;
        let me = active.me();

        // 1. PAINT FIRST: whatever the store already holds (instant — the
        // LMDB cache persists across runs). Cached follows drive the filter.
        let cached_follows: Vec<PublicKey> = active
            .pool
            .store
            .latest_by_kind(me, Kind::ContactList)
            .await
            .unwrap_or(None)
            .map(|ev| ev.tags.public_keys().collect())
            .unwrap_or_default();
        if !cached_follows.is_empty() {
            active.follows = cached_follows.clone();
            let page = self.page_from_store(&active, None, &kinds).await?;
            let _ = tx.send(FeedEvent::Loaded(page));
        }
        let _ = tx.send(FeedEvent::BootstrapDone);

        // 2. The network phase — everything independent, IN PARALLEL. The
        // UI re-renders when the fresh page lands (the reactive profile
        // patching fills names/avatars as they arrive).
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
        self.start_live(kinds.clone()).await;

        // Profiles + engagement + deep fetch: parallel, none block paint.
        let mut warm_set = follows.clone();
        warm_set.push(me);
        active.profiles.warm(warm_set);

        // The warm skip is per surface (keyed by kind set) — a timeline
        // deep-filled five minutes ago says nothing about the articles
        // shelf's store depth.
        let warm_key: String = {
            let mut ks: Vec<u16> = kinds.iter().map(|k| u16::from(*k)).collect();
            ks.sort();
            ks.dedup();
            ks.iter().map(|k| k.to_string()).collect::<Vec<_>>().join(",")
        };
        let warm = crate::read_state::load()
            .warm_at
            .get(&warm_key)
            .is_some_and(|t| *t > now_secs().saturating_sub(600));
        // Only a deep fetch that actually RAN earns the stamp — stamping a
        // skipped bootstrap would let a surface opened every few minutes
        // defer its fill forever.
        let ran_deep = !follows.is_empty() && !warm;
        let (engagement, fetched, dels) = tokio::join!(
            async {
                let mine = Filter::new()
                    .author(me)
                    .kinds([
                        Kind::Reaction,
                        Kind::Repost,
                        Kind::Custom(16),
                        Kind::Custom(10003),
                    ])
                    .limit(2000);
                active.pool.get(mine).await.unwrap_or_default()
            },
            async {
                if !follows.is_empty() && !warm {
                    let filter = Filter::new()
                        .kinds(kinds.clone())
                        .authors(follows.clone())
                        .limit(BOOTSTRAP_DEPTH);
                    active.pool.get(filter).await.unwrap_or_default()
                } else {
                    eprintln!(
                        "[core] deep fetch skipped: follows_empty={} warm={}",
                        follows.is_empty(),
                        warm
                    );
                    Default::default()
                }
            },
            // Deletions ride along (NIP-09): rare, follows-scoped, needed
            // before the render filter can drop what they cover.
            async {
                if !follows.is_empty() && !warm {
                    let filter = Filter::new()
                        .kind(Kind::EventDeletion)
                        .authors(follows.clone())
                        .limit(500);
                    active.pool.get(filter).await.unwrap_or_default()
                } else {
                    Default::default()
                }
            }
        );
        eprintln!("[core] deep fetch: {} events", fetched.len());
        for ev in engagement {
            let _ = active.pool.store.save(&ev).await;
        }
        // The deep fetch lands in the store like every other network yield
        // (refresh_inner and older's top-up save theirs) — it used to be
        // logged and DROPPED, so a fresh surface (the articles shelf) kept
        // painting only whatever older/refresh had happened to carry.
        for ev in fetched {
            let _ = active.pool.store.save(&ev).await;
        }
        for ev in dels {
            let _ = active.pool.store.save(&ev).await;
        }
        self.load_own_sets(&active).await;

        if ran_deep {
            crate::read_state::advance(|st| {
                st.warm_at.insert(warm_key, now_secs());
            });
        }
        let page = self.page_from_store(&active, None, &kinds).await?;
        eprintln!("[core] bootstrap page: {} notes", page.notes.len());
        let _ = tx.send(FeedEvent::Loaded(page));
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

        // Own reposts (kind 6 and its generic cousin 16): the e-tag IS the
        // target.
        if let Ok(reposts) = active.pool.store.own_reposts(me).await {
            let mut rp = self.own_reposted.lock().unwrap();
            for ev in reposts {
                if let Some(target) = ev.tags.clone().event_ids().next() {
                    rp.insert(target.to_string());
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
    pub fn refresh(self: &Arc<Self>, kinds: Vec<Kind>) -> FeedRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            if let Err(e) = this.refresh_inner(&tx, kinds).await {
                let _ = tx.send(FeedEvent::Failed(e.to_string()));
            }
        });
        rx
    }

    async fn refresh_inner(
        &self,
        tx: &mpsc::UnboundedSender<FeedEvent>,
        kinds: Vec<Kind>,
    ) -> Result<()> {
        let active = self.active().await?;
        let follows = active.follows.clone();
        if follows.is_empty() {
            return self.bootstrap_inner(tx, kinds).await;
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
        // NIP-01: an empty filter list can never match — never send
        // `authors: []`; a no-follow account just projects the store.
        let fetched = if follows.is_empty() {
            Default::default()
        } else {
            let filter = Filter::new()
                .kinds(kinds.clone())
                .authors(follows.clone())
                .since(Timestamp::from(since))
                .limit(100);
            active.pool.get(filter).await.unwrap_or_default()
        };
        if fetched.is_empty() {
            // Relays unreachable (post-suspend, offline): this used to end
            // the refresh as NothingNew — which the feed swallows — so a
            // new-notes chip click no-oped until the relays recovered. The
            // live sub saves pushed notes to the store even while relays
            // are down, so project from the store regardless: the missed
            // notes appear with zero relay round-trips.
            crate::dlog!("refresh: relay fetch empty; projecting the store");
            let page = self.page_from_store(&active, None, &kinds).await?;
            let _ = tx.send(FeedEvent::Loaded(page));
            return Ok(());
        }
        for ev in fetched {
            let _ = active.pool.store.save(&ev).await;
        }
        // Deletions ride along (NIP-09) — a follow's kind-5s since the
        // newest note cover everything the refresh window can carry
        // (skipped with no follows: NIP-01, never send an empty list).
        if !follows.is_empty() {
            let del_filter = Filter::new()
                .kind(Kind::EventDeletion)
                .authors(follows.clone())
                .since(Timestamp::from(since.saturating_sub(86400)));
            for ev in active.pool.get(del_filter).await.unwrap_or_default() {
                let _ = active.pool.store.save(&ev).await;
            }
        }
        let page = self.page_from_store(&active, None, &kinds).await?;
        eprintln!("[core] bootstrap page: {} notes", page.notes.len());
        let _ = tx.send(FeedEvent::Loaded(page));
        Ok(())
    }

    /// Older page: project the store; when it runs dry, top up from the
    /// relays — strictly older than `until` (the oldest note currently
    /// shown), all visible kinds — and project again. The store stays the
    /// chokepoint; the relays are the reservoir.
    pub fn older(self: &Arc<Self>, until: u64, kinds: Vec<Kind>) -> FeedRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res: Result<FeedEvent> = async {
                let active = this.active().await?;
                let mut notes = this.project_page(&active, Some(until), &kinds).await?;
                // NIP-01: an empty list can never match — with no follows
                // there is nothing to top up from the relays.
                if notes.len() <= PAGE && !active.follows.is_empty() {
                    let filter = Filter::new()
                        .kinds(kinds.clone())
                        .authors(active.follows.clone())
                        .until(Timestamp::from(until))
                        .limit(TOPUP_DEPTH);
                    let fetched = active.pool.get(filter).await.unwrap_or_default();
                    for ev in fetched {
                        let _ = active.pool.store.save(&ev).await;
                    }
                    // Deletions ride along (NIP-09).
                    let del_filter = Filter::new()
                        .kind(Kind::EventDeletion)
                        .authors(active.follows.clone())
                        .until(Timestamp::from(until))
                        .limit(500);
                    for ev in active.pool.get(del_filter).await.unwrap_or_default() {
                        let _ = active.pool.store.save(&ev).await;
                    }
                    notes = this.project_page(&active, Some(until), &kinds).await?;
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
            m.insert(p.clone());
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

    /// @-mention candidates for the composer: the people you follow whose
    /// cached name (or pubkey hex) matches the typed handle. Profiles not
    /// yet in the cache match only by hex — the profiles service fills the
    /// rest as the feed loads them. One lock, eight hits max: this runs
    /// per keystroke.
    pub fn mention_candidates(&self, query: &str) -> Vec<Profile> {
        let (follows, me): (Vec<PublicKey>, String) = {
            let held = self.active.lock().unwrap();
            let me = held
                .as_ref()
                .map(|a| a.session.pubkey.clone())
                .unwrap_or_default();
            let follows = held.as_ref().map(|a| a.follows.clone()).unwrap_or_default();
            (follows, me)
        };
        let q = query.to_lowercase();
        let m = self.profiles_mem.lock().unwrap();
        let mut out: Vec<Profile> = Vec::new();
        for pk in &follows {
            if pk.to_string() == me {
                continue;
            }
            let Some(p) = m.get(&pk.to_string()) else {
                continue;
            };
            if q.is_empty()
                || p.name.to_lowercase().contains(&q)
                || p.pubkey.to_lowercase().contains(&q)
            {
                out.push(p.clone());
                if out.len() == 8 {
                    break;
                }
            }
        }
        out
    }

    /// People results for the search pane: a substring scan of the hydrated
    /// profile cache (name, NIP-05, pubkey hex), any author — mention
    /// candidates without the follow gate. Cache-local by design: kind-0
    /// search is thin relay-side, and the cache hydrates as the session
    /// reads. Sorted by name so the rows hold still run to run.
    pub fn search_people(&self, query: &str) -> Vec<Profile> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return vec![];
        }
        let m = self.profiles_mem.lock().unwrap();
        let mut hits: Vec<Profile> = m
            .values()
            .filter(|p| {
                p.name.to_lowercase().contains(&q)
                    || p.nip05.to_lowercase().contains(&q)
                    || p.pubkey.to_lowercase().contains(&q)
            })
            .cloned()
            .collect();
        hits.sort_by_key(|p| p.name.to_lowercase());
        // The search pane's People tab is a proper results page now (not a
        // sidebar strip) — room for a real page.
        hits.truncate(20);
        hits
    }

    fn active_pubkey(&self) -> String {        self.active
            .lock()
            .unwrap()
            .as_ref()
            .map(|a| a.session.pubkey.clone())
            .unwrap_or_default()
    }

    /// One page projection: store query (a PAGE+1 window so "has more" is
    /// knowable), then the mutes gate at the chokepoint.
    /// NIP-09 render filter: drop events their own author deleted (a
    /// kind-5 `e` tag from the deleted event's pubkey). A stranger's
    /// kind-5 can't delete your notes, so the author must match.
    async fn drop_deleted(&self, active: &Active, mut notes: Vec<Event>) -> Vec<Event> {
        if notes.is_empty() {
            return notes;
        }
        let hit = self.deleted_cache.lock().unwrap().clone();
        let deleted = match hit {
            Some((at, set)) if at.elapsed() < DELETED_TTL => set,
            _ => {
                let dels = active
                    .pool
                    .store
                    .deletions(&active.follows)
                    .await
                    .unwrap_or_default();
                let mut map: HashSet<(PublicKey, EventId)> = HashSet::new();
                for d in &dels {
                    for id in d.tags.clone().event_ids() {
                        map.insert((d.pubkey, id));
                    }
                }
                let set: DeletedSet = Arc::new(map);
                *self.deleted_cache.lock().unwrap() =
                    Some((std::time::Instant::now(), Arc::clone(&set)));
                set
            }
        };
        if deleted.is_empty() {
            return notes;
        }
        notes.retain(|ev| !deleted.contains(&(ev.pubkey, ev.id)));
        notes
    }

    async fn project_page(
        &self,
        active: &Active,
        until: Option<u64>,
        kinds: &[Kind],
    ) -> Result<Vec<Event>> {
        let mut notes = active
            .pool
            .store
            .feed_page_kinds(&active.follows, until, PAGE + 1, kinds.to_vec())
            .await
            .map_err(|e| LoginError::Resume(e.to_string()))?;
        {
            let muted = self.muted.lock().unwrap();
            notes.retain(|ev| !muted.contains(&ev.pubkey.to_string()));
        }
        Ok(self.drop_deleted(active, notes).await)
    }

    async fn page_from_store(
        &self,
        active: &Active,
        until: Option<u64>,
        kinds: &[Kind],
    ) -> Result<FeedPage> {
        let follows = active.follows.clone();
        let mut notes = active
            .pool
            .store
            .feed_page_kinds(&follows, until, PAGE, kinds.to_vec())
            .await
            .map_err(|e| LoginError::Resume(e.to_string()))?;
        // Mutes + deletions gate here — every feed page passes this
        // chokepoint (pool.ts: NIP-09/NIP-40 consume-at-the-chokepoint role).
        {
            let muted = self.muted.lock().unwrap();
            notes.retain(|ev| !muted.contains(&ev.pubkey.to_string()));
        }
        notes = self.drop_deleted(active, notes).await;
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

                // Cached candidates paint instantly; the relay phase runs in
                // parallel below and the response re-renders.
                let my_notes = active
                    .pool
                    .store
                    .feed_page(&[me], None, 100)
                    .await
                    .unwrap_or_default();
                let ids: Vec<EventId> = my_notes.iter().map(|e| e.id).collect();

                // Instant paint: the store's cached candidates (kind-1
                // replies, NIP-22 comments — the Amethyst migration
                // made 1111 the carrier for non-kind-1 replies — and the
                // engagement kinds: reactions/zaps/votes by `e` tag).
                // Three batched index passes for the whole id set: this
                // used to run one query per category PER NOTE — up to 300
                // sequential LMDB transactions for a 100-note window.
                let reaction_notifs = this.reaction_notifs();
                let mut cached: Vec<Event> = Vec::new();
                cached.extend(
                    active
                        .pool
                        .store
                        .replies_of(ids.clone())
                        .await
                        .unwrap_or_default(),
                );
                cached.extend(
                    active
                        .pool
                        .store
                        .comments_of(ids.clone())
                        .await
                        .unwrap_or_default(),
                );
                cached.extend(
                    active
                        .pool
                        .store
                        .engagement_of(ids.clone())
                        .await
                        .unwrap_or_default(),
                );
                cached.retain(|ev| ev.pubkey != me);
                // The relay phase filters these too — the cached phase
                // must wear the same gates (a cold start previously showed
                // a stranger's thumbs-down the relay phase would have
                // dropped, and reactions when the pref was off).
                cached.retain(|ev| {
                    !(u16::from(ev.kind) == 7
                        && (!reaction_notifs || ev.content.trim() == "-"))
                });
                {
                    let muted = this.muted.lock().unwrap();
                    cached.retain(|ev| !muted.contains(&ev.pubkey.to_string()));
                }
                {
                    let authors: Vec<PublicKey> = cached.iter().map(|e| e.pubkey).collect();
                    let profiles = active
                        .pool
                        .store
                        .profiles(&authors)
                        .await
                        .unwrap_or_default()
                        .iter()
                        .filter_map(Profile::from_event)
                        .collect();
                    let _ = tx.send(FeedEvent::Notifications(cached, profiles));
                }

                let since = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
                    .saturating_sub(7 * 86400);

                // Mentions (#p=me) and replies (#e=my notes): PARALLEL.
                // (The pref was already read for the cached phase — no
                // second disk read here.)
                let mut mention_kinds = vec![Kind::TextNote, Kind::Custom(1111), Kind::Custom(9735)];
                if reaction_notifs {
                    mention_kinds.push(Kind::Reaction);
                }
                let mention_q = Box::pin(active.pool.get(
                    Filter::new()
                        .kinds(mention_kinds)
                        .pubkeys([me])
                        .since(Timestamp::from(since))
                        .limit(200),
                ));
                let reply_q = if ids.is_empty() {
                    None
                } else {
                    Some(Box::pin(active.pool.get(
                        Filter::new()
                            .kinds([Kind::TextNote, Kind::Custom(1111)])
                            .events(ids)
                            .since(Timestamp::from(since))
                            .limit(200),
                    )))
                };
                let poll_ids: Vec<EventId> = my_notes
                    .iter()
                    .filter(|e| u16::from(e.kind) == 1068)
                    .map(|e| e.id)
                    .collect();
                let vote_q = Box::pin(async {
                    if poll_ids.is_empty() {
                        return Default::default();
                    }
                    let mut f = Filter::new()
                        .kind(Kind::Custom(1018))
                        .since(Timestamp::from(since))
                        .limit(200);
                    for id in poll_ids {
                        f = f.event(id);
                    }
                    active.pool.get(f).await.unwrap_or_default()
                });
                let (mentions_res, replies_res, votes_res) = tokio::join!(mention_q, async {
                    match reply_q {
                        Some(q) => q.await.unwrap_or_default(),
                        None => Default::default(),
                    }
                }, vote_q);
                let mine_events: Vec<Event> = mentions_res
                    .into_iter()
                    .flat_map(|set| set.into_iter())
                    .chain(replies_res)
                    .chain(votes_res)
                    .collect();
                for ev in &mine_events {
                    let _ = active.pool.store.save(ev).await;
                }

                // Dedup + drop own + sort desc.
                let mut seen = std::collections::HashSet::new();
                let muted = this.muted.lock().unwrap().clone();
                let mut notifs: Vec<Event> = mine_events
                    .into_iter()
                    .filter(|ev| {
                        ev.pubkey != me
                            && seen.insert(ev.id)
                            && !muted.contains(&ev.pubkey.to_string())
                            // A stranger's thumbs-down is pure agitation
                            // (notifications.ts): never shown. +/emoji stay.
                            && !(u16::from(ev.kind) == 7 && ev.content.trim() == "-")
                    })
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

    /// The "Show reactions in notifications" pref (prefs.json
    /// reactionNotifs, default off — render/settings.ts PREF_LABELS).
    fn reaction_notifs(&self) -> bool {
        crate::config::prefs()
            .and_then(|v| v.get("reactionNotifs").and_then(|b| b.as_bool()))
            .unwrap_or(false)
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
        mentions: Vec<PublicKey>,
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
                let mut seen_p: Vec<String> = tags
                    .iter()
                    .filter(|t| t.as_slice().first().map(String::as_str) == Some("p"))
                    .filter_map(|t| t.as_slice().get(1).cloned())
                    .collect();
                for who in mentions {
                    let hex = who.to_string();
                    if hex != active.me().to_string() && !seen_p.contains(&hex) {
                        seen_p.push(hex.clone());
                        tags.push(Tag::public_key(who));
                    }
                }
                tags.push(Tag::custom("client", ["satori-desktop"]));
                let unsigned =
                    UnsignedEvent::new(active.me(), Timestamp::now(), Kind::Custom(1111), tags, content);
                let signed = active.signer.sign_event(unsigned).await?;
                // Same undo window as notes (the reply toast is the note
                // toast — one seam, hold_and_commit).
                match active.undo.hold_duration() {
                    Some(hold) => {
                        let token = this.hold_seq.fetch_add(1, Ordering::SeqCst);
                        let _ = tx.send(PublishEvent::Held {
                            token,
                            event: signed.clone(),
                            hold_secs: hold.as_secs(),
                        });
                        this.hold_and_commit(
                            Arc::new(active.clone()),
                            token,
                            signed,
                            hold,
                            tx.clone(),
                        )
                        .await;
                    }
                    None => {
                        Self::commit(&active, &signed, &tx).await?;
                        let _ = active.pool.store.save(&signed).await;
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

    /// Repost (NIP-18): kind 6 for notes, kind 16 + a `k` tag for anything
    /// else — and the same undo window as every publish (hold_and_commit;
    /// the shell's toast carries the countdown and the Undo).
    pub fn repost(self: &Arc<Self>, target: &Event) -> PublishRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        let target = target.clone();
        self.rt.spawn(async move {
            let res: Result<()> = async {
                let active = this.active().await?;
                let note = u16::from(target.kind) == 1;
                let mut tags = vec![Tag::event(target.id), Tag::public_key(target.pubkey)];
                if !note {
                    tags.push(Tag::custom("k", [u16::from(target.kind).to_string()]));
                }
                let unsigned = UnsignedEvent::new(
                    active.me(),
                    Timestamp::now(),
                    if note { Kind::Repost } else { Kind::Custom(16) },
                    tags,
                    format!("nostr:{}", target.id.to_bech32().unwrap_or_default()),
                );
                let signed = active.signer.sign_event(unsigned).await?;
                match active.undo.hold_duration() {
                    Some(hold) => {
                        let token = this.hold_seq.fetch_add(1, Ordering::SeqCst);
                        let _ = tx.send(PublishEvent::Held {
                            token,
                            event: signed.clone(),
                            hold_secs: hold.as_secs(),
                        });
                        this.hold_and_commit(
                            Arc::new(active.clone()),
                            token,
                            signed,
                            hold,
                            tx.clone(),
                        )
                        .await;
                    }
                    None => {
                        Self::commit(&active, &signed, &tx).await?;
                        let _ = active.pool.store.save(&signed).await;
                        let _ = tx.send(PublishEvent::Committed { token: 0 });
                    }
                }
                Ok(())
            }
            .await;
            if let Err(e) = res {
                let _ = tx.send(PublishEvent::Failed { token: 0, error: e.to_string() });
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
    /// Batch-resolve reply parents (the timeline's context whisper): store
    /// first, then one by-id relay round trip for the misses. Emits
    /// FeedEvent::Parents with whatever resolved — misses (deleted parents,
    /// relay gaps) just don't resolve, and the whisper degrades.
    /// The signed-in identity's pubkey hex, if any — views use it to
    /// phrase "your note" vs "{name}'s note" (the notifications whisper).
    pub fn me(&self) -> Option<String> {
        self.active
            .lock()
            .unwrap()
            .as_ref()
            .map(|a| a.session.pubkey.clone())
    }

    pub fn load_parents(self: &Arc<Self>, ids: Vec<String>) -> FeedRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let mut resolved: std::collections::HashMap<String, ParentNote> =
                std::collections::HashMap::new();
            let _: Result<()> = async {
                let active = this.active().await?;
                let want: Vec<EventId> = ids
                    .iter()
                    .filter_map(|h| EventId::from_hex(h).ok())
                    .collect();
                if want.is_empty() {
                    return Ok(());
                }
                let mut events: Vec<Event> = Vec::new();
                for id in &want {
                    if let Some(ev) = active.pool.store.event_by_id(*id).await.ok().flatten() {
                        events.push(ev);
                    }
                }
                let have: HashSet<EventId> = events.iter().map(|e| e.id).collect();
                let missing: Vec<EventId> =
                    want.iter().filter(|id| !have.contains(id)).cloned().collect();
                if !missing.is_empty() {
                    let n = missing.len();
                    let fetched = active
                        .pool
                        .get(Filter::new().ids(missing).limit(n))
                        .await
                        .unwrap_or_default();
                    for ev in &fetched {
                        let _ = active.pool.store.save(ev).await;
                    }
                    events.extend(fetched);
                }
                for ev in events {
                    let snippet = plain_snippet(&ev.content, 140);
                    resolved.insert(
                        ev.id.to_string(),
                        ParentNote { author: ev.pubkey.to_string(), snippet },
                    );
                }
                Ok(())
            }
            .await;
            if !resolved.is_empty() {
                let _ = tx.send(FeedEvent::Parents(resolved));
            }
        });
        rx
    }

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

    /// A hashtag timeline (Phase 2 §4.2): the `#t` filter across the
    /// visible catalog, any author — the profile() shape with a tag filter
    /// instead of an author set. Relay fetch into the store, then project
    /// straight off the fetch (the store's matcher isn't trusted for tags);
    /// mutes and deletions gate like every feed page.
    pub fn hashtag(self: &Arc<Self>, tag: String) -> FeedRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res: Result<FeedPage> = async {
                let active = this.active().await?;
                // t tags are conventionally lowercase (NIP-24); the tap
                // keeps its raw case for display, the filter normalizes.
                let filter = Filter::new()
                    .kinds(crate::store::visible_kinds())
                    .custom_tag(SingleLetterTag::LOWERCASE_T, tag.to_lowercase())
                    .limit(PAGE + 1);
                let mut notes: Vec<Event> =
                    active.pool.get(filter).await.unwrap_or_default().into_iter().collect();
                for ev in &notes {
                    let _ = active.pool.store.save(ev).await;
                }
                {
                    let muted = this.muted.lock().unwrap();
                    notes.retain(|ev| !muted.contains(&ev.pubkey.to_string()));
                }
                let mut notes = this.drop_deleted(&active, notes).await;
                notes.sort_by_key(|e| std::cmp::Reverse(e.created_at.as_secs()));
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

    /// The hashtag timeline's older page — the home feed's scroll
    /// pagination (older()), with the tag filter standing in for the
    /// follow set. A full page keeps `has_more` lit; the next probe
    /// refetches around the oldest shown.
    pub fn hashtag_older(self: &Arc<Self>, tag: String, until: u64) -> FeedRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res: Result<FeedEvent> = async {
                let active = this.active().await?;
                let filter = Filter::new()
                    .kinds(crate::store::visible_kinds())
                    .custom_tag(SingleLetterTag::LOWERCASE_T, tag.to_lowercase())
                    .until(Timestamp::from(until))
                    .limit(PAGE + 1);
                let mut notes: Vec<Event> =
                    active.pool.get(filter).await.unwrap_or_default().into_iter().collect();
                for ev in &notes {
                    let _ = active.pool.store.save(ev).await;
                }
                {
                    let muted = this.muted.lock().unwrap();
                    notes.retain(|ev| !muted.contains(&ev.pubkey.to_string()));
                }
                let mut notes = this.drop_deleted(&active, notes).await;
                notes.sort_by_key(|e| std::cmp::Reverse(e.created_at.as_secs()));
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

    /// The profile's next older page (endless scroll): author-scoped mirror
    /// of `older` — the store page behind the anchor, then a relay top-up
    /// for the author only (deletions ride along), then re-project.
    pub fn profile_older(self: &Arc<Self>, who: PublicKey, until: u64) -> FeedRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res: Result<FeedEvent> = async {
                let active = this.active().await?;
                let mut notes = active
                    .pool
                    .store
                    .feed_page(&[who], Some(until), PAGE)
                    .await
                    .unwrap_or_default();
                if notes.len() <= PAGE {
                    let filter = Filter::new()
                        .kinds(crate::store::visible_kinds())
                        .author(who)
                        .until(Timestamp::from(until))
                        .limit(TOPUP_DEPTH);
                    let fetched = active.pool.get(filter).await.unwrap_or_default();
                    for ev in fetched {
                        let _ = active.pool.store.save(&ev).await;
                    }
                    let del_filter = Filter::new()
                        .kind(Kind::EventDeletion)
                        .author(who)
                        .until(Timestamp::from(until))
                        .limit(500);
                    for ev in active.pool.get(del_filter).await.unwrap_or_default() {
                        let _ = active.pool.store.save(&ev).await;
                    }
                    notes = active
                        .pool
                        .store
                        .feed_page(&[who], Some(until), PAGE)
                        .await
                        .unwrap_or_default();
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
                let mut replies = active
                    .pool
                    .store
                    .replies(id)
                    .await
                    .map_err(|e| LoginError::Resume(e.to_string()))?;
                // NIP-22 comments addressing the note (Amethyst replies to
                // non-kind-1 roots as 1111; comments-on-comments nest under
                // their parents through the same `e` tag). Store first, then
                // a relay top-up, then merge into one threaded set.
                for ev in active.pool.store.comments(id).await.unwrap_or_default() {
                    replies.push(ev);
                }
                let comment_filter = Filter::new().kind(Kind::Custom(1111)).event(id).limit(200);
                for ev in active.pool.get(comment_filter).await.unwrap_or_default() {
                    let _ = active.pool.store.save(&ev).await;
                    replies.push(ev);
                }
                // Deletions (NIP-09), targeted by id — replies come from
                // beyond the follows, so the sync can't cover them. Only a
                // delete from the reply's own author counts.
                let reply_ids: Vec<EventId> = replies.iter().map(|e| e.id).collect();
                if !reply_ids.is_empty() {
                    let mut del_filter = Filter::new().kind(Kind::EventDeletion).limit(200);
                    for id in reply_ids {
                        del_filter = del_filter.event(id);
                    }
                    let dels = active.pool.get(del_filter).await.unwrap_or_default();
                    for d in &dels {
                        let _ = active.pool.store.save(d).await;
                    }
                    if !dels.is_empty() {
                        let mut deleted: HashSet<(PublicKey, EventId)> = HashSet::new();
                        for d in &dels {
                            for id in d.tags.clone().event_ids() {
                                deleted.insert((d.pubkey, id));
                            }
                        }
                        replies.retain(|e| !deleted.contains(&(e.pubkey, e.id)));
                    }
                }
                let mut seen: HashSet<EventId> = HashSet::new();
                replies.retain(|e| seen.insert(e.id));
                replies.sort_by_key(|e| e.created_at.as_secs());
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

    /// Post a note — a NIP-10 reply, a NIP-18 quote, or plain. Signed, then
    /// held for the undo window with a backstop commit; events stream on
    /// the channel.
    pub fn post_note(
        self: &Arc<Self>,
        content: String,
        reply_to: Option<Event>,
        mentions: Vec<PublicKey>,
        quote: Option<Event>,
    ) -> PublishRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res = this
                .post_note_inner(&tx, content, reply_to, mentions, quote)
                .await;
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
        mentions: Vec<PublicKey>,
        quote: Option<Event>,
    ) -> Result<()> {
        let active = self.active().await?;
        let me = active.me();
        let mut tags: Vec<Tag> = Vec::new();
        // The NIP-18 quote rides FIRST: a q tag (id, relay hint, author) and
        // the nostr: reference folded into the text the reader sees.
        let content = if let Some(q) = &quote {
            tags.push(Tag::custom(
                "q",
                [
                    q.id.to_string(),
                    String::new(),
                    q.pubkey.to_string(),
                ],
            ));
            format!("{}\n\nnostr:{}", content, q.id.to_bech32().unwrap_or_default())
        } else {
            content
        };
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
        // @-mentions (NIP-10 p tags — what fires notifications). The
        // parent's own p's (root/parent authors) take precedence.
        let mut seen_p: Vec<String> = tags
            .iter()
            .filter(|t| t.as_slice().first().map(String::as_str) == Some("p"))
            .filter_map(|t| t.as_slice().get(1).cloned())
            .collect();
        for who in mentions {
            let hex = who.to_string();
            if hex != me.to_string() && !seen_p.contains(&hex) {
                seen_p.push(hex.clone());
                tags.push(Tag::public_key(who));
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

    /// A random 9-char option id (nip88.ts generateOptionId): xorshift off
    /// the clock and the option index — no rand dep for nine chars.
    fn poll_option_id(n: usize) -> String {
        const ID_CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
        let mut seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0)
            ^ (n as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let mut id = String::new();
        for _ in 0..9 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            id.push(ID_CHARS[(seed % ID_CHARS.len() as u64) as usize] as char);
        }
        id
    }

    /// Publish a NIP-88 poll (kind:1068): question in content, `option`
    /// tags with random 9-char ids, polltype, optional endsAt (seconds
    /// from now) + relay hints. nip88.ts buildPollTags shape. Same undo
    /// hold as notes.
    pub fn post_poll(
        self: &Arc<Self>,
        question: String,
        options: Vec<String>,
        multi: bool,
        ends_in_secs: Option<u64>,
    ) -> PublishRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res: Result<()> = async {
                let active = this.active().await?;
                const ID_CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
                let mut tag_list: Vec<Tag> = Vec::new();
                for (n, label) in options.iter().enumerate() {
                    tag_list.push(Tag::custom(
                        "option",
                        [Self::poll_option_id(n), label.as_str().to_string()],
                    ));
                }
                tag_list.push(Tag::custom(
                    "polltype",
                    [if multi { "multiplechoice" } else { "singlechoice" }],
                ));
                if let Some(secs) = ends_in_secs {
                    let ends_at = Timestamp::now().as_secs() + secs;
                    tag_list.push(Tag::custom("endsAt", [ends_at.to_string()]));
                }
                for url in &active.read_relays {
                    tag_list.push(Tag::custom("relay", [url.as_str()]));
                }
                let unsigned = UnsignedEvent::new(
                    active.me(),
                    Timestamp::now(),
                    Kind::Custom(1068),
                    tag_list,
                    question,
                );
                let signed = active.signer.sign_event(unsigned).await?;
                match active.undo.hold_duration() {
                    Some(hold) => {
                        let token = this.hold_seq.fetch_add(1, Ordering::SeqCst);
                        let _ = tx.send(PublishEvent::Held {
                            token,
                            event: signed.clone(),
                            hold_secs: hold.as_secs(),
                        });
                        this.hold_and_commit(Arc::new(active.clone()), token, signed, hold, tx.clone())
                            .await;
                    }
                    None => Self::commit(&active, &signed, &tx).await?,
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

    /// Post a NIP-68 picture (kind:20). `imeta` carries one or more full NIP-92
    /// imeta tags (from `upload::blossom_upload_all`) — a picture needs at least
    /// one. Same undo window as notes. Port of `publish.ts signPicture`.
    pub fn post_picture(
        self: &Arc<Self>,
        title: String,
        content: String,
        imeta: Vec<Vec<String>>,
        content_warning: Option<String>,
    ) -> PublishRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res = this.post_picture_inner(&tx, title, content, imeta, content_warning).await;
            if let Err(e) = res {
                let _ = tx.send(PublishEvent::Failed { token: 0, error: e.to_string() });
            }
        });
        rx
    }

    async fn post_picture_inner(
        self: &Arc<Self>,
        tx: &mpsc::UnboundedSender<PublishEvent>,
        title: String,
        content: String,
        imeta: Vec<Vec<String>>,
        content_warning: Option<String>,
    ) -> Result<()> {
        if imeta.is_empty() {
            return Err(LoginError::Other("a picture needs at least one image".into()));
        }
        let active = self.active().await?;
        let me = active.me();
        // NIP-68: a picture event carries a `title` tag (structural). A blank
        // composer title derives one from the caption's first non-empty line.
        let mut tags: Vec<Tag> = vec![Tag::custom(
            "title",
            [if title.trim().is_empty() {
                first_caption_line(&content)
            } else {
                title.trim().to_string()
            }],
        )];
        if let Some(cw) = &content_warning {
            tags.push(Tag::custom("content-warning", [cw.as_str()]));
        }
        for m in &imeta {
            if m.len() >= 2 && m[0] == "imeta" {
                tags.push(Tag::custom("imeta", m[1..].iter()));
            }
        }
        // NIP-68 also surfaces each image's hash (`x`) + mime (`m`) as TOP-LEVEL
        // tags (relay/client filtering), deduped across the imetas.
        let mut xs: Vec<String> = Vec::new();
        let mut mimes: Vec<String> = Vec::new();
        for m in &imeta {
            for v in m.iter().skip(1) {
                if let Some(x) = v.strip_prefix("x ")
                    && !xs.contains(&x.to_string())
                {
                    xs.push(x.to_string());
                }
                if let Some(mm) = v.strip_prefix("m ")
                    && !mimes.contains(&mm.to_string())
                {
                    mimes.push(mm.to_string());
                }
            }
        }
        for x in &xs {
            tags.push(Tag::custom("x", [x.as_str()]));
        }
        for m in &mimes {
            tags.push(Tag::custom("m", [m.as_str()]));
        }
        // TODO(port): src/nostr/content.ts mention tokenization → p tags (the
        // note path carries the same deferral).
        let unsigned = UnsignedEvent::new(me, Timestamp::now(), Kind::Custom(20), tags, content);
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

    /// Upload a blob to your Blossom servers (upload.ts). Emits
    /// `PublishEvent::Uploaded` with the blob URL + NIP-92 imeta tag; feed that
    /// imeta to `post_picture`.
    pub fn blossom_upload(self: &Arc<Self>, bytes: Vec<u8>, content_type: String) -> PublishRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res: std::result::Result<(), String> = async {
                if bytes.len() as u64 > crate::upload::MAX_BYTES {
                    return Err(format!(
                        "that file is too large (max {} MB)",
                        crate::upload::MAX_BYTES / (1024 * 1024)
                    ));
                }
                let active = this.active().await.map_err(|e| e.to_string())?;
                let me = active.me();
                // Your kind:10063 media-server list, newest wins (upload.ts
                // fetchBlossomServers); out-of-box default when unset.
                let filter = Filter::new()
                    .author(me)
                    .kind(Kind::Custom(crate::upload::KIND_BLOSSOM_LIST))
                    .limit(1);
                let server = active
                    .pool
                    .get(filter)
                    .await
                    .ok()
                    .and_then(|events| {
                        events
                            .into_iter()
                            .max_by_key(|ev| ev.created_at.as_secs())
                            .and_then(|ev| {
                                ev.tags
                                    .iter()
                                    .find(|t| t.as_slice().first().map(String::as_str) == Some("server"))
                                    .and_then(|t| t.as_slice().get(1).cloned())
                            })
                    })
                    .unwrap_or_else(|| crate::upload::DEFAULT_BLOSSOM_SERVER.to_string());
                let servers: Vec<String> = vec![server];
                let hash = crate::upload::sha256_hex(&bytes);
                let unsigned = crate::upload::blossom_auth_template(me, &hash);
                let signed = active
                    .signer
                    .sign_event(unsigned)
                    .await
                    .map_err(|e| e.to_string())?;                let up = crate::upload::blossom_upload_all(
                    &servers,
                    bytes,
                    &content_type,
                    &hash,
                    &signed,
                )
                .await?;
                let _ = tx.send(PublishEvent::Uploaded { url: up.url, imeta: up.imeta });
                Ok(())
            }
            .await;
            if let Err(e) = res {
                let _ = tx.send(PublishEvent::Failed { token: 0, error: e });
            }
        });
        rx
    }

    /// Fetch your kind:10063 media-server list for the settings pane. Emits
    /// `FeedEvent::MediaServers` (empty list = the out-of-box default applies).
    pub fn fetch_media_servers(self: &Arc<Self>) -> FeedRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res: Result<Vec<String>> = async {
                let active = this.active().await?;
                let me = active.me();
                let filter = Filter::new()
                    .author(me)
                    .kind(Kind::Custom(crate::upload::KIND_BLOSSOM_LIST))
                    .limit(1);
                let list = active
                    .pool
                    .get(filter)
                    .await
                    .ok()
                    .and_then(|events| {
                        events
                            .into_iter()
                            .max_by_key(|ev| ev.created_at.as_secs())
                    })
                    .map(|ev| server_tags(&ev))
                    .unwrap_or_default();
                Ok(list)
            }
            .await;
            match res {
                Ok(list) => {
                    let _ = tx.send(FeedEvent::MediaServers(list));
                }
                Err(e) => {
                    let _ = tx.send(FeedEvent::Failed(e.to_string()));
                }
            }
        });
        rx
    }

    /// Publish the media-server list (kind:10063, BUD-03) to your write
    /// relays — routes/settings.ts postMedia + upload.ts serverListTemplate.
    pub fn set_media_servers(self: &Arc<Self>, servers: Vec<String>) -> PublishRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res: Result<()> = async {
                let active = this.active().await?;
                let me = active.me();
                let tags: Vec<Tag> = servers
                    .iter()
                    .map(|s| Tag::custom("server", [s.trim_end_matches('/').to_string()]))
                    .collect();
                let unsigned = UnsignedEvent::new(
                    me,
                    Timestamp::now(),
                    Kind::Custom(crate::upload::KIND_BLOSSOM_LIST),
                    tags,
                    "",
                );
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
        self.mirror_hold(&PublishEvent::Held { token, event: event.clone(), hold_secs: hold.as_secs() });
        tokio::spawn({
            let core = Arc::clone(self);
            async move {
                tokio::select! {
                    _ = tokio::time::sleep(hold + crate::undo::BACKSTOP_GRACE) => {
                        if let Err(e) = Self::commit(&active, &event, &tx).await {
                            core.mirror_hold(&PublishEvent::Failed { token, error: e.to_string() });
                            let _ = tx.send(PublishEvent::Failed { token, error: e.to_string() });
                        } else {
                            core.mirror_hold(&PublishEvent::Committed { token });
                            let _ = tx.send(PublishEvent::Committed { token });
                        }
                    }
                    _ = cancel_rx => {
                        core.mirror_hold(&PublishEvent::Undone { token });
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

    /// Listen for push signals (bell dot, new-notes pill). Broadcast —
    /// every open pane keeps its own receiver.
    pub fn subscribe_live(&self) -> tokio::sync::broadcast::Receiver<Live> {
        self.live.subscribe()
    }

    /// The hold-state mirror for the shell's undo toast (Held → countdown,
    /// Committed/Undone/Failed → clear). The composer pops on Held, so the
    /// toast outlives it — subscribe once at shell creation.
    pub fn subscribe_hold(&self) -> tokio::sync::broadcast::Receiver<PublishEvent> {
        self.hold_tx.subscribe()
    }

    fn mirror_hold(&self, ev: &PublishEvent) {
        let _ = self.hold_tx.send(ev.clone());
    }

    /// ONE persistent subscription per session, three filters: fresh posts
    /// from follows (timeline pill), #p me (bell — every NIP-10/NIP-22 reply
    /// tags the parent author), kind-5 deletions (render filter). First-seen
    /// events route to broadcast signals; everything saves to the store.
    /// Follows are snapshotted at start — a mid-session follow change rides
    /// the next refresh's fetches until the next session.
    async fn start_live(&self, kinds: Vec<Kind>) {
        use futures_util::StreamExt as _;
        // Resolve the session BEFORE arming the flag: a failed resolve must
        // not wedge live_on true (it used to — the sub could never re-arm).
        let Ok(active) = self.active().await else { return };
        if self.live_on.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let pool = active.pool.clone();
        let live = self.live.clone();
        let live_on = self.live_on.clone();
        let live_epoch = Arc::clone(&self.live_epoch);
        // Captured at arm time; a mismatch at loop time means this session
        // was signed out and the loop tears its sub down below.
        let my_epoch = live_epoch.load(std::sync::atomic::Ordering::SeqCst);
        let me = active.me();
        let follows = active.follows.clone();
        self.rt.spawn(async move {
            let now = Timestamp::now();
            // NIP-01: an empty list can never match — the follows-scoped
            // filters join only when there are follows to scope by.
            let mut filters = vec![Filter::new()
                .kinds([Kind::TextNote, Kind::Custom(1111), Kind::Custom(9735)])
                .pubkey(me)
                .since(now)];
            if !follows.is_empty() {
                filters.push(
                    Filter::new()
                        .kinds(kinds.clone())
                        .authors(follows.clone())
                        .since(now),
                );
                filters.push(
                    Filter::new()
                        .kind(Kind::EventDeletion)
                        .authors(follows.clone())
                        .since(now),
                );
            }
            let Ok(output) = pool.client().subscribe(filters).await else {
                eprintln!("[core] live: subscribe failed; re-arming");
                live_on.store(false, std::sync::atomic::Ordering::SeqCst);
                return;
            };
            let mut notifications = pool.client().notifications();
            while let Some(n) = notifications.next().await {
                // Session changed (sign-out, or a different account): drop
                // the stale identity's subscription. The check runs before
                // anything touches the store or the bell. live_on is left
                // alone here — sign-out already reset it, and storing
                // again could clobber a NEWER session's armed flag.
                if live_epoch.load(std::sync::atomic::Ordering::SeqCst) != my_epoch {
                    eprintln!("[core] live: session changed; unsubscribing");
                    let _ = pool.client().unsubscribe(output.id()).await;
                    return;
                }
                let event = match n {
                    nostr_sdk::client::ClientNotification::Event { subscription_id, event, .. }
                        if output.id() == &subscription_id =>
                    {
                        *event
                    }
                    nostr_sdk::client::ClientNotification::Shutdown => break,
                    _ => continue,
                };
                let _ = pool.store.save(&event).await;
                let sig = if u16::from(event.kind) == 5 {
                    None // deletions ride the store; the render filter sees them
                } else if event.tags.clone().public_keys().any(|pk| pk == me) {
                    Some(Live::Mention)
                } else {
                    Some(Live::Post)
                };
                if let Some(sig) = sig {
                    let _ = live.send(sig);
                }
            }
            eprintln!("[core] live stream ended (relay disconnects resume via fetches)");
        });
    }

    async fn active(&self) -> Result<Active> {
        let held = self.active.lock().unwrap().clone();
        held.ok_or_else(|| {
            eprintln!("[core] ERROR: something asked for the session while logged out");
            eprintln!("{}", std::backtrace::Backtrace::force_capture());
            LoginError::Resume("no active session".into())
        })
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Releases the one-per-note flight slot on every exit path (success,
/// failure, early return) — the manager's arm of the Feather rule.
struct FlightGuard<'a>(&'a Wallet, &'a str);
impl Drop for FlightGuard<'_> {
    fn drop(&mut self) {
        self.0.release_flight(self.1);
    }
}

/// A profile's lightning pointer: lud16 before lud06 (NIP-57's order).
/// `None` = the profile has no zap address.
fn lightning_field(content: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(content).ok()?;
    let field = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    field("lud16").or_else(|| field("lud06"))
}

/// NIP-05's wire check: `https://<domain>/.well-known/nostr.json?name=<name>`
/// maps `"names": { "<name>": "<hex pubkey>" }`. The local part is
/// spec-lowercased alphanumerics/`-`/`_` (`_` = the domain root), so it is
/// URL-safe as-is. Hostile/malformed answers read as unproven, not `false`.
async fn verify_nip05_inner(nip05: &str, pubkey: &str) -> Option<bool> {
    let nip05 = nip05.trim().to_lowercase();
    let (name, domain) = nip05.rsplit_once('@')?;
    if name.is_empty() || domain.is_empty() {
        return None;
    }
    let url = format!("https://{domain}/.well-known/nostr.json?name={name}");
    let v = crate::media::fetch_json(&url).await.ok()?;
    v.get("names")?.get(name)?.as_str()
        .map(|mapped| mapped.eq_ignore_ascii_case(pubkey))
}

/// The caption's first non-empty line, capped at 120 chars — nip68.ts
/// `firstCaptionLine`, the ONE definition the blank-picture-title derive uses.
fn first_caption_line(content: &str) -> String {
    content
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .chars()
        .take(120)
        .collect()
}

/// The `server` tag values of a kind:10063 list (BUD-03).
fn server_tags(ev: &Event) -> Vec<String> {
    ev.tags
        .iter()
        .filter(|t| t.as_slice().first().map(String::as_str) == Some("server"))
        .filter_map(|t| t.as_slice().get(1).cloned())
        .filter(|s| !s.is_empty())
        .collect()
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

/// Read-modify-write prefs.json (0600): overlay `pairs` onto the existing
/// object, preserving app-owned keys. Core's single prefs write path —
/// set_undo and the NIP-78 sync both land here.
fn write_prefs_overlay(pairs: &[(String, serde_json::Value)]) {
    let path = crate::config::state_file(crate::config::files::PREFS);
    let mut value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
    if let Some(obj) = value.as_object_mut() {
        for (k, v) in pairs {
            obj.insert(k.clone(), v.clone());
        }
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

/// NIP-78 login sync (Phase 2 §4.1): fetch our own kind-30078 slots, keep
/// the newest per d tag, decrypt, and overlay the Satori keys into
/// prefs.json. Best-effort — relay misses, foreign d tags and undecryptable
/// slots are skipped. PrefsSynced lets the shell re-read what it renders.
async fn sync_app_data(
    pool: &Arc<Pool>,
    me: &PublicKey,
    signer: &Signer,
    live: &tokio::sync::broadcast::Sender<Live>,
) {
    let filter = Filter::new().kind(Kind::Custom(nip78::KIND)).author(*me);
    let events = match pool.get(filter).await {
        Ok(set) => set.into_iter().collect::<Vec<_>>(),
        Err(_) => return,
    };
    let mut pairs: Vec<(String, serde_json::Value)> = Vec::new();
    for slot in nip78::group_latest(&events) {
        if !nip78::synced_key(&slot.key) || slot.content.is_empty() {
            continue;
        }
        let Ok(plain) = signer.nip44_decrypt_self(me, slot.content.as_str()).await
        else {
            continue;
        };
        let Some(value) = nip78::parse_payload(&plain) else { continue };
        pairs.push((slot.key, value));
    }
    if pairs.is_empty() {
        return;
    }
    write_prefs_overlay(&pairs);
    let _ = live.send(Live::PrefsSynced);
}

#[cfg(test)]
/// Test-only local-key login — the user-facing nsec door is gone (D17: the
/// app never handles the master key), but the logout/re-login flow tests
/// still need a way in.
#[cfg(test)]
impl Core {
    async fn login_nsec_inner(&self, secret: &str) -> Result<PublicKey> {
        let keys = signer::parse_local_secret(secret)?;
        let pk = keys.public_key();

        secrets::store(secrets::NSEC, &keys.secret_key().display_secret().to_string()).await?;
        let session = Session { pubkey: pk.to_string(), mode: SessionMode::Local };
        session::save(&session)?;

        let active = self.build_active(&session).await?;
        *self.active.lock().unwrap() = Some(active);
        Ok(pk)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// NIP-92 kind-0 adoption: the banner's blurhash arrives only from an
    /// imeta tag whose url matches the banner field exactly (Oct 2026 spec
    /// section; the TS reference predates it).
    #[test]
    fn profile_banner_blurhash_needs_an_exact_url_match() {
        use nostr::prelude::*;
        let banner = "https://b/banner.webp";
        let content = format!(
            "{{\"name\":\"luna\",\"banner\":\"{banner}\"}}"
        );
        let build = |imeta: &[&str]| {
            let keys = Keys::generate();
            let mut tags = Tags::new();
            tags.push(Tag::custom("imeta", imeta.iter().copied()));
            let unsigned = UnsignedEvent::new(
                keys.public_key(),
                Timestamp::from_secs(1),
                Kind::Metadata,
                tags,
                content.clone(),
            );
            keys.sign_event(unsigned).unwrap()
        };
        let hit = Profile::from_event(&build(&[
            "url https://b/banner.webp",
            "m image/webp",
            "blurhash LEHV6nWB2yk8pyo0adR*.7kCMdnj",
        ]))
        .unwrap();
        assert_eq!(
            hit.banner_blurhash.as_deref(),
            Some("LEHV6nWB2yk8pyo0adR*.7kCMdnj")
        );
        // A tag describing the PICTURE (or any other url) says nothing
        // about the banner.
        let miss = Profile::from_event(&build(&[
            "url https://b/picture.jpg",
            "blurhash LEHV6nWB2yk8pyo0adR*.7kCMdnj",
        ]))
        .unwrap();
        assert_eq!(miss.banner_blurhash, None);
    }


    /// The D32 crash seam: the poster job must run on THIS runtime no
    /// matter which thread summons it — the first cut let the UI poll the
    /// futures on gpui's executor, where `spawn_blocking` has no tokio
    /// context, and thread 'main panicked the app. A plain thread calling
    /// the wrapper (no runtime anywhere on it) must still get its verdict.
    /// The unsafe URL returns before any network or sniff work.
    #[test]
    fn poster_job_runs_on_the_core_runtime() {
        let core = Arc::new(Core::new());
        let mut rx = core.fetch_poster("not a url".into(), 16, |_| Err(()));
        let verdict =
            std::thread::spawn(move || rx.blocking_recv()).join().unwrap();
        assert!(matches!(
            verdict,
            Some(Err(crate::media::MediaError::NotMedia))
        ));
    }

    /// Logout → login → notifications must NOT fail with "no active
    /// session" (the user-report). Runs headless; network errors are fine,
    /// session-state errors are not. Login runs via block_on: the
    /// spawned-task variant hangs in this container; the host does not.
    #[test]
    fn login_then_notifications_has_session() {
        let _env = crate::test_env::ENV_LOCK.lock().unwrap();
        let core = Arc::new(Core::new());
        core.logout(); // start clean
        let secret_path = std::env::temp_dir().join("satori-test-nsec");
        let secret =
            "e1d4a5b33a9d2f0c8b7e6f5a4d3c2b1a0918273645f4e3d2c1b0a99887766554";
        let _ = std::fs::write(&secret_path, secret);

        core.rt
            .block_on(core.login_nsec_inner(secret))
            .expect("login");
        assert!(core.pubkey().is_some(), "no pubkey after login");

        // The bug: notifications must not report "no active session".
        let mut rx = core.notifications();
        let mut got = None;
        for _ in 0..500 {
            if let Ok(ev) = rx.try_recv() {
                got = Some(ev);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        match got {
            Some(FeedEvent::Failed(msg)) => {
                assert!(!msg.contains("no active session"), "session lost: {msg}");
                eprintln!("network failure (acceptable headless): {msg}");
            }
            Some(_) => {}
            None => panic!("notifications never resolved"),
        }

        // THE USER'S FLOW: logout → re-login → notifications.
        core.logout();
        assert!(core.pubkey().is_none(), "pubkey survived logout");
        core.rt
            .block_on(core.login_nsec_inner(secret))
            .expect("re-login");
        assert!(core.pubkey().is_some(), "no pubkey after re-login");

        let mut rx = core.notifications();
        let mut got = None;
        for _ in 0..500 {
            if let Ok(ev) = rx.try_recv() {
                got = Some(ev);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        match got {
            Some(FeedEvent::Failed(msg)) => {
                assert!(!msg.contains("no active session"), "session lost after re-login: {msg}");
                eprintln!("network failure after re-login (acceptable headless): {msg}");
            }
            Some(_) => {}
            None => panic!("notifications never resolved after re-login"),
        }
    }
}
