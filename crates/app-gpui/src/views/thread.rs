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
    Context, Entity, EventEmitter, FontWeight, ListAlignment, ListScrollEvent, ListState, Render,
    SharedString, Window, div, list, prelude::*, px, rgb,
};
use nostr::prelude::*;
use satori_core::manager::{Core, FeedEvent, FeedRx, ParentNote, Profile};
use satori_core::wot::Verdict;

use super::{actions, content, engagement, nav::Nav, quotes, states};
use crate::{media, theme};

pub struct ThreadView {
    core: Arc<Core>,
    anchor: Option<Event>,
    /// Replies flattened pre-order with their NIP-10 depth (nest_replies).
    replies: Vec<(Event, usize)>,
    /// One-session expanded set for the WS3 collapse: a reply the user
    /// expanded stays open until the pane closes (click is a state change,
    /// not a re-hide).
    expanded: HashSet<EventId>,
    profiles: HashMap<PublicKey, Profile>,
    quotes: content::Quotes,
    status: String,
    /// §4.3: the kinded error strip (replaces the raw `… {e}` status dump).
    error: Option<states::PaneError>,
    /// The pane's anchor — Retry re-asks it (and the thread) by id.
    /// `None` in the address door's mode (NIP-22): the anchor rides a
    /// coordinate until it lands and learns its id.
    anchor_id: Option<EventId>,
    /// The address the anchor is fetched by (the coordinate door's
    /// variant); Retry re-asks by address while the anchor is unknown.
    addr: Option<(u16, PublicKey, String)>,
    /// Lazy parent hydration (the webapp's "↩ in reply to an earlier note",
    /// render/note.ts reply-context): direct parents that aren't in the
    /// thread — id → author + snippet, asked-set keeps misses from looping.
    parents: HashMap<String, ParentNote>,
    parents_asked: HashSet<String>,
    list: ListState,
    /// The action row's mirrors (engagement.rs): seeded from core's
    /// persisted engagement sets at construction, kept optimistic per
    /// toggle.
    engagement: engagement::Engagement,
    /// Render snapshot (feed.rs's FeedSnap discipline, #31's port): the
    /// render used to deep-clone the whole reply tree, the profiles map
    /// and the five engagement sets per repaint — a popular thread with
    /// images loading paid a full deep copy per landing. Rebuilt only when
    /// the data changed.
    snap: Option<ThreadSnap>,
    snap_dirty: bool,
    /// The visible reply-row window (notifications.rs's fields): the
    /// scroll stream keeps it current; the media pins read it.
    first_visible_start: usize,
    last_visible_end: usize,
    /// Thread option B (Nov 27 2026): a reply opens FOCUSED, its parent
    /// chain (root first, `Core::ancestors`) compact above it, expandable,
    /// with a door to the full thread at the root.
    ancestors: Vec<Event>,
    /// The walk stopped at an unreachable parent.
    ancestors_gap: bool,
    /// The chain is expanded (collapsed to a count line by default).
    ancestors_open: bool,
    /// The walk was started for this anchor (once per pane).
    ancestors_asked: bool,
}

/// The render-time snapshot: everything the item closure reads, rebuilt
/// (one deep copy) only when the data changed instead of every frame; the
/// Arcs make the per-frame clone a refcount bump.
struct ThreadSnap {
    anchor: std::sync::Arc<Option<Event>>,
    replies: std::sync::Arc<Vec<(Event, usize)>>,
    profiles: std::sync::Arc<HashMap<PublicKey, Profile>>,
    quotes: std::sync::Arc<content::Quotes>,
    engagement: std::sync::Arc<engagement::Engagement>,
}

impl EventEmitter<Nav> for ThreadView {}

/// The thread's toggles (actions.rs): the optimistic-mirror discipline —
/// flip the glyph now, reconcile against the publish stream later. The
/// behavior itself lives in engagement.rs (the extracted engine); the
/// thread has no deltas, so its Host impl is just the plumbing.
impl actions::ActionHost for ThreadView {
    fn toggle_like(&mut self, ev: &Event, cx: &mut Context<Self>) {
        engagement::toggle_like(self, ev, cx)
    }
    fn toggle_reaction_picker(&mut self, id: EventId, cx: &mut Context<Self>) {
        engagement::toggle_reaction_picker(self, id, cx)
    }
    fn react_with(&mut self, ev: &Event, content: &str, cx: &mut Context<Self>) {
        engagement::react_with(self, ev, content, cx)
    }
    fn toggle_repost(&mut self, ev: &Event, cx: &mut Context<Self>) {
        engagement::toggle_repost(self, ev, cx)
    }
    fn toggle_bookmark(&mut self, ev: &Event, cx: &mut Context<Self>) {
        engagement::toggle_bookmark(self, ev, cx)
    }
    fn zap_tap(&mut self, ev: &Event, cx: &mut Context<Self>) {
        engagement::zap_tap(self, ev, cx)
    }
}

