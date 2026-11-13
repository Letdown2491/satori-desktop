//! Thread view: the anchor note + its replies, nested NIP-10 style.
//!
//! TODO(port): crates/app/src/thread.rs — P1/P2. `nest_replies`: the direct
//! parent is the reply's LAST `e` tag present in the reply set (NIP-10
//! marked-e reads right-to-left), depth <= 3. The tree is flattened
//! pre-order for the list widget; depth drives the indent. Replies open
//! their own threads on click.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use gpui::{
    Context, Entity, EventEmitter, FontWeight, ListAlignment, ListState, Render, SharedString,
    Window, div, list, prelude::*, px, rgb,
};
use nostr::prelude::*;
use satori_core::manager::{Core, FeedEvent, FeedRx, ParentNote, Profile, PublishEvent};
use satori_core::wallet::{Status, WalletEvent};
use satori_core::wot::Verdict;

use super::{actions, content, nav::Nav, states};
use crate::{media, prefs, theme};

pub struct ThreadView {
    core: Arc<Core>,
    anchor: Option<Event>,
    /// Replies flattened pre-order with their NIP-10 depth (nest_replies).
    replies: Vec<(Event, usize)>,
    /// One-session expanded set for the WS3 collapse: a reply the user
    /// expanded stays open until the pane closes (click is a state change,
    /// not a re-hide).
    expanded: HashSet<EventId>,
    profiles: HashMap<String, Profile>,
    quotes: content::Quotes,
    status: String,
    /// §4.3: the kinded error strip (replaces the raw `… {e}` status dump).
    error: Option<states::PaneError>,
    /// The pane's anchor — Retry re-asks it (and the thread) by id.
    anchor_id: EventId,
    /// Lazy parent hydration (the webapp's "↩ in reply to an earlier note",
    /// render/note.ts reply-context): direct parents that aren't in the
    /// thread — id → author + snippet, asked-set keeps misses from looping.
    parents: HashMap<String, ParentNote>,
    parents_asked: HashSet<String>,
    list: ListState,
    /// The action row's mirrors (actions.rs): loaded from core's persisted
    /// engagement sets at construction, kept optimistic per toggle.
    liked: HashSet<String>,
    reposted: HashSet<String>,
    bookmarked: HashSet<String>,
    zapped: HashSet<String>,
    /// Session zap-in-flight set; the wallet broadcast clears it.
    zap_pending: HashSet<String>,
    wallet_ready: bool,
}

impl EventEmitter<Nav> for ThreadView {}

/// The thread's toggles (actions.rs): the optimistic-mirror discipline —
/// flip the glyph now, reconcile against the publish stream later (feed.rs's
/// pattern; the mirrors live on the view, the publish rides core's commit()).
impl actions::ActionHost for ThreadView {
    /// Optimistic like: mutate state first, reconcile against core's publish
    /// events later (§1.2).
    fn toggle_like(&mut self, ev: &Event, cx: &mut Context<Self>) {
        let id = ev.id.to_string();
        if !self.liked.insert(id) {
            return;
        }
        let mut rx = self.core.react(ev);
        cx.spawn(async move |_, _| while rx.recv().await.is_some() {})
            .detach();
        cx.notify();
    }

