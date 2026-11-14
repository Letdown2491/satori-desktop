//! NIP-47 Wallet Connect — the pay half of NIP-57 zaps (D20).
//!
//! TODO(port): feather AGENTS.md — "NWC zaps". The rules that port as law:
//!
//! - **The wallet uses its separate connection secret.** It is a revocable
//!   wallet credential, not identity: the secret rides `crate::secrets`
//!   (keyring-first, 0600-file fallback) and never touches `prefs.json` or
//!   any other state file (wallet.json holds only pubkey + relays, so a
//!   restart can rebuild the client). D17 is untouched — no master key.
//! - **Never automatically retry an uncertain payment.** A timeout or an
//!   unknown NIP-47 response is a terminal `Uncertain` state; only an
//!   explicit user action retries (the manager reuses the STORED request —
//!   same event ID, no duplicate zaps).
//! - **Store the signed zap request before paying** (the ledger below),
//!   so a crash between sign and pay can't double-zap.
//! - One in-flight payment per note (the manager enforces the set).
//!
//! The manager (`manager.rs`) orchestrates sign→store→pay; this module owns
//! the NWC client, the LNURL-pay resolution, the pay primitive, and the
//! zap ledger. All I/O is async on the core's tokio runtime; results cross
//! to the UI over the broadcast channel (the channel-pump seam, §3.2).

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nostr::prelude::*;
use nwc::prelude::*;

use crate::config;
use crate::net_guard;
use crate::secrets;

/// How long a pay_invoice may run before it becomes Uncertain. Lightning
/// routing can legitimately take tens of seconds; below ~15s healthy
/// payments would time out into Uncertain on a busy day.
const PAY_TIMEOUT: Duration = Duration::from_secs(30);
/// Connection probe deadline (get_info/get_balance on connect).
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
/// The ledger keeps the last N zaps (amount-picker seeds + retry window).
const LEDGER_CAP: usize = 200;

// --- status & events ------------------------------------------------------

/// Wallet state. `Ready` carries what the wallet answered about itself.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Status {
    #[default]
    Disconnected,
    Connecting,
    Ready(WalletInfo),
}

/// What `get_info`/`get_balance` answered (all optional — wallets vary).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WalletInfo {
    pub alias: String,
    pub balance_msat: Option<u64>,
}

/// Streamed to the UI (settings pane, feed bolt glyphs).
#[derive(Debug, Clone)]
pub enum WalletEvent {
    Status(Status),
    /// The URI failed to connect (status is back to Disconnected).
    ConnectFailed(String),
    /// The NWC wallet confirmed the payment: bolt settles accent.
    ZapPaid {
        note: String,
        req: String,
        msat: u64,
    },
    /// Terminal uncertainty: the payment MAY have gone through (timeout or
    /// unknown response). Never auto-retried; surfaced quietly.
    ZapUncertain {
        note: String,
        req: String,
        msat: u64,
    },
    /// Honest quiet failure (NIP-47 code or LNURL error) — the payment
    /// definitely did not happen; retrying is safe.
    ZapFailed {
        note: String,
        reason: String,
    },
    /// A verified receipt landed on the relays for a zap we paid — the
    /// settled state reconciles (also recovers an old Uncertain entry).
    ZapReceipt {
        note: String,
        req: String,
    },
    /// Paste-time relay screening refused a private-network relay in the
    /// pasted URI (issue #11): the full verdicts. The dialog derives the
    /// banner line and the allow affordance from them (the consent write
    /// is net_guard::allow_direct_relay). Nothing was stored or dialed.
    RelayScreened {
        verdicts: Vec<net_guard::RelayVerdict>,
    },
}

pub type WalletRx = tokio::sync::broadcast::Receiver<WalletEvent>;

// --- pay errors -----------------------------------------------------------

/// Classified pay failure. The distinction carries the Feather rule:
/// `Code` = the wallet SPOKE and did not pay (retrying is safe);
/// `Uncertain` = we don't KNOW (never auto-retry).
#[derive(Debug, Clone, PartialEq)]
pub enum PayError {
    /// NIP-47 error code from the wallet (message is UI-ready).
    Code(String),
    /// Timeout / unknown response / transport failure.
    Uncertain,
    /// No wallet is connected (the UI's dim-bolt path is the first line).
    NoWallet,
}

impl std::fmt::Display for PayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Code(s) => write!(f, "{s}"),
            Self::Uncertain => write!(
                f,
                "payment state unknown, check your wallet before retrying"
            ),
            Self::NoWallet => write!(f, "no wallet connected"),
        }
    }
}

/// Classify an `nwc` error string. The NIP-47 codes arrive folded into the
/// message (rust-nostr renders them `"message [Debug(code)]"`), so the
/// classification matches on the code names; anything that is not a
/// definite wallet answer is Uncertain — the conservative side of the rule.
fn classify_pay_error(raw: &str) -> PayError {
    let lower = raw.to_lowercase();
    if lower.contains("insufficientbalance") {
        return PayError::Code("not enough balance in the wallet".into());
    }
    if lower.contains("ratelimited") {
        return PayError::Code("wallet is rate-limiting, try again later".into());
    }
    if lower.contains("quotaexceeded") {
        return PayError::Code("wallet spending quota exceeded".into());
    }
    if lower.contains("paymentfailed") {
        return PayError::Code("the payment failed (routing)".into());
    }
    if lower.contains("restricted") || lower.contains("unauthorized") {
        return PayError::Code("the wallet restricts this connection".into());
    }
    if lower.contains("notimplemented") {
        return PayError::Code("the wallet doesn't support pay_invoice".into());
    }
    if lower.contains("timeout") || lower.contains("timed out") {
        return PayError::Uncertain;
    }
    // A definite NIP-47 answer we don't recognize still says "no payment":
    // only UNKNOWN outcomes are Uncertain. Transport noise (relay gone,
    // dropped connection) is unknown — the request may have reached the
    // wallet. Codes arrive as "[CodeName]"; a bare transport error doesn't.
    if raw.contains('[') && raw.ends_with(']') {
        return PayError::Code(raw.trim_end_matches(']').to_string());
    }
    PayError::Uncertain
}

