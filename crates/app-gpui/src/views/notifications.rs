//! Notifications — port of crates/app/src/notifications.rs (and
//! src/routes/notifications.ts): replies + mentions newest-first as compact
//! cards, mark-seen on open, the New/Earlier split at the pre-visit
//! high-water, "Nothing new. Sit quietly." when empty. The header bell dot
//! polls every 90s from the shell (shell.rs poll_notif).

use std::{collections::HashMap, sync::Arc};

use std::collections::HashSet;

use gpui::{
    Context, Entity, EventEmitter, FontWeight, ListAlignment, ListOffset, ListScrollEvent,
    ListState, Render, SharedString, Window, div, list, prelude::*, px, rgb,
};
use nostr::prelude::*;
use satori_core::manager::{Core, FeedEvent, FeedRx, ParentNote, Profile, PublishEvent};
use satori_core::nip::{comment, nip10};
use satori_core::notifications::parse_zap_receipt;
use satori_core::wallet::{Status, WalletEvent};

use super::{actions, content, nav::Nav, states};
use crate::{media, theme};

/// What the event did to you (notifications.ts `add`): a comment is always
/// a reply; kind 1 splits by whether it carries an `e` tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotifType {
    Reply,
    Mention,
    Reaction,
    PollVote,
    Zap,
}

fn notif_type(ev: &Event) -> NotifType {
    match u16::from(ev.kind) {
        9735 => NotifType::Zap,
        7 => NotifType::Reaction,
        1018 => NotifType::PollVote,
        1111 => NotifType::Reply,
        _ => {
            let has_e = ev
                .tags
                .iter()
                .any(|t| t.as_slice().first().map(String::as_str) == Some("e"));
            if has_e {
                NotifType::Reply
            } else {
                NotifType::Mention
            }
        }
    }
}

fn action_word(t: NotifType, ev: &Event) -> String {
    match t {
        NotifType::Reply => "replied".into(),
        NotifType::Mention => "mentioned you".into(),
        NotifType::Reaction => "reacted".into(),
        NotifType::PollVote => "voted in your poll".into(),
        NotifType::Zap => {
            let info = parse_zap_receipt(ev);
            if info.sats == 0 {
                "zapped".into()
            } else {
                format!("zapped {} sats", info.sats)
            }
        }
    }
}

