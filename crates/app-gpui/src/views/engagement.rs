//! The optimistic engagement engine: like/repost/bookmark/zap mirrors and
//! their publish pumps, extracted (#31) from the triplication across
//! feed.rs, thread.rs and notifications.rs (thread was the template; feed
//! added touch() and the Bookmarks row-drop; drift was already visible).
//!
//! The state lives as ONE field ([`Engagement`]) on each host view; the
//! behavior comes from this module's generic functions, driven by the
//! [`Host`] trait. The discipline is §1.2's: mutate the mirror first (the
//! glyph flips vermilion now), reconcile against core's publish events
//! later; core enforces the money rules (one zap per note, §1.2).

use std::collections::HashSet;
use std::sync::Arc;

use gpui::{Context, Entity, EventEmitter};
use nostr::prelude::*;
use satori_core::manager::{Core, PublishEvent};
use satori_core::wallet::{Status, WalletEvent};

use super::nav::Nav;
use crate::prefs;

/// The five mirrors + wallet readiness — the glyphs' render state. Every
/// host view holds one; `Engagement::new` seeds it from core's persisted
/// sets at construction.
#[derive(Clone)]
pub struct Engagement {
    pub liked: HashSet<String>,
    pub reposted: HashSet<String>,
    pub bookmarked: HashSet<String>,
    pub zapped: HashSet<String>,
    /// Session zap-in-flight set; the wallet broadcast clears it.
    pub zap_pending: HashSet<String>,
    /// Whether the NWC wallet answers (the dim-bolt affordance).
    pub wallet_ready: bool,
    /// The row (note id) whose reaction quick-set is open, if any — one
    /// at a time; a pick or a second right-click closes it.
    pub reaction_picker: Option<String>,
}

impl Engagement {
    pub fn new(core: &Arc<Core>) -> Self {
        Self {
            liked: core.liked_set(),
            bookmarked: core.bookmarked_set(),
            reposted: core.reposted_set(),
            zapped: core.zapped_set(),
            zap_pending: HashSet::new(),
            wallet_ready: core.wallet().is_ready(),
            reaction_picker: None,
        }
    }

    /// One row's paint state, from the mirrors (the per-id ActionState
    /// build the five views had been spelling out by hand — wire.rs and
    /// notifications.rs verbatim twins born across two releases; the
    /// D55 lens pulled the kernel at birth's end). `reduced_motion` is
    /// the paint-time pref, `actions::reduced_motion(cx)` at the call
    /// site. feed.rs's two rows are NOT this shape (their like/repost
    /// booleans are computed, not mirror lookups) and stay hand-rolled.
    pub fn action_state(
        &self,
        id: &str,
        replies: usize,
        reduced_motion: bool,
    ) -> super::actions::ActionState {
        super::actions::ActionState {
            liked: self.liked.contains(id),
            reposted: self.reposted.contains(id),
            bookmarked: self.bookmarked.contains(id),
            zapped: self.zapped.contains(id),
            zapping: self.zap_pending.contains(id),
            wallet_ready: self.wallet_ready,
            picker_open: self.reaction_picker.as_deref() == Some(id),
            replies,
            reduced_motion,
        }
    }
}

/// What the engine needs from its host view. The two optional flourishes
/// were feed.rs's only deltas from the verbatim twins: the snapshot
/// invalidation ([`Host::touched`]) and the Bookmarks row-drop
/// ([`Host::unbookmark_committed`], the Bookmarks page's D38 list).
pub trait Host: EventEmitter<Nav> + Sized + 'static {
    /// The host's mirror field.
    fn engagement(&mut self) -> &mut Engagement;
    fn core(&self) -> &Arc<Core>;
    /// The pane's quiet status whisper (the failure voice).
    fn whisper(&mut self, msg: String);
    /// The host's snapshot invalidation: feed's `touch()` (its render
    /// snapshot must rebuild); a no-op where render reads the mirrors
    /// directly.
    fn touched(&mut self) {}
    /// A COMMITTED unbookmark dropped the row? Only the Bookmarks
    /// page's list is the bookmark set's projection (D38); everywhere
    /// else only the star unfills.
    fn unbookmark_committed(&mut self, _id: EventId, _cx: &mut Context<Self>) {}
}