impl engagement::Host for ThreadView {
    fn engagement(&mut self) -> &mut engagement::Engagement {
        &mut self.engagement
    }
    fn core(&self) -> &Arc<Core> {
        &self.core
    }
    fn whisper(&mut self, msg: String) {
        self.status = msg;
    }
    fn touched(&mut self) {
        self.touch();
    }
}

// A quote embed landing invalidates the snapshot (the reply list may
// re-measure) — the same touch discipline as engagement.
impl quotes::Host for ThreadView {
    fn quotes(&mut self) -> &mut content::Quotes {
        &mut self.quotes
    }
    fn core(&self) -> &Arc<Core> {
        &self.core
    }
    fn profiles(&self) -> &HashMap<PublicKey, Profile> {
        &self.profiles
    }
    fn touched(&mut self) {
        self.touch();
    }
}

impl ThreadView {
    pub fn new(core: Arc<Core>, id: EventId, cx: &mut Context<Self>) -> Self {
        let view = Self::build(core, None, Some(id), cx);
        // The anchor rides in with its author's profile; replies follow.
        view.pump(view.core.event_by_id(id, Vec::new(), None), cx);
        view.pump(view.core.thread(id), cx);
        view
    }

    /// The address door (NIP-22): the anchor is an ADDRESSABLE event,
    /// fetched by its coordinate; when it lands it learns its id and the
    /// replies follow (the landing wires that itself). A long-form anchor
    /// redirects into the reader (the thread's landing rule).
    pub fn new_addr(
        core: Arc<Core>,
        kind: u16,
        who: PublicKey,
        d: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let view = Self::build(core, Some((kind, who, d.clone())), None, cx);
        view.pump(view.core.event_by_address(kind, who, d), cx);
        view
    }

    /// The pane's bones + the wallet broadcast pump (both constructors).
    fn build(
        core: Arc<Core>,
        addr: Option<(u16, PublicKey, String)>,
        anchor_id: Option<EventId>,
        cx: &mut Context<Self>,
    ) -> Self {
        crate::theme::watch(cx); // repaint on theme change
        crate::theme::watch_list(cx, |v: &Self| &v.list);
        crate::timefmt::watch(cx); // repaint on mode/height change
        crate::video::watch(cx); // repaint on player open/toggle/close
        let engagement = engagement::Engagement::new(&core);
        let view = Self {
            core,
            anchor: None,
            replies: vec![],
            expanded: HashSet::new(),
            profiles: HashMap::new(),
            quotes: HashMap::new(),
            status: "loading the thread…".into(),
            error: None,
            anchor_id,
            addr,
            parents: HashMap::new(),
            parents_asked: HashSet::new(),
            list: ListState::new(0, ListAlignment::Top, px(400.)),
            engagement,
            snap: None,
            snap_dirty: true,
            first_visible_start: 0,
            ancestors: Vec::new(),
            ancestors_gap: false,
            ancestors_open: false,
            ancestors_asked: false,
            last_visible_end: 0,
        };
        // The wallet broadcast (D20): the thread's bolts follow every zap,
        // theirs or the sheet's, wherever it started.
        engagement::pump_wallet(&view.core, &cx.entity(), cx);
        // The visible window (notifications.rs's shape): the scroll stream
        // tracks which reply rows are on screen so the media pins stay
        // window-scoped (D60's rule — the thread was the last whole-set
        // pinner, and a photo thread blew the per-owner cap, leaving the
        // tail unpinned: the flash-then-refetch flicker).
        let this = cx.entity().downgrade();
        view.list
            .set_scroll_handler(move |ev: &ListScrollEvent, _w, cx| {
                let _ = this.update(cx, |n, cx| {
                    // Only a real window move runs the sweep: the list
                    // emits scroll events on row patches too, and the
                    // sweep per tick (fetch + LRU rotation + repin) was
                    // this fix's own new cost (D80 perf lens).
                    if ev.visible_range.start == n.first_visible_start
                        && ev.visible_range.end == n.last_visible_end
                    {
                        return;
                    }
                    n.first_visible_start = ev.visible_range.start;
                    n.last_visible_end = ev.visible_range.end;
                    n.pin_window(cx);
                });
            });
        view
    }