/// One list row: a section label (New / Earlier) or a notification card.
#[derive(Clone)]
enum Row {
    Label(&'static str),
    Note(Event, NotifType),
}

pub struct NotificationsView {
    core: Arc<Core>,
    rows: Vec<Row>,
    profiles: HashMap<String, Profile>,
    quotes: content::Quotes,
    /// What each notification answers (feed.rs's same resolver): id →
    /// author + snippet. `parents_asked` keeps misses from re-requesting.
    parents: HashMap<String, ParentNote>,
    parents_asked: HashSet<String>,
    me: String,
    status: String,
    /// §4.3: the kinded error strip (replaces "Could not load …: {e}").
    error: Option<states::PaneError>,
    /// The pre-visit high-water this pane was opened with — Retry re-asks
    /// with the same one (the New/Earlier split must not shift on a retry).
    prev: u64,
    /// Deep-scroll state, emitted to the shell (Nav::PaneDeep) so the
    /// floating back-to-top whisper covers this pane too — same shape as
    /// the feed's (feed.rs deep).
    pub(crate) deep: bool,
    /// WS4 receipts: the bell's audit trail open? (the "N hidden, see
    /// why" strip at the pane's bottom, a silent void is a bug).
    receipts_open: bool,
    /// The action-row mirrors (feed.rs's, thread.rs's shape): optimistic
    /// glyph states, reconciled by the wallet broadcast and the publish
    /// pumps. The rows ARE notes (replies + mentions) — #16's one-row
    /// rule: the notifications page carries the timeline's affordances.
    liked: HashSet<String>,
    reposted: HashSet<String>,
    bookmarked: HashSet<String>,
    zapped: HashSet<String>,
    /// Session zap-in-flight set; the wallet broadcast clears it.
    zap_pending: HashSet<String>,
    wallet_ready: bool,
    list: ListState,
}

impl EventEmitter<Nav> for NotificationsView {}

impl NotificationsView {
    /// Fetched at construction — a GPUI pane is built when shown, so this is
    /// the GTK page's fetch-on-show hook. Opening also marks seen: the
    /// pre-visit high-water splits the page into New / Earlier.
    pub fn new(core: Arc<Core>, cx: &mut Context<Self>) -> Self {
        crate::theme::watch(cx); // repaint on theme change
        crate::timefmt::watch(cx); // repaint on mode/height change
        crate::video::watch(cx); // repaint on player open/toggle/close
        let prev = satori_core::read_state::load().notif;
        core.mark_notifications_seen();
        let liked = core.liked_set();
        let reposted = core.reposted_set();
        let bookmarked = core.bookmarked_set();
        let zapped = core.zapped_set();
        let wallet_ready = core.wallet().is_ready();
        let view = Self {
            core,
            rows: vec![],
            profiles: HashMap::new(),
            quotes: HashMap::new(),
            parents: HashMap::new(),
            parents_asked: HashSet::new(),
            me: String::new(),
            status: states::loading("Gathering…"),
            error: None,
            prev,
            deep: false,
            receipts_open: false,
            liked,
            reposted,
            bookmarked,
            zapped,
            zap_pending: HashSet::new(),
            wallet_ready,
            list: ListState::new(0, ListAlignment::Top, px(400.)),
        };
        view.pump_at(prev, cx);
        // Deep-scroll whisper: same threshold and emission shape as the
        // feed's (feed.rs set_scroll_handler) — emit on change only. The
        // threshold is shallow (three rows): the button should arrive
        // early, not after the reader has given up looking for it.
        let this = cx.entity().downgrade();
        view.list
            .set_scroll_handler(move |ev: &ListScrollEvent, _w, cx| {
                let _ = this.update(cx, |n, cx| {
                    let deep = ev.visible_range.start >= 3;
                    if n.deep != deep {
                        n.deep = deep;
                        cx.emit(Nav::PaneDeep);
                    }
                });
            });
        // The wallet broadcast (D20): the rows' bolts follow every zap,
        // theirs or the sheet's, wherever it started (thread.rs's
        // subscription, verbatim shape).
        let this = cx.entity().downgrade();
        let mut wallet_rx = view.core.subscribe_wallet();
        cx.spawn(async move |_, cx| {
            loop {
                match wallet_rx.recv().await {
                    Ok(ev) => {
                        let _ = this.update(cx, |view, cx| view.on_wallet(ev, cx));
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(_) => continue, // lagged: the next event re-syncs
                }
            }
        })
        .detach();
        view
    }

    /// Back to the top (rail re-click, the floating whisper): home is item
    /// zero (feed.rs scroll_top — no paging anchor to reset here).
    pub fn scroll_top(&mut self) {
        self.deep = false;
        self.list.scroll_to(ListOffset {
            item_ix: 0,
            offset_in_item: px(0.),
        });
    }

    fn pump_at(&self, prev_high_water: u64, cx: &mut Context<Self>) {
        let mut rx = self.core.notifications();
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.recv().await {
                let _ = this.update(cx, |this, cx| this.on_event(ev, prev_high_water, cx));
            }
        })
        .detach();
    }