// --- zap ledger (zaps.json) ----------------------------------------------

/// Payment state of one stored zap request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ZapState {
    /// Signed + stored, pay in flight or interrupted — an explicit retry
    /// reuses this record's event ID.
    Pending,
    /// The NWC wallet confirmed the payment.
    Paid,
    /// Terminal uncertainty (the Feather rule) — user decides.
    Uncertain,
    /// A verified receipt landed (the strongest state).
    Confirmed,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ZapRecord {
    /// The signed kind-9734 request's event id (hex) — the retry key.
    pub req: String,
    /// The zapped note's event id (hex).
    pub note: String,
    pub recipient: String,
    pub msat: u64,
    pub state: ZapState,
    pub created_at: u64,
    /// The FULL signed 9734 (JSON) — a retry fetches a fresh invoice for
    /// the SAME request; re-signing would mint a new event id and risk a
    /// duplicate zap event. zaps.json is 0600; the request is public data.
    pub event: String,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct Ledger {
    zaps: Vec<ZapRecord>,
}

fn load_ledger() -> Ledger {
    std::fs::read_to_string(config::state_file(config::files::ZAPS))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_ledger(l: &Ledger) {
    if let Ok(body) = serde_json::to_string(l) {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let _ = std::fs::create_dir_all(config::data_dir());
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(config::state_file(config::files::ZAPS))
            .and_then(|mut f| f.write_all(body.as_bytes()));
    }
}

/// Append-or-update by request id (newest last, capped).
pub fn ledger_update(record: ZapRecord) {
    let mut l = load_ledger();
    if let Some(slot) = l.zaps.iter_mut().find(|z| z.req == record.req) {
        *slot = record;
    } else {
        l.zaps.push(record);
    }
    if l.zaps.len() > LEDGER_CAP {
        l.zaps.drain(0..l.zaps.len() - LEDGER_CAP);
    }
    save_ledger(&l);
}

/// All stored records for a note, newest first (the retry path). A split
/// zap (NIP-57 Appendix G) stores one record per receiver — same note,
/// distinct request ids.
pub fn ledger_for_note(note: &str) -> Vec<ZapRecord> {
    let mut out: Vec<ZapRecord> = load_ledger()
        .zaps
        .into_iter()
        .filter(|z| z.note == note)
        .collect();
    out.reverse();
    out
}

/// Remove a record (a definite failure paid nothing — no history to keep).
pub fn ledger_remove(req: &str) {
    let mut l = load_ledger();
    l.zaps.retain(|z| z.req != req);
    save_ledger(&l);
}

/// Transition one record's state (by request id).
pub fn ledger_set_state(req: &str, state: ZapState) {
    let mut l = load_ledger();
    if let Some(z) = l.zaps.iter_mut().find(|z| z.req == req) {
        z.state = state;
        save_ledger(&l);
    }
}

/// Recent zap history, newest first (amount-picker seeds).
pub fn ledger_history() -> Vec<ZapRecord> {
    let mut zaps = load_ledger().zaps;
    zaps.reverse();
    zaps
}

// --- wallet.json (secret-free connection record) --------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Conn {
    /// Wallet service pubkey (hex).
    pub wallet: String,
    /// Relay(s) the wallet service listens on.
    pub relays: Vec<String>,
    /// Relay hosts the user explicitly allowed onto non-public space at
    /// paste time (the issue-#11 consent). Re-seeds the net_guard ledger
    /// on resume, so an approved LAN wallet still connects after a
    /// restart. Old wallet.json files load unchanged (serde default).
    #[serde(default)]
    pub direct: Vec<String>,
}

fn save_conn(c: &Conn) {
    if let Ok(body) = serde_json::to_string(c) {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let _ = std::fs::create_dir_all(config::data_dir());
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(config::state_file(config::files::WALLET))
            .and_then(|mut f| f.write_all(body.as_bytes()));
    }
}

fn load_conn() -> Option<Conn> {
    serde_json::from_str(&std::fs::read_to_string(config::state_file(config::files::WALLET)).ok()?)
        .ok()
}

fn clear_conn() {
    let _ = std::fs::remove_file(config::state_file(config::files::WALLET));
}

// --- LNURL-pay (NIP-57's fetch path) --------------------------------------

/// Resolve a profile's lightning field to its LNURL-pay endpoint URL.
/// Accepts `lud16` (`user@domain`) and `lud06` (bech32 `lnurl`).
/// Pure string logic — the HTTP calls happen in [`fetch_lnurlp`]/[`fetch_zap_invoice`].
pub fn lnurlp_url(lud: &str) -> Result<String, String> {
    let lud = lud.trim();
    if let Some((user, domain)) = lud.split_once('@') {
        if user.is_empty()
            || domain.is_empty()
            || user.chars().any(|c| c.is_whitespace() || c == '/')
            || domain.contains('/')
            || domain.contains('@')
        {
            return Err("not a lightning address".into());
        }
        return Ok(format!("https://{domain}/.well-known/lnurlp/{user}"));
    }
    if lud.to_lowercase().starts_with("lnurl") {
        let (hrp, data) = bech32::decode(lud).map_err(|e| format!("bad lnurl: {e}"))?;
        if hrp.as_str().to_lowercase() != "lnurl" {
            return Err("not an lnurl".into());
        }
        return String::from_utf8(data).map_err(|_| "bad lnurl payload".into());
    }
    Err("no lightning address on the profile".into())
}

/// bech32-encode a lnurl-pay URL as an `lnurl` (for the 9734 `lnurl` tag —
/// NIP-57 spells it bech32; the lud06 form arrives pre-encoded).
pub fn lnurl_encode(url: &str) -> String {
    let hrp = bech32::Hrp::parse("lnurl").expect("valid hrp");
    bech32::encode::<bech32::Bech32>(hrp, url.as_bytes()).unwrap_or_else(|_| url.to_string())
}

/// GET a LNURL-pay endpoint (SSRF-guarded, uncached) → its callback URL.
pub async fn fetch_lnurlp(url: &str) -> Result<String, String> {
    let json = crate::media::fetch_json_uncached(url).await?;
    let cb = json
        .get("callback")
        .and_then(|v| v.as_str())
        .ok_or("lnurl-pay response has no callback")?
        .to_string();
    if crate::media::is_safe_media_url(&cb) {
        Ok(cb)
    } else {
        Err("lnurl-pay callback is not a safe URL".into())
    }
}

/// Ask the callback for a bolt11 invoice for `msat`, carrying the signed
/// zap request (a public zap). Returns the invoice string.
pub async fn fetch_zap_invoice(
    callback: &str,
    msat: u64,
    zap_req: &Event,
) -> Result<String, String> {
    let req_json = serde_json::to_string(zap_req).map_err(|e| e.to_string())?;
    let mut url = url::Url::parse(callback).map_err(|e| e.to_string())?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("amount", &msat.to_string());
        q.append_pair("nostr", &req_json);
    }
    let json = crate::media::fetch_json_uncached(url.as_str()).await?;
    if json.get("status").and_then(|v| v.as_str()) == Some("ERROR") {
        let reason = json
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown lnurl error");
        return Err(format!("lightning address said: {reason}"));
    }
    let pr = json
        .get("pr")
        .and_then(|v| v.as_str())
        .ok_or("lnurl callback returned no invoice")?
        .to_string();
    if !pr.to_lowercase().starts_with("lnbc") {
        return Err("lnurl callback returned no bolt11 invoice".into());
    }
    // Amount check: the server mints to OUR number (LNURL-pay). A mismatch
    // would pay someone else's price — refuse; zero-amount invoices (no HRP
    // amount to parse) pass through to the wallet's own display.
    let asked = crate::notifications::msats_from_bolt11(&pr);
    if asked != 0 && asked != msat {
        return Err("invoice amount doesn't match the zap".into());
    }
    Ok(pr)
}