/// A viewport engagement event landed in the store (WS2): re-seed the
/// own-engagement mirrors (they are the glyphs' render state) and let the
/// host invalidate. Only feed subscribes today.
pub fn reseed<H: Host>(host: &mut H) {
    let (liked, reposted, zapped) = {
        let core = host.core();
        (core.liked_set(), core.reposted_set(), core.zapped_set())
    };
    let e = host.engagement();
    e.liked = liked;
    e.reposted = reposted;
    e.zapped = zapped;
    host.touched();
}

/// The wallet broadcast → glyph states + the quiet status whisper. (The
/// ConnectFailed arm is the wallet pane's concern, not the mirrors'.)
pub fn on_wallet<H: Host>(host: &mut H, ev: WalletEvent, cx: &mut Context<H>) {
    match ev {
        WalletEvent::Status(s) => {
            host.engagement().wallet_ready = matches!(s, Status::Ready(_));
        }
        WalletEvent::RelayScreened { .. } => {
            // The wallet pane's concern (its dialog derives the banner
            // and the allow list); the mirrors keep their glyph states.
        }
        WalletEvent::ZapPaid { note, .. } => {
            let e = host.engagement();
            e.zap_pending.remove(&note);
            e.zapped.insert(note);
        }
        WalletEvent::ZapUncertain { note, .. } => {
            host.engagement().zap_pending.remove(&note);
            host.whisper("zap state unknown, check your wallet before retrying".into());
        }
        WalletEvent::ZapFailed { note, reason } => {
            host.engagement().zap_pending.remove(&note);
            host.whisper(format!("zap didn't go through: {reason}"));
        }
        WalletEvent::ZapReceipt { note, .. } => {
            host.engagement().zapped.insert(note);
        }
        WalletEvent::ConnectFailed(_) => {}
    }
    host.touched();
    cx.notify();
}