    /// Ask the core to resolve the notes these notifications answer
    /// (feed.rs want_parents: store first, one relay round trip for
    /// misses, asked-set keeps it from looping).
    fn want_parents(&mut self, cx: &mut Context<Self>) {
        let mut ids: Vec<String> = Vec::new();
        for row in &self.rows {
            if let Row::Note(ev, _) = row
                && let Some(pid) = notif_parent_id(ev)
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
                let _ = this.update(cx, |this, cx| this.on_event(ev, 0, cx));
            }
        })
        .detach();
    }

    fn on_event(&mut self, ev: FeedEvent, prev: u64, cx: &mut Context<Self>) {
        match ev {
            FeedEvent::Notifications(notifs, profiles) => {
                // Each event is a full page — the cached paint, then its
                // UNION with the relay results (a superset: the list only
                // grows, nothing reloads). Rebuild on each; the manager
                // owns the merge. The manager gates the types (kind-1
                // replies, NIP-22 comments, mutes); the view renders
                // whatever that stream vouches for.
                let notifs: Vec<Event> = notifs
                    .into_iter()
                    // Zap receipts have empty content — the label carries
                    // them (parse_zap_receipt does the trust work).
                    .filter(|e| !e.content.trim().is_empty() || u16::from(e.kind) == 9735)
                    .collect();
                for p in profiles {
                    self.profiles.insert(p.pubkey.clone(), p);
                }
                for n in &notifs {
                    self.want_media(n, cx);
                    self.want_quotes(n, cx);
                }
                self.rows = sectioned(prev, &notifs);
                self.me = self.core.me().unwrap_or_default();
                self.want_parents(cx);
                self.want_receipt_names(cx);
                self.status.clear();
                self.error = None;
            }
            FeedEvent::Parents(p) => {
                // The resolver's answer (store-first, relay for misses) —
                // whispers fill in as these land.
                self.parents.extend(p);
                cx.notify();
            }
            FeedEvent::Failed(e) => {
                // §4.3: transient — Retry re-asks with the same high-water.
                self.error = Some(states::PaneError::transient("Notifications", &e));
            }
            _ => {}
        }
        self.list.reset(self.rows.len());
        cx.notify();
    }

    fn want_media(&mut self, note: &Event, cx: &mut Context<Self>) {
        for url in content::media_urls_cached(note).iter().cloned() {
            media::fetch(&self.core, url, cx.entity_id(), cx);
        }
    }

    /// WS4 receipts: resolve the suppressed authors' names so the audit
    /// trail shows who, not a bare npub (one ensure_profiles pump; the
    /// render-side strip reads the same profiles map).
    fn want_receipt_names(&mut self, cx: &mut Context<Self>) {
        let missing: Vec<PublicKey> = self
            .core
            .suppressed_notifications()
            .iter()
            .map(|s| s.event.pubkey)
            .filter(|pk| !self.profiles.contains_key(&pk.to_string()))
            .collect();
        if missing.is_empty() {
            return;
        }
        let mut rx = self.core.ensure_profiles(missing);
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.recv().await {
                let _ = this.update(cx, |this, cx| this.on_event(ev, 0, cx));
            }
        })
        .detach();
    }

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

    /// The wallet broadcast → glyph states + the quiet status whisper
    /// (feed.rs's on_wallet, verbatim — the fields are the same mirrors).
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
}

/// What this notification answers — the parent id to resolve and whisper
/// about. A mention answers you directly (no parent); a reply points at
/// the note it replies to (nip10.rs reply_parent), a comment at its
/// lowercase-e parent, a zap at the zapped note (profile zaps have no
/// e tag), a poll vote at the poll.
fn notif_parent_id(ev: &Event) -> Option<String> {
    match u16::from(ev.kind) {
        1 => nip10::reply_parent(ev).map(|(id, _)| id),
        1111 => match comment::parse_refs(ev).parent {
            Some(comment::Ref::Event(id)) => Some(id.to_string()),
            _ => None,
        },
        9735 | 1018 => ev
            .tags
            .iter()
            .find(|t| t.as_slice().first().map(String::as_str) == Some("e"))
            .and_then(|t| t.as_slice().get(1).cloned()),
        _ => None,
    }
}

/// The row's toggles (actions.rs's state-blind contract; thread.rs's impl
/// is the template — optimistic mirrors, publish pumps, funnel through
/// core). The failure whispers land in the pane's status line.
impl actions::ActionHost for NotificationsView {
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
        let public = crate::prefs::get_bool("bookmarkPublic");
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

/// The parent reference, the timeline whisper's voice aimed at YOUR
/// things: "↳ on your note" / "↳ on {name}'s note", the parent's words
/// in serif italic beneath, clickable into the parent's thread.
/// Unresolved parents whisper nothing — the pump fills them in.
fn notif_whisper<V: EventEmitter<Nav> + 'static>(
    idx: usize,
    parent_id: &str,
    parents: &HashMap<String, ParentNote>,
    profiles: &HashMap<String, Profile>,
    me: &str,
    handle: Entity<V>,
) -> Option<gpui::AnyElement> {
    let parent = parents.get(parent_id)?;
    let label = if parent.author == me {
        "\u{21b3} on your note".to_string()
    } else {
        let name = profiles
            .get(&parent.author)
            .map(|p| p.display())
            .unwrap_or_else(|| format!("{}\u{2026}", &parent.author[..8]));
        format!("\u{21b3} on {name}'s note")
    };
    let snippet = (!parent.snippet.is_empty()).then(|| parent.snippet.clone());
    let id = parent_id.to_string();
    Some(
        div()
            .id(SharedString::from(format!("notif-parent-{idx}")))
            .cursor_pointer()
            .pl(px(36.0)) // under the avatar column, like the feed whisper
            .flex()
            .flex_col()
            .gap_0p5()
            .on_click(move |_, _, cx| {
                let id = id.clone();
                handle.update(cx, |_, cx| {
                    if let Ok(event_id) = EventId::from_hex(&id) {
                        cx.emit(Nav::OpenThread(event_id));
                    }
                });
            })
            .children({
                let mut kids = vec![
                    div()
                        .text_size(px(theme::T_SMALL))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::ink()))
                        .child(label)
                        .into_any_element(),
                ];
                if let Some(snippet) = snippet {
                    kids.push(
                        div()
                            .font_family(theme::SERIF)
                            .italic()
                            .text_size(px(theme::T_UI))
                            .text_color(rgb(theme::muted()))
                            .child(snippet)
                            .into_any_element(),
                    );
                }
                kids
            })
            .into_any_element(),
    )
}