// --- the wallet handle ----------------------------------------------------

pub struct Wallet {
    events: tokio::sync::broadcast::Sender<WalletEvent>,
    status: Mutex<Status>,
    client: Mutex<Option<Arc<NostrWalletConnect>>>,
    /// Note ids with a payment in flight (one per note — the Feather rule).
    in_flight: Mutex<HashSet<String>>,
}

impl Default for Wallet {
    fn default() -> Self {
        Self::new()
    }
}

impl Wallet {
    pub fn new() -> Self {
        Self {
            events: tokio::sync::broadcast::Sender::new(64),
            status: Mutex::new(Status::Disconnected),
            client: Mutex::new(None),
            in_flight: Mutex::new(HashSet::new()),
        }
    }

    pub fn subscribe(&self) -> WalletRx {
        self.events.subscribe()
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    pub fn is_ready(&self) -> bool {
        matches!(*self.status.lock().unwrap(), Status::Ready(_))
    }

    fn set_status(&self, s: Status) {
        *self.status.lock().unwrap() = s.clone();
        let _ = self.events.send(WalletEvent::Status(s));
    }

    /// Connect to a `nostr+walletconnect://` URI: parse, screen the URI's
    /// relays through net_guard (issue #11 — this transport sits outside
    /// the pool's screening), seal the secret in the secret store, persist
    /// the secret-free record, probe the wallet. Status events stream on
    /// the channel (Connecting → Ready | failed).
    pub async fn connect(&self, uri: String) {
        let parsed = match NostrWalletConnectUri::parse(uri.trim()) {
            Ok(u) => u,
            Err(e) => {
                let _ = self.events.send(WalletEvent::ConnectFailed(format!(
                    "not a valid nostr+walletconnect:// URI: {e}"
                )));
                return;
            }
        };
        // Paste-time relay screen: the NWC client dials these relays
        // directly (no pool screening), so private-space answers are
        // refused BEFORE anything opens — the dialog offers the consent
        // toggle instead. Nothing is stored on a refusal.
        let relay_urls: Vec<String> = parsed.relays.iter().map(|r| r.to_string()).collect();
        let verdicts = net_guard::screen_uri_relays(relay_urls.iter().map(String::as_str)).await;
        let refused = net_guard::refused_verdicts(&verdicts);
        if !refused.is_empty() {
            let _ = self
                .events
                .send(WalletEvent::RelayScreened { verdicts: refused });
            return;
        }
        let conn = Conn {
            wallet: parsed.public_key.to_string(),
            relays: relay_urls,
            // The consent ledger members among this URI's relays —
            // persisted so resume re-seeds the ledger and keeps an
            // approved LAN wallet working across restarts.
            direct: verdicts
                .iter()
                .filter(|v| v.screen == net_guard::RelayScreen::Exempted)
                .map(|v| v.host.clone())
                .collect(),
        };
        self.set_status(Status::Connecting);
        if let Err(e) = secrets::store(secrets::NWC_SECRET, &parsed.secret.to_secret_hex()).await {
            let _ = self
                .events
                .send(WalletEvent::ConnectFailed(format!("secret store: {e}")));
            self.set_status(Status::Disconnected);
            return;
        }
        save_conn(&conn);

        let client = Arc::new(
            NostrWalletConnect::builder(parsed)
                .timeout(PAY_TIMEOUT)
                .build(),
        );
        match probe(&client).await {
            Ok(info) => {
                // A live client (wallet B pasted while A is Ready, or a
                // double connect) must be shut down, not just dropped:
                // drop does not stop its relay sockets or pumps.
                let old = self.client.lock().unwrap().replace(client);
                if let Some(c) = old {
                    c.shutdown().await;
                }
                self.set_status(Status::Ready(info));
            }
            Err(e) => {
                client.shutdown().await;
                clear_conn();
                secrets::delete(secrets::NWC_SECRET);
                let _ = self.events.send(WalletEvent::ConnectFailed(format!(
                    "wallet didn't answer: {e}"
                )));
                self.set_status(Status::Disconnected);
            }
        }
    }

    /// Rebuild a persisted connection without user interaction (restart
    /// resume): wallet.json + the secret from the store. Best-effort probe.
    pub async fn resume(&self) {
        if self.client.lock().unwrap().is_some() {
            return;
        }
        let Some(conn) = load_conn() else {
            return;
        };
        // Re-seed the consent ledger from the persisted record, then
        // re-screen: the paste-time approval covers the HOST, not a
        // snapshot of its addresses — a name that now resolves into
        // private space fails the resume loudly, with the same affordance
        // the connect path offers (issue #11; review Tier 2.4).
        net_guard::allow_direct_relays(conn.direct.iter().map(String::as_str));
        let verdicts = net_guard::screen_uri_relays(conn.relays.iter().map(String::as_str)).await;
        let refused = net_guard::refused_verdicts(&verdicts);
        if !refused.is_empty() {
            let _ = self
                .events
                .send(WalletEvent::RelayScreened { verdicts: refused });
            self.set_status(Status::Disconnected);
            return;
        }
        let Some(secret_hex) = secrets::load(secrets::NWC_SECRET).await.ok().flatten() else {
            clear_conn(); // secret gone — the connection is unrecoverable
            return;
        };
        let Ok(secret) = SecretKey::from_str(&secret_hex) else {
            clear_conn();
            return;
        };
        let Ok(wallet) = PublicKey::from_str(&conn.wallet) else {
            clear_conn();
            return;
        };
        let relays = conn
            .relays
            .iter()
            .filter_map(|r| RelayUrl::parse(r).ok())
            .collect();
        let uri = NostrWalletConnectUri::new(wallet, relays, secret, None);
        self.set_status(Status::Connecting);
        let client = Arc::new(
            NostrWalletConnect::builder(uri)
                .timeout(PAY_TIMEOUT)
                .build(),
        );
        match probe(&client).await {
            Ok(info) => {
                // A live client (wallet B pasted while A is Ready, or a
                // double connect) must be shut down, not just dropped:
                // drop does not stop its relay sockets or pumps.
                let old = self.client.lock().unwrap().replace(client);
                if let Some(c) = old {
                    c.shutdown().await;
                }
                self.set_status(Status::Ready(info));
            }
            Err(e) => {
                client.shutdown().await;
                eprintln!(
                    "[core] wallet resume failed: {}",
                    crate::pool::clean_reason(&e.to_string())
                );
                self.set_status(Status::Disconnected);
            }
        }
    }

    /// Disconnect and forget: shut the client down, wipe both stores.
    pub async fn disconnect(&self) {
        let old = self.client.lock().unwrap().take();
        if let Some(c) = old {
            c.shutdown().await;
        }
        clear_conn();
        secrets::delete(secrets::NWC_SECRET);
        self.in_flight.lock().unwrap().clear();
        self.set_status(Status::Disconnected);
    }

    /// Pay one bolt11 invoice. Ok = the wallet confirmed (preimage back).
    pub async fn pay(&self, bolt11: &str) -> Result<String, PayError> {
        let Some(client) = self.client.lock().unwrap().clone() else {
            return Err(PayError::NoWallet);
        };
        let req = PayInvoiceRequest::new(bolt11);
        match client.pay_invoice(req).timeout(PAY_TIMEOUT).await {
            Ok(resp) => Ok(resp.preimage),
            Err(e) => Err(classify_pay_error(&e.to_string())),
        }
    }

    /// One in-flight payment per note. True = this call owns the slot.
    pub fn claim_flight(&self, note: &str) -> bool {
        let mut set = self.in_flight.lock().unwrap();
        // HashSet::insert is false when the key was already present.
        set.insert(note.to_string())
    }

    pub fn release_flight(&self, note: &str) {
        self.in_flight.lock().unwrap().remove(note);
    }

    /// Push one event to every subscriber (the wallet broadcast). The
    /// manager emits pay outcomes here too — global state (bolt glyphs,
    /// settings) must survive the view that started the zap.
    pub(crate) fn emit(&self, ev: WalletEvent) {
        let _ = self.events.send(ev);
    }
}

/// Probe what the wallet answers about itself (get_info → get_balance).
/// A wallet that answers NEITHER isn't usable — connect fails honestly.
async fn probe(client: &NostrWalletConnect) -> Result<WalletInfo, String> {
    let mut info = WalletInfo::default();
    match tokio::time::timeout(PROBE_TIMEOUT, client.get_info()).await {
        Ok(Ok(resp)) => {
            info.alias = resp.alias.unwrap_or_default();
        }
        Ok(Err(e)) => return Err(e.to_string()),
        Err(_) => return Err("timed out".into()),
    }
    if let Ok(Ok(resp)) = tokio::time::timeout(PROBE_TIMEOUT, client.get_balance()).await {
        info.balance_msat = Some(resp.balance);
    }
    Ok(info)
}

// --- zap request building -------------------------------------------------

/// Build the unsigned kind-9734 zap request (public zap — NIP-57): recipient
/// p-tag, target e-tag, the amount, the lnurl, and the relays the wallet
/// should publish the receipt to (ours — so the receipt-watch can see it).
pub fn zap_request(
    me: PublicKey,
    recipient: PublicKey,
    note_id: EventId,
    msat: u64,
    lnurl: &str,
    receipt_relays: Vec<String>,
) -> UnsignedEvent {
    let relays: Vec<RelayUrl> = receipt_relays
        .iter()
        .filter_map(|r| RelayUrl::parse(r).ok())
        .collect();
    let data = ZapRequestData::new(recipient, relays)
        .message(String::new())
        .amount(msat)
        .lnurl(lnurl_encode(lnurl))
        .event_id(note_id);
    // finalize_unsigned stamps the AUTHOR (the zapper) — the request is
    // signed through the owner's signer downstream (bunker; D20).
    data.into_event_builder().finalize_unsigned(me)
}

// --- NIP-57 zap splits (Appendix G) ---------------------------------------

/// One `zap` tag on the event being zapped:
/// `["zap", <hex pubkey>, <relay to fetch kind-0>, <weight?>]`.
#[derive(Clone)]
pub struct ZapSplit {
    pub pk: PublicKey,
    /// The relay to download the receiver's metadata from. Advisory —
    /// Satori resolves profiles through its own pool.
    pub relay: Option<String>,
    pub weight: Option<f64>,
}

/// The `zap` tags on the target event (NIP-57 Appendix G): the split
/// receivers. The author is NOT implicitly among them — a split note's
/// recipients are exactly its tags. Junk tags (bad hex) drop out; weights
/// parse as f64, non-finite or negative treated as absent.
/// The split receiver ceiling (audit hardening): the `zap` tags are
/// ATTACKER-WRITABLE on any note, and each receiver costs a signed 9734,
/// an LNURL fetch, an invoice fetch and an NWC pay — an uncapped fan-out
/// stretched one 21-sat tap into hundreds of sequential round trips.
/// The first N in tag order win (NIP-57 lists receivers in tag order);
/// beyond that the split fails closed. Amethyst's planner caps lower.
pub const MAX_SPLIT_RECEIVERS: usize = 16;

pub fn zap_splits(ev: &Event) -> Vec<ZapSplit> {
    ev.tags
        .iter()
        .filter(|t| t.as_slice().first().map(String::as_str) == Some("zap"))
        .filter_map(|t| {
            let pk = PublicKey::from_hex(t.as_slice().get(1)?).ok()?;
            let weight = t
                .as_slice()
                .get(3)
                .and_then(|w| w.parse::<f64>().ok())
                .filter(|w| w.is_finite() && *w >= 0.0);
            Some(ZapSplit {
                pk,
                relay: t.as_slice().get(2).cloned(),
                weight,
            })
        })
        .take(MAX_SPLIT_RECEIVERS)
        .collect()
}

/// Divide `msat` across the split receivers (NIP-57 Appendix G): weights
/// are relative; when NO tag carries a weight the zap divides equally;
/// when weights are PARTIALLY present the unweighted receivers get
/// nothing (`weight = 0`, the spec's own rule). Per-share floors sweep
/// their rounding remainder onto the largest receiver so the shares sum
/// to `msat` exactly. Empty input → empty (the caller keeps the
/// author-only path).
pub fn split_shares(msat: u64, splits: &[ZapSplit]) -> Vec<(PublicKey, u64)> {
    if splits.is_empty() {
        return Vec::new();
    }
    let total: f64 = splits.iter().filter_map(|s| s.weight).sum();
    let shares: Vec<(PublicKey, f64)> = if splits.iter().all(|s| s.weight.is_some()) {
        // All weighted (the total may be 0 only if every weight is 0 —
        // then there is nothing to divide).
        if total <= 0.0 {
            return Vec::new();
        }
        splits
            .iter()
            .map(|s| (s.pk, s.weight.unwrap_or_default() / total))
            .collect()
    } else if total > 0.0 {
        // Partially weighted: the unweighted drop out (weight = 0).
        splits
            .iter()
            .filter_map(|s| s.weight.map(|w| (s.pk, w / total)))
            .collect()
    } else {
        // No weights at all: equal division.
        let n = splits.len() as f64;
        splits.iter().map(|s| (s.pk, 1.0 / n)).collect()
    };
    let mut out: Vec<(PublicKey, u64)> = shares
        .iter()
        .map(|(pk, frac)| (*pk, (msat as f64 * frac).floor() as u64))
        .filter(|(_, m)| *m > 0)
        .collect();
    let allotted: u64 = out.iter().map(|(_, m)| *m).sum();
    if let Some(rest) = msat.checked_sub(allotted)
        && let Some((_, largest)) = out.iter_mut().max_by_key(|(_, m)| *m)
    {
        *largest += rest;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- URI / connection record round-trip ------------------------------

    const NWC_URI: &str = "nostr+walletconnect://b889ff5b1513b641e2a139f661a661364979c5beee91842f8f0ef42ab558e9d4?secret=71a8c14c1407c113601079c4302dab36460f0ccd0ad506f1f2dc73b5100e4f3c&relay=wss%3A%2F%2Frelay.damus.io";

    #[test]
    fn uri_parses_and_record_roundtrips() {
        let parsed = NostrWalletConnectUri::parse(NWC_URI).expect("uri parses");
        assert_eq!(
            parsed.public_key.to_string(),
            "b889ff5b1513b641e2a139f661a661364979c5beee91842f8f0ef42ab558e9d4"
        );
        // The secret-free record survives a JSON round-trip; the secret is
        // NOT in it (it rides the secret store).
        let conn = Conn {
            wallet: parsed.public_key.to_string(),
            relays: parsed.relays.iter().map(|r| r.to_string()).collect(),
            direct: Vec::new(),
        };
        let json = serde_json::to_string(&conn).unwrap();
        assert!(!json.contains("71a8c14c"), "secret leaked into wallet.json");
        let back: Conn = serde_json::from_str(&json).unwrap();
        assert_eq!(back.relays, vec!["wss://relay.damus.io".to_string()]);
        assert!(back.direct.is_empty(), "no consent recorded by default");

        // Old wallet.json (before the issue-#11 `direct` field) still loads.
        let legacy = r#"{"wallet":"a","relays":["wss://r.example"]}"#;
        let old: Conn = serde_json::from_str(legacy).expect("legacy record loads");
        assert!(old.direct.is_empty());

        // And the URI round-trips through Display (the resume path rebuilds
        // from parts; this proves the parse side).
        let redisplay = parsed.to_string();
        let reparsed = NostrWalletConnectUri::parse(&redisplay).unwrap();
        assert_eq!(reparsed.public_key, parsed.public_key);
        assert_eq!(
            reparsed.secret.to_secret_hex(),
            parsed.secret.to_secret_hex()
        );
    }

    #[test]
    fn garbage_uri_is_rejected() {
        assert!(NostrWalletConnectUri::parse("https://example.com").is_err());
        assert!(NostrWalletConnectUri::parse("nostr+walletconnect://nope").is_err());
    }

    // --- msat math / bolt11 amount check ---------------------------------

    #[test]
    fn bolt11_amounts_parse() {
        // 21 sats = lnbc210n (nano-BTC × 100 msat). The signature separator
        // ('1') must follow the unit — anything else is not an invoice.
        assert_eq!(
            crate::notifications::msats_from_bolt11("lnbc210n1abc"),
            21_000
        );
        assert_eq!(
            crate::notifications::msats_from_bolt11("LNBC210N1ABC"),
            21_000
        );
        // 1 sat = lnbc10n; 1000 sats = lnbc1u (micro).
        assert_eq!(crate::notifications::msats_from_bolt11("lnbc10n1x"), 1_000);
        assert_eq!(
            crate::notifications::msats_from_bolt11("lnbc1u1abc"),
            100_000
        );
        // No separator after the unit → malformed, reads as 0 (the caller
        // passes those through to the wallet's own display).
        assert_eq!(crate::notifications::msats_from_bolt11("lnbc210nabc"), 0);
        assert_eq!(crate::notifications::msats_from_bolt11("not-an-invoice"), 0);
    }

    // --- pay error classification (the Uncertain rule) -------------------

    #[test]
    fn wallet_answers_classify_as_code_not_uncertain() {
        assert_eq!(
            classify_pay_error("not enough funds [InsufficientBalance]"),
            PayError::Code("not enough balance in the wallet".into())
        );
        assert_eq!(
            classify_pay_error("slow down [RateLimited]"),
            PayError::Code("wallet is rate-limiting, try again later".into())
        );
        assert!(matches!(
            classify_pay_error("route not found [PaymentFailed]"),
            PayError::Code(_)
        ));
    }

    #[test]
    fn timeouts_classify_uncertain() {
        assert_eq!(classify_pay_error("request timed out"), PayError::Uncertain);
        assert_eq!(classify_pay_error("Timeout"), PayError::Uncertain);
        // Transport noise: no definite answer → Uncertain (never auto-retry).
        assert_eq!(
            classify_pay_error("connection closed before a response"),
            PayError::Uncertain
        );
    }

    // --- LNURL resolution -------------------------------------------------

    #[test]
    fn lightning_addresses_map_to_lnurlp() {
        assert_eq!(
            lnurlp_url("zaps@example.com").unwrap(),
            "https://example.com/.well-known/lnurlp/zaps"
        );
        assert!(lnurlp_url("no-at-sign").is_err());
        assert!(lnurlp_url("bad@@domain").is_err());
        assert!(lnurlp_url("").is_err());
    }

    #[test]
    fn lnurl_lud06_decodes_to_url() {
        // lnurl1... bech32 of the lnurlp URL for "satoshi@snort.social".
        let url = "https://snort.social/.well-known/lnurlp/satoshi";
        let encoded = lnurl_encode(url);
        assert!(encoded.starts_with("lnurl1"));
        assert_eq!(lnurlp_url(&encoded).unwrap(), url);
        // The 9734 lnurl tag wants the bech32 form (NIP-57).
        assert_eq!(lnurl_encode(url), encoded);
    }

    // --- zap request shape -------------------------------------------------

    #[test]
    fn zap_request_carries_nip57_tags() {
        let me = Keys::parse("e1d4a5b33a9d2f0c8b7e6f5a4d3c2b1a0918273645f4e3d2c1b0a99887766554")
            .unwrap();
        let recipient =
            Keys::parse("a1d4a5b33a9d2f0c8b7e6f5a4d3c2b1a0918273645f4e3d2c1b0a99887766554")
                .unwrap();
        let note_id =
            EventId::from_hex("0707070707070707070707070707070707070707070707070707070707070707")
                .unwrap();
        let unsigned = zap_request(
            me.public_key(),
            recipient.public_key(),
            note_id,
            21_000,
            "zaps@example.com",
            vec!["wss://relay.example".into()],
        );
        assert_eq!(u16::from(unsigned.kind), 9734);
        let tags = unsigned.tags.clone().to_vec();
        let has = |name: &str| {
            tags.iter()
                .any(|t| t.as_slice().first() == Some(&name.to_string()))
        };
        assert!(has("p"), "recipient tag");
        assert!(has("e"), "note tag");
        assert!(has("amount"), "msat tag");
        assert!(has("lnurl"), "bech32 lnurl tag");
        assert!(has("relays"), "receipt relays tag");
        assert_eq!(unsigned.pubkey, me.public_key());
    }

    // --- paste-time consent persistence (issue #11) ----------------------

    #[test]
    fn conn_persists_and_reseeds_consent_hosts() {
        // The consent hosts survive the JSON round-trip (resume re-seeds
        // the net_guard ledger from this list) and the secret never does.
        let conn = Conn {
            wallet: "ab".repeat(32),
            relays: vec!["wss://umbrel.local:7777".to_string()],
            direct: vec!["umbrel.local".to_string()],
        };
        let json = serde_json::to_string(&conn).unwrap();
        let back: Conn = serde_json::from_str(&json).unwrap();
        assert_eq!(back.direct, vec!["umbrel.local".to_string()]);
        assert!(!json.contains("secret"), "record stays secret-free");
    }

    // --- ledger: request-stored-before-pay, retry reuses the ID ----------

    #[test]
    fn ledger_stores_before_pay_and_retry_reuses_id() {
        let _env = crate::test_env::EnvGuard::new("zaps");

        let note = "a".repeat(64);
        let record = ZapRecord {
            req: "req1".into(),
            note: note.clone(),
            recipient: "b".repeat(64),
            msat: 21_000,
            state: ZapState::Pending,
            created_at: 1,
            event: r#"{"id":"c","kind":9734}"#.into(),
        };
        // The rule: the request is PERSISTED before the pay call happens
        // (the manager stores it, then pays — a crash here leaves a Pending
        // record, not a paid wallet and no memory of it).
        ledger_update(record.clone());
        let stored = &ledger_for_note(&note)[0];
        assert_eq!(stored.state, ZapState::Pending);
        assert_eq!(stored.msat, 21_000);

        // An explicit retry reuses the SAME event ID (no duplicate zap
        // events after a crash/restart).
        assert_eq!(ledger_for_note(&note)[0].req, "req1");

        // Pay confirmed → Paid. A later crash keeps the bolt settled.
        let mut paid = record.clone();
        paid.state = ZapState::Paid;
        ledger_update(paid);
        assert_eq!(ledger_for_note(&note)[0].state, ZapState::Paid);

        // History (amount-picker seeds), newest first.
        let hist = ledger_history();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].req, "req1");
    }