    /// Optimistic repost (NIP-18 kind 6): the glyph flips vermilion now; a
    /// failure — or an undo — rolls it back.
    fn toggle_repost(&mut self, ev: &Event, cx: &mut Context<Self>) {
        let id = ev.id.to_string();
        if !self.reposted.insert(id.clone()) {
            return;
        }
        let mut rx = self.core.repost(ev);
        cx.spawn(async move |this, cx| {
            while let Some(pv) = rx.recv().await {
                match pv {
                    PublishEvent::Failed { .. } | PublishEvent::Undone { .. } => {
                        let _ = this.update(cx, |this, cx| {
                            this.reposted.remove(&id);
                            this.status = "repost didn't go through, try again".into();
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

    /// Bookmark toggle (D38): the star fills for either list (private 10003 /
    /// public 30001 — the settings pref picks which one a star WRITES); a
    /// second click unbookmarks through the RMW that clears both.
    fn toggle_bookmark(&mut self, ev: &Event, cx: &mut Context<Self>) {
        let id = ev.id.to_string();
        let ev_id = ev.id;
        let adding = self.bookmarked.insert(id.clone());
        if !adding {
            self.bookmarked.remove(&id);
        }
        // The scope read rides the click (feed.rs's comment stands: a
        // mid-session pref flip applies to the NEXT star).
        let public = prefs::get_bool("bookmarkPublic");
        let mut rx = if adding {
            self.core.bookmark(ev, public)
        } else {
            self.core.unbookmark(ev_id)
        };
        cx.spawn(async move |this, cx| {
            while let Some(pv) = rx.recv().await {
                if let PublishEvent::Failed { .. } = pv {
                    let _ = this.update(cx, |this, cx| {
                        if adding {
                            this.bookmarked.remove(&id);
                        } else {
                            this.bookmarked.insert(id.clone());
                        }
                        this.status = "bookmark didn't go through, try again".into();
                        cx.notify();
                    });
                }
            }
        })
        .detach();
        cx.notify();
    }

    /// One zap in flight per note (the Feather rule; core enforces too).
    fn zap_tap(&mut self, ev: &Event, cx: &mut Context<Self>) {
        if !self.wallet_ready {
            cx.emit(Nav::OpenWallet);
            return;
        }
        if !self.zap_pending.insert(ev.id.to_string()) {
            return;
        }
        self.core.zap(ev.clone(), super::zap::TAP_SATS * 1000);
        cx.notify();
    }
}

impl ThreadView {
    pub fn new(core: Arc<Core>, id: EventId, cx: &mut Context<Self>) -> Self {
        crate::theme::watch(cx); // repaint on theme change
        crate::timefmt::watch(cx); // repaint on mode/height change
        crate::video::watch(cx); // repaint on player open/toggle/close
        let liked = core.liked_set();
        let reposted = core.reposted_set();
        let bookmarked = core.bookmarked_set();
        let zapped = core.zapped_set();
        let wallet_ready = core.wallet().is_ready();
        let view = Self {
            core,
            anchor: None,
            replies: vec![],
            expanded: HashSet::new(),
            profiles: HashMap::new(),
            quotes: HashMap::new(),
            status: "loading the thread…".into(),
            error: None,
            anchor_id: id,
            parents: HashMap::new(),
            parents_asked: HashSet::new(),
            list: ListState::new(0, ListAlignment::Top, px(400.)),
            liked,
            reposted,
            bookmarked,
            zapped,
            zap_pending: HashSet::new(),
            wallet_ready,
        };
        // The anchor rides in with its author's profile; replies follow.
        view.pump(view.core.event_by_id(id, Vec::new(), None), cx);
        view.pump(view.core.thread(id), cx);
        // The wallet broadcast (D20): the thread's bolts follow every zap,
        // theirs or the sheet's, wherever it started.
        let this = cx.entity().downgrade();
        let mut wallet_rx = view.core.subscribe_wallet();
        cx.spawn(async move |_, cx| {
            loop {
                match wallet_rx.recv().await {
                    Ok(ev) => {
                        // Dead pane (popped): the pump's exit is update() failing.
                        if this.update(cx, |view, cx| view.on_wallet(ev, cx)).is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(_) => continue, // lagged: the next event re-syncs
                }
            }
        })
        .detach();
        view
    }

    /// The wallet broadcast → glyph states + the quiet status whisper
    /// (feed.rs's on_wallet, minus the Bookmarks row-drop: a thread never
    /// IS a bookmark list).
    fn on_wallet(&mut self, ev: WalletEvent, cx: &mut Context<Self>) {
        match ev {
            WalletEvent::Status(s) => {
                self.wallet_ready = matches!(s, Status::Ready(_));
            }
            WalletEvent::ZapPaid { note, .. } => {
                self.zap_pending.remove(&note);
                self.zapped.insert(note);
            }
            WalletEvent::ZapUncertain { note, .. } => {
                self.zap_pending.remove(&note);
                self.status = "zap state unknown, check your wallet before retrying".into();
            }
            WalletEvent::ZapFailed { note, reason } => {
                self.zap_pending.remove(&note);
                self.status = format!("zap didn't go through: {reason}");
            }
            WalletEvent::ZapReceipt { note, .. } => {
                self.zapped.insert(note);
            }
            WalletEvent::ConnectFailed(_) => {} // the wallet pane's concern
        }
        cx.notify();
    }

    fn pump(&self, mut rx: FeedRx, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.recv().await {
                let _ = this.update(cx, |this, cx| this.on_event(ev, cx));
            }
        })
        .detach();
    }

    /// One quote-reference resolution, kept off `on_event` (its `One` arm
    /// means "the anchor").
    fn pump_quote(&self, bech: String, mut rx: FeedRx, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.recv().await {
                if let FeedEvent::One(Some(target), prof) = ev {
                    let _ = this.update(cx, |this, cx| {
                        let key = target.pubkey.to_string();
                        let name = prof
                            .as_ref()
                            .map(|p| p.display())
                            .or_else(|| this.profiles.get(&key).map(|p| p.display()))
                            .unwrap_or_else(|| format!("{}…", &key[..8]));
                        this.quotes.insert(
                            bech.clone(),
                            Some(content::Embed {
                                event: target,
                                name,
                            }),
                        );
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    /// Hydrate names for note authors + mentioned pubkeys (TODO(port):
    /// content.ts mentionPubkeys) — otherwise mentions degrade to @npub1….
    fn want_names(&self, notes: &[Event], cx: &mut Context<Self>) {
        let mut seen = HashSet::new();
        let missing: Vec<PublicKey> = notes
            .iter()
            .flat_map(|n| {
                let mut pks = vec![n.pubkey];
                pks.extend(content::mention_pks(&n.content));
                pks
            })
            .filter(|pk| {
                !self.profiles.contains_key(&pk.to_string()) && seen.insert(pk.to_string())
            })
            .collect();
        if !missing.is_empty() {
            self.pump(self.core.ensure_profiles(missing), cx);
        }
    }

    fn on_event(&mut self, ev: FeedEvent, cx: &mut Context<Self>) {
        match ev {
            FeedEvent::One(Some(note), profile) => {
                // An article landing as a thread anchor (quote embed, reply
                // target) belongs in the reader, not the raw thread.
                if u16::from(note.kind) == 30023 {
                    cx.emit(Nav::OpenArticle(note.clone()));
                    return;
                }
                self.anchor = Some(note.clone());
                self.error = None;
                if let Some(p) = profile {
                    self.profiles.insert(p.pubkey.clone(), p);
                }
                self.want_media(&note, cx);
                self.want_quotes(&note, cx);
                self.want_names(std::slice::from_ref(&note), cx);
                self.want_parents(std::slice::from_ref(&note), cx);
            }
            FeedEvent::One(None, _) => self.status = "this note is gone or unreachable".into(),
            FeedEvent::Replies(replies, profiles) => {
                for note in &replies {
                    self.want_media(note, cx);
                    self.want_quotes(note, cx);
                }
                self.replies = nest_replies(&replies);
                for p in profiles {
                    self.profiles.insert(p.pubkey.clone(), p);
                }
                self.status.clear();
                self.want_names(&replies, cx);
                self.want_parents(&replies, cx);
            }
            FeedEvent::Profiles(profiles) => {
                for p in profiles {
                    self.profiles.insert(p.pubkey.clone(), p);
                }
            }
            FeedEvent::Failed(e) => {
                // §4.3: transient — a relay round trip; Retry re-asks.
                self.error = Some(states::PaneError::transient("The thread", &e));
            }
            FeedEvent::Parents(p) => {
                // Lazy parents landing (want_parents): the whisper upgrades
                // from the generic line to author + snippet. The parent's
                // author needs a name too — one ensure_profiles pump.
                let mut missing: Vec<PublicKey> = Vec::new();
                for (pid, note) in p {
                    self.parents.insert(pid, note.clone());
                    if !self.profiles.contains_key(&note.author)
                        && let Ok(pk) = PublicKey::from_hex(&note.author)
                    {
                        missing.push(pk);
                    }
                }
                if !missing.is_empty() {
                    self.pump(self.core.ensure_profiles(missing), cx);
                }
            }
            _ => {}
        }
        self.list.reset(self.replies.len());
        cx.notify();
    }

    /// Queue core fetches for every media URL we haven't got yet (results
    /// land in the shared media cache; notify re-renders pick them up).
    fn want_media(&mut self, note: &Event, cx: &mut Context<Self>) {
        for url in content::media_urls_cached(note).iter().cloned() {
            media::fetch(&self.core, url, cx.entity_id(), cx);
        }
    }

    /// Queue event fetches for standalone note/nevent references (embeds).
    fn want_quotes(&mut self, note: &Event, cx: &mut Context<Self>) {
        for bech in content::note_refs(note) {
            if self.quotes.contains_key(&bech) {
                continue;
            }
            // Reserve the key so we don't re-request while loading.
            self.quotes.insert(bech.clone(), None);
            let rx = self.core.event_by_id_dech32(&bech);
            self.pump_quote(bech, rx, cx);
        }
    }

    fn name_for(&self, ev: &Event) -> String {
        let key = ev.pubkey.to_string();
        self.profiles
            .get(&key)
            .map(|p| p.display())
            .unwrap_or_else(|| format!("{}…", &key[..8]))
    }

    /// Lazy parent hydration (webapp render/note.ts:216–219, the "↩ in
    /// reply to an earlier note" line): a note whose DIRECT parent isn't in
    /// the thread shows the whisper now and fetches the parent in the
    /// background (load_parents: store first, one targeted indexer round
    /// trip for the misses) instead of blocking the tree on it. The
    /// asked-set keeps misses from re-requesting per render.
    fn want_parents(&mut self, notes: &[Event], cx: &mut Context<Self>) {
        let present: HashSet<String> = self
            .replies
            .iter()
            .map(|(e, _)| e.id.to_string())
            .chain(self.anchor.iter().map(|a| a.id.to_string()))
            .collect();
        let mut ids: Vec<String> = Vec::new();
        for ev in notes {
            if let Some(pid) = direct_parent_id(ev)
                && !present.contains(&pid)
                && !self.parents.contains_key(&pid)
                && self.parents_asked.insert(pid.clone())
            {
                ids.push(pid);
            }
        }
        if ids.is_empty() {
            return;
        }
        let mut rx = self.core.load_parents(ids);
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.recv().await {
                let _ = this.update(cx, |this, cx| this.on_event(ev, cx));
            }
        })
        .detach();
    }

    /// The whisper text for an unloaded parent: the generic line until the
    /// lazy fetch lands, then author + a one-line snippet (T_SMALL quiet,
    /// ink_faint — context, not content).
    fn parent_whisper(&self, pid: &str) -> String {
        match self.parents.get(pid) {
            Some(p) => {
                let name = self
                    .profiles
                    .get(&p.author)
                    .map(|pr| pr.display())
                    .unwrap_or_else(|| format!("{}…", &p.author[..8]));
                if p.snippet.is_empty() {
                    format!("↩ in reply to {name}'s earlier note")
                } else {
                    format!("↩ in reply to {name}: {}", p.snippet)
                }
            }
            None => "↩ in reply to an earlier note…".to_string(),
        }
    }
}

/// A note's direct parent id (NIP-10 marked-e reads right-to-left: the
/// LAST e tag), when that isn't the note itself — the same rule
/// nest_replies uses to place rows.
fn direct_parent_id(ev: &Event) -> Option<String> {
    ev.tags
        .clone()
        .event_ids()
        .last()
        .map(|id| id.to_string())
        .filter(|pid| *pid != ev.id.to_string())
}

/// The NIP-10 tree, flattened pre-order with depths (thread.rs nest_replies).
fn nest_replies(replies: &[Event]) -> Vec<(Event, usize)> {
    let ids: HashSet<String> = replies.iter().map(|r| r.id.to_string()).collect();
    let mut children: HashMap<String, Vec<Event>> = HashMap::new();
    let mut roots: Vec<Event> = Vec::new();
    for r in replies {
        // Last e tag = direct parent (NIP-10 marked-e reads right-to-left).
        let parent = r
            .tags
            .clone()
            .event_ids()
            .last()
            .filter(|pid| ids.contains(&pid.to_string()) && *pid != r.id);
        match parent {
            Some(pid) => children.entry(pid.to_string()).or_default().push(r.clone()),
            None => roots.push(r.clone()),
        }
    }
    fn walk(
        ev: &Event,
        children: &HashMap<String, Vec<Event>>,
        depth: usize,
        out: &mut Vec<(Event, usize)>,
    ) {
        out.push((ev.clone(), depth));
        if depth < 3
            && let Some(kids) = children.get(&ev.id.to_string())
        {
            for kid in kids {
                walk(kid, children, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    for root in &roots {
        walk(root, &children, 0, &mut out);
    }
    out
}

// --- WS3: collapse of out-of-network replies ---------------------------------

/// One reply row's collapse plan (render-time projection; unit-tested
/// below). `hidden_by` is the row index of the collapsed reply that
/// suppresses this row — Some(i) means row i IS a collapsed root; None
/// renders normally. `hidden` counts 1 (the root itself) + every
/// descendant folded under it — the "N from outside your network" line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CollapsePlan {
    hidden_by: Option<usize>,
    hidden: usize,
}

/// The collapse rule (issue #2 WS3, Amethyst ThreadFeedView model): with
/// the pref on, a reply whose author the graph KNOWS is NotInNetwork
/// starts collapsed; its descendants fold under it and are counted (not
/// deleted — one click reveals them). Follows/Self/Trusted never collapse;
/// Unknown authors never collapse (informed absence of judgment); an
/// explicitly expanded reply stays open.
fn collapse_plan(
    replies: &[(Event, usize)],
    outside: &impl Fn(&PublicKey) -> bool,
    expanded: &HashSet<EventId>,
    enabled: bool,
) -> Vec<CollapsePlan> {
    let mut plan = replies
        .iter()
        .map(|_| CollapsePlan {
            hidden_by: None,
            hidden: 0,
        })
        .collect::<Vec<_>>();
    // Roots first: each out-of-network, not-yet-expanded reply collapses.
    for (i, (ev, _)) in replies.iter().enumerate() {
        if enabled && !expanded.contains(&ev.id) && outside(&ev.pubkey) {
            plan[i] = CollapsePlan {
                hidden_by: Some(i),
                hidden: 1,
            };
        }
    }
    // Descendants fold under their nearest collapsed ancestor (the flat
    // list is pre-ordered, so a subtree is a contiguous run of rows deeper
    // than the ancestor's).
    for j in 0..plan.len() {
        if plan[j].hidden_by != Some(j) {
            continue;
        }
        let depth = replies[j].1;
        for i in j + 1..replies.len() {
            if replies[i].1 <= depth {
                break;
            }
            plan[i].hidden_by = Some(j);
            plan[j].hidden += 1;
        }
    }
    plan
}

// The card takes the full render-time snapshot — one struct would hide the
// shape, and GPUI's no-entity-reads-during-render rule makes them explicit.
#[allow(clippy::too_many_arguments)]
fn reply_row(
    idx: usize,
    ev: &Event,
    depth: usize,
    name: String,
    parent_whisper: Option<String>,
    media: &media::Snapshot,
    quotes: &content::Quotes,
    profiles: &HashMap<String, Profile>,
    time: String,
    state: actions::ActionState,
    handle: Entity<ThreadView>,
) -> impl IntoElement {
    let picture = profiles
        .get(&ev.pubkey.to_string())
        .map(|p| p.picture.clone())
        .unwrap_or_default();
    let ev_for_click = ev.clone();
    let open_handle = handle.clone();
    let body_handle = handle.clone();
    // thread.rs comment-children: 20px for the first level, 14px deeper.
    let indent = if depth == 0 {
        0.0
    } else {
        20.0 + 14.0 * (depth - 1) as f32
    };

    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .pl(px(indent))
        .pr_4()
        .py_3()
        // The lazy-parent whisper (render/note.ts reply-context): "↩ in
        // reply to an earlier note…" until the background fetch lands.
        .children(parent_whisper.map(|w| {
            div()
                .text_size(px(theme::T_SMALL))
                .text_color(theme::ink_faint())
                .child(w)
        }))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(media::avatar(media, &picture, &name, 24.0))
                        .child(
                            div()
                                .text_size(px(theme::T_UI))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(theme::ink()))
                                .child(name),
                        ),
                )
                .child(
                    // The timestamp is the third mode toggle (timefmt.rs):
                    // tap cycles relative → date → block height.
                    div()
                        .id(SharedString::from(format!("reply-time-{idx}")))
                        .cursor_pointer()
                        .text_size(px(theme::T_SMALL))
                        .text_color(rgb(theme::muted()))
                        .on_click(|_, _, cx| crate::timefmt::cycle(cx))
                        .child(time),
                ),
        )
        .child(
            div()
                .id(SharedString::from(format!("reply-{idx}")))
                .cursor_pointer()
                .on_click(move |_, _, cx| {
                    let ev = ev_for_click.clone();
                    // A reply opens the conversation at its ROOT (the
                    // reference never links the immediate parent) — unless
                    // we're already reading that root's thread.
                    open_handle.update(cx, |view, cx| {
                        let root = satori_core::nip::nip10::reply_root(&ev)
                            .and_then(|(id, _)| EventId::from_hex(&id).ok())
                            .unwrap_or(ev.id);
                        if view.anchor.as_ref().map(|a| a.id) != Some(root) {
                            cx.emit(Nav::OpenThread(root));
                        }
                    });
                })
                .child(content::body(
                    ev,
                    &content::Body {
                        media,
                        quotes,
                        profiles,
                    },
                    body_handle,
                )),
        )
        .child(actions::action_row(
            &ev.id.to_string(),
            &std::sync::Arc::new(ev.clone()),
            state,
            handle,
        ))
}

impl Render for ThreadView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let handle = cx.entity();
        let status = self.status.clone();
        let replies_empty = self.replies.is_empty();
        let replies = self.replies.clone();
        let profiles = self.profiles.clone();
        let media = media::Snapshot::new(cx);
        // Evicted media comes back on render — a thread is bounded content,
        // so sweep everything it paints (Ready/not-media URLs are silent
        // no-ops in fetch; only a landing notifies, so no render loop).
        let viewer = cx.entity_id();
        for p in self.profiles.values() {
            media::fetch(&self.core, p.picture.clone(), viewer, cx);
        }
        for a in self.anchor.as_ref().into_iter() {
            for url in content::media_urls_cached(a).iter().cloned() {
                media::fetch(&self.core, url, viewer, cx);
            }
        }
        for (ev, _) in &self.replies {
            for url in content::media_urls_cached(ev).iter().cloned() {
                media::fetch(&self.core, url, viewer, cx);
            }
        }
        let quotes = self.quotes.clone();
        let anchor = self.anchor.clone();
        let anchor_handle = handle.clone();
        let reply_all_handle = handle.clone();
        // The action row's glyph states, snapshotted once per render
        // (mirrors mutated optimistically by the ActionHost impl).
        let (liked, reposted, bookmarked) = (
            self.liked.clone(),
            self.reposted.clone(),
            self.bookmarked.clone(),
        );
        let (zapped, zap_pending) = (self.zapped.clone(), self.zap_pending.clone());
        let wallet_ready = self.wallet_ready;
        let reduced_motion = actions::reduced_motion(cx);
        let note_state = move |note: &Event| actions::ActionState {
            liked: liked.contains(&note.id.to_string()),
            reposted: reposted.contains(&note.id.to_string()),
            bookmarked: bookmarked.contains(&note.id.to_string()),
            zapped: zapped.contains(&note.id.to_string()),
            zapping: zap_pending.contains(&note.id.to_string()),
            wallet_ready,
            reduced_motion,
        };
        // The WS3 collapse line's click target (the list closure moves its
        // own clone; `handle` stays for the anchor above).
        let collapse_handle = handle.clone();
        // Lazy-parent whispers (want_parents): computed once per render —
        // the generic line until the fetch lands, then author + snippet.
        let present: HashSet<String> = replies
            .iter()
            .map(|(e, _)| e.id.to_string())
            .chain(anchor.iter().map(|a| a.id.to_string()))
            .collect();
        let whispers: Vec<Option<String>> = replies
            .iter()
            .map(|(ev, _)| {
                direct_parent_id(ev)
                    .filter(|pid| !present.contains(pid))
                    .map(|pid| self.parent_whisper(&pid))
            })
            .collect();
        let anchor_whisper = anchor
            .as_ref()
            .and_then(direct_parent_id)
            .filter(|pid| !present.contains(pid))
            .map(|pid| self.parent_whisper(&pid));
        // WS3 collapse plan: out-of-network replies start collapsed, their
        // descendants folded under them and counted (never deleted). The
        // pref (`thread_collapse`, default ON) turns the whole rule off.
        let enabled = crate::prefs::get_bool_or("thread_collapse", true);
        let outside = |pk: &PublicKey| self.core.verdict(pk) == Verdict::NotInNetwork;
        let plans = collapse_plan(&replies, &outside, &self.expanded.clone(), enabled);

        div()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    // The centered timeline column (the web app's reading
                    // width) — anchor and replies ride it together.
                    .items_center()
                    .child(match anchor {
                        Some(note) => {
                            let anchor_ev = std::sync::Arc::new(note.clone());
                            div()
                                .w_full()
                                .max_w(px(content::COL_W))
                                .flex()
                                .flex_col()
                                .gap_2()
                                .p_4()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(media::avatar(
                                            &media,
                                            &self
                                                .profiles
                                                .get(&note.pubkey.to_string())
                                                .map(|p| p.picture.clone())
                                                .unwrap_or_default(),
                                            &self.name_for(&note),
                                            28.0,
                                        ))
                                        .child(
                                            div()
                                                .text_size(px(theme::T_UI))
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(rgb(theme::ink()))
                                                .child(self.name_for(&note)),
                                        ),
                                )
                                .children(anchor_whisper.map(|w| {
                                    div()
                                        .text_size(px(theme::T_SMALL))
                                        .text_color(theme::ink_faint())
                                        .child(w)
                                }))
                                .child(content::body(
                                    &note,
                                    &content::Body {
                                        media: &media,
                                        quotes: &quotes,
                                        profiles: &profiles,
                                    },
                                    handle.clone(),
                                ))
                                .child(actions::action_row(
                                    &note.id.to_string(),
                                    &anchor_ev,
                                    note_state(&note),
                                    reply_all_handle,
                                ))
                                .into_any_element()
                        }
                        None => div()
                            .w_full()
                            .max_w(px(content::COL_W))
                            .p_4()
                            .text_color(rgb(theme::muted()))
                            .child(status.clone())
                            .into_any_element(),
                    })
                    // §4.3: the kinded error strip on the reading column.
                    .when_some(self.error.clone(), |d, err| {
                        let retry = handle.clone();
                        let dismiss = handle.clone();
                        d.child(div().w_full().max_w(px(content::COL_W)).child(
                            states::error_strip(
                                &err,
                                // Retry: re-ask the anchor and the thread.
                                move |_, _, cx| {
                                    retry.update(cx, |view, cx| {
                                        view.error = None;
                                        view.pump(
                                            view.core.event_by_id(view.anchor_id, Vec::new(), None),
                                            cx,
                                        );
                                        view.pump(view.core.thread(view.anchor_id), cx);
                                    });
                                },
                                move |_, _, cx| {
                                    dismiss.update(cx, |view, cx| {
                                        view.error = None;
                                        cx.notify();
                                    });
                                },
                            ),
                        ))
                    })
                    .child(if replies_empty {
                        div()
                            .w_full()
                            .max_w(px(content::COL_W))
                            .p_4()
                            .text_color(rgb(theme::muted()))
                            .child(if status.is_empty() {
                                "No replies yet.".to_string()
                            } else {
                                status.clone()
                            })
                            .into_any_element()
                    } else {
                        list(self.list.clone(), move |idx, _w, cx| {
                            match replies.get(idx) {
                                Some((ev, depth)) => {
                                    // WS3: the collapse projection — a
                                    // collapsed root renders the count line
                                    // (whisper style, the ↩ house model); a
                                    // row folded under one renders nothing.
                                    match plans[idx].hidden_by {
                                        Some(j) if j == idx => {
                                            let ev = ev.clone();
                                            let n = plans[idx].hidden;
                                            let collapse_handle = collapse_handle.clone();
                                            div()
                                                .w_full()
                                                .max_w(px(content::COL_W))
                                                .pl(px(20.0))
                                                .py_1()
                                                .child(
                                                    theme::pressable(
                                                        div()
                                                            .id(SharedString::from(format!(
                                                                "collapse-{idx}"
                                                            )))
                                                            .cursor_pointer()
                                                            .text_size(px(theme::T_SMALL)),
                                                    )
                                                    .on_click(move |_, _, cx| {
                                                        collapse_handle.update(cx, |view, cx| {
                                                            view.expanded.insert(ev.id);
                                                            cx.notify();
                                                        });
                                                    })
                                                    .child(format!(
                                                        "{n} from outside your network, expand"
                                                    )),
                                                )
                                                .into_any_element()
                                        }
                                        Some(_) => div().into_any_element(),
                                        None => {
                                            let key = ev.pubkey.to_string();
                                            let name = profiles
                                                .get(&key)
                                                .map(|p| p.display())
                                                .unwrap_or_else(|| format!("{}…", &key[..8]));
                                            // The stamp reads the current mode +
                                            // blocktime cache per frame; ensure
                                            // refines Block mode from the bridge.
                                            let secs = ev.created_at.as_secs();
                                            let time = crate::timefmt::stamp(secs, cx);
                                            crate::timefmt::ensure(secs, cx);
                                            reply_row(
                                                idx,
                                                ev,
                                                *depth,
                                                name,
                                                whispers.get(idx).cloned().flatten(),
                                                &media,
                                                &quotes,
                                                &profiles,
                                                time,
                                                note_state(ev),
                                                anchor_handle.clone(),
                                            )
                                            .into_any_element()
                                        }
                                    }
                                }
                                None => div().into_any_element(),
                            }
                        })
                        .w_full()
                        .max_w(px(content::COL_W))
                        .h_full()
                        .into_any_element()
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> Keys {
        Keys::generate()
    }

    /// One kind-1 note, optionally an NIP-10 reply to `parent`.
    fn note(author: &Keys, parent: Option<&Event>) -> Event {
        let mut t = Tags::new();
        if let Some(p) = parent {
            t.push(Tag::event(p.id));
        }
        let unsigned = UnsignedEvent::new(
            author.public_key(),
            Timestamp::from_secs(1),
            Kind::TextNote,
            t,
            "a reply body long enough to be a note".to_string(),
        );
        author.sign_event(unsigned).unwrap()
    }

    fn plan(
        replies: &[(Event, usize)],
        outside: &[PublicKey],
        expanded: &HashSet<EventId>,
        enabled: bool,
    ) -> Vec<CollapsePlan> {
        collapse_plan(replies, &|pk| outside.contains(pk), expanded, enabled)
    }

    /// The rule: out-of-network roots collapse (counting themselves);
    /// follows/self/unknown never collapse; Unknown authors pass (informed
    /// absence of judgment); a collapsed subtree folds and is COUNTED,
    /// never deleted; the pref off renders everything.
    #[test]
    fn collapse_counting() {
        let known = keys(); // NotInNetwork author (the graph knows: outside)
        let unseen = keys(); // Unknown author (the graph can't say)
        let root = note(&known, None);
        let child = note(&unseen, Some(&root)); // Unknown under a collapsed root
        let replies = vec![(root, 0), (child, 1)];

        // Both fold under the root's collapse: N = 2 (root + descendant).
        let p = plan(&replies, &[known.public_key()], &HashSet::new(), true);
        assert_eq!(p[0].hidden_by, Some(0));
        assert_eq!(p[0].hidden, 2);
        assert_eq!(p[1].hidden_by, Some(0));

        // Expanded: the user's click is a state change — everything shows.
        let mut expanded = HashSet::new();
        expanded.insert(replies[0].0.id);
        let p = plan(&replies, &[known.public_key()], &expanded, true);
        assert_eq!(p[0].hidden_by, None);
        assert_eq!(p[1].hidden_by, None);

        // An Unknown author never collapses (not a judgment).
        let r = note(&unseen, None);
        let p = plan(
            &[(r.clone(), 0)],
            &[known.public_key()],
            &HashSet::new(),
            true,
        );
        assert_eq!(
            p[0],
            CollapsePlan {
                hidden_by: None,
                hidden: 0
            }
        );

        // The pref off: nothing collapses (feature-off is a first-class state).
        let p = plan(&replies, &[known.public_key()], &HashSet::new(), false);
        assert!(p.iter().all(|p| p.hidden_by.is_none()));

        // Two separate out-of-network roots each count their own subtree.
        let r2 = note(&known, None);
        let replies = vec![(replies[0].0.clone(), 0), (r2, 0)];
        let p = plan(&replies, &[known.public_key()], &HashSet::new(), true);
        assert_eq!(p[0].hidden_by, Some(0));
        assert_eq!(p[0].hidden, 1);
        assert_eq!(p[1].hidden_by, Some(1));
        assert_eq!(p[1].hidden, 1);
    }
}