    /// Any snapshot-state mutation goes through here so the render
    /// snapshot knows to rebuild (feed.rs's touch discipline).
    fn touch(&mut self) {
        self.snap_dirty = true;
    }

    fn pump(&self, mut rx: FeedRx, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.recv().await {
                let _ = this.update(cx, |this, cx| this.on_event(ev, cx));
            }
        })
        .detach();
    }

    /// Request-time snapshot invalidation + the shared engine: the thread
    /// touches when a quote fetch is ASKED (its list re-measures) —
    /// landing-side invalidation rides quotes::Host::touched. The touch
    /// here is once-unconditional (the pre-extraction copy touched per
    /// newly-reserved ref); the superset is harmless — every on_event
    /// already ends in an unconditional touch.
    fn want_quotes(&mut self, note: &Event, cx: &mut Context<Self>) {
        self.touch();
        quotes::want_quotes(self, note, cx);
    }

    /// Hydrate names for note authors + mentioned pubkeys (TODO(port):
    /// content.ts mentionPubkeys) — otherwise mentions degrade to @npub1….
    fn want_names(&self, notes: &[Event], cx: &mut Context<Self>) {
        let mut seen = HashSet::new();
        let missing: Vec<PublicKey> = notes
            .iter()
            .flat_map(|n| {
                let mut pks = vec![n.pubkey];
                pks.extend(content::mention_pks_cached(n));
                pks
            })
            .filter(|pk| !self.profiles.contains_key(pk) && seen.insert(*pk))
            .collect();
        if !missing.is_empty() {
            self.pump(self.core.ensure_profiles(missing), cx);
        }
    }

    fn on_event(&mut self, ev: FeedEvent, cx: &mut Context<Self>) {
        // The reflow guard (feed.rs's Reflow): reset() clears the scroll
        // anchor and wipes the rows' measures — a reset on a patch event
        // (profiles, parents, failures: same row count) yanks and blinks
        // the pane for nothing. Only a real row-count change resets.
        let before = self.replies.len();
        match ev {
            FeedEvent::One(Some(note), profile) => {
                // An article landing as a thread anchor (quote embed, reply
                // target) belongs in the reader, not the raw thread.
                if u16::from(note.kind) == 30023 {
                    cx.emit(Nav::OpenArticle(note.clone()));
                    return;
                }
                // The address door's first landing: the anchor learns its
                // id and the replies follow (new_addr has no id to ask).
                if self.anchor_id.is_none() {
                    self.anchor_id = Some(note.id);
                    self.pump(self.core.thread(note.id), cx);
                }
                // Option B: a reply anchor is a FOCUS; walk its parents.
                if !self.ancestors_asked && satori_core::nip::nip10::reply_parent(&note).is_some() {
                    self.ancestors_asked = true;
                    self.pump(self.core.ancestors(note.clone()), cx);
                }
                self.anchor = Some(note.clone());
                self.error = None;
                if let Some(p) = profile {
                    self.profiles.insert(p.pubkey, p);
                }
                self.touch();
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
                // Held replies (spam veils, out-of-network collapses)
                // sink below the conversation (D114).
                let collapse_on = satori_core::filter::thread_collapse_on();
                self.replies = nest_replies(&replies, &|ev: &Event| {
                    satori_core::filter::reply_held(ev, collapse_on)
                });
                for p in profiles {
                    self.profiles.insert(p.pubkey, p);
                }
                self.status.clear();
                self.touch();
                self.want_names(&replies, cx);
                self.want_parents(&replies, cx);
            }
            FeedEvent::Ancestors(chain, profiles, gap) => {
                for p in profiles {
                    self.profiles.insert(p.pubkey, p);
                }
                self.ancestors = chain;
                self.ancestors_gap = gap;
                for i in 0..self.ancestors.len() {
                    let ev = self.ancestors[i].clone();
                    self.want_media(&ev, cx);
                    self.want_quotes(&ev, cx);
                }
                // The chain's names: authors the page didn't carry.
                let missing: Vec<PublicKey> = self
                    .ancestors
                    .iter()
                    .map(|e| e.pubkey)
                    .filter(|pk| !self.profiles.contains_key(pk))
                    .collect();
                if !missing.is_empty() {
                    self.pump(self.core.ensure_profiles(missing), cx);
                }
                self.touch();
            }
            FeedEvent::Profiles(profiles) => {
                // The snapshot gate (D80 perf lens): only a real patch
                // marks the snap dirty — an empty Profiles landing (core
                // sends one when no session is active) used to force a
                // full ThreadSnap deep clone for nothing.
                if !profiles.is_empty() {
                    for p in profiles {
                        self.profiles.insert(p.pubkey, p);
                    }
                    self.touch();
                }
            }
            FeedEvent::Failed(e) => {
                // §4.3: transient — a relay round trip; Retry re-asks.
                // status/error are read fresh at render, outside the snap:
                // no touch.
                self.error = Some(states::PaneError::transient("The thread", &e));
            }
            FeedEvent::Parents(p) => {
                // Lazy parents landing (want_parents): the whisper upgrades
                // from the generic line to author + snippet. The parent's
                // author needs a name too — one ensure_profiles pump. The
                // whisper is render-body state (not in the snap): no touch.
                let mut missing: Vec<PublicKey> = Vec::new();
                for (pid, note) in p {
                    self.parents.insert(pid, note.clone());
                    if !self.profiles.contains_key(&note.author) {
                        missing.push(note.author);
                    }
                }
                if !missing.is_empty() {
                    self.pump(self.core.ensure_profiles(missing), cx);
                }
            }
            _ => {}
        }
        if self.replies.len() != before {
            // The replies stream in (store paint, then relay rounds): a
            // later landing must not yank a reader who already scrolled.
            let top = self.list.logical_scroll_top();
            self.list.reset(self.replies.len());
            if before > 0 {
                self.list.scroll_to(top);
            }
        }
        cx.notify();
    }

    /// Queue core fetches for every media URL we haven't got yet (results
    /// land in the shared media cache; notify re-renders pick them up).
    fn want_media(&mut self, note: &Event, cx: &mut Context<Self>) {
        for url in content::media_urls_cached(note).iter().cloned() {
            media::fetch(&self.core, url, cx.entity_id(), cx);
        }
    }

    /// Sweep + pin the visible window's media (feed/notifications.rs's
    /// pin_window discipline). Called at render (landings) and from the
    /// scroll stream — the two moments the on-screen set changes. The set:
    /// the anchor card's media + og thumbnails (it never scrolls out), the
    /// VISIBLE reply rows' media + og thumbnails, and the avatars on
    /// screen (the anchor's author plus the visible rows' authors) keyed
    /// by their ≤128px thumb. D80 (perf + resource lenses): the avatar
    /// loop used to sweep the WHOLE profiles map — the mention pump fills
    /// it with pks that never render here — with a fetch (and its O(cache)
    /// LRU rotation) per profile per scroll tick; and pins rode the
    /// originals' full decodes, so avatar-heavy threads ate the per-owner
    /// budget and lost exactly the on-screen pins the window exists to
    /// protect. Off-screen reply images age out of the cache and return on
    /// scroll-back like any pane's.
    fn pin_window(&mut self, cx: &mut Context<Self>) {
        let viewer = cx.entity_id();
        let win_start = self.first_visible_start.min(self.replies.len());
        let win_end = self
            .last_visible_end
            .max(win_start + 8)
            .min(self.replies.len());
        let mut painted: Vec<String> = Vec::new();
        // On-screen avatars: collect first (the window borrow ends), fetch
        // + pin by thumb key after — the thumb is the entry the avatar
        // paints and it is near-byte-free in the pin budget.
        let mut avatar_urls: Vec<String> = Vec::new();
        // The anchor and its parent chain (option B) never scroll out.
        for a in self.anchor.iter().chain(self.ancestors.iter()) {
            if let Some(p) = self.profiles.get(&a.pubkey)
                && !p.picture.is_empty()
                && !avatar_urls.contains(&p.picture)
            {
                avatar_urls.push(p.picture.clone());
            }
        }
        for (ev, _) in self.replies[win_start..win_end].iter() {
            if let Some(p) = self.profiles.get(&ev.pubkey)
                && !p.picture.is_empty()
                && !avatar_urls.contains(&p.picture)
            {
                avatar_urls.push(p.picture.clone());
            }
        }
        for url in avatar_urls {
            media::fetch(&self.core, url.clone(), viewer, cx);
            painted.push(media::thumb_key(&url));
        }
        for a in self.anchor.iter().chain(self.ancestors.iter()) {
            for url in content::note_media_urls(a) {
                if !painted.contains(&url) {
                    media::fetch(&self.core, url.clone(), viewer, cx);
                    painted.push(url);
                }
            }
        }
        for (ev, _) in self.replies[win_start..win_end].iter() {
            for url in content::note_media_urls(ev) {
                if !painted.contains(&url) {
                    media::fetch(&self.core, url.clone(), viewer, cx);
                    painted.push(url);
                }
            }
        }
        media::repin(viewer, painted, cx);
    }

    fn name_for(&self, ev: &Event) -> String {
        let key = ev.pubkey;
        content::display_name(self.profiles.get(&key), &key)
    }

    /// Lazy parent hydration (webapp render/note.ts:216–219, the "↩ in
    /// reply to an earlier note" line): a note whose DIRECT parent isn't in
    /// the thread shows the whisper now and fetches the parent in the
    /// background (load_parents: store first, one targeted indexer round
    /// trip for the misses) instead of blocking the tree on it. The
    /// asked-set keeps misses from re-requesting per render.
    fn want_parents(&mut self, notes: &[Event], cx: &mut Context<Self>) {
        let present: HashSet<EventId> = self
            .replies
            .iter()
            .map(|(e, _)| e.id)
            .chain(self.anchor.iter().map(|a| a.id))
            .collect();
        let mut ids: Vec<String> = Vec::new();
        for ev in notes {
            if let Some(pid) = direct_parent_id(ev)
                && !present.contains(&pid)
                && let pid = pid.to_hex()
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

    /// Option B's context above a focused reply: the parent chain root
    /// first, each post a compact card (avatar, name, time, its words in
    /// ink) on a hairline thread rail that runs down to the focus. Up to
    /// three show whole; a longer chain shows the direct parent under a
    /// "Show N earlier posts" line. "Full thread" opens the root once the
    /// walk reaches it. None when the anchor is not a reply.
    fn ancestors_block(
        &self,
        handle: &Entity<Self>,
        media: &media::Snapshot,
        quotes: &content::Quotes,
        profiles: &HashMap<PublicKey, Profile>,
    ) -> Option<gpui::AnyElement> {
        let anchor = self.anchor.as_ref()?;
        satori_core::nip::nip10::reply_parent(anchor)?;
        let quiet = |text: String| {
            div()
                .text_size(px(theme::T_SMALL))
                .text_color(rgb(theme::muted()))
                .child(text)
        };
        if self.ancestors.is_empty() {
            return Some(
                quiet(if self.ancestors_gap {
                    "The earlier posts in this thread are unavailable".into()
                } else {
                    "Loading earlier posts…".into()
                })
                .into_any_element(),
            );
        }
        let n = self.ancestors.len();
        let root_reached = !self.ancestors_gap
            && satori_core::nip::nip10::reply_parent(&self.ancestors[0]).is_none();
        let folded = n > 3 && !self.ancestors_open;
        let link = |id: &'static str, text: String| {
            div()
                .id(id)
                .cursor_pointer()
                .text_size(px(theme::T_SMALL))
                .text_color(rgb(theme::muted()))
                .hover(|s| s.text_color(rgb(theme::ink())))
                .child(text)
        };
        let unfold = folded.then(|| {
            let handle = handle.clone();
            link(
                "thread-ancestors-toggle",
                format!("Show {} earlier posts", n - 1),
            )
            .on_click(move |_, _, cx| {
                handle.update(cx, |view, cx| {
                    view.ancestors_open = true;
                    cx.notify();
                })
            })
        });
        let full = root_reached.then(|| {
            let handle = handle.clone();
            let root = self.ancestors[0].id;
            link("thread-full", "Full thread".into())
                .on_click(move |_, _, cx| handle.update(cx, |_, cx| cx.emit(Nav::OpenThread(root))))
        });
        let mut block = div().w_full().flex().flex_col();
        if self.ancestors_gap {
            block = block.child(
                div()
                    .pb_2()
                    .child(quiet("Earlier posts are unavailable".into())),
            );
        }
        if unfold.is_some() || full.is_some() {
            block = block.child(
                div()
                    .w_full()
                    .pb_2()
                    .flex()
                    .items_center()
                    .gap_3()
                    .children(unfold)
                    .child(div().flex_1())
                    .children(full),
            );
        }
        let shown = if folded {
            &self.ancestors[n - 1..]
        } else {
            &self.ancestors[..]
        };
        let first = if folded { n - 1 } else { 0 };
        for (k, ev) in shown.iter().enumerate() {
            block = block.child(self.ancestor_card(first + k, ev, handle, media, quotes, profiles));
        }
        Some(block.into_any_element())
    }

    /// One post in the parent chain. The left column carries the avatar
    /// and the rail down to the next post; the body is the real body
    /// (ink, media, mentions), a click on it focusing that post.
    fn ancestor_card(
        &self,
        i: usize,
        ev: &Event,
        handle: &Entity<Self>,
        media: &media::Snapshot,
        quotes: &content::Quotes,
        profiles: &HashMap<PublicKey, Profile>,
    ) -> gpui::AnyElement {
        let picture = profiles
            .get(&ev.pubkey)
            .map(|p| p.picture.clone())
            .unwrap_or_default();
        let name = self.name_for(ev);
        let open = {
            let handle = handle.clone();
            let id = ev.id;
            move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut gpui::App| {
                handle.update(cx, |_, cx| cx.emit(Nav::OpenThread(id)))
            }
        };
        let who = {
            let handle = handle.clone();
            let pk = ev.pubkey;
            move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut gpui::App| {
                handle.update(cx, |_, cx| cx.emit(Nav::OpenProfile(pk)))
            }
        };
        div()
            .w_full()
            .flex()
            .gap_2()
            .child(
                // The rail: avatar, then a hairline to the next post.
                div()
                    .flex_none()
                    .w(px(28.0))
                    .flex()
                    .flex_col()
                    .items_center()
                    .child(media::avatar(media, &picture, &name, 28.0))
                    .child(div().flex_1().w(px(2.0)).mt_1().bg(rgb(theme::hairline()))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .pb_3()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap_2()
                            .child(
                                div()
                                    .id(SharedString::from(format!("ancestor-who-{i}")))
                                    .cursor_pointer()
                                    .text_size(px(theme::T_UI))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(theme::ink()))
                                    .child(name)
                                    .on_click(who),
                            )
                            .child(
                                div()
                                    .text_size(px(theme::T_SMALL))
                                    .text_color(rgb(theme::muted()))
                                    .child(crate::timefmt::stamp_text(ev.created_at.as_secs())),
                            ),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("ancestor-{i}")))
                            .cursor_pointer()
                            .on_click(open)
                            .child(content::body(
                                ev,
                                &content::Body {
                                    media,
                                    quotes,
                                    profiles,
                                    emoji: content::body_parse(ev).emoji.clone(),
                                },
                                handle.clone(),
                            )),
                    ),
            )
            .into_any_element()
    }

    /// The whisper text for an unloaded parent: the generic line until the
    /// lazy fetch lands, then author + a one-line snippet (T_SMALL quiet,
    /// muted, context, not content).
    fn parent_whisper(&self, pid: &str) -> String {
        match self.parents.get(pid) {
            Some(p) => {
                let name = content::display_name(self.profiles.get(&p.author), &p.author);
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
fn direct_parent_id(ev: &Event) -> Option<EventId> {
    ev.tags
        .clone()
        .event_ids()
        .last()
        .filter(|pid| *pid != ev.id)
}

/// The NIP-10 tree, flattened pre-order with depths (thread.rs nest_replies).
/// Siblings keep their order, except that `held` replies (D114: the spam
/// veils and out-of-network collapses) sink below the visible ones at
/// every level, so the conversation reads first and the folded rows
/// gather at the bottom instead of interrupting it. A held reply's own
/// subtree travels with it.
fn nest_replies(replies: &[Event], held: &dyn Fn(&Event) -> bool) -> Vec<(Event, usize)> {
    let ids: HashSet<EventId> = replies.iter().map(|r| r.id).collect();
    let mut children: HashMap<EventId, Vec<Event>> = HashMap::new();
    let mut roots: Vec<Event> = Vec::new();
    for r in replies {
        // Last e tag = direct parent (NIP-10 marked-e reads right-to-left).
        let parent = r
            .tags
            .clone()
            .event_ids()
            .last()
            .filter(|pid| ids.contains(pid) && *pid != r.id);
        match parent {
            Some(pid) => children.entry(pid).or_default().push(r.clone()),
            None => roots.push(r.clone()),
        }
    }
    fn walk(
        ev: &Event,
        children: &HashMap<EventId, Vec<Event>>,
        depth: usize,
        out: &mut Vec<(Event, usize)>,
    ) {
        out.push((ev.clone(), depth));
        if depth < 3
            && let Some(kids) = children.get(&ev.id)
        {
            for kid in kids {
                walk(kid, children, depth + 1, out);
            }
        }
    }
    // Stable partition, visible first (sort_by_key is stable).
    let sink = |list: &mut Vec<Event>| list.sort_by_cached_key(|e| held(e));
    sink(&mut roots);
    for kids in children.values_mut() {
        sink(kids);
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
    profiles: &HashMap<PublicKey, Profile>,
    time: String,
    state: actions::ActionState,
    handle: Entity<ThreadView>,
) -> impl IntoElement {
    let picture = profiles
        .get(&ev.pubkey)
        .map(|p| p.picture.clone())
        .unwrap_or_default();
    let ev_for_click = ev.clone();
    let open_handle = handle.clone();
    let body_handle = handle.clone();
    let who_handle = handle.clone();
    let who = ev.pubkey;
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
                .text_color(rgb(theme::muted()))
                .child(w)
        }))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    // The identity opens the profile (the feed's
                    // actor-header discipline): avatar AND name are one
                    // click target — the user report was "clicking a user
                    // in a thread does nothing".
                    div()
                        .id(SharedString::from(format!("reply-who-{idx}")))
                        .flex()
                        .items_center()
                        .gap_2()
                        .cursor_pointer()
                        .on_click(move |_, _, cx| {
                            who_handle.update(cx, |_, cx| cx.emit(Nav::OpenProfile(who)));
                        })
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
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(content::copy_seal(
                            &format!("reply-{idx}"),
                            satori_core::nip::nip19::uri(ev),
                        ))
                        // The timestamp is the third mode toggle (timefmt.rs):
                        // tap cycles relative → date → block height.
                        .child(content::time_toggle(
                            SharedString::from(format!("reply-time-{idx}")),
                            time,
                        )),
                ),
        )
        .child(
            div()
                .id(SharedString::from(format!("reply-{idx}")))
                .cursor_pointer()
                .on_click(move |_, _, cx| {
                    let ev = ev_for_click.clone();
                    // Thread option B: a reply row drills in FOCUSED on
                    // that reply (its subtree below, this conversation as
                    // its chain above); back returns here.
                    open_handle.update(cx, |view, cx| {
                        if view.anchor.as_ref().map(|a| a.id) != Some(ev.id) {
                            cx.emit(Nav::OpenThread(ev.id));
                        }
                    });
                })
                .child(content::body(
                    ev,
                    &content::Body {
                        media,
                        quotes,
                        profiles,
                        emoji: content::body_parse(ev).emoji.clone(),
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
        let media = media::Snapshot::new(cx);
        // The media discipline (D60's rule, finally thread-wide): sweep +
        // pin the VISIBLE window — avatars and the anchor card (it never
        // scrolls out) always, the replies' rows only while on screen. The
        // old whole-set pin capped out on photo threads (two 24MP photos
        // are the whole per-owner budget) and the unpinned TAIL — replies
        // currently on screen — died to the next landing's eviction sweep:
        // the flash-then-refetch flicker.
        self.pin_window(cx);
        // Rebuild the render snapshot only when the DATA changed (any
        // mutation sets snap_dirty): the reply tree, profiles, quotes and
        // the engagement mirrors used to deep-clone per repaint (the
        // pre-FeedSnap regression class, #31).
        if self.snap_dirty || self.snap.is_none() {
            self.snap = Some(ThreadSnap {
                anchor: std::sync::Arc::new(self.anchor.clone()),
                replies: std::sync::Arc::new(self.replies.clone()),
                profiles: std::sync::Arc::new(self.profiles.clone()),
                quotes: std::sync::Arc::new(self.quotes.clone()),
                engagement: std::sync::Arc::new(self.engagement.clone()),
            });
            self.snap_dirty = false;
        }
        let snap = self.snap.as_ref().unwrap();
        let anchor = snap.anchor.as_ref(); // &Option<Event>
        let replies = snap.replies.clone();
        let profiles = snap.profiles.clone();
        let quotes = snap.quotes.clone();
        let eng = snap.engagement.clone();
        let anchor_handle = handle.clone();
        let reply_all_handle = handle.clone();
        // The action row's glyph states, snapshotted once per render
        // (mirrors mutated optimistically by the ActionHost impl).
        let reduced_motion = actions::reduced_motion(cx);
        let note_state = move |note: &Event| {
            eng.action_state(
                &note.id,
                // The thread pane shows the conversation itself; a count
                // on the bubble would repeat what the pane already renders.
                0,
                reduced_motion,
            )
        };
        // The WS3 collapse line's click target (the list closure moves its
        // own clone; `handle` stays for the anchor above).
        let collapse_handle = handle.clone();
        // Lazy-parent whispers (want_parents): computed once per render —
        // the generic line until the fetch lands, then author + snippet.
        let present: HashSet<EventId> = replies
            .iter()
            .map(|(e, _)| e.id)
            .chain(anchor.iter().map(|a| a.id))
            .collect();
        let whispers: Vec<Option<String>> = replies
            .iter()
            .map(|(ev, _)| {
                direct_parent_id(ev)
                    .filter(|pid| !present.contains(pid))
                    .map(|pid| self.parent_whisper(&pid.to_hex()))
            })
            .collect();
        // The chain above the focus names the parent already: the one-line
        // whisper only stands in until it lands.
        let anchor_whisper = anchor
            .as_ref()
            .filter(|_| self.ancestors.is_empty())
            .and_then(direct_parent_id)
            .filter(|pid| !present.contains(pid))
            .map(|pid| self.parent_whisper(&pid.to_hex()));
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
                                // No left inset: the focus and its chain
                                // share the reply rows' left edge.
                                .pr_4()
                                .py_4()
                                .children(self.ancestors_block(&handle, &media, &quotes, &profiles))
                                .child(
                                    // The identity opens the profile (the
                                    // reply rows wear the same click): one
                                    // avatar+name target, the user report.
                                    div()
                                        .id(SharedString::from("thread-who"))
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .cursor_pointer()
                                        .on_click({
                                            let handle = handle.clone();
                                            let who = note.pubkey;
                                            move |_, _, cx| {
                                                handle.update(cx, |_, cx| {
                                                    cx.emit(Nav::OpenProfile(who))
                                                })
                                            }
                                        })
                                        .child(media::avatar(
                                            &media,
                                            &self
                                                .profiles
                                                .get(&note.pubkey)
                                                .map(|p| p.picture.clone())
                                                .unwrap_or_default(),
                                            &self.name_for(note),
                                            28.0,
                                        ))
                                        .child(
                                            div()
                                                .text_size(px(theme::T_UI))
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(rgb(theme::ink()))
                                                .child(self.name_for(note)),
                                        ),
                                )
                                .children(anchor_whisper.map(|w| {
                                    div()
                                        .text_size(px(theme::T_SMALL))
                                        .text_color(rgb(theme::muted()))
                                        .child(w)
                                }))
                                .child(content::body(
                                    note,
                                    &content::Body {
                                        media: &media,
                                        quotes: &quotes,
                                        profiles: &profiles,
                                        emoji: content::body_parse(note).emoji.clone(),
                                    },
                                    handle.clone(),
                                ))
                                .child(actions::action_row(
                                    &note.id.to_string(),
                                    &anchor_ev,
                                    note_state(note),
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
                                        if let Some(id) = view.anchor_id {
                                            view.pump(
                                                view.core.event_by_id(id, Vec::new(), None),
                                                cx,
                                            );
                                            view.pump(view.core.thread(id), cx);
                                        } else if let Some((k, who, d)) = view.addr.clone() {
                                            view.pump(view.core.event_by_address(k, who, d), cx);
                                        }
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
                                            let key = ev.pubkey;
                                            let name =
                                                content::display_name(profiles.get(&key), &key);
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

    /// D114: held replies sink below visible siblings, subtrees intact.
    #[test]
    fn held_replies_sink_to_the_bottom() {
        let k = Keys::generate();
        let root = EventId::from_byte_array([9u8; 32]);
        let mk = |text: &str, parent: EventId, at: u64| {
            let mut tags = Tags::new();
            tags.push(Tag::event(parent));
            k.sign_event(UnsignedEvent::new(
                k.public_key(),
                Timestamp::from_secs(at),
                Kind::TextNote,
                tags,
                text.to_string(),
            ))
            .unwrap()
        };
        let spam = mk("spam", root, 1);
        let good = mk("good", root, 2);
        let under_spam = mk("under spam", spam.id, 3);
        let replies = vec![spam.clone(), good.clone(), under_spam.clone()];
        let held = |e: &Event| e.content == "spam";
        let order: Vec<String> = nest_replies(&replies, &held)
            .into_iter()
            .map(|(e, _)| e.content)
            .collect();
        assert_eq!(order, ["good", "spam", "under spam"]);
        let plain: Vec<String> = nest_replies(&replies, &|_| false)
            .into_iter()
            .map(|(e, _)| e.content)
            .collect();
        assert_eq!(plain, ["spam", "under spam", "good"]);
    }

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