/// A notification row. The action word lives in the body — it's the
/// news, not a timestamp garnish — and the parent whisper above it
/// references what the notification answers (the timeline's trick).
#[allow(clippy::too_many_arguments)]
fn notif_card(
    idx: usize,
    ev: &Event,
    t: NotifType,
    st: actions::ActionState,
    profiles: &HashMap<String, Profile>,
    parents: &HashMap<String, ParentNote>,
    media: &media::Snapshot,
    quotes: &content::Quotes,
    me: &str,
    time: String,
    handle: Entity<NotificationsView>,
) -> gpui::AnyElement {
    let key = ev.pubkey.to_string();
    let name = profiles
        .get(&key)
        .map(|p| p.display())
        .unwrap_or_else(|| format!("{}\u{2026}", &key[..8]));
    let picture = profiles
        .get(&key)
        .map(|p| p.picture.clone())
        .unwrap_or_default();
    let parent_id = notif_parent_id(ev);
    // Clicking opens the conversation: the parent's thread when there is
    // one (your note's thread, for a reply), else the note itself. A
    // profile zap has no thread to open.
    // The reference links a notice to the thread ROOT, never the immediate
    // parent — a reply-to-a-reply lands in the root's conversation.
    let open: Option<EventId> = satori_core::nip::nip10::reply_root(ev)
        .and_then(|(id, _)| EventId::from_hex(&id).ok())
        .or_else(|| parent_id.as_ref().and_then(|p| EventId::from_hex(p).ok()))
        .or_else(|| matches!(t, NotifType::Reply | NotifType::Mention).then_some(ev.id));
    let ev_for_profile = ev.clone();
    let handle_profile = handle.clone();
    let ctx = content::Body {
        media,
        quotes,
        profiles,
    };

    let mut row = div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .px_4()
        .py_3()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(media::avatar(media, &picture, &name, 28.0))
                .child(
                    div()
                        .id(SharedString::from(format!("notif-author-{idx}")))
                        .cursor_pointer()
                        .flex_1()
                        .text_size(px(theme::T_UI))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::ink()))
                        .on_click(move |_, _, cx| {
                            let ev = ev_for_profile.clone();
                            handle_profile.update(cx, |_, cx| cx.emit(Nav::OpenProfile(ev.pubkey)));
                        })
                        .child(name),
                )
                .child(
                    // The timestamp is the third mode toggle (timefmt.rs):
                    // tap cycles relative → date → block height.
                    div()
                        .id(SharedString::from(format!("notif-time-{idx}")))
                        .cursor_pointer()
                        .text_size(px(theme::T_SMALL))
                        .text_color(rgb(theme::muted()))
                        .on_click(|_, _, cx| crate::timefmt::cycle(cx))
                        .child(time),
                ),
        );
    if let Some(pid) = &parent_id
        && let Some(w) = notif_whisper(idx, pid, parents, profiles, me, handle.clone())
    {
        row = row.child(w);
    }
    let action_line = div()
        .text_size(px(theme::T_UI))
        .text_color(rgb(theme::muted()))
        .child(action_word(t, ev));
    let handle_body = handle.clone();
    let handle_actions = handle.clone();
    let open_block = match open {
        Some(target) => div()
            .id(SharedString::from(format!("notif-open-{idx}")))
            .cursor_pointer()
            .on_click(move |_, _, cx| {
                let target = target;
                handle.update(cx, |_, cx| cx.emit(Nav::OpenThread(target)));
            })
            .child(action_line)
            .child(content::body(ev, &ctx, handle_body.clone()))
            .into_any_element(),
        None => div()
            .child(action_line)
            .child(content::body(ev, &ctx, handle_body.clone()))
            .into_any_element(),
    };
    // The one-row rule (#16): reply and mention rows ARE notes — they
    // wear the timeline's full action row, so the most common response
    // to a notification (a like, a zap) doesn't demand opening the
    // thread. Receipt rows (reactions, zaps, poll votes) aren't notes;
    // no row for them.
    if !matches!(t, NotifType::Reply | NotifType::Mention) {
        return row.child(open_block).into_any_element();
    }
    let ev_arc = Arc::new(ev.clone());
    row.child(open_block)
        .child(actions::action_row(
            &ev.id.to_string(),
            &ev_arc,
            st,
            handle_actions,
        ))
        .into_any_element()
}

