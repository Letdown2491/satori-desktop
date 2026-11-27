//! Core: owns the tokio runtime and the active session — the seam between the
//! GTK UI and all I/O. UI code submits commands here; results flow back over
//! executor-agnostic channels (`tokio::sync`) awaited from the GLib main
//! context (DESIGN.md §3.2). The web app's server-session machinery
//! (`src/session.ts`) collapses to this single-user handle.

use parking_lot::Mutex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use nostr::prelude::*;
use nostr_connect::prelude::*;
use nostr_sdk::client::Output;
use tokio::sync::mpsc;

use crate::blocktime::Blocktime;
use crate::deletions;
use crate::error::{LoginError, Result};
use crate::hints;
use crate::nip::nip78;
use crate::nip40;
use crate::outbox;
use crate::pool::{INDEXERS, Pool};
use crate::relay_latency;
use crate::secrets;
use crate::seen;
use crate::seen_event;
use crate::session::{self, Session, SessionMode};
use crate::signer::{self, Signer};
use crate::wallet::{self, Wallet, WalletEvent};
use zeroize::Zeroizing;

// The Core handle's methods, split by surface (review Nov 27 2026: one
// 10k-line file was the slowest thing in the crate to review). Each
// child holds one `impl Core` block; shared helpers stay here.
mod feed;
mod lists;
mod login;
mod notifications;
mod reads;
mod viewport;
mod write;
mod zaps;

/// How long to wait for the remote signer to approve a connect request.
const LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Feed page size; the bootstrap fetch keeps depth for "view older".
const PAGE: usize = 40;
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
    /// Paste-time relay screening refused a private relay (issue #11):
    /// the full verdicts, for the login dialog's allow-and-retry
    /// affordance (review Tier 2.5). Terminal, like `Failed`: the banner
    /// line is derived from the verdicts, nothing was dialed.
    RelayScreened(Vec<crate::net_guard::RelayVerdict>),
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

/// Who signs the kind-9734 (D92's privacy dropdown). `Owner` rides the
/// active signer (the bunker); `Anonymous` signs locally with a fresh
/// ephemeral key (NIP-57) — the wallet pays and the receipt lands as
/// before, but no identity rides the request. One key per zap: a
/// split's requests all share it.
enum ZapSigner {
    Owner,
    Anonymous(Keys),
}

impl ZapSigner {
    fn new(privacy: wallet::ZapPrivacy) -> Self {
        match privacy {
            wallet::ZapPrivacy::Attributed => Self::Owner,
            wallet::ZapPrivacy::Anonymous => Self::Anonymous(Keys::generate()),
        }
    }

    /// The pubkey stamped into the request: the owner's (finalized
    /// unsigned here, signed through the bunker) or the ephemeral one.
    fn author(&self, me: PublicKey) -> PublicKey {
        match self {
            Self::Owner => me,
            Self::Anonymous(k) => k.public_key(),
        }
    }

    async fn sign(
        &self,
        active: &Active,
        unsigned: UnsignedEvent,
    ) -> std::result::Result<Event, String> {
        match self {
            Self::Owner => active
                .sign_report(unsigned)
                .await
                .map_err(|e| e.to_string()),
            Self::Anonymous(k) => k.sign_event(unsigned).map_err(|e| e.to_string()),
        }
    }
}

/// Where an ANONYMOUS zap's receipt is asked to publish (the 9734's
/// relays tag, visible to anyone reading the receipt): a fixed neutral
/// public set, never the owner's write relays — a personal relay set
/// is itself a fingerprint (D94's audit finding). The trade-off is
/// honest: our own receipt watch may miss the confirmation (it polls
/// our read relays), and Paid stays the truth of record.
const ANON_ZAP_RELAYS: [&str; 3] = [
    "wss://nostr.wine/",
    "wss://relay.damus.io/",
    "wss://nos.lol/",
];

/// The signed request's own e tag as an EventId — the receipt-watch's
/// geometry read from the REQUEST, not guessed from the ledger key (a
/// pubkey-hex key parses as an EventId; the audit's phantom-e-tag bug).
/// Profile zaps have none, and their receipts must not be filtered on one.
fn request_e_tag(signed: &Event) -> Option<EventId> {
    signed
        .tags
        .iter()
        .find(|t| t.as_slice().first().map(String::as_str) == Some("e"))
        .and_then(|t| t.as_slice().get(1))
        .and_then(|hex| EventId::from_hex(hex).ok())
}

/// The receipt-relay choice per privacy (see ANON_ZAP_RELAYS): ours for
/// attributed (we want the confirmation), the neutral set for anonymous.
fn zap_receipt_relays(privacy: wallet::ZapPrivacy, write_relays: &[String]) -> Vec<String> {
    match privacy {
        wallet::ZapPrivacy::Attributed => write_relays.to_vec(),
        wallet::ZapPrivacy::Anonymous => ANON_ZAP_RELAYS.iter().map(|s| s.to_string()).collect(),
    }
}

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
    /// A focused reply's ancestor chain (the thread pane's context above
    /// the focus), root first; re-sent as each hop resolves. `true` =
    /// the walk stopped at an unreachable parent (deleted, or no relay
    /// has it): the pane says earlier posts are missing.
    Ancestors(Vec<Event>, Vec<Profile>, bool),
    /// Resolved reply parents for the timeline's context whisper: parent
    /// event id → author + snippet. Misses resolve on a later ask.
    Parents(std::collections::HashMap<String, ParentNote>),
    /// Poll tallies: (option id, count) over each voter's latest ballot.
    PollVotes(Vec<(String, u32)>),
    /// NIP-75 zap-goal progress: the goal's raised total in msat (the
    /// receipts' bolt11-authoritative amounts, summed).
    GoalMsat(EventId, u64),
    /// Comments on a target, oldest first + profiles.
    Comments(Vec<Event>, Vec<Profile>),
    /// Per-note comment counts (the bubble): (note id, count) for the
    /// notes that HAVE replies/comments.
    CommentCounts(Vec<(EventId, u32)>),
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

// --- Notification tier + receipts (issue #2 WS4) --------------------------------

/// One notification the WoT tier suppressed — a receipt, never a silent
/// void: the bell's "N hidden, see why" surface lists these with the
/// verdict and reason, and a per-item "show anyway" exemption.
/// Serializes for the D48 sidecar: receipts survive restarts (a receipt
/// that evaporated at every launch was a weak receipt).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SuppressedNotif {
    /// The suppressed event itself (the surface renders author + action).
    pub event: Event,
    /// The verdict that gated it ("outside your network").
    pub reason: String,
}

static SUPPRESSED: Mutex<Vec<SuppressedNotif>> = Mutex::new(Vec::new());

// --- Receipts persistence (D48): the suppressed ledger is the audit
// trail of the app's only auto-actions; it survives restarts. Two
// effects: the "N hidden" surface is populated from the first frame,
// and — the load-bearing part — `split_wot_tier` re-receipts any
// candidate already in the ledger, so a verdict that EXPIRED (the
// velocity window) while the event sat in the store doesn't resurrect
// old spam at boot. Same discipline as the other sidecars.

/// Newest receipts kept on disk (the bell's queries are 200-limited —
/// anything older can't re-candidate anyway).
const SUPPRESSED_CAP: usize = 200;
const SUPPRESSED_FLUSH_SECS: u64 = 10;

/// How long the zap path waits for split receivers' kind-0 to warm
/// (NIP-57 App. G tags can name profiles we've never met) before
/// resolving pointers from whatever landed.
const SPLIT_WARM_SECS: u64 = 5;
static SUPPRESSED_DIRTY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static SUPPRESSED_FLUSHER: std::sync::OnceLock<()> = std::sync::OnceLock::new();
static SUPPRESSED_LOADED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// One wot source fetch per launch (spawn_wot_source_fetch's doc): a
/// bootstrap runs per Feed surface, and each used to re-fire the same
/// 217-target routed fan-out.
static WOT_SOURCE_FETCHED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn suppressed_dirty() {
    SUPPRESSED_DIRTY.store(true, std::sync::atomic::Ordering::Relaxed);
    SUPPRESSED_FLUSHER.get_or_init(|| {
        std::thread::Builder::new()
            .name("suppressed-flush".into())
            .spawn(|| {
                loop {
                    std::thread::park_timeout(std::time::Duration::from_secs(
                        SUPPRESSED_FLUSH_SECS,
                    ));
                    if SUPPRESSED_DIRTY.swap(false, std::sync::atomic::Ordering::Relaxed) {
                        write_suppressed_disk();
                    }
                }
            })
            .ok();
    });
}

fn write_suppressed_disk() {
    let mut items = SUPPRESSED.lock().clone();
    items.sort_by_key(|s| std::cmp::Reverse(s.event.created_at.as_secs()));
    items.truncate(SUPPRESSED_CAP);
    let arr: Vec<serde_json::Value> = items
        .iter()
        .map(|s| serde_json::json!({ "reason": s.reason, "event": s.event }))
        .collect();
    let path = crate::config::state_file(crate::config::files::SUPPRESSED);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let tmp = path.with_extension(format!("tmp-{}-{nanos}", std::process::id()));
    let body = serde_json::Value::Array(arr).to_string();
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    let Ok(mut f) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
    else {
        return;
    };
    if f.write_all(body.as_bytes()).is_ok() {
        drop(f);
        let _ = std::fs::rename(&tmp, &path);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Merge the persisted receipts into the live store (bootstrap arm;
/// once per process — later runs REPLACE the store with their
/// re-derivation, and the receipted-gate keeps the merged ids in it).
fn load_suppressed_disk() {
    if SUPPRESSED_LOADED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let Ok(raw) =
        std::fs::read_to_string(crate::config::state_file(crate::config::files::SUPPRESSED))
    else {
        return;
    };
    let Ok(arr) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return;
    };
    let mut held = SUPPRESSED.lock();
    for e in arr.as_array().unwrap_or(&Vec::new()) {
        let (Ok(event), Some(reason)) = (
            serde_json::from_value::<Event>(e.get("event").cloned().unwrap_or_default()),
            e.get("reason").and_then(|x| x.as_str()).map(String::from),
        ) else {
            continue;
        };
        if held.iter().any(|s| s.event.id == event.id) {
            continue;
        }
        held.push(SuppressedNotif { event, reason });
    }
    held.sort_by_key(|s| std::cmp::Reverse(s.event.created_at.as_secs()));
    held.truncate(SUPPRESSED_CAP);
}
/// One-session "show anyway" exemptions (pubkey hex): the user chose to
/// see an author's suppressed notifications — the tier stops gating them
/// until sign-out. NOT a pref (the posture: the exemption is a conscious
/// per-session act, visible in the receipts it empties).
static NOTIF_EXEMPT: std::sync::LazyLock<Mutex<HashSet<String>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashSet::new()));

/// The notification tier's pref (`notif_wot_tier` in prefs.json, default
/// ON per the issue posture: the only auto-actions live in notifications,
/// and they leave receipts).
fn notif_tier_enabled() -> bool {
    crate::config::prefs()
        .and_then(|v| v.get("notif_wot_tier").and_then(|b| b.as_bool()))
        .unwrap_or(true)
}