/// The wallet broadcast pump (D20): bolt glyphs follow every zap —
/// theirs or the sheet's — wherever it started. Was pasted into the
/// three mirror hosts' constructors (feed, thread, notifications; the
/// wallet and settings panes keep their own richer pumps — they handle
/// arms the mirrors don't).
pub fn pump_wallet<H: Host>(core: &Arc<Core>, entity: &Entity<H>, cx: &mut Context<H>) {
    let this = entity.downgrade();
    let mut wallet_rx = core.subscribe_wallet();
    cx.spawn(async move |_, cx| {
        loop {
            match wallet_rx.recv().await {
                Ok(ev) => {
                    // Dead pane (popped): the pump's exit is update() failing.
                    if this
                        .update(cx, |host, cx| {
                            let ev = ev;
                            on_wallet(host, ev, cx)
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(_) => continue, // lagged: the next event re-syncs
            }
        }
    })
    .detach();
}

/// Optimistic like, per §1.2: mutate state first (glyph flips vermilion
/// now), reconcile against core's publish events later.
pub fn toggle_like<H: Host>(host: &mut H, ev: &Event, cx: &mut Context<H>) {
    let id = ev.id.to_string();
    if !host.engagement().liked.insert(id) {
        return;
    }
    host.touched();
    let mut rx = host.core().react(ev, "+");
    cx.spawn(async move |_, _| while rx.recv().await.is_some() {})
        .detach();
    cx.notify();
}

/// The row's reaction quick-set toggle (the heart's right-click): one
/// picker open at a time; a second right-click closes it.
pub fn toggle_reaction_picker<H: Host>(host: &mut H, id: &str, cx: &mut Context<H>) {
    let e = host.engagement();
    e.reaction_picker = if e.reaction_picker.as_deref() == Some(id) {
        None
    } else {
        Some(id.to_string())
    };
    host.touched();
    cx.notify();
}

/// A picked reaction (the quick-set): optimistic like the heart is — any
/// own reaction lights the glyph — then publish with that content. The
/// picker closes behind it.
pub fn react_with<H: Host>(host: &mut H, ev: &Event, content: &str, cx: &mut Context<H>) {
    let id = ev.id.to_string();
    host.engagement().liked.insert(id);
    host.engagement().reaction_picker = None;
    host.touched();
    let mut rx = host.core().react(ev, content);
    cx.spawn(async move |_, _| while rx.recv().await.is_some() {})
        .detach();
    cx.notify();
}

/// Optimistic repost (NIP-18 kind 6): the glyph flips vermilion now, the
/// event waits out the undo window in core (the shell's toast carries
/// Undo), and a failure — or an undo — rolls the glyph back.
pub fn toggle_repost<H: Host>(host: &mut H, ev: &Event, cx: &mut Context<H>) {
    let id = ev.id.to_string();
    if !host.engagement().reposted.insert(id.clone()) {
        return;
    }
    host.touched();
    let mut rx = host.core().repost(ev);
    cx.spawn(async move |this, cx| {
        while let Some(pv) = rx.recv().await {
            match pv {
                PublishEvent::Failed { .. } | PublishEvent::Undone { .. } => {
                    let _ = this.update(cx, |host, cx| {
                        host.engagement().reposted.remove(&id);
                        host.whisper("repost didn't go through, try again".into());
                        host.touched();
                        cx.notify();
                    });
                    return;
                }
                _ => {}
            }
        }
    })
    .detach();
    cx.notify();
}

/// Bookmark toggle (D38): a real toggle now. The star fills for either
/// list (private 10003 / public 30001 — the settings pref picks which one
/// a star WRITES); a second click unbookmarks through the RMW that clears
/// both. Optimistic, with rollback (the old add-only star made a second
/// click a no-op).
pub fn toggle_bookmark<H: Host>(host: &mut H, ev: &Event, cx: &mut Context<H>) {
    let id = ev.id.to_string();
    let ev_id = ev.id;
    let adding = host.engagement().bookmarked.insert(id.clone());
    if !adding {
        host.engagement().bookmarked.remove(&id);
    }
    // The scope read rides the click: a mid-session pref flip applies to
    // the NEXT star, the running one keeps its list (prefs.rs's mtime
    // cache makes this a file stat).
    let public = crate::prefs::get_bool("bookmarkPublic");
    host.touched();
    let mut rx = if adding {
        host.core().bookmark(ev, public)
    } else {
        host.core().unbookmark(ev_id)
    };
    cx.spawn(async move |this, cx| {
        while let Some(pv) = rx.recv().await {
            match pv {
                PublishEvent::Failed { .. } => {
                    let _ = this.update(cx, |host, cx| {
                        let e = host.engagement();
                        if adding {
                            e.bookmarked.remove(&id);
                        } else {
                            e.bookmarked.insert(id.clone());
                        }
                        host.whisper("bookmark didn't go through, try again".into());
                        host.touched();
                        cx.notify();
                    });
                }
                PublishEvent::Committed { .. } if !adding => {
                    // Unbookmarking from the Bookmarks page's list drops
                    // the row (D38); everywhere else only the star unfills.
                    let _ = this.update(cx, |host, cx| host.unbookmark_committed(ev_id, cx));
                }
                _ => {}
            }
        }
    })
    .detach();
    cx.notify();
}

/// The bolt's tap: 21 sats, no dialog — the money rule. No wallet behind
/// it? The dim glyph opens the wallet pane instead. Errors and settle flow
/// back on the wallet broadcast (on_wallet).
pub fn zap_tap<H: Host>(host: &mut H, ev: &Event, cx: &mut Context<H>) {
    // The instant pref (D92): off means the tap opens the sheet instead
    // (the sheet's toggle row owns it).
    if !prefs::zap_instant() {
        cx.emit(Nav::OpenZapSheet(ev.clone()));
        return;
    }
    if !host.engagement().wallet_ready {
        cx.emit(Nav::OpenWallet);
        return;
    }
    // One zap in flight per note (the Feather rule; core enforces too).
    if !host.engagement().zap_pending.insert(ev.id.to_string()) {
        return;
    }
    host.touched();
    host.core().zap(
        ev.clone(),
        super::zap::TAP_SATS * 1000,
        String::new(),
        satori_core::wallet::ZapPrivacy::Attributed,
    );
    cx.notify();
}