/// New / Earlier section labels at the high-water boundary
/// (routes/notifications.ts) — only when the page straddles it.
fn sectioned(prev: u64, notifs: &[Event]) -> Vec<Row> {
    let new_count = notifs
        .iter()
        .filter(|n| n.created_at.as_secs() > prev)
        .count();
    let split = (new_count > 0 && new_count < notifs.len()).then_some(new_count);
    notifs
        .iter()
        .enumerate()
        .flat_map(|(i, n)| {
            let label = match split {
                Some(at) if i == at => Some("Earlier"),
                _ if split.is_some() && i == 0 => Some("New"),
                _ => None,
            };
            let label = label.map(Row::Label);
            label
                .into_iter()
                .chain(std::iter::once(Row::Note(n.clone(), notif_type(n))))
                .collect::<Vec<_>>()
        })
        .collect()
}

impl Render for NotificationsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let handle = cx.entity();
        let status = self.status.clone();
        let rows = self.rows.clone();
        let profiles = self.profiles.clone();
        let media = media::Snapshot::new(cx);
        // Evicted media comes back on render — a notifications page is
        // bounded content, so sweep everything it paints (thread.rs render,
        // same shape): the rows' authors AND the notes' own media. The
        // sweep matters: want_media only runs at rebuild, and once the
        // cache evicted a note's image under feed-photo pressure NOTHING
        // asked again — the tile went dark mid-session (owner: the image
        // "first loads, then disappears"). fetch is a silent no-op for
        // entries the cache holds; only a landing notifies, so this can't
        // loop a render.
        let viewer = cx.entity_id();
        for p in self.profiles.values() {
            media::fetch(&self.core, p.picture.clone(), viewer, cx);
        }
        for row in &self.rows {
            if let Row::Note(ev, _) = row {
                for url in content::media_urls_cached(ev).iter().cloned() {
                    media::fetch(&self.core, url, viewer, cx);
                }
            }
        }
        let quotes = self.quotes.clone();
        let parents = self.parents.clone();
        let me = self.me.clone();
        // The action row's glyph states, snapshotted per render (feed.rs's
        // FeedSnap discipline, inline — the page is bounded content).
        let liked = self.liked.clone();
        let reposted = self.reposted.clone();
        let bookmarked = self.bookmarked.clone();
        let zapped = self.zapped.clone();
        let zap_pending = self.zap_pending.clone();
        let wallet_ready = self.wallet_ready;
        let reduced_motion = crate::desktop::Desktop::get(cx).reduced_motion;
        let empty = rows.is_empty();
        // WS4 receipts: the suppressed page (core's last notifications
        // build) — the pane's bottom strip lists each hidden item (author,
        // verdict, reason) with a per-item one-session "show anyway".
        let suppressed = self.core.suppressed_notifications();
        let receipts_open = self.receipts_open;
        let receipts: Vec<(String, String, String, String)> = suppressed
            .iter()
            .map(|s| {
                let key = s.event.pubkey.to_string();
                let name = self
                    .profiles
                    .get(&key)
                    .map(|p| p.display())
                    .unwrap_or_else(|| format!("{}…", &key[..8]));
                (
                    name,
                    action_word(notif_type(&s.event), &s.event),
                    s.reason.clone(),
                    key,
                )
            })
            .collect();

        div()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .bg(rgb(theme::washi()))
            .text_color(rgb(theme::ink()))
            .font_family(theme::SANS)
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .justify_center()
                    // §4.3: the kinded error strip — Retry re-asks with the same
                    // pre-visit high-water.
                    .when_some(self.error.clone(), |d, err| {
                        d.child(div().px_4().py_2().child(states::error_strip(
                            &err,
                            cx.listener(move |this, _, _, cx| {
                                let prev = this.prev;
                                this.error = None;
                                this.pump_at(prev, cx);
                                cx.notify();
                            }),
                            cx.listener(move |this, _, _, cx| {
                                this.error = None;
                                cx.notify();
                            }),
                        )))
                    })
                    .child(if empty {
                        div()
                            .w_full()
                            .max_w(px(content::COL_W))
                            .p_8()
                            .text_color(rgb(theme::muted()))
                            .child(if status.is_empty() {
                                "Nothing new. Sit quietly.".to_string()
                            } else {
                                status
                            })
                            .into_any_element()
                    } else {
                        let list_handle = handle.clone();
                        list(self.list.clone(), move |idx, _w, cx| {
                            match rows.get(idx) {
                                Some(Row::Label(text)) => div()
                                    .w_full()
                                    .max_w(px(content::COL_W))
                                    .px_4()
                                    .pt_3()
                                    .text_size(px(theme::T_SMALL))
                                    .font_family(theme::SERIF)
                                    .text_color(rgb(theme::muted()))
                                    .child(text.to_string())
                                    .into_any_element(),
                                Some(Row::Note(ev, t)) => {
                                    // The stamp reads the current mode +
                                    // blocktime cache per frame; ensure
                                    // refines Block mode from the bridge.
                                    let secs = ev.created_at.as_secs();
                                    let time = crate::timefmt::stamp(secs, cx);
                                    crate::timefmt::ensure(secs, cx);
                                    let id_hex = ev.id.to_string();
                                    let st = actions::ActionState {
                                        liked: liked.contains(&id_hex),
                                        reposted: reposted.contains(&id_hex),
                                        bookmarked: bookmarked.contains(&id_hex),
                                        zapped: zapped.contains(&id_hex),
                                        zapping: zap_pending.contains(&id_hex),
                                        wallet_ready,
                                        reduced_motion,
                                    };
                                    notif_card(
                                        idx,
                                        ev,
                                        *t,
                                        st,
                                        &profiles,
                                        &parents,
                                        &media,
                                        &quotes,
                                        &me,
                                        time,
                                        list_handle.clone(),
                                    )
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
            // WS4 receipts: "N hidden, see why" at the bottom of the pane
            // — the audit trail is the core requirement; a silent void is
            // a bug. Open, it lists every suppressed item (author, verdict,
            // reason) with a per-item one-session "show anyway".
            .when(!suppressed.is_empty(), |d| {
                let mut strip = div()
                    .w_full()
                    .flex_none()
                    .border_t_1()
                    .border_color(rgb(theme::hairline()))
                    .flex()
                    .flex_col()
                    .px_4()
                    .py_2()
                    .child(
                        div()
                            .id("receipts-toggle")
                            .cursor_pointer()
                            .text_size(px(theme::T_SMALL))
                            .text_color(rgb(theme::muted()))
                            .hover(|s| s.text_color(rgb(theme::ink())))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.receipts_open = !this.receipts_open;
                                cx.notify();
                            }))
                            .child(if receipts_open {
                                format!("{} hidden, close", suppressed.len())
                            } else {
                                format!("{} hidden, see why", suppressed.len())
                            }),
                    );
                if receipts_open {
                    for (i, (name, action, reason, key)) in receipts.iter().enumerate() {
                        let hex = key.clone();
                        strip = strip.child(
                            div()
                                .w_full()
                                .flex()
                                .items_center()
                                .gap_2()
                                .py_0p5()
                                .child(
                                    div()
                                        .text_size(px(theme::T_UI))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(rgb(theme::ink()))
                                        .child(name.clone()),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_size(px(theme::T_SMALL))
                                        .text_color(rgb(theme::muted()))
                                        .child(format!("{action} · {reason}")),
                                )
                                .child(
                                    div()
                                        .id(SharedString::from(format!("receipt-show-{i}")))
                                        .cursor_pointer()
                                        .text_size(px(theme::T_SMALL))
                                        .child(theme::pressable(div().child("show anyway")))
                                        .on_click({
                                            let handle = handle.clone();
                                            move |_, _, cx| {
                                                let hex = hex.clone();
                                                handle.update(cx, |view, cx| {
                                                    // One-session exemption: the
                                                    // tier stops gating this author
                                                    // until sign-out; the pane
                                                    // re-asks so the item paints.
                                                    view.core.show_suppressed_author(&hex);
                                                    let prev = view.prev;
                                                    view.pump_at(prev, cx);
                                                    cx.notify();
                                                });
                                            }
                                        }),
                                ),
                        );
                    }
                }
                d.child(strip)
            })
    }
}