/// Apply the tier to one page: reactions/reposts (kinds 6/7 — engagement
/// noise, not speech) AND kind-1 replies to the user's own notes (D43's
/// anti-bot amendment — bot replies are the cheapest notification attack)
/// from authors the graph KNOWS are NotInNetwork are pulled out as
/// receipts. Mentions/zaps pass; and the second D43 path: an Unknown
/// author whose replies hit the reply-velocity verdict (rate.rs) is
/// suppressed into the receipts too — 1–4 replies pass untouched, the
/// counter is fed only by DELIVERED replies, and exempted authors pass
/// everything (the exemption is a conscious per-session act).
///
/// `my_notes` is the user's recent note ids — what makes a kind-1 a REPLY
/// (an `e` tag at one of them) rather than a mention. `count_velocity`
/// feeds the rate counter: the final-union split sets it; the cached-phase
/// split leaves it false so overlapping phases don't double-count (the
/// cached phase still wears the flood GATE via `reply_flooded` — the
/// phase-equality rule).
/// Is this event actually ABOUT me? The "@geek" report: the page showed
/// "mentioned you" rows with nothing to do with me — the mention query
/// asks relays for #p=me, but the ANSWERS were trusted, so a relay
/// mis-serving the filter (a firehose under our subscription id, an
/// indexer that ignores p) injected strangers' notes. The live bell
/// never had this hole (`run_live` verifies p == me exactly); the page
/// now verifies too: a p tag carrying MY key (hex or bech32, with or
/// without the `nostr:` prefix), an a tag whose pubkey field is me, or
/// an e/q tag referencing one of my recent notes. Zaps (9735, p = the
/// recipient), reactions and reposts (p = the author, e = the note) all
/// clear it naturally — anything else was never addressed to me.
fn notif_about_me(ev: &Event, me: &PublicKey, my_notes: &HashSet<EventId>) -> bool {
    let key = crate::outbox::parse_pubkey_ref;
    for t in ev.tags.iter() {
        let v = t.as_slice();
        let (Some(kind), Some(value)) = (v.first().map(String::as_str), v.get(1)) else {
            continue;
        };
        match kind {
            "p" => {
                if key(value).is_some_and(|k| k == *me) {
                    return true;
                }
            }
            // NIP-01 parameterized replaceable reference: kind:pubkey:d
            "a" => {
                if let Some((_, pk)) = value.split_once(':')
                    && key(pk).is_some_and(|k| k == *me)
                {
                    return true;
                }
            }
            // e/q referencing one of my recent notes (reply, quote,
            // poll vote on my poll)
            "e" | "q"
                if EventId::from_hex(value)
                    .ok()
                    .is_some_and(|id| my_notes.contains(&id)) =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

fn split_wot_tier(
    notifs: Vec<Event>,
    my_notes: &HashSet<EventId>,
    count_velocity: bool,
) -> (Vec<Event>, Vec<SuppressedNotif>) {
    // Hoisted (audit fix): the pref read (one metadata syscall + Arc)
    // and the exempt-set lock were per EVENT — now per PAGE. The
    // receipted ledger snapshots too (D48): its ids re-suppress below.
    let tier_on = notif_tier_enabled();
    let exempt_set: HashSet<String> = NOTIF_EXEMPT.lock().clone();
    let receipted: HashMap<String, String> = {
        let held = SUPPRESSED.lock();
        held.iter()
            .map(|s| (s.event.id.to_string(), s.reason.clone()))
            .collect()
    };
    let mut kept = Vec::new();
    let mut suppressed = Vec::new();
    for ev in notifs {
        let kind = u16::from(ev.kind);
        let engagement = matches!(kind, 6 | 7);
        let reply =
            matches!(kind, 1 | 1111) && ev.tags.event_ids().any(|id| my_notes.contains(&id));
        // Show-anyway first: the conscious per-session act outranks
        // every auto-gate — including the dedup store door below
        // (otherwise the receipts strip's button lied for
        // duplicate-content receipts).
        if exempt_set.contains(&ev.pubkey.to_string()) {
            kept.push(ev);
            continue;
        }
        // The store door: the chokepoint stops a hidden spammer's
        // DELIVERIES, but the SDK already persisted their earlier
        // events — the cached phase reads those straight back. The
        // dedup ledger is the truth about who's hidden (D41's
        // "unreachable through any read path" made true here too).
        // Suppressed with a receipt, consistent with the tier.
        if crate::dedup::is_hidden(&ev.pubkey.to_string()) {
            suppressed.push(SuppressedNotif {
                reason: "duplicate-content filter".to_string(),
                event: ev,
            });
            continue;
        }
        if !(tier_on && (engagement || reply)) {
            kept.push(ev);
            continue;
        }
        // Receipt durability (D48): a verdict that expired (the
        // velocity window) while the event sat in the store must not
        // resurrect old spam on the first frame of a launch — once
        // receipted, still receipted. The show-anyway exemption above
        // is the per-session override; the tier switch below (`tier_on`)
        // is the feature-off state.
        if let Some(reason) = receipted.get(&ev.id.to_string()) {
            suppressed.push(SuppressedNotif {
                reason: reason.clone(),
                event: ev,
            });
            continue;
        }
        match crate::wot::verdict(&ev.pubkey) {
            // Known and outside: gated regardless of kind or velocity.
            crate::wot::Verdict::NotInNetwork => suppressed.push(SuppressedNotif {
                reason: "outside your network".to_string(),
                event: ev,
            }),
            // The D43 mule rule: Unknown + reply velocity. The check runs
            // on both phases; only the delivering phase feeds the counter.
            crate::wot::Verdict::Unknown if reply => {
                if crate::rate::reply_flooded(&ev.pubkey, ev.created_at.as_secs())
                    || (count_velocity
                        && !crate::rate::admit_reply(
                            &ev.pubkey,
                            ev.created_at.as_secs(),
                            &ev.id.to_string(),
                        ))
                {
                    suppressed.push(SuppressedNotif {
                        reason: crate::rate::flood_reason(),
                        event: ev,
                    });
                } else {
                    kept.push(ev);
                }
            }
            _ => kept.push(ev),
        }
    }
    (kept, suppressed)
}

/// What the write path reports back.
#[derive(Debug, Clone)]
pub enum PublishEvent {
    /// The signed event is held for the undo window; countdown starts.
    Held {
        token: u64,
        event: Event,
        hold_secs: u64,
    },
    /// The hold was undone; nothing was published.
    Undone {
        token: u64,
    },
    /// Published (or hold expired — the backstop committed).
    Committed {
        token: u64,
    },
    /// A Blossom upload finished (upload.rs): blob URL + the NIP-92 imeta tag.
    Uploaded {
        url: String,
        imeta: Vec<String>,
    },
    /// A restore finished: n events re-signed + published.
    Restored(u32),
    Failed {
        token: u64,
        error: String,
    },
}

pub type PublishRx = mpsc::UnboundedReceiver<PublishEvent>;

/// What the scheduling path reports back (schedule_note).
#[derive(Debug, Clone)]
pub enum ScheduleEvent {
    /// Held on disk; the sweep publishes it at `scheduled_at`.
    Scheduled { token: String, scheduled_at: u64 },
    /// Refused (cap reached, sign failure, logged out).
    Failed { error: String },
}

#[derive(Clone)]
struct Active {
    session: Session,
    signer: Signer,
    /// The signer bar's watch (D103), shared with the Core that built
    /// this session: sign_report writes it, the shell pumps it.
    signer_health: Arc<tokio::sync::watch::Sender<bool>>,
    pool: Arc<Pool>,
    profiles: Arc<crate::profiles::ProfileService>,
    undo: crate::undo::UndoWindow,
    /// Own NIP-65 write relays.
    write_relays: Vec<String>,
    /// Own NIP-65 read relays (indexers are always on).
    read_relays: Vec<String>,
    follows: Vec<PublicKey>,
    /// The follow list re-encoded to lowercase hex, refreshed whenever
    /// `follows` is. The mention typeahead walks it per keystroke (the
    /// empty-@ tier in follow order, membership tests for the ranking);
    /// hex-encoding per call paid one allocation per follow per
    /// keystroke. Arc'd so the per-call clone is a refcount bump.
    follows_hex: Arc<Vec<String>>,
    /// Own pubkey, parsed once at session build. me() hands it out; it
    /// used to re-parse the session hex per call (26 call sites, some on
    /// hot paths) and expect-panicked if session.json held a corrupt
    /// pubkey — build_active now refuses the session instead.
    me: PublicKey,
}

impl Active {
    /// The one owner-sign doorway (D103): every sign call site goes
    /// through here so the shell's amber bar rides the same truth. A
    /// success clears the bar; a signer's ANSWER clears it too (a
    /// refusal proves reachability); a transport-class failure lights
    /// it. Local keys can never report a transport failure, so the bar
    /// is a bunker-only signal by construction.
    async fn sign_report(&self, unsigned: UnsignedEvent) -> Result<Event> {
        match self.signer.sign_event_classified(unsigned).await {
            Ok(ev) => {
                self.set_signer_reach(true);
                Ok(ev)
            }
            Err(crate::signer::SignFailure::Answered(e)) => {
                self.set_signer_reach(true);
                Err(e)
            }
            Err(crate::signer::SignFailure::Transport(e)) => {
                self.set_signer_reach(false);
                Err(e)
            }
        }
    }

    /// Write the watch only on change (a changed() tick per redundant
    /// send would wake the shell for nothing).
    fn set_signer_reach(&self, ok: bool) {
        self.signer_health.send_if_modified(|v| {
            if *v == ok {
                false
            } else {
                *v = ok;
                true
            }
        });
    }

    /// Set the follow list and its hex mirror together — the pair must
    /// never drift (the mirror feeds the mention tiers).
    fn set_follows(&mut self, follows: Vec<PublicKey>) {
        self.follows_hex = Arc::new(follows.iter().map(|pk| pk.to_string()).collect::<Vec<_>>());
        self.follows = follows;
    }

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

/// The composer tab a scheduled post rides as — the D49 routing as a
/// type (a photo post schedules the SAME kind-20 event an instant post
/// makes; a video post kind 22; a note kind 1 + imeta when it carries
/// any). Replaced the (photo_post, video_post) bool pair: call sites
/// read `false, false, at` with nothing labeling which flag meant which
/// tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostKind {
    Note,
    Photo,
    Video,
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
    /// Engagement on a viewport note arrived live (WS2, viewport.rs): the
    /// event is already in the store (the SDK saves on receive) — the open
    /// feed re-reads its state and repaints.
    Engagement,
}

/// A reply's parent, for the timeline context whisper: author + a plain
/// one-liner from the parent's content.
#[derive(Debug, Clone)]
pub struct ParentNote {
    pub author: PublicKey,
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

#[derive(Debug, Clone)]
pub struct Profile {
    pub pubkey: PublicKey,
    /// The display name: kind-0 `display_name`, falling back to `name`.
    /// The raw `name` handle rides alongside (`handle`) so @-mention
    /// matching can find "@mpft" when the display name reads "Mike P.".
    pub name: String,
    /// The kind-0 `name` handle as self-asserted (the @-identity most
    /// clients type; distinct from NIP-05, which is verified).
    pub handle: String,
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
            pubkey: ev.pubkey,
            handle: raw.name.clone().unwrap_or_default(),
            name: raw.display_name.or(raw.name).unwrap_or_default(),
            picture: raw.picture.unwrap_or_default(),
            about: raw.about.unwrap_or_default(),
            banner: raw.banner.unwrap_or_default(),
            nip05: raw.nip05.unwrap_or_default(),
            lud: raw.lud16.or(raw.lud06).unwrap_or_default(),
            banner_blurhash,
        })
    }

    /// Display name fallback chain: name → short npub (a REAL bech32
    /// npub's tail — the old fallback mislabeled hex as npub; D84 audit
    /// nit). render/util.ts style.
    pub fn display(&self) -> String {
        if self.name.is_empty() {
            let npub = self.pubkey.to_bech32().unwrap_or_default();
            if npub.is_empty() {
                return self.pubkey.to_hex();
            }
            format!("npub1…{}", &npub[npub.len().saturating_sub(6)..])
        } else {
            self.name.clone()
        }
    }
}

/// The mention-candidate selection, pure for testability: substring match
/// on display name, raw handle, NIP-05, and pubkey hex; the follow tier
/// sorts ahead of everyone else's; rows hold still (name-sorted) within a
/// tier; the empty query is "the people you follow" in follow order (the
/// bare-@ view). `skip` is the caller's own hex — you don't mention
/// yourself. Eight hits max, the strip's length.
fn select_mention_candidates(
    query: &str,
    follows_hex: &[String],
    skip: &str,
    cache: &[&ProfileEntry],
) -> Vec<Profile> {
    // One allocation per keystroke (the query itself); every per-profile
    // key below was pre-lowered at absorb time.
    let q = query.to_lowercase();
    let follow_set: HashSet<&String> = follows_hex.iter().collect();
    let mut out: Vec<Profile> = Vec::new();
    if q.is_empty() {
        // The bare-@ view: follows in follow order — the only tier that
        // needs pubkey→profile, so the index builds here only.
        let by_hex: HashMap<&str, &ProfileEntry> =
            cache.iter().map(|e| (e.hex.as_str(), *e)).collect();
        for hex in follows_hex {
            if out.len() == 8 {
                break;
            }
            if hex == skip {
                continue;
            }
            if let Some(e) = by_hex.get(hex.as_str()) {
                out.push(e.profile.clone());
            }
        }
        return out;
    }
    let mut tier_follow: Vec<&ProfileEntry> = Vec::new();
    let mut tier_other: Vec<&ProfileEntry> = Vec::new();
    for e in cache {
        if e.hex == skip {
            continue;
        }
        let hit = e.name_l.contains(&q)
            || e.handle_l.contains(&q)
            || e.nip05_l.contains(&q)
            || e.hex.contains(&q);
        if !hit {
            continue;
        }
        if follow_set.contains(&e.hex) {
            tier_follow.push(e);
        } else {
            tier_other.push(e);
        }
    }
    tier_follow.sort_by(|a, b| a.name_l.cmp(&b.name_l));
    tier_other.sort_by(|a, b| a.name_l.cmp(&b.name_l));
    for e in tier_follow.into_iter().chain(tier_other) {
        if out.len() == 8 {
            break;
        }
        out.push(e.profile.clone());
    }
    out
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
    map: HashMap<PublicKey, ProfileEntry>,
    order: VecDeque<PublicKey>,
}

/// A hydrated profile plus the pre-lowered keys its scans match on:
/// the mention typeahead runs per keystroke on the UI thread, so the
/// lowercase copies are computed once at absorb time (lowering all
/// three per profile per keystroke cost ~15k short-lived allocations
/// at the mirror cap — and the pubkey hex is already lowercase, so it
/// never needed one at all).
struct ProfileEntry {
    profile: Profile,
    /// The pubkey's hex, once: the scans substring-match on it.
    hex: String,
    name_l: String,
    handle_l: String,
    nip05_l: String,
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
        let key = p.pubkey;
        let entry = ProfileEntry {
            hex: key.to_hex(),
            name_l: p.name.to_lowercase(),
            handle_l: p.handle.to_lowercase(),
            nip05_l: p.nip05.to_lowercase(),
            profile: p,
        };
        if !self.map.contains_key(&key)
            && self.map.len() >= PROFILES_MEM_CAP
            && let Some(oldest) = self.order.pop_front()
        {
            self.map.remove(&oldest);
        }
        if !self.map.contains_key(&key) {
            self.order.push_back(key);
        }
        self.map.insert(key, entry);
    }

    fn get(&self, pk: &PublicKey) -> Option<&Profile> {
        self.map.get(pk).map(|e| &e.profile)
    }

    fn entries(&self) -> impl Iterator<Item = &ProfileEntry> {
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
    /// The signer's reachability (D103): a watch the shell pumps into
    /// the amber bar. sign_report writes it; a success or a signer's
    /// ANSWER clears it, a double transport failure lights it.
    signer_health: Arc<tokio::sync::watch::Sender<bool>>,
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
    /// The boot gate's hand-through (D100): when this boot migrated or
    /// adopted the store (a directory swap), the swapped-in env is THE
    /// store — the heed path-cache forbids reopening the path. pool()
    /// takes this instead of Store::open, once.
    preopen: Mutex<Option<crate::store::Store>>,
    /// Own reactions: target event id (hex) → engaged.
    own_liked: Mutex<HashSet<EventId>>,
    /// Own reposts (NIP-18 kind 6/16) — same shape as own_liked.
    own_reposted: Mutex<HashSet<EventId>>,
    own_bookmarked: Mutex<HashSet<EventId>>,
    /// Synced composer drafts (D38): NIP-78 kind 30078 d="satori/drafts",
    /// the array nip44-encrypted — text + reply/quote context (ids; the
    /// full events live in the store) + the attachment imetas (v1.4; the
    /// blobs are on Blossom).
    own_drafts: Mutex<Vec<DraftEntry>>,
    /// Muted authors (pubkey hex) — NIP-51 kind:10000, content encrypted.
    muted: Mutex<HashSet<PublicKey>>,
    /// Own zaps: note id (hex) → zapped. The ledger (zaps.json) is the
    /// durable authority; this mirrors it for render, like own_liked.
    own_zapped: Mutex<HashSet<EventId>>,
    /// Session negative cache for NIP-A3 payment-target cold misses
    /// (D94's audit): authors we already wire-asked and got nothing
    /// from. "Absent" is the permanent answer for most authors — without
    /// this, every profile visit and sheet open re-fires the fan-out.
    /// Cleared never (session-scoped by design; a freshly published
    /// target list lands when the store next checks it).
    payto_asked: Mutex<HashSet<String>>,
    /// Tombstone hydration: the one-time store scan that seeds the
    /// deletions module with kind:5s that predate it (drop_deleted's
    /// doc). The module persists past this point; the flag just keeps
    /// the scan from repeating.
    tombstones_hydrated: std::sync::atomic::AtomicBool,
    /// The outbox drainer spawns once per process (D36), on the first
    /// bootstrap that has a write set; later logins are no-ops.
    outbox_started: std::sync::atomic::AtomicBool,
    /// The scheduled sweep spawns once per process (WS4), on the first
    /// bootstrap — same discipline as the outbox drainer.
    scheduled_started: std::sync::atomic::AtomicBool,
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
    /// Refresh single-flight (audit perf M4).
    refreshing: std::sync::atomic::AtomicBool,
    /// Viewport engagement ids (WS2, viewport.rs): the feed's settled
    /// visible window. The long-lived viewport task watches this channel,
    /// replacing ONE `#e` subscription as the window moves.
    viewport_ids: tokio::sync::watch::Sender<Vec<EventId>>,
    /// The viewport task spawns once per process (the outbox drainer's
    /// discipline).
    viewport_started: std::sync::atomic::AtomicBool,
    /// Bootstrap session-phase single-flight (audit perf M5).
    session_boot: tokio::sync::Mutex<()>,
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
        rt.spawn(async {
            crate::media::purge_legacy_cache();
        });
        let blocktime = Arc::new(Blocktime::new(rt.handle().clone()));
        // The viewport channel: the receiver half rides the viewport task
        // (subscribe() below); this seed receiver keeps send() honest
        // before the task arms.
        let (viewport_ids, _viewport_seed_rx) = tokio::sync::watch::channel(Vec::new());
        Self {
            rt,
            blocktime,
            active: Mutex::new(None),
            signer_health: Arc::new(tokio::sync::watch::Sender::new(true)),
            pending: Mutex::new(None),
            hold_seq: AtomicU64::new(1),
            outbox_started: std::sync::atomic::AtomicBool::new(false),
            scheduled_started: std::sync::atomic::AtomicBool::new(false),
            login_seq: AtomicU64::new(1),
            pool: Mutex::new(None),
            preopen: Mutex::new(None),
            own_liked: Mutex::new(HashSet::new()),
            own_reposted: Mutex::new(HashSet::new()),
            own_bookmarked: Mutex::new(HashSet::new()),
            own_drafts: Mutex::new(Vec::new()),
            muted: Mutex::new(HashSet::new()),
            own_zapped: Mutex::new(HashSet::new()),
            payto_asked: Mutex::new(HashSet::new()),
            tombstones_hydrated: std::sync::atomic::AtomicBool::new(false),
            wallet: Arc::new(Wallet::new()),
            profiles_mem: Mutex::new(ProfileMem::new()),
            holds: Mutex::new(std::collections::HashMap::new()),
            live: tokio::sync::broadcast::Sender::new(64),
            hold_tx: tokio::sync::broadcast::Sender::new(16),
            live_on: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            live_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            refreshing: std::sync::atomic::AtomicBool::new(false),
            viewport_ids,
            viewport_started: std::sync::atomic::AtomicBool::new(false),
            session_boot: tokio::sync::Mutex::new(()),
        }
    }

    /// Store retention (audit memory H-2): every event ever seen was kept
    /// forever - several MB/day, GB-class over a month of live saves. One
    /// The store's footprint for D99's Settings readout: (bytes on disk,
    /// event count). Bytes is the LMDB file's extent (the plateau shape:
    /// freed pages wait for the rebuild); count is one index walk, no
    /// materialized events.
    pub async fn store_stats(&self) -> std::result::Result<(u64, u64), String> {
        let active = self.active().await.map_err(|e| e.to_string())?;
        let count = active
            .pool
            .store
            .event_count()
            .await
            .map_err(|e| e.to_string())?;
        let bytes = std::fs::metadata(crate::config::events_dir().join("data.mdb"))
            .map(|m| m.len())
            .unwrap_or(0);
        Ok((bytes, count))
    }

    /// prune pass shortly after launch (bootstraps settle first), then
    /// daily. The store is a cache; the relays are the archive. Call once,
    /// from main, on the Arc'd core.
    pub fn spawn_store_prune(self: &Arc<Self>) {
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(90)).await;
                // Clone the pool out of the lock; no lock spans the awaits.
                let pool = this.pool.lock().as_ref().map(Arc::clone);
                if let Some(p) = pool {
                    // The own bookmark set survives the sweep — the
                    // Bookmarks tab renders exactly these notes (v1.2).
                    let protect: std::collections::HashSet<EventId> =
                        this.own_bookmarked.lock().clone();
                    match p.store.prune(&protect).await {
                        Ok(0) => {}
                        Ok(n) => eprintln!("[core] store prune: {n} event(s) past retention"),
                        Err(e) => eprintln!("[core] store prune failed: {e}"),
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(24 * 3600)).await;
            }
        });
    }

    /// The follows' kind:10002 relay lists age out of the store, and
    /// routed reads go stale with them (pool.rs relay_lists: "no background
    /// refresher yet — that lands with the seen-on memory slice"). This is
    /// that refresher: hourly (first pass ~90s after launch, bootstraps
    /// settle first, like the prune), every follow whose cached list is
    /// older than 7 days - or missing - gets one TARGETED indexer query for
    /// a fresh list; the SDK persists what it receives, so the routed reads
    /// warm again on their own. Chunks of 200 (the store's per-query author
    /// cap). Desktop-only divergence from the TS (which cached per pubkey
    /// in memory and never refreshed): the daemon-sized process can afford
    /// the hourly sweep, and a long-lived session SHOULD heal its routing.
    /// Call once, from main, on the Arc'd core.
    pub fn spawn_list_refresher(self: &Arc<Self>) {
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(90)).await;
                // Logged out = nothing to refresh (the watchdog's rule).
                let active = match this.active().await {
                    Ok(a) => a,
                    Err(_) => continue,
                };
                let pool = Arc::clone(&active.pool);
                let me = active.me();
                // The active account's follows (same store read the
                // bootstrap paints from).
                let follows: Vec<PublicKey> = pool
                    .store
                    .latest_by_kind(me, Kind::ContactList)
                    .await
                    .unwrap_or(None)
                    .map(|ev| ev.tags.public_keys().collect())
                    .unwrap_or_default();
                if follows.is_empty() {
                    continue;
                }
                // Stale = no cached list, or one older than the window.
                let have = pool.store.relay_lists(&follows).await.unwrap_or_default();
                let cutoff =
                    Timestamp::now() - std::time::Duration::from_secs(7 * 24 * 3600).as_secs();
                let stale: Vec<PublicKey> = follows
                    .iter()
                    .filter(|pk| {
                        have.iter()
                            .any(|ev| ev.pubkey == **pk && ev.created_at < cutoff)
                            || !have.iter().any(|ev| ev.pubkey == **pk)
                    })
                    .copied()
                    .collect();
                if stale.is_empty() {
                    continue;
                }
                eprintln!("[core] list refresher: {} stale list(s)", stale.len());
                let fallback: Vec<String> = crate::pool::INDEXERS
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
                for chunk in crate::outbox::chunk(
                    &stale.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
                    200,
                ) {
                    let pks: Vec<PublicKey> = chunk
                        .iter()
                        .filter_map(|a| PublicKey::from_str(a).ok())
                        .collect();
                    if pks.is_empty() {
                        continue;
                    }
                    let n = pks.len();
                    let filter = Filter::new().kind(Kind::RelayList).authors(pks).limit(n);
                    // The SDK persists what it receives; the store's
                    // per-author newest-list read warms by itself.
                    let got = pool.get_from(&fallback, &filter).await;
                    eprintln!("[core] list refresher: {} fresh list(s)", got.len());
                }
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            }
        });
    }

    /// Suspend watchdog (DESIGN.md §3.4): after sleep/hibernate every relay
    /// socket is dead. A wall-clock jump far larger than the monotonic one
    /// means the machine slept: force-retry every non-connected relay so
    /// reads recover in seconds, not on the SDK's timer (≤55s ping + up to
    /// 60s backoff). The old poke called client().connect() — a NO-OP for
    /// Disconnected relays, which is exactly what every relay is after
    /// sleep (the SDK's can_connect() accepts only Initialized /
    /// Terminated / Sleeping; D85 audit, verified in nostr-relay-pool's
    /// source). disconnect() lands Terminated — waking the SDK's sleeping
    /// retry loop — and connect() dials immediately; healthy relays are
    /// untouched, so a spurious fire (manual clock jump) costs nothing.
    /// One probe per 30s for the process lifetime; skipped while logged
    /// out. Call once, from main, on the Arc'd core.
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
                // 25s, not 90s (D85): the diff is per-probe-window, so a
                // 60s lid-close used to slip every window entirely — and
                // with the force-retry below, a spurious fire is free.
                if wall_d.saturating_sub(mono_d) <= std::time::Duration::from_secs(25) {
                    continue;
                }
                // The pool() getter would BUILD a pool while logged out —
                // read the raw field and skip when there's nothing to poke.
                let Some(pool) = this.pool.lock().clone() else {
                    continue;
                };
                crate::dlog!(
                    "watchdog: wall jumped {:?} vs monotonic {:?}, machine slept; reconnecting relays",
                    wall_d, mono_d
                );
                for (_url, relay) in pool.client().relays().all().await {
                    if !relay.status().is_connected() {
                        relay.disconnect();
                        relay.connect();
                    }
                }
                // (The NWC wallet needs no poke: its socket rides its own
                // nostr-sdk client with the same auto-reconnect, D85.)
            }
        });
    }

    // --- lifecycle -----------------------------------------------------------

    /// Resume a persisted session, if any. Returns the session plus the
    /// failure reason (shown on the login page — silent resume failures are
    /// how users get lost). The bunker signer is rebuilt from the transport
    /// secret without user interaction.
    /// Shared session teardown (sign_out + logout): shut the bunker
    /// transport down FIRST (zombie kind-24133 subscriptions = phantom
    /// approval prompts in the user's bunker app once the session is
    /// gone), drop the pool's NIP-42 identity (never answer AUTH as a
    /// stale account), close the NWC wallet (phantom subscriptions are the
    /// bunker bug's cousin; the LEDGER stays — history, not state), and
    /// clear the engagement/mute/profile mirrors so a different account
    /// never inherits them. sign_out used to skip all of this and bleed
    /// mirrors across accounts.
    fn teardown_session(&self) {
        let old = self.active.lock().take();
        if let Some(a) = old
            && let Signer::Bunker(connect) = &a.signer
        {
            let connect = Arc::clone(connect);
            self.rt
                .spawn(async move { Arc::unwrap_or_clone(connect).shutdown().await });
        }
        // The pool answered NIP-42 AUTH challenges as this session's
        // identity (pool.rs): drop it now, before the bunker transport
        // finishes dying and before session::clear drops the session
        // record.
        if let Some(p) = self.pool.lock().as_ref() {
            p.clear_identity();
            // Routed relays belonged to this session's route graph: sweep
            // them so the next login (any account) starts clean. Nothing
            // removed a relay before — the set grew for the process
            // lifetime (audit Nov 2026).
            let p = Arc::clone(p);
            self.rt.spawn(async move { p.clear_routed().await });
        }
        let w = Arc::clone(&self.wallet);
        self.rt.spawn(async move { w.disconnect().await });
        self.own_liked.lock().clear();
        self.own_reposted.lock().clear();
        self.own_bookmarked.lock().clear();
        self.own_drafts.lock().clear();
        self.own_zapped.lock().clear();
        self.muted.lock().clear();
        // The WoT graph + dedup exemptions are per-session identity
        // state: disarm so verdicts read Unknown until the next login.
        // The bell's receipts + one-session exemptions go with them.
        crate::wot::clear_identity();
        crate::dedup::clear();
        // The reply-velocity counter is per-session identity state too
        // (D43): a different account never inherits a mule verdict.
        crate::rate::clear();
        SUPPRESSED.lock().clear();
        NOTIF_EXEMPT.lock().clear();
        // The profile mirror is display state for the logged-in session's
        // reading; a next login re-hydrates it from its own feed pages.
        self.profiles_mem.lock().clear();
        // The amber bar (D103) is per-session truth: a session that died
        // with the signer unreachable leaves the watch false, and a
        // re-login onto a still-dead bunker would write false->false —
        // no tick, and the bar would never light for that whole session
        // (five-lens audit Nov 2026). A new session starts healthy.
        self.signer_health.send_if_modified(|v| {
            if *v {
                false
            } else {
                *v = true;
                true
            }
        });
    }

    /// Sign out: run the full session teardown (transports, pool identity,
    /// mirrors) and forget the session file + both secret-store entries —
    /// the next login rebuilds everything exactly as a fresh process
    /// would.
    pub fn sign_out(&self) {
        self.teardown_session();
        session::clear();
        secrets::delete(secrets::NSEC);
        secrets::delete(secrets::BUNKER_CLIENT_SECRET);
        secrets::delete(secrets::BUNKER_PAIRING_SECRET);
        // The tombstone store outlives sessions (it is keyed by
        // deleter, not by follows) — nothing to clear at logout.
        // The live sub is armed per session: the epoch bump makes the old
        // loop tear down at its next event, and live_on false lets the
        // next login's bootstrap arm a fresh one with the new identity.
        self.live_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.live_on
            .store(false, std::sync::atomic::Ordering::SeqCst);
        // The viewport engagement sub closes with the session too (the
        // empty set plans a Drop; the pool it rides outlives the session).
        let _ = self.viewport_ids.send(Vec::new());
    }

    pub fn init(&self) -> (Option<Session>, Option<String>) {
        let Some(session) = session::load() else {
            return (None, None); // first run — not an error
        };
        match self.rt.block_on(self.build_active(&session, None)) {
            Ok(active) => {
                *self.active.lock() = Some(active);
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
                            Some(
                                "Couldn't unlock your saved session, sign in again \
                                  below. Your keys stay with your signer."
                                    .into(),
                            ),
                        )
                    }
                    // Transient (relay down at launch, store timeout): keep
                    // the session — the next launch retries the resume.
                    _ => (
                        None,
                        Some(
                            "Couldn't reach your signer just now, Satori retries \
                              on next launch, or sign in below."
                                .into(),
                        ),
                    ),
                }
            }
        }
    }

    pub fn pubkey(&self) -> Option<PublicKey> {
        self.active
            .lock()
            .as_ref()
            .and_then(|a| PublicKey::from_str(&a.session.pubkey).ok())
    }

    pub fn logout(&self) {
        // Full shared teardown: bunker transport first (zombie kind-24133
        // subscriptions = phantom approval prompts), then pool identity,
        // wallet, mirrors.
        self.teardown_session();
        session::clear();
        secrets::delete(secrets::BUNKER_CLIENT_SECRET);
        secrets::delete(secrets::BUNKER_PAIRING_SECRET);
        secrets::delete(secrets::NSEC);
        eprintln!("[core] logout");
    }

    /// Rebuild the two-hop WoT graph from the store's cached kind-3
    /// contact lists of the follows, then compute the extended-network
    /// relay cover from the qualified set's kind-10002s (issue #2 WS1;
    /// dark-wisp ExtendedNetworkRepository's qualification + greedy
    /// `computeRelaySetCover`). In-memory projection of the LMDB store
    /// — disposable, nothing persisted.
    async fn rebuild_wot_graph(&self, active: &Active, follows: &[PublicKey]) {
        let start = std::time::Instant::now();
        // The sidecar first (D45): the applied sources the SDK's one-shot
        // fetch never persisted. The store's cached kind-3s replay after
        // and win on newer (observe's newest-wins diff).
        let persisted = crate::wot::load_disk();
        for (pk, at, list) in &persisted {
            crate::wot::apply_persisted(pk, *at, list);
        }
        let sidecar: HashMap<String, u64> = persisted
            .iter()
            .map(|(pk, at, _)| (pk.clone(), *at))
            .collect();
        let sources = active
            .pool
            .store
            .contact_lists(follows)
            .await
            .unwrap_or_default();
        for ev in &sources {
            crate::wot::observe(ev);
        }
        let qualified = crate::wot::qualified();
        let cover = if qualified.is_empty() {
            Vec::new()
        } else {
            let lists = active
                .pool
                .store
                .relay_lists(&qualified)
                .await
                .unwrap_or_default();
            let mut parsed: HashMap<String, Vec<String>> = HashMap::new();
            for ev in lists {
                let urls: Vec<String> = crate::outbox::parse_relay_list(&ev)
                    .read
                    .into_iter()
                    .filter(|u| crate::outbox::routable_relay(u))
                    .collect();
                if !urls.is_empty() {
                    parsed.insert(ev.pubkey.to_string(), urls);
                }
            }
            let authors: Vec<String> = qualified.iter().map(|p| p.to_string()).collect();
            crate::wot::relay_cover(
                &parsed,
                &authors,
                crate::wot::MAX_EXTENDED_RELAYS,
                crate::wot::MAX_AUTHORS_PER_RELAY,
            )
        };
        crate::wot::set_cover(cover);
        crate::wot::note_rebuilt();
        eprintln!(
            "[core] wot: {} source list(s), {} qualified, {} cover relay(s) ({:?})",
            sources.len(),
            crate::wot::qualified().len(),
            crate::wot::extended_relays().len(),
            start.elapsed()
        );
        self.spawn_wot_source_fetch(active, follows, &sources, &sidecar)
            .await;
    }

    /// Bootstrap source fetch (issue #2): the graph rebuilds only from
    /// kind-3s the store HAPPENS to hold — without this pass a fresh
    /// install reads "253 follows · 0 qualified" forever, because nothing
    /// ever fetched the follows' contact lists. Select the follows whose
    /// cached list is missing or older than `wot::STALE_SECS`, cap at
    /// `wot::MAX_FETCH_TARGETS` (D24: two outbox shards of 200 authors),
    /// and fire ONE routed read-tier fetch per launch — no retry loop,
    /// the next bootstrap retries what this one missed. Fire-and-forget
    /// like the other bootstrap fetches: as results land, the pool
    /// chokepoint's `wot::observe` applies every kind-3 to the graph
    /// incrementally (diff-based, newest-wins), so authors re-qualify
    /// live — verdicts are computed from `scores` on demand, no second
    /// rebuild is needed.
    async fn spawn_wot_source_fetch(
        &self,
        active: &Active,
        follows: &[PublicKey],
        cached: &[Event],
        sidecar: &HashMap<String, u64>,
    ) {
        if crate::wot::status() == crate::wot::WotStatus::OverCap {
            return; // the graph shut off; fetching sources feeds nothing
        }
        // Session guard (perf lens): bootstrap runs once per Feed surface —
        // every pane creation used to re-fire this routed fan-out for the
        // same stale set (the targets stay stale until lists land, so N
        // panes = N redundant full fan-outs). One attempt per launch; a
        // round that returns NOTHING clears the guard so the next
        // bootstrap retries what the dead pool missed.
        if WOT_SOURCE_FETCHED.swap(true, Ordering::SeqCst) {
            return;
        }
        let targets = crate::wot::fetch_targets(
            follows,
            cached,
            now_secs(),
            crate::wot::MAX_FETCH_TARGETS,
            sidecar,
        );
        if targets.is_empty() {
            return;
        }
        eprintln!(
            "[core] wot: fetching {} stale/missing contact list(s)",
            targets.len()
        );
        let pool = Arc::clone(&active.pool);
        let limit = targets.len();
        self.rt.spawn(async move {
            // The value is not read: the SDK persists what it receives,
            // and the chokepoint's wot::observe feeds the graph.
            let got = pool
                .get_routed(vec![Kind::ContactList], &targets, None, None, limit)
                .await;
            if got.is_empty() {
                WOT_SOURCE_FETCHED.store(false, Ordering::SeqCst);
            }
            eprintln!(
                "[core] wot: source fetch returned {} list(s) for {} target(s)",
                got.len(),
                limit
            );
            {
                let (src, tgt, qual) = crate::wot::stats();
                eprintln!(
                    "[core] wot: graph now: {src} source list(s) applied, {tgt} scored target(s), {qual} qualified"
                );
            }
        });
    }

    /// Follow-state lookup for profile pages (from the cached contact list).
    pub fn is_following(&self, who: &PublicKey) -> bool {
        self.active
            .lock()
            .as_ref()
            .map(|a| a.follows.iter().any(|f| f == who))
            .unwrap_or(false)
    }

    /// Trust verdict for an author (issue #2 WS1): the shared
    /// vocabulary every gate consumes — see `wot::Verdict`. UNKNOWN
    /// means the graph can't say yet, never "outside".
    pub fn verdict(&self, who: &PublicKey) -> crate::wot::Verdict {
        crate::wot::verdict(who)
    }

    /// Shared-follows count for the trust badge ("N shared follows").
    pub fn shared_follows(&self, who: &PublicKey) -> usize {
        crate::wot::shared_follows(who)
    }

    // The contacts page's reads (Following / Muted). The own sets are
    // process mirrors kept by set_following/set_muted.

    /// The own follow set (the cached contact list's p-keys). Empty
    /// signed-out.
    pub fn follows(&self) -> Vec<PublicKey> {
        self.active
            .lock()
            .as_ref()
            .map(|a| a.follows.clone())
            .unwrap_or_default()
    }

    /// Hand the mute set to the WoT graph (it keys on hex).
    pub(super) fn push_wot_mutes(&self) {
        crate::wot::set_muted(self.muted.lock().iter().map(PublicKey::to_hex).collect());
    }

    /// The own mute set (NIP-51 kind:10000, decrypted at session start).
    pub fn mutes(&self) -> Vec<PublicKey> {
        self.muted.lock().iter().copied().collect()
    }

    /// The WoT graph's honest status for the settings surface (graph size,
    /// qualified set, last rebuild, staleness state).
    pub fn wot_status(&self) -> crate::wot::WotStatus {
        crate::wot::status()
    }

    /// The bell's receipts: notifications the WoT tier suppressed this
    /// session (the "N hidden, see why" surface). Empty when the tier is
    /// off or nothing was gated — never a silent void.
    pub fn suppressed_notifications(&self) -> Vec<SuppressedNotif> {
        SUPPRESSED.lock().clone()
    }

    /// The receipts' per-item "show anyway": a ONE-SESSION exemption for
    /// this author — the tier stops gating them until sign-out (session
    /// teardown clears the set). The caller re-opens the notifications
    /// stream; the exempted items then pass the gate.
    pub fn show_suppressed_author(&self, pubkey_hex: &str) {
        NOTIF_EXEMPT.lock().insert(pubkey_hex.to_string());
    }

    /// Caught spammers with per-relay attribution (issue #2 WS2
    /// receipts; the settings surface lands with the UI phase).
    pub fn spam_report(&self) -> Vec<crate::dedup::SpamRecord> {
        crate::dedup::snapshot()
    }

    /// The SpammingUsers surface's un-hide: clear one author's spammer
    /// record so their events store again (dedup::unhide — the threshold
    /// re-arms on a fresh duplicate streak).
    pub fn unhide_spammer(&self, pubkey_hex: &str) {
        crate::dedup::unhide(pubkey_hex);
    }

    pub fn is_muted(&self, who: &PublicKey) -> bool {
        self.muted.lock().contains(who)
    }

    /// Engagement lookups for card rendering (pure set membership, render is a
    /// projection — DESIGN.md §1.2).
    pub fn is_liked(&self, event_id: &EventId) -> bool {
        self.own_liked.lock().contains(event_id)
    }

    /// The engagement sets, for a view's session snapshot (feed.rs seeds its
    /// optimistic sets from these at construction — after a restart the
    /// glyphs still read engaged).
    pub fn liked_set(&self) -> HashSet<EventId> {
        self.own_liked.lock().clone()
    }

    pub fn bookmarked_set(&self) -> HashSet<EventId> {
        self.own_bookmarked.lock().clone()
    }

    pub fn reposted_set(&self) -> HashSet<EventId> {
        self.own_reposted.lock().clone()
    }

    pub fn is_bookmarked(&self, event_id: &EventId) -> bool {
        self.own_bookmarked.lock().contains(event_id)
    }

    /// Fetch a JSON document (oEmbed previews) through the guarded pipeline.
    pub fn fetch_json(
        self: &Arc<Self>,
        url: String,
    ) -> mpsc::UnboundedReceiver<Option<serde_json::Value>> {
        self.task_once(|| Box::pin(async move { crate::media::fetch_json(&url).await.ok() }))
    }

    /// A link's og: preview metadata (the link-preview card's fields), on
    /// the core runtime for the same reason fetch_poster is: the guarded
    /// fetch and the cache IO need the tokio context. The channel is
    /// fetch_json's exact seam; the receiver is the UI's notify point.
    pub fn fetch_link_preview(
        self: &Arc<Self>,
        url: String,
    ) -> mpsc::UnboundedReceiver<
        std::result::Result<crate::media::LinkPreview, crate::media::MediaError>,
    > {
        self.task_once(move || {
            Box::pin(async move { crate::media::fetch_link_preview(&url).await })
        })
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
        self.task_once(|| Box::pin(async move { verify_nip05_inner(&nip05, &pubkey).await }))
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
        self.task_once(|| Box::pin(async move { crate::media::fetch_image(&url).await }))
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
        sniff_uri: impl FnOnce(&str) -> std::result::Result<(u32, u32, Vec<u8>), ()> + Send + 'static,
    ) -> mpsc::UnboundedReceiver<
        std::result::Result<crate::media::DecodedImage, crate::media::MediaError>,
    > {
        self.task_once(move || {
            Box::pin(async move { crate::media::fetch_poster(url, cap, sniff, sniff_uri).await })
        })
    }

    /// The GIF service search (D104), on this runtime — same seam as
    /// fetch_poster: reqwest needs the tokio IO driver, and polled from
    /// gpui's executor it panics thread 'main (the D32 crash class).
    pub fn gifs_search(
        self: &Arc<Self>,
        q: String,
    ) -> mpsc::UnboundedReceiver<std::result::Result<crate::gifs::GifPage, crate::gifs::GifError>>
    {
        self.task_once(move || Box::pin(async move { crate::gifs::search(&q, 24, 0).await }))
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
    v.get("names")?
        .get(name)?
        .as_str()
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

/// NIP-51 encrypted list content, entries kept WHOLE: `[["p", hex],
/// ["e", id, relay], ["t", tag], ["word", w], ...]`. The parse used to
/// be `[String; 2]` keeping only the value: one relay-hinted entry failed
/// the whole array (an empty list), and every other client's `t`/`word`/
/// `a`/`r` entry came back out of the RMW retagged as `p` or `e` (review
/// follow-up, Nov 27 2026). Non-string or one-element entries drop.
fn parse_list_entries(plain: &str) -> Vec<Vec<String>> {
    let arr: Vec<serde_json::Value> = serde_json::from_str(plain).unwrap_or_default();
    arr.into_iter()
        .filter_map(|v| {
            let entry: Vec<String> = v
                .as_array()?
                .iter()
                .map(|s| s.as_str().map(str::to_string))
                .collect::<Option<_>>()?;
            (entry.len() >= 2).then_some(entry)
        })
        .collect()
}

/// The values of one entry kind (`"p"` keys, `"e"` ids).
fn list_values<'a>(entries: &'a [Vec<String>], letter: &'a str) -> impl Iterator<Item = &'a str> {
    entries
        .iter()
        .filter(move |e| e[0] == letter)
        .map(|e| e[1].as_str())
}

/// The list RMW, pure: every foreign entry rides through verbatim (other
/// kinds, relay hints, order), `mirror` values the list lacks are unioned
/// in (D89: a key anything knows about is never dropped), then `target`
/// is added or removed.
fn list_rmw(
    mut entries: Vec<Vec<String>>,
    letter: &str,
    mirror: impl IntoIterator<Item = String>,
    target: &str,
    add: bool,
) -> Vec<Vec<String>> {
    let mut known: HashSet<String> = list_values(&entries, letter).map(str::to_string).collect();
    for m in mirror {
        if known.insert(m.clone()) {
            entries.push(vec![letter.to_string(), m]);
        }
    }
    if add {
        if !known.contains(target) {
            entries.push(vec![letter.to_string(), target.to_string()]);
        }
    } else {
        entries.retain(|e| !(e[0] == letter && e[1] == target));
    }
    entries
}

fn list_json(entries: &[Vec<String>]) -> String {
    serde_json::to_string(entries).unwrap_or_default()
}

/// The kind-3 tag surgery (D89), pure so it can be tested: union the
/// mirror's keys in (a key anything knows about is never dropped), then
/// apply the toggle. `None` is Amethyst's refusal rule — an unfollow
/// from a set that is empty after the union publishes nothing.
fn contact_list_tags(
    base: Vec<Tag>,
    mirror: &[PublicKey],
    who: PublicKey,
    follow: bool,
) -> Option<Vec<Tag>> {
    let mut tags = base;
    for pk in mirror.iter() {
        let h = pk.to_string();
        let present = tags
            .iter()
            .any(|t| t.kind() == "p" && t.content() == Some(h.as_str()));
        if !present {
            tags.push(Tag::public_key(*pk));
        }
    }
    let hex = who.to_string();
    if follow {
        let present = tags
            .iter()
            .any(|t| t.kind() == "p" && t.content() == Some(hex.as_str()));
        if !present {
            tags.push(Tag::public_key(who));
        }
        return Some(tags);
    }
    // Unfollow: only a set that held the key publishes its removal.
    let before = tags.len();
    tags.retain(|t| !(t.kind() == "p" && t.content() == Some(hex.as_str())));
    if tags.len() == before {
        return None;
    }
    Some(tags)
}

/// Read-modify-write prefs.json (0600): overlay `pairs` onto the existing
/// object, preserving app-owned keys. Core's single prefs write path —
/// set_undo and the NIP-78 sync both land here.
fn write_prefs_overlay(pairs: &[(String, serde_json::Value, u64)]) {
    let path = crate::config::state_file(crate::config::files::PREFS);
    let mut value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
    if let Some(obj) = value.as_object_mut() {
        for (k, v, updated) in pairs {
            // The recency stamp (`{key}_at`, written by every local
            // toggle): a NEWER local edit means the remote slot is stale
            // (its publish lost the relay LWW) — the local value stands
            // and the next local toggle republishes forward. No stamp
            // (fresh install, restore target) → the remote applies.
            let local_at = obj
                .get(format!("{k}_at").as_str())
                .and_then(|s| s.as_u64())
                .unwrap_or(0);
            if *updated <= local_at {
                continue;
            }
            obj.insert(k.clone(), v.clone());
        }
    }
    if let Ok(body) = serde_json::to_string_pretty(&value) {
        let _ = std::fs::create_dir_all(crate::config::data_dir());
        let _ = crate::sidecar::try_write_atomic(&path, body.as_bytes());
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
    let mut pairs: Vec<(String, serde_json::Value, u64)> = Vec::new();
    for slot in nip78::group_latest(&events) {
        if !nip78::synced_key(&slot.key) || slot.content.is_empty() {
            continue;
        }
        let Ok(plain) = signer.nip44_decrypt_self(me, slot.content.as_str()).await else {
            continue;
        };
        let Some(value) = nip78::parse_payload(&plain) else {
            continue;
        };
        pairs.push((slot.key, value, slot.updated));
    }
    if pairs.is_empty() {
        return;
    }
    write_prefs_overlay(&pairs);
    let _ = live.send(Live::PrefsSynced);
}

/// One synced composer draft (D38): the buffer text plus its context —
/// reply parent / quoted note as IDS (the full events live in the store).
/// The array rides NIP-78 kind 30078 d="satori/drafts", nip44-encrypted;
/// the (reply, quote) pair is the composer slot (top-level = both None).
///
/// Entries imported from the web app's NIP-37 kind 31234 wraps (v1.1)
/// carry `id` (the wrap's d) — Satori's own slot entries have none, and
/// a close-sync never clobbers an id-marked entry; opening or deleting
/// one converts it (the empty-content wrap republish, NIP-37's delete).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DraftEntry {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Uploaded attachment imetas (v1.4): the blobs are on Blossom before
    /// the draft saves (upload happens at ATTACH time), so the entry
    /// carries the NIP-92 tag arrays verbatim — a restore republishes
    /// them unchanged. No bytes ride the draft; the composer tile shows
    /// the CDN copy through the media cache.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Vec<String>>,
    /// The composer tab the draft was written in (issue #3 WS1): true =
    /// Photo tab, publishes kind 20; false = Note tab, attachments ride
    /// kind 1 + imeta. Defaults false so legacy entries (and web-app
    /// imports) restore as notes — the only thing they could ever be.
    #[serde(default, skip_serializing_if = "is_false")]
    pub photo_post: bool,
    /// Last touch (wrap created_at / save time) — the tab sorts by it,
    /// the composer's restore takes the newest top-level.
    #[serde(default)]
    pub at: u64,
}

/// serde skip predicate: omit `photo_post` when false (the legacy shape).
fn is_false(b: &bool) -> bool {
    !*b
}

impl DraftEntry {
    /// Which composer slot this draft belongs to.
    pub fn matches(&self, reply_to: Option<&str>, quote: Option<&str>) -> bool {
        self.reply_to.as_deref() == reply_to && self.quote.as_deref() == quote
    }
}

/// One NIP-51 named list (kind 30000, D38's Lists tab): the d tag IS the
/// list's identifier (its name unless a title tag rides along); the p tags
/// are the members. People lists only in v1 — e/a members are skipped at
/// parse. Read-only surface: lists are MADE in other clients today; Satori
/// shows the user's own.
#[derive(Debug, Clone, PartialEq)]
pub struct NostrList {
    pub d: String,
    pub title: Option<String>,
    pub members: Vec<PublicKey>,
    /// The source event (v1.2): a private list's members live encrypted
    /// in its content — the detail view decrypts them LAZILY (per open),
    /// the rows never wait on crypto.
    pub event: Event,
}

/// Minimal percent-decoding (%XX hex pairs) for display: list ds
/// percent-encode path separators; malformed escapes pass through.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let hex = |b: u8| (b as char).to_digit(16);
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl NostrList {
    /// The display name: a title tag beats the d identifier; percent
    /// escapes decode (Bloom's folder ds encode `/` as %2F — the raw
    /// identifier is wire format, the row shows the path).
    pub fn display(&self) -> String {
        match &self.title {
            Some(t) if !t.trim().is_empty() => t.clone(),
            _ => percent_decode(&self.d),
        }
    }

    /// Parse one kind 30000 event: the d, an optional title, the p-tag
    /// pubkeys (malformed ones skipped).
    pub fn from_event(ev: &Event) -> Option<Self> {
        let mut d = String::new();
        let mut title = None;
        let mut members = Vec::new();
        for tag in ev.tags.clone().to_vec() {
            match tag.kind() {
                "d" => d = tag.content().unwrap_or_default().to_string(),
                "title" => title = Some(tag.content().unwrap_or_default().to_string()),
                "p" => {
                    if let Some(pk) = tag.content().and_then(|c| c.parse::<PublicKey>().ok()) {
                        members.push(pk);
                    }
                }
                _ => {}
            }
        }
        // NIP-51: a named list MUST carry a d; dedupe members preserving
        // order (a list event can repeat a p tag across edits — not always
        // adjacent).
        if d.is_empty() {
            return None;
        }
        let mut seen = std::collections::HashSet::new();
        members.retain(|pk| seen.insert(pk.to_string()));
        Some(Self {
            d,
            title,
            members,
            event: ev.clone(),
        })
    }
}

/// A profile edit (D38): each field maps to a kind:0 content key. None =
/// leave the stored value; Some("") = clear the key; Some(s) = set it.
/// Ported from the TS reference's edit form (routes/profile.ts — git
/// a4b09e8), whose header states the invariant this merge keeps: "fields
/// the app doesn't show … survive a save".
#[derive(Debug, Clone, Default)]
pub struct ProfileEdit {
    pub name: Option<String>,
    pub display_name: Option<String>,
    pub about: Option<String>,
    pub picture: Option<String>,
    pub banner: Option<String>,
    pub website: Option<String>,
    pub nip05: Option<String>,
}

/// Merge a ProfileEdit into a kind:0 content string (set_profile's pure
/// half — unit-tested): unknown keys survive, Some overwrites, Some("")
/// removes, a non-object or unparseable store body starts fresh.
pub fn apply_profile_patch(stored: &str, patch: &ProfileEdit) -> String {
    let mut obj = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(stored)
        .unwrap_or_default();
    for (key, value) in [
        ("name", &patch.name),
        ("display_name", &patch.display_name),
        ("about", &patch.about),
        ("picture", &patch.picture),
        ("banner", &patch.banner),
        ("website", &patch.website),
        ("nip05", &patch.nip05),
    ] {
        match value {
            None => {}
            Some(s) if s.is_empty() => {
                obj.remove(key);
            }
            Some(s) => {
                obj.insert(key.to_string(), serde_json::Value::String(s.clone()));
            }
        }
    }
    serde_json::to_string(&obj).unwrap_or_default()
}

/// A reply/comment's parent within the requested set: the last `e` tag
/// naming one of `ids` (NIP-10 writes the parent LAST in the e-chain;
/// NIP-22's lowercase parent `e` is the only event id a top-level comment
/// carries). A parent OUTSIDE the set (a reply-to-a-reply's parent is
/// another reply) means "not this note's direct child" — counts and faces
/// alike attribute to the note the conversation hangs off, one level only.
fn parent_in_set(ev: &Event, ids: &std::collections::HashSet<EventId>) -> Option<EventId> {
    ev.tags.event_ids().last().filter(|pid| ids.contains(pid))
}

/// The comment-count tally (pure — the feed_task body above only fetches):
/// each row says how its comments hang off it. Event targets count kinds
/// 1/1111 whose parent e-tag is the row (the NIP-10 reply rule); address
/// targets count kind-1111 comments hung off the exact a-tag ADDRESS
/// (NIP-22: a parameterized picture's comments never reference the event
/// id). A comment naming BOTH shapes credits the address row (an
/// addressable parent hangs by `a`; the `e` is a convenience mark).
fn count_comment_targets(
    rows: Vec<Event>,
    targets: &[(EventId, crate::nip::comment::CommentTarget)],
) -> std::collections::HashMap<EventId, u32> {
    // The same comment can come back from two of the fetches (a
    // both-shapes comment matches the #e batch AND its #a filter):
    // dedup by id before tallying.
    let rows: Vec<Event> = rows
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let id_set: std::collections::HashSet<EventId> = targets
        .iter()
        .filter(|(_, t)| matches!(t, crate::nip::comment::CommentTarget::Event { .. }))
        .map(|(id, _)| *id)
        .collect();
    let mut addr_rows: std::collections::HashMap<String, EventId> =
        std::collections::HashMap::new();
    for (row, t) in targets {
        if let crate::nip::comment::CommentTarget::Address { kind, author, d } = t {
            addr_rows.insert(format!("{kind}:{author}:{d}"), *row);
        }
    }
    let mut per_note: std::collections::HashMap<EventId, u32> = std::collections::HashMap::new();
    for r in rows {
        let mut credited = false;
        for t in r.tags.clone().to_vec() {
            if t.as_slice().first().map(String::as_str) == Some("a")
                && let Some(v) = t.as_slice().get(1)
                && let Some(row) = addr_rows.get(v)
            {
                *per_note.entry(*row).or_default() += 1;
                credited = true;
                break;
            }
        }
        if !credited && let Some(pid) = parent_in_set(&r, &id_set) {
            *per_note.entry(pid).or_default() += 1;
        }
    }
    per_note
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

        secrets::store(
            secrets::NSEC,
            &keys.secret_key().display_secret().to_string(),
        )
        .await?;
        let session = Session {
            pubkey: pk.to_string(),
            mode: SessionMode::Local,
        };
        session::save(&session)?;

        let active = self.build_active(&session, None).await?;
        *self.active.lock() = Some(active);
        Ok(pk)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// D94 (the audit's phantom-e-tag bug): the watch's e tag comes from
    /// the REQUEST's own tag, never parsed out of the ledger key — a
    /// pubkey-hex key IS 64 hex chars and parses as an EventId, which
    /// used to filter profile-zap watches on a tag no receipt carries.
    #[test]
    fn watch_e_tag_reads_the_request_not_the_ledger_key() {
        let me = Keys::generate();
        let recipient = Keys::generate().public_key();
        let note_id =
            EventId::from_hex("0707070707070707070707070707070707070707070707070707070707070707")
                .unwrap();
        let relays = vec!["wss://relay.example".to_string()];
        let note_req = wallet::zap_request(
            me.public_key(),
            recipient,
            note_id,
            21_000,
            "zaps@example.com",
            relays.clone(),
            "",
        );
        let signed_note = me.sign_event(note_req).unwrap();
        assert_eq!(request_e_tag(&signed_note), Some(note_id));

        let profile_req = wallet::zap_request_profile(
            me.public_key(),
            recipient,
            21_000,
            "zaps@example.com",
            relays,
            "",
        );
        let signed_profile = me.sign_event(profile_req).unwrap();
        // The regression: from_hex(who.to_string()) used to be Some here.
        assert_eq!(
            request_e_tag(&signed_profile),
            None,
            "a profile zap's watch must not filter on an e tag"
        );
        // And the trap itself: the ledger key WOULD have parsed.
        assert!(EventId::from_hex(&recipient.to_string()).is_ok());
    }

    /// D94: an anonymous request's relays tag is the NEUTRAL public set,
    /// never the owner's write relays (the relay list is a fingerprint).
    #[test]
    fn anonymous_requests_carry_neutral_relays() {
        let mine = vec!["wss://my-personal-relay.example".to_string()];
        assert_eq!(
            zap_receipt_relays(wallet::ZapPrivacy::Anonymous, &mine),
            ANON_ZAP_RELAYS
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
            "the owner's relays must not ride an anonymous 9734"
        );
        assert_eq!(
            zap_receipt_relays(wallet::ZapPrivacy::Attributed, &mine),
            mine,
            "attributed zaps keep the confirmation path"
        );
    }

    /// D89: the tag surgery never loses what the relay's list event
    /// already carried. The store base's non-p tags (relay hints from
    /// NIP-02's legacy days) and its other p-tags survive both toggles,
    /// and the mirror unions in on top.
    #[test]
    fn contact_list_surgery_preserves_the_base_and_unions_the_mirror() {
        let who = Keys::generate().public_key();
        let other = Keys::generate().public_key();
        let base = vec![
            Tag::custom("relay", ["wss://old.example"]),
            Tag::public_key(other),
        ];
        let mirror = vec![Keys::generate().public_key()];

        // Follow: relay tag, both p-tags, mirror key all present.
        let added = contact_list_tags(base.clone(), &mirror, who, true).unwrap();
        assert!(added.iter().any(|t| t.kind() == "relay"));
        assert!(
            added
                .iter()
                .any(|t| t.kind() == "p" && t.content() == Some(other.to_string().as_str()))
        );
        assert!(
            added
                .iter()
                .any(|t| t.kind() == "p" && t.content() == Some(who.to_string().as_str()))
        );
        assert!(
            added
                .iter()
                .any(|t| t.kind() == "p" && t.content() == Some(mirror[0].to_string().as_str()))
        );

        // Unfollow: the toggled key goes, everything else stays.
        let removed = contact_list_tags(base, &mirror, other, false).unwrap();
        assert!(
            !removed
                .iter()
                .any(|t| t.kind() == "p" && t.content() == Some(other.to_string().as_str()))
        );
        assert!(removed.iter().any(|t| t.kind() == "relay"));
        assert!(
            removed
                .iter()
                .any(|t| t.kind() == "p" && t.content() == Some(mirror[0].to_string().as_str()))
        );
    }

    /// D89's refusal rule (Amethyst's): an unfollow when nothing anywhere
    /// holds the key publishes nothing — the early-session empty-mirror
    /// toggle must not replace the relay's list with a one-entry one.
    /// A follow with nothing known still starts a list (from scratch).
    #[test]
    fn contact_list_unfollow_from_nothing_refuses_to_publish() {
        let who = Keys::generate().public_key();
        assert!(contact_list_tags(vec![], &[], who, false).is_none());
        let started = contact_list_tags(vec![], &[], who, true).unwrap();
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].content(), Some(who.to_string().as_str()));
    }

    /// The synced draft round-trip (D38 v1.4): attachment imetas survive
    /// the serde pass verbatim, and a legacy entry (no attachments key)
    /// deserializes to an empty vec — old events stay readable.
    #[test]
    fn draft_entry_attachments_round_trip() {
        let imeta = vec![
            "imeta".to_string(),
            "url https://cdn.example.com/abc.png".to_string(),
            "m image/png".to_string(),
            "x deadbeef".to_string(),
        ];
        let entry = DraftEntry {
            text: "with a picture".into(),
            reply_to: None,
            quote: None,
            id: None,
            attachments: vec![imeta.clone()],
            photo_post: false,
            at: 42,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("attachments"), "non-empty must serialize");
        let back: DraftEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.attachments, vec![imeta]);

        // Legacy shape: the key is absent entirely.
        let legacy: DraftEntry = serde_json::from_str(r#"{"text":"old","at":7}"#).unwrap();
        assert!(legacy.attachments.is_empty());
        assert!(!legacy.photo_post, "absent = a note draft");
        assert!(legacy.matches(None, None));

        // The photo_post flag (issue #3 WS1): false is omitted (the legacy
        // shape stays byte-stable), true round-trips.
        let note_draft = DraftEntry {
            photo_post: false,
            ..back
        };
        let note_json = serde_json::to_string(&note_draft).unwrap();
        assert!(!note_json.contains("photo_post"), "false must be skipped");
        let photo_draft = DraftEntry {
            photo_post: true,
            ..note_draft
        };
        let photo_json = serde_json::to_string(&photo_draft).unwrap();
        let back: DraftEntry = serde_json::from_str(&photo_json).unwrap();
        assert!(back.photo_post, "true must round-trip");
    }

    /// A note with attachments is a NOTE (issue #3 WS1): note_tags carries
    /// the NIP-92 imeta arrays as kind-1 tags (signNote's
    /// `for (const m of imeta) tags.push(m)`), alongside the reply tags —
    /// the reply-with-attachment case keeps its thread context.
    #[test]
    fn note_tags_carry_imeta() {
        let imeta = vec![
            "imeta".to_string(),
            "url https://cdn.example.com/abc.png".to_string(),
            "m image/png".to_string(),
            "x deadbeef".to_string(),
        ];
        let me = Keys::generate().public_key();
        let (content, tags) = Core::note_tags(
            "look at this".into(),
            None,
            Vec::new(),
            None,
            std::slice::from_ref(&imeta),
            me,
        );
        // NIP-92: the URL rides the content too (D63's interop report).
        assert_eq!(content, "look at this\n\nhttps://cdn.example.com/abc.png");
        let imeta_tags: Vec<&Tag> = tags.iter().filter(|t| t.kind() == "imeta").collect();
        assert_eq!(imeta_tags.len(), 1, "one imeta tag rides the kind-1");
        let parts = imeta_tags[0].as_slice();
        assert!(parts.contains(&"url https://cdn.example.com/abc.png".to_string()));
        assert!(parts.contains(&"x deadbeef".to_string()));

        // A malformed imeta (missing the leading "imeta" marker) is
        // dropped, not injected as garbage tags.
        let bad = vec!["url https://cdn.example.com/x.png".to_string()];
        let (_, tags) = Core::note_tags("x".into(), None, Vec::new(), None, &[bad], me);
        assert!(tags.iter().all(|t| t.kind() != "imeta"));
    }

    /// The interop report (Nov 13 2026, D63's tail): a real published
    /// kind-1 note (20e4d786fc26fc76, "this is how i nostr") carried its
    /// screenshot ONLY in the imeta tag, so NIP-92-strict clients (dark
    /// wisp) rendered no image at all — the NIP allows ignoring an imeta
    /// that matches no content URL. The real event's tag array as the
    /// fixture: the URL now rides the content on its own line.
    #[test]
    fn note_content_carries_the_imeta_url() {
        let imeta = vec![
            "imeta".to_string(),
            "url https://blossom.geektoshi.tech/79bbaad937101fcad8da1fecb8c418ac4abfe83ef47f9aeb65f57b8bd92b3457.png".to_string(),
            "m image/png".to_string(),
            "dim 1920x1200".to_string(),
            "x 79bbaad937101fcad8da1fecb8c418ac4abfe83ef47f9aeb65f57b8bd92b3457".to_string(),
        ];
        let me = Keys::generate().public_key();
        let (content, tags) = Core::note_tags(
            "this is how i nostr".into(),
            None,
            Vec::new(),
            None,
            &[imeta],
            me,
        );
        assert_eq!(
            content,
            "this is how i nostr\n\nhttps://blossom.geektoshi.tech/79bbaad937101fcad8da1fecb8c418ac4abfe83ef47f9aeb65f57b8bd92b3457.png"
        );
        assert_eq!(tags.iter().filter(|t| t.kind() == "imeta").count(), 1);
    }

    /// An author who pasted the media URL themselves gets no duplicate:
    /// content-contains wins over the append.
    #[test]
    fn pasted_media_url_is_not_appended_twice() {
        let url = "https://cdn.example.com/abc.png";
        let content = Core::content_with_media_urls(
            format!("look at {url}"),
            &[vec![
                "imeta".to_string(),
                format!("url {url}"),
                "m image/png".to_string(),
            ]],
        );
        assert_eq!(content, format!("look at {url}"));
    }

    /// NIP-92 kind-0 adoption: the banner's blurhash arrives only from an
    /// The profile patch (D38): Some overwrites, Some("") clears, None
    /// keeps, unknown keys (lud16 and friends) ride along, garbage input
    /// starts fresh.
    #[test]
    fn profile_patch_merges_and_preserves() {
        let base = r#"{"name":"luna","about":"hi","lud16":"luna@zap.me"}"#;
        let patch = ProfileEdit {
            name: Some("luna2".into()),
            about: Some(String::new()), // clear
            nip05: Some("luna@example.com".into()),
            ..Default::default()
        };
        let out = apply_profile_patch(base, &patch);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["name"], "luna2");
        assert_eq!(v["nip05"], "luna@example.com");
        assert!(v.get("about").is_none(), "Some(\"\") clears the key");
        assert_eq!(v["lud16"], "luna@zap.me", "unknown keys ride along");
        // None leaves everything; garbage starts a fresh object.
        let untouched = apply_profile_patch(base, &ProfileEdit::default());
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&untouched).unwrap()["about"],
            "hi"
        );
        let fresh = apply_profile_patch("not json at all", &patch);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&fresh).unwrap()["name"],
            "luna2"
        );
    }

    /// imeta tag whose url matches the banner field exactly (Oct 2026 spec
    /// section; the TS reference predates it).
    #[test]
    fn profile_banner_blurhash_needs_an_exact_url_match() {
        use nostr::prelude::*;
        let banner = "https://b/banner.webp";
        let content = format!("{{\"name\":\"luna\",\"banner\":\"{banner}\"}}");
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
        // Core::new() rotates the diag mirror into the data dir: keep
        // that off the real $HOME (the guard isolates it, lock held).
        let _env = crate::test_env::EnvGuard::new("poster");
        let core = Arc::new(Core::new());
        let mut rx = core.fetch_poster("not a url".into(), 16, |_| Err(()), |_| Err(()));
        let verdict = std::thread::spawn(move || rx.blocking_recv())
            .join()
            .unwrap();
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
        // Login's bootstrap arms the process-global wot graph (set_identity
        // + the store's cached kind-3s): serialize against wot's own tests,
        // BEFORE the env guard (the wot serial's ordering contract).
        let _serial = crate::wot::test_serial();
        let env = crate::test_env::EnvGuard::new("login");
        let core = Arc::new(Core::new());
        core.logout(); // start clean
        // The nsec lands inside the guard's isolated dir (never a shared
        // temp path a parallel run could race).
        let secret_path = env.dir().join("test-nsec");
        let secret = "e1d4a5b33a9d2f0c8b7e6f5a4d3c2b1a0918273645f4e3d2c1b0a99887766554";
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
                assert!(
                    !msg.contains("no active session"),
                    "session lost after re-login: {msg}"
                );
                eprintln!("network failure after re-login (acceptable headless): {msg}");
            }
            Some(_) => {}
            None => panic!("notifications never resolved after re-login"),
        }
    }

    // --- notification tier + reply velocity (D41/D43) -----------------------

    use crate::test_env::EnvGuard as TierEnv;

    fn note(k: &Keys, kind: Kind, tags: Tags, content: &str, at: u64) -> Event {
        let unsigned = UnsignedEvent::new(
            k.public_key(),
            Timestamp::from_secs(at),
            kind,
            tags,
            content.to_string(),
        );
        k.sign_event(unsigned).unwrap()
    }

    /// A graph where `outsider` is known NotInNetwork (one fresh follow's
    /// list vouches for them, sub-threshold) — the rule-1 fixture.
    fn tier_session() -> (EventId, Keys, Keys, Keys) {
        crate::wot::clear_identity();
        crate::rate::clear();
        let me = Keys::generate();
        let follow = Keys::generate();
        let outsider = Keys::generate();
        crate::wot::set_identity(me.public_key(), &[follow.public_key()]); // kind3(follow → outsider): seen, sub-threshold → NotInNetwork.
        // Fresh list (now_secs): stale lists contribute nothing (D41b).
        let mut tags = Tags::new();
        tags.push(Tag::public_key(outsider.public_key()));
        crate::wot::observe(&note(
            &follow,
            Kind::ContactList,
            tags,
            "",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        ));
        let my_note = EventId::from_byte_array([7u8; 32]);
        (my_note, me, follow, outsider)
    }

    fn reply(k: &Keys, parent: EventId, at: u64) -> Event {
        let mut tags = Tags::new();
        tags.push(Tag::event(parent));
        note(k, Kind::TextNote, tags, "gm", at)
    }

    #[test]
    fn tier_gates_not_in_network_replies_and_engagement() {
        // tier_session mutates the process-global wot graph: the serial
        // goes BEFORE the env guard (wot's Session ordering contract).
        let _serial = crate::wot::test_serial();
        let _env = TierEnv::new("tier-nin");
        let (my_note, _me, _f, outsider) = tier_session();
        let my_notes: HashSet<EventId> = [my_note].into();
        let at = 1000;
        // Replies: the amendment's target — gated.
        let (kept, sup) = split_wot_tier(vec![reply(&outsider, my_note, at)], &my_notes, false);
        assert!(kept.is_empty());
        assert_eq!(sup.len(), 1);
        assert_eq!(sup[0].reason, "outside your network");
        // Reactions were already gated (D41).
        let mut tags = Tags::new();
        tags.push(Tag::event(my_note));
        let (kept, sup) = split_wot_tier(
            vec![note(&outsider, Kind::Reaction, tags, "+", at)],
            &my_notes,
            false,
        );
        assert!(kept.is_empty() && sup.len() == 1 && sup[0].reason == "outside your network");
        // Mentions and zaps stay passing — direct attention that is NOT a
        // reply to your note.
        let mut mention = Tags::new();
        mention.push(Tag::public_key(_me.public_key()));
        let (kept, sup) = split_wot_tier(
            vec![note(&outsider, Kind::TextNote, mention, "hi", at)],
            &my_notes,
            false,
        );
        assert_eq!(kept.len(), 1, "mention passes");
        assert!(sup.is_empty());
        let (kept, sup) = split_wot_tier(
            vec![note(&outsider, Kind::Custom(9735), Tags::new(), "", at)],
            &my_notes,
            false,
        );
        assert_eq!(kept.len(), 1, "zap passes");
        assert!(sup.is_empty());
        crate::wot::clear_identity();
        crate::rate::clear();
    }

    #[test]
    fn unknown_single_reply_passes() {
        let _serial = crate::wot::test_serial();
        let _env = TierEnv::new("tier-unknown");
        let (my_note, _me, _f, _outsider) = tier_session();
        let my_notes: HashSet<EventId> = [my_note].into();
        // `unseen` was observed by nobody → Unknown. One reply: passes,
        // and the delivering pass feeds the counter (1 in window).
        let unseen = Keys::generate();
        let (kept, sup) = split_wot_tier(vec![reply(&unseen, my_note, 1000)], &my_notes, true);
        assert_eq!(kept.len(), 1, "Unknown ≠ outside; one reply passes");
        assert!(sup.is_empty());
        assert!(!crate::rate::reply_flooded(&unseen.public_key(), 1000));
        crate::wot::clear_identity();
        crate::rate::clear();
    }

    #[test]
    fn velocity_flood_trips_with_receipt_reason() {
        let _serial = crate::wot::test_serial();
        let _env = TierEnv::new("tier-velocity");
        let (my_note, _me, _f, _outsider) = tier_session();
        let my_notes: HashSet<EventId> = [my_note].into();
        let mule = Keys::generate();
        let at = 1000;
        let notifs: Vec<Event> = (0..7u64).map(|i| reply(&mule, my_note, at + i)).collect();
        let (kept, sup) = split_wot_tier(notifs, &my_notes, true);
        // 4 pass untouched; the 5th is the trip and every later reply in
        // the window joins it — each with the named reason.
        assert_eq!(kept.len(), 4);
        assert_eq!(sup.len(), 3);
        let want = crate::rate::flood_reason();
        assert_eq!(want, "5 replies in 30 min");
        assert!(sup.iter().all(|s| s.reason == want));
        // The counter was fed only by the 4 DELIVERED replies — the
        // suppressed ones never joined (suppression is not self-justifying:
        // the ring still trips, it did not grow past it).
        assert!(crate::rate::reply_flooded(&mule.public_key(), at + 100));
        // Window reset: silence past the window and the author passes.
        assert!(!crate::rate::reply_flooded(
            &mule.public_key(),
            at + crate::rate::REPLY_WINDOW_SECS + 1
        ));
        crate::wot::clear_identity();
        crate::rate::clear();
    }

    #[test]
    fn velocity_clears_at_session_boundary() {
        let _serial = crate::wot::test_serial();
        let _env = TierEnv::new("tier-clear");
        let (my_note, _me, _f, _outsider) = tier_session();
        let my_notes: HashSet<EventId> = [my_note].into();
        let mule = Keys::generate();
        for i in 0..4u64 {
            split_wot_tier(vec![reply(&mule, my_note, 1000 + i)], &my_notes, true);
        }
        // teardown_session's rule: the counter is per-session identity
        // state (rate::clear(); the graph clear is asserted in wot.rs).
        crate::rate::clear();
        assert!(!crate::rate::reply_flooded(&mule.public_key(), 1000));
        crate::wot::clear_identity();
    }

    /// The bubble's counting rule (D59 fixture shape): a NIP-10 kind-1
    /// reply and a NIP-22 top-level comment both attribute to the note;
    /// a NESTED comment (parent = another comment) and a bare mention do
    /// not. The tag sets are the ones Amethyst publishes.
    #[test]
    fn comment_counts_attribute_to_the_note() {
        use crate::nip::comment::CommentTarget;
        let author = Keys::generate();
        let signer = |k: &Keys, kind: Kind, tags: Tags| {
            k.sign_event(UnsignedEvent::new(
                k.public_key(),
                Timestamp::now(),
                kind,
                tags,
                String::new(),
            ))
            .unwrap()
        };
        let note = signer(&author, Kind::TextNote, Tags::new());

        // A kind-1 reply (e names the note, NIP-10 parent-last order:
        // a mention e rides BEFORE the parent e and must not win).
        let other = Keys::generate();
        let mention = signer(
            &other,
            Kind::TextNote,
            Tags::from_list(vec![Tag::public_key(other.public_key())]),
        );
        let reply = signer(
            &other,
            Kind::TextNote,
            Tags::from_list(vec![Tag::event(mention.id), Tag::event(note.id)]),
        );

        // A top-level NIP-22 comment (parent e = the note).
        let comment = signer(
            &other,
            Kind::Custom(1111),
            Tags::from_list(
                CommentTarget::Event {
                    id: note.id,
                    kind: 1,
                    author: author.public_key(),
                }
                .root_tags(),
            ),
        );
        // A nested reply to THAT comment (root E = the note, parent e =
        // the comment): attributed to the comment, not the note.
        let nested = signer(
            &author,
            Kind::Custom(1111),
            Tags::from_list(crate::nip::comment::reply_tags(
                &comment,
                &CommentTarget::for_event(&comment),
            )),
        );

        let ids = std::collections::HashSet::from([note.id]);
        // The reply's parent is the note (last e in the set — the mention
        // e-tag BEFORE it loses).
        assert_eq!(parent_in_set(&reply, &ids), Some(note.id));
        assert_eq!(parent_in_set(&comment, &ids), Some(note.id));
        // The nested reply hangs off the comment — not the note's count.
        assert_eq!(parent_in_set(&nested, &ids), None);
        // A bare mention (p-tag only, no e) is nothing's child.
        assert_eq!(parent_in_set(&mention, &ids), None);
    }

    /// The NIP-78 login overlay must defer to a NEWER local edit: the
    /// feed's hidden-kinds list was eaten by a stale relay slot (an
    /// unpublished local curation lost the relay's last-write-wins, and
    /// every login re-applied "nothing hidden" — all kinds checked).
    /// Three timelines against a slot updated at t=1000: local stamp
    /// newer → local stands; stamp older → remote applies; no stamp
    /// (fresh install / restore) → remote applies.
    #[test]
    fn prefs_overlay_defers_to_a_newer_local_edit() {
        let env = crate::test_env::EnvGuard::new("prefs-overlay");
        let _ = env;
        let path = crate::config::state_file(crate::config::files::PREFS);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(
            &path,
            serde_json::json!({
                "content_hidden": [20, 1068],
                "content_hidden_at": 2000,
                "machine_notes_hidden": true,
                "machine_notes_hidden_at": 500
            })
            .to_string(),
        )
        .unwrap();
        write_prefs_overlay(&[
            // Stale slot: the local curation (stamp 2000) stands.
            ("content_hidden".into(), serde_json::json!([]), 1000),
            // Fresh slot: the local edit (stamp 500) is older — remote wins.
            (
                "machine_notes_hidden".into(),
                serde_json::Value::Bool(false),
                1000,
            ),
            // No local stamp: a fresh install — remote applies.
            ("dark".into(), serde_json::Value::Bool(true), 1000),
        ]);
        let got: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            got.get("content_hidden").unwrap(),
            &serde_json::json!([20, 1068]),
            "a newer local edit must not be clobbered by a stale slot"
        );
        assert_eq!(
            got.get("machine_notes_hidden").unwrap(),
            &serde_json::Value::Bool(false),
            "an older local edit loses to a fresh slot"
        );
        assert_eq!(
            got.get("dark").unwrap(),
            &serde_json::Value::Bool(true),
            "no stamp = fresh install, remote applies"
        );
    }

    /// A real key per fixture label (the mirror keys on `PublicKey`;
    /// the labels stay readable in the assertions).
    fn pk(label: &str) -> PublicKey {
        let mut b = [7u8; 32];
        b[..label.len()].copy_from_slice(label.as_bytes());
        Keys::new(SecretKey::from_slice(&b).unwrap()).public_key()
    }

    fn hx(label: &str) -> String {
        pk(label).to_hex()
    }

    fn profile(label: &str, name: &str, handle: &str, nip05: &str) -> Profile {
        Profile {
            pubkey: pk(label),
            name: name.to_string(),
            handle: handle.to_string(),
            picture: String::new(),
            about: String::new(),
            banner: String::new(),
            nip05: nip05.to_string(),
            lud: String::new(),
            banner_blurhash: None,
        }
    }

    fn entry(hex: &str, name: &str, handle: &str, nip05: &str) -> ProfileEntry {
        ProfileEntry {
            hex: hx(hex),
            name_l: name.to_lowercase(),
            handle_l: handle.to_lowercase(),
            nip05_l: nip05.to_lowercase(),
            profile: profile(hex, name, handle, nip05),
        }
    }

    #[test]
    fn list_parse_survives_relay_hints_and_odd_entries() {
        // One relay-hinted entry used to fail the whole `[String; 2]`
        // parse into an empty list.
        let plain = r#"[["e","aa","wss://r.example"],["e","bb"],["t","nostr"],["x"],[1,2]]"#;
        let entries = parse_list_entries(plain);
        assert_eq!(entries.len(), 3, "one-element and non-string entries drop");
        assert_eq!(
            list_values(&entries, "e").collect::<Vec<_>>(),
            vec!["aa", "bb"]
        );
        assert!(parse_list_entries("not json").is_empty());
    }

    #[test]
    fn list_rmw_keeps_foreign_entries_verbatim() {
        // Another client's mute list: words, hashtags, a relay-hinted key.
        let entries =
            parse_list_entries(r#"[["word","spam"],["t","nsfw"],["p","k1","wss://r.example"]]"#);
        let out = list_rmw(
            entries,
            "p",
            ["k1".to_string(), "k2".to_string()],
            "k3",
            true,
        );
        assert_eq!(
            out,
            vec![
                vec!["word".to_string(), "spam".to_string()],
                vec!["t".to_string(), "nsfw".to_string()],
                vec![
                    "p".to_string(),
                    "k1".to_string(),
                    "wss://r.example".to_string()
                ],
                vec!["p".to_string(), "k2".to_string()],
                vec!["p".to_string(), "k3".to_string()],
            ],
            "foreign kinds and hints ride through; the mirror unions in once; the target lands"
        );
        let out = list_rmw(out, "p", [], "k1", false);
        assert!(
            list_values(&out, "p").all(|v| v != "k1"),
            "remove drops the hinted entry too"
        );
        assert!(
            out.iter().any(|e| e[0] == "word"),
            "and leaves the words alone"
        );
        // A bookmarked article (`a`) never turns into an `e`.
        let out = list_rmw(
            parse_list_entries(r#"[["a","30023:pk:slug"]]"#),
            "e",
            [],
            "id1",
            true,
        );
        assert_eq!(out[0], vec!["a".to_string(), "30023:pk:slug".to_string()]);
    }

    #[test]
    fn mention_candidates_match_the_handle_not_just_the_display_name() {
        // The reported case: followed, display name carries no trace of
        // the typed handle, only the raw kind-0 `name` (or NIP-05) hits.
        let mike = entry("aaaa", "Mike P.", "mpft", "mike@example.com");
        let satoshi = entry("bbbb", "Satoshi", "sato", "");
        let cache = vec![&mike, &satoshi];
        let follows = vec![hx("aaaa")];
        let got = select_mention_candidates("mpft", &follows, "", &cache);
        assert_eq!(got.len(), 1, "the handle must match");
        assert_eq!(got[0].pubkey, pk("aaaa"));
        let got = select_mention_candidates("mike@", &follows, "", &cache);
        assert_eq!(got.len(), 1, "NIP-05 matches");
        let got = select_mention_candidates("sato", &follows, "", &cache);
        assert_eq!(got.len(), 1, "an unfollowed cached profile still matches");
    }

    #[test]
    fn mention_candidates_rank_follows_first_and_hold_still() {
        let zed = entry("cccc", "zed unfollowed", "z", "");
        let amy = entry("aaaa", "amy follow", "amy", "");
        let bob = entry("bbbb", "bob follow", "bob", "");
        let cache = vec![&zed, &amy, &bob];
        let follows = vec![hx("bbbb"), hx("aaaa")];
        let got = select_mention_candidates("o", &follows, "", &cache);
        // "o" hits bob ("bob"), amy ("amy"), zed ("unfollowed"): the two
        // follows lead, name-sorted; the outsider trails.
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].pubkey, pk("aaaa"));
        assert_eq!(got[1].pubkey, pk("bbbb"));
        assert_eq!(got[2].pubkey, pk("cccc"));
    }

    #[test]
    fn mention_candidates_bare_at_lists_follows_in_follow_order() {
        let bob = entry("bbbb", "bob", "bob", "");
        let amy = entry("aaaa", "amy", "amy", "");
        let cache = vec![&bob, &amy];
        let follows = vec![hx("aaaa"), hx("bbbb")];
        let got = select_mention_candidates("", &follows, "", &cache);
        assert_eq!(
            got.iter().map(|p| p.pubkey).collect::<Vec<_>>(),
            vec![pk("aaaa"), pk("bbbb")],
            "follow order, not cache order"
        );
    }

    #[test]
    fn mention_candidates_skip_me_and_cap_at_eight() {
        let owned: Vec<ProfileEntry> = (0..10)
            .map(|i| {
                let hex = format!("pk{i:02}");
                entry(&hex, &format!("u{i:02}"), "", "")
            })
            .collect();
        let cache: Vec<&ProfileEntry> = owned.iter().collect();
        let follows: Vec<String> = (0..10).map(|i| hx(&format!("pk{i:02}"))).collect();
        let got = select_mention_candidates("", &follows, &hx("pk03"), &cache);
        assert_eq!(got.len(), 8);
        assert!(
            got.iter().all(|p| p.pubkey != pk("pk03")),
            "you never mention yourself"
        );
    }

    /// Fixture gate (D59) for the card comment counts (#52): the rows
    /// relays answer the counts fetch with — kind-1 replies and NIP-22
    /// comments on a note (parent e-tag), kind-1111 comments on a
    /// parameterized picture (parent a-tag ADDRESS, the shape Amethyst
    /// publishes for NIP-68 media), plus the both-shapes comment and a
    /// wrong-d decoy. Signed events, real wire shapes.
    #[test]
    fn comment_count_fixtures_tally_by_target_shape() {
        use crate::nip::comment::CommentTarget;
        use crate::test_support::{keys, sign};

        let author = keys();
        let commenter = keys();

        // Row 1: a plain note; two comments (a kind-1 NIP-10 reply and a
        // kind-1111 NIP-22 comment — both hang off the event id).
        let note = sign(&author, Kind::TextNote, Vec::new(), "hello feed");
        let reply = sign(
            &commenter,
            Kind::TextNote,
            vec![Tag::event(note.id), Tag::public_key(note.pubkey)],
            "a plain reply",
        );
        let note_comment = sign(
            &commenter,
            Kind::Custom(1111),
            vec![
                Tag::custom("E", [note.id.to_string()]),
                Tag::custom("e", [note.id.to_string()]),
                Tag::custom("K", ["1"]),
                Tag::custom("k", ["1"]),
                Tag::custom("P", [note.pubkey.to_string()]),
                Tag::custom("p", [note.pubkey.to_string()]),
            ],
            "a NIP-22 comment",
        );

        // Row 2: a parameterized picture (kind 20 + d). Its comment hangs
        // off the ADDRESS — the event id appears nowhere in the refs.
        let pic = sign(
            &author,
            Kind::Custom(20),
            vec![
                Tag::custom("d", ["blossom-x"]),
                Tag::custom("m", ["image/jpeg"]),
                Tag::custom("imeta", ["url https://cdn.example.com/x.jpg m image/jpeg"]),
            ],
            "caption",
        );
        let pic_target = crate::nip::comment::CommentTarget::for_event(&pic);
        let CommentTarget::Address {
            kind: pk, d: pd, ..
        } = &pic_target
        else {
            panic!("parameterized picture targets the address");
        };
        assert_eq!((*pk, pd.as_str()), (20, "blossom-x"));
        let pic_comment = sign(
            &commenter,
            Kind::Custom(1111),
            pic_target.root_tags(),
            "love this",
        );
        // The comment's a-tag value must be the exact address string the
        // counts fetch filters on.
        let a_val = pic_comment
            .tags
            .clone()
            .to_vec()
            .iter()
            .find(|t| t.as_slice().first().map(String::as_str) == Some("a"))
            .map(|t| t.as_slice()[1].to_string())
            .unwrap();
        assert_eq!(a_val, format!("20:{}:blossom-x", pic.pubkey));

        // A comment on a DIFFERENT d (same kind and author) — the decoy
        // that proves the address match is exact, not prefix-loose.
        let decoy = sign(
            &commenter,
            Kind::Custom(1111),
            crate::nip::comment::CommentTarget::Address {
                kind: 20,
                author: pic.pubkey,
                d: "blossom-y".into(),
            }
            .root_tags(),
            "on the other picture",
        );

        // A comment naming BOTH shapes (the a-tag address AND a
        // convenience e-tag of the picture event): credits the address
        // row once.
        let both = sign(
            &commenter,
            Kind::Custom(1111),
            vec![
                Tag::custom("A", [format!("20:{}:blossom-x", pic.pubkey)]),
                Tag::custom("a", [format!("20:{}:blossom-x", pic.pubkey)]),
                Tag::custom("e", [pic.id.to_string()]),
                Tag::custom("p", [pic.pubkey.to_string()]),
            ],
            "both refs",
        );

        let targets = vec![
            (note.id, CommentTarget::for_event(&note)),
            (pic.id, pic_target),
        ];
        let got = count_comment_targets(
            vec![reply, note_comment, pic_comment, decoy, both],
            &targets,
        );
        assert_eq!(got.get(&note.id), Some(&2), "note: reply + NIP-22 comment");
        assert_eq!(
            got.get(&pic.id),
            Some(&2),
            "picture: address comment + both-shapes, decoy excluded"
        );
        assert_eq!(got.len(), 2, "no stray rows counted");
    }

    /// The notifications hole (user report: "after a while I quit getting
    /// new note notifications"): a relay answering CLOSED for a live sub
    /// makes the SDK REMOVE it (no re-send on reconnect), so the bell and
    /// the new-notes pill starve silently for the rest of the session.
    /// run_live must re-arm the subs when that happens — here: green
    /// delivery, CLOSED, server kick + reconnect, then delivery AGAIN.
    #[tokio::test]
    async fn live_rearms_after_a_relay_closed_reply() {
        use std::time::Duration;
        let (_guard, mock, pool, _url) = crate::test_relay::pool_with_mock("core-live-rearm").await;
        let alice = Keys::generate();

        // Seed alice's NIP-65 so subscribe_routed routes the fan-out to
        // the mock (an empty store would fall back to the real indexers).
        let list = EventBuilder::new(Kind::RelayList, "")
            .tag(Tag::custom("r", [mock.url().as_str()]))
            .finalize(&alice)
            .unwrap();
        mock.publish(&list);
        let seeded = pool
            .client()
            .fetch_events(
                Filter::new()
                    .kind(Kind::RelayList)
                    .author(alice.public_key()),
            )
            .timeout(Duration::from_secs(5))
            .await
            .unwrap_or_default();
        assert!(!seeded.is_empty(), "relay-list seed did not cache");

        // Run the REAL loop: me = alice, follows = alice.
        let live = tokio::sync::broadcast::Sender::new(64);
        let live_on = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        tokio::spawn(Core::run_live(
            pool.into(),
            vec![Kind::TextNote],
            vec![alice.public_key()],
            alice.public_key(),
            live.clone(),
            live_on.clone(),
            epoch.clone(),
            0,
        ));
        let mut sigs = live.subscribe();

        // Both REQs up (the bell filters + the routed fan-out).
        mock.wait_open_subs(2).await;

        // GREEN: a fresh note pings the bell.
        let n1 = EventBuilder::new(Kind::TextNote, "before the closed")
            .finalize(&alice)
            .unwrap();
        mock.publish(&n1);
        let got = tokio::time::timeout(Duration::from_secs(3), sigs.recv()).await;
        assert!(
            matches!(got, Ok(Ok(_))),
            "live bell dead BEFORE the closed reply — the harness is wrong"
        );

        // The relay closes the routed sub (in band), then drops the
        // socket entirely (a restart / idle cull). The SDK reconnects.
        mock.closed_sub(1);
        mock.kick();
        mock.wait_conns_seen(2).await;
        tokio::time::sleep(Duration::from_millis(500)).await;

        // RED before the fix / GREEN after: a fresh note pings the bell
        // again (the CLOSED sub was re-armed).
        let n2 = EventBuilder::new(Kind::TextNote, "after the closed")
            .finalize(&alice)
            .unwrap();
        mock.publish(&n2);
        let got = tokio::time::timeout(Duration::from_secs(8), sigs.recv()).await;
        assert!(
            matches!(got, Ok(Ok(_))),
            "bell never recovered from the CLOSED reply — notifications stay silent for the session"
        );
    }

    /// The notifications page verifies its rows (the @geek report:
    /// "messages that say they mentioned me that have nothing to do
    /// with me"). The mention query asks relays for #p=me — but the
    /// answers are trusted, so a relay that mis-serves the filter (its
    /// firehose under our subscription id) painted strangers' notes as
    /// "mentioned you". The union must verify each row is actually
    /// about me: p/a tags carrying MY key, or e/q tags referencing one
    /// of my recent notes. The mock relay ignores p tags entirely (it
    /// matches kind+author only) — exactly the misbehaving shape.
    #[test]
    fn notifications_page_drops_junk_mentions() {
        let _serial = crate::wot::test_serial();
        let env = crate::test_env::EnvGuard::new("notif-junk");
        let core = Arc::new(Core::new());
        core.logout();
        let secret_path = env.dir().join("test-nsec");
        let secret = "e1d4a5b33a9d2f0c8b7e6f5a4d3c2b1a0918273645f4e3d2c1b0a99887766554";
        let _ = std::fs::write(&secret_path, secret);
        core.rt
            .block_on(core.login_nsec_inner(secret))
            .expect("login");
        let me = core.pubkey().expect("pubkey after login");

        let junk = core.rt.block_on(async {
            // Wire the misbehaving mock into the SESSION's pool, and
            // load it with junk + a control BEFORE the queries run (the
            // mock replays its store on REQ).
            let (mock, url) = {
                let active = core.active().await.expect("session");
                let mock = crate::test_relay::MockRelay::spawn().await;
                let url = mock.url();
                crate::net_guard::allow_direct_url(&url);
                active
                    .pool
                    .connect_reads(vec![url.clone()])
                    .await
                    .expect("mock joins");
                (mock, url)
            };
            let geek = Keys::generate();
            let speaker = Keys::generate();
            // JUNK: a note mentioning @geek (p = geek, no e/q) —
            // nothing to do with me, exactly the report.
            let junk = EventBuilder::new(Kind::TextNote, "hey @geek nice post")
                .tag(Tag::custom("p", [geek.public_key().to_string()]))
                .finalize(&geek)
                .unwrap();
            mock.publish(&junk);
            // CONTROL: a true mention (p = me, no e/q) from another
            // author.
            let real = EventBuilder::new(Kind::TextNote, "hi @you")
                .tag(Tag::custom("p", [me.to_string()]))
                .finalize(&speaker)
                .unwrap();
            mock.publish(&real);

            let mut rx = core.notifications();
            // The task emits the cached paint, then the relay phase;
            // the queries are READ_BUDGET(4s)-bounded per stream. Take
            // the LAST Notifications emit within a generous window.
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
            let mut notifs = Vec::new();
            loop {
                let got = tokio::time::timeout_at(deadline, rx.recv()).await;
                match got {
                    Ok(Some(FeedEvent::Notifications(list, _))) => notifs = list,
                    Ok(Some(_)) => continue,
                    Ok(None) | Err(_) => break,
                }
                if tokio::time::Instant::now() >= deadline {
                    break;
                }
            }
            crate::net_guard::disallow_direct_url(&url);
            (junk.id, real.id, notifs)
        });
        let (junk_id, real_id, notifs) = junk;

        assert!(
            notifs.iter().any(|e| e.id == real_id),
            "the TRUE mention (p=me) never reached the page — the pipeline is broken"
        );
        assert!(
            !notifs.iter().any(|e| e.id == junk_id),
            "a stranger's note (p=geek, nothing to do with me) painted as a mention — \
             the page trusted a relay's answer to the #p=me filter"
        );
    }

    /// Denial-shaped CLOSEDs split at the auth wall (the purplepag.es
    /// case): `auth-required:` is the NIP-42 dance — the SDK answers it
    /// with the session identity and resubscribes on success, so the
    /// loop must NOT stamp read-death (the wall relay is healthy and
    /// feeds profile fetches). Any other refusal prefix IS the read-dead
    /// signal (12h stamp, routing scores it 0).
    #[tokio::test]
    async fn closed_denials_split_at_the_auth_wall() {
        use std::time::Duration;

        let (_guard, mock, pool, url) = crate::test_relay::pool_with_mock("core-auth-wall").await;
        let alice = Keys::generate();

        // Run the REAL loop; no follows (one sub only).
        let live = tokio::sync::broadcast::Sender::new(64);
        let live_on = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        tokio::spawn(Core::run_live(
            pool.into(),
            vec![Kind::TextNote],
            vec![],
            alice.public_key(),
            live.clone(),
            live_on.clone(),
            epoch.clone(),
            0,
        ));
        mock.wait_open_subs(1).await;

        // The relay walls the sub. The SDK marks the sub closed (an
        // authenticator is always attached) and waits for a successful
        // auth to resubscribe; our loop logs the challenge, no stamp.
        mock.closed_sub_reason(0, "auth-required: sign in to read");
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(
            !crate::relay_latency::is_read_dead(&url),
            "an auth wall stamped read-dead — profile fetches lose a healthy relay for 12h"
        );

        // A limit rejection is ALSO not futility (the purplepag.es
        // case): our filter's cap exceeds the relay's max; the relay
        // serves the query capped, and the other relays carry the
        // fetch. No stamp.
        mock.closed_sub_reason(0, "blocked: limit too high: 1000 (max 500)");
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(
            !crate::relay_latency::is_read_dead(&url),
            "a limit rejection stamped read-dead — a parameter mismatch starved a healthy relay"
        );

        // A real refusal IS the read-dead signal.
        mock.closed_sub_reason(0, "restricted: reads require a paid plan");
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(
            crate::relay_latency::is_read_dead(&url),
            "a real refusal was not stamped read-dead — the routing waste guard is broken"
        );
    }
}