    #[test]
    fn ledger_holds_one_record_per_split_receiver() {
        let _env = crate::test_env::EnvGuard::new("zaps");

        // A split zap stores N records for one note (distinct request ids,
        // same note id); the retry path sees them all, newest first.
        let note = "c".repeat(64);
        let mk = |req: &str, pk: &str| ZapRecord {
            req: req.into(),
            note: note.clone(),
            recipient: pk.into(),
            msat: 10_000,
            state: ZapState::Pending,
            created_at: 1,
            event: format!(r#"{{"id":"{req}","kind":9734}}"#),
        };
        ledger_update(mk("reqA", "aa"));
        ledger_update(mk("reqB", "bb"));
        let stored = ledger_for_note(&note);
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].req, "reqB", "newest first");

        // A retry skips the one that already settled.
        let mut settled = mk("reqA", "aa");
        settled.state = ZapState::Paid;
        ledger_update(settled);
        let pendings: Vec<_> = ledger_for_note(&note)
            .into_iter()
            .filter(|z| matches!(z.state, ZapState::Pending | ZapState::Uncertain))
            .collect();
        assert_eq!(pendings.len(), 1);
        assert_eq!(pendings[0].req, "reqB");
    }

    // --- NIP-57 zap splits (Appendix G) ----------------------------------

    fn split_event(tags: &[&str]) -> Event {
        // zap_splits only reads the tag vec; the event itself is a plain
        // signed text note (notifications.rs's forged-receipt pattern).
        let keys = Keys::generate();
        let tags: Vec<Tag> = tags
            .iter()
            .map(|t| {
                let parts: Vec<&str> = t.split('|').collect();
                Tag::custom(
                    parts[0].to_string(),
                    parts[1..].iter().map(|s| s.to_string()),
                )
            })
            .collect();
        let unsigned = UnsignedEvent::new(
            keys.public_key(),
            Timestamp::now(),
            Kind::TextNote,
            tags,
            String::new(),
        );
        keys.sign_event(unsigned).unwrap()
    }

    #[test]
    fn zap_splits_parse_tags_and_drop_junk() {
        let ev = split_event(&[
            "zap|1111111111111111111111111111111111111111111111111111111111111111|wss://a|1",
            "zap|not-hex",
            "note|whatever",
            "zap|2222222222222222222222222222222222222222222222222222222222222222|wss://b|2",
        ]);
        let splits = zap_splits(&ev);
        assert_eq!(splits.len(), 2);
        assert_eq!(splits[1].weight, Some(2.0));
        assert_eq!(splits[1].relay.as_deref(), Some("wss://b"));
    }

    /// The receiver ceiling (audit hardening): an attacker note with
    /// hundreds of zap tags stops at MAX_SPLIT_RECEIVERS, in tag order.
    #[test]
    fn split_receivers_are_capped() {
        let mut lines = Vec::new();
        for n in 1..=40 {
            // Distinct hex receivers 01.., one per line.
            lines.push(format!("zap|{n:064x}|wss://r|1"));
        }
        let ev = split_event(&lines.iter().map(String::as_str).collect::<Vec<_>>());
        let splits = zap_splits(&ev);
        assert_eq!(splits.len(), MAX_SPLIT_RECEIVERS);
        assert_eq!(splits[0].pk.to_string(), format!("{:064x}", 1));
        assert_eq!(
            splits[MAX_SPLIT_RECEIVERS - 1].pk.to_string(),
            format!("{:064x}", MAX_SPLIT_RECEIVERS)
        );
    }

    #[test]
    fn split_shares_weights_are_relative_and_sum_exactly() {
        let mk = |hex: &str, w: Option<f64>| ZapSplit {
            pk: PublicKey::from_hex(hex).expect("pk"),
            relay: None,
            weight: w,
        };
        let a = mk(
            "1111111111111111111111111111111111111111111111111111111111111111",
            Some(1.0),
        );
        let b = mk(
            "2222222222222222222222222222222222222222222222222222222222222222",
            Some(1.0),
        );
        let c = mk(
            "3333333333333333333333333333333333333333333333333333333333333333",
            Some(2.0),
        );

        // 1 + 1 + 2 → 25% / 25% / 50%, floors + remainder sum to msat.
        let shares = split_shares(10_000, &[a.clone(), b.clone(), c.clone()]);
        assert_eq!(
            shares.iter().map(|(_, m)| *m).collect::<Vec<_>>(),
            vec![2500, 2500, 5000]
        );
        assert_eq!(shares.iter().map(|(_, m)| *m).sum::<u64>(), 10_000);

        // Rounding remainder lands on the largest receiver.
        let shares = split_shares(9999, &[a.clone(), b.clone(), c.clone()]);
        assert_eq!(
            shares.iter().map(|(_, m)| *m).collect::<Vec<_>>(),
            vec![2499, 2499, 5001]
        );

        // No weights at all: equal division.
        let unweighted = vec![
            mk(
                "1111111111111111111111111111111111111111111111111111111111111111",
                None,
            ),
            mk(
                "2222222222222222222222222222222222222222222222222222222222222222",
                None,
            ),
        ];
        let shares = split_shares(3001, &unweighted);
        assert_eq!(
            shares.iter().map(|(_, m)| *m).collect::<Vec<_>>(),
            vec![1500, 1501]
        );

        // Partially weighted: the unweighted receiver gets nothing (the
        // spec's own rule).
        let partial = vec![
            a.clone(),
            mk(
                "4444444444444444444444444444444444444444444444444444444444444444",
                None,
            ),
        ];
        let shares = split_shares(5000, &partial);
        assert_eq!(shares.len(), 1);
        assert_eq!(shares[0].1, 5000);

        // Every weight zero → nothing to divide.
        let zero = vec![
            mk(
                "1111111111111111111111111111111111111111111111111111111111111111",
                Some(0.0),
            ),
            mk(
                "2222222222222222222222222222222222222222222222222222222222222222",
                Some(0.0),
            ),
        ];
        assert!(split_shares(5000, &zero).is_empty());
    }

    #[test]
    fn in_flight_is_one_per_note() {
        let w = Wallet::new();
        assert!(w.claim_flight("note-a"));
        assert!(!w.claim_flight("note-a"), "second tap while pending");
        assert!(w.claim_flight("note-b"), "other notes are free");
        w.release_flight("note-a");
        assert!(w.claim_flight("note-a"), "release frees the slot");
    }
}
