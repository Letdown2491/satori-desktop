//! The home feed, GPUI-rendered.
//!
//! TODO(port): crates/app/src/feed.rs + note_card.rs (and src/feed.js) — P0.
//! Read-only following feed with optimistic like; the receiver-pumping seam is
//! the GPUI twin of `bridge.rs`'s `drain`. Rich cards for the manifest
//! registry kinds render via cards.rs (P3).

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use gpui::{
    div, list, prelude::*, px, relative, rgb, AnimationExt, Context, Entity, EventEmitter,
    FontWeight, ListAlignment, ListOffset, ListScrollEvent, ListState, Render, SharedString,
    Window, Div,
};
use nostr::prelude::*;
use satori_core::manager::{Core, FeedEvent, FeedRx, Live, ParentNote, Profile, PublishEvent};
use satori_core::nip::nip10;
use satori_core::wallet::{Status, WalletEvent};

use crate::prefs;

use crate::{icons, media, theme};
use super::{content, nav::Nav, cards, states};

/// Which surface a feed pane carries: the timeline (the visible catalog),
/// the articles shelf (kind 30023 from the same follows), or a hashtag
/// timeline (Phase 2 §4.2 — the `#t` filter, any author).
#[derive(Clone, PartialEq)]
pub(crate) enum FeedMode {
    Timeline,
    Articles,
    Hashtag(String),
    /// One author's notes — the profile page's Notes tab (an embedded Feed
    /// entity; the profile view only renders the header + media strips).
    Profile(PublicKey),
    /// The Library pane's Bookmarks tab (D38): the own bookmark set
    /// (private 10003 + public 30001 merged), no paging.
    Bookmarks,
}
impl FeedMode {
    pub(crate) fn kinds(&self) -> Vec<Kind> {
        match self {
            // Never dispatched on for Hashtag/Profile — those fetches switch
            // on the mode (the tag/author filter is the query; there's no
            // follow set). The kinds themselves honor the content prefs.
            FeedMode::Timeline
            | FeedMode::Hashtag(_)
            | FeedMode::Profile(_)
            | FeedMode::Bookmarks => satori_core::store::visible_kinds(),
            FeedMode::Articles => vec![Kind::Custom(30023)],
        }
    }
}

pub struct Feed {
    mode: FeedMode,
    core: Arc<Core>,
    notes: Vec<Event>,
    profiles: HashMap<String, Profile>,
    /// Reply parents (nip10) resolved for the context whisper: parent id →
    /// author + snippet. `parents_asked` keeps misses from re-requesting.
    parents: HashMap<String, ParentNote>,
    parents_asked: HashSet<String>,
    /// Push signals since the last load (the live subscription): the
    /// new-notes pill's count. Nothing moves until the reader clicks.
    live_new: usize,
    liked: HashSet<String>,
    bookmarked: HashSet<String>,
    reposted: HashSet<String>,
    /// Own zaps (D20): note id → settled (the ledger mirrors in core) and
    /// note id → in flight (optimistic — the amber bolt).
    zapped: HashSet<String>,
    zap_pending: HashSet<String>,
    /// Whether the NWC wallet answers (the dim-bolt affordance).
    wallet_ready: bool,
    quotes: content::Quotes,
    // Rich-card interaction state (cards.rs): poll tallies + ballots, and
    // my RSVP per calendar event.
    poll_tallies: HashMap<EventId, Vec<(String, u32)>>,
    poll_sel: HashMap<EventId, Vec<String>>,
    voted: HashSet<EventId>,
    rsvped: HashMap<EventId, String>,
    status: String,
    /// §4.3: the pane's kinded error — the strip replaces the old raw
    /// `format!("… {e}")` status-line dump (deficiency 8).
    error: Option<states::PaneError>,
    /// Endless scroll: the relays may have more; a round trip is in flight.
    has_more: bool,
    paging: bool,
    /// The paging anchor: the oldest note timestamp EVER absorbed — not the
    /// window's last row. The RAM window (trim_window) drops the tail, and
    /// `older()` must continue from where the STREAM ended, not where the
    /// Vec ends, or the next page would re-fetch the rows just dropped.
    oldest_ts: Option<u64>,
    /// Render snapshot (D13's snapshot rule, made cheap): the render item
    /// closure used to clone the whole notes vec + profile map on every
    /// frame — and `cx.notify()` fires for every profile resolution, image
    /// arrival, poll tally and publish event. The snapshot rebuilds only
    /// when the data changed (or the media cache's epoch moved).
    snap: Option<FeedSnap>,
    snap_dirty: bool,
    /// Mirror of `notes` ids — dedupe was an O(n) scan per incoming note
    /// (O(n·m) per bootstrap batch); now a hash lookup.
    note_ids: HashSet<EventId>,
    /// The scroll stream's last visible end — remembered so a page landing
    /// while the reader idles at the end cap can chain the next one (the
    /// scroll handler only fires on wheel ticks, which stop at the cap).
    last_visible_end: usize,
    /// Deep-scroll state, emitted to the shell (Nav::PaneDeep); the shell
    /// re-reads this field for the ACTIVE pane at render, so it must be
    /// visible across views' modules.
    pub(crate) deep: bool,
    /// j/k's row cursor (key_nav); the scroll handler re-anchors it.
    cursor: usize,
    /// WS2 viewport debounce: bumps on every scroll/nav/landing; the
    /// spawned push only fires if the pulse_id is still current (fast
    /// scrolling coalesces into ONE core.set_viewport per settle).
    viewport_gen: usize,
    list: ListState,
}

/// Everything the render's item closure reads, rebuilt (one deep copy) only
/// when the data changed instead of every frame; the Arcs make the per-frame
/// clone a refcount bump.
struct FeedSnap {
    notes: std::sync::Arc<Vec<Event>>,
    profiles: std::sync::Arc<HashMap<String, Profile>>,
    parents: std::sync::Arc<HashMap<String, ParentNote>>,
    liked: std::sync::Arc<HashSet<String>>,
    bookmarked: std::sync::Arc<HashSet<String>>,
    reposted: std::sync::Arc<HashSet<String>>,
    zapped: std::sync::Arc<HashSet<String>>,
    zap_pending: std::sync::Arc<HashSet<String>>,
    wallet_ready: bool,
    quotes: std::sync::Arc<content::Quotes>,
    cards: std::sync::Arc<cards::CardState>,
}

impl EventEmitter<Nav> for Feed {}

impl Feed {
    pub fn new(core: Arc<Core>, cx: &mut Context<Self>) -> Self {
        crate::theme::watch(cx); // repaint on theme change
        crate::timefmt::watch(cx); // repaint on mode/height change
        crate::video::watch(cx); // repaint on player open/toggle/close
        Self::with_mode(core, FeedMode::Timeline, cx)
    }

    /// The articles shelf: same follows, same paging, kind 30023 only.
    pub fn articles(core: Arc<Core>, cx: &mut Context<Self>) -> Self {
        Self::with_mode(core, FeedMode::Articles, cx)
    }

    /// The hashtag timeline (Phase 2 §4.2): the `#t` filter, any author —
    /// opened by tapping a #tag in a note body.
    pub fn hashtag(core: Arc<Core>, tag: String, cx: &mut Context<Self>) -> Self {
        Self::with_mode(core, FeedMode::Hashtag(tag), cx)
    }

    /// One author's notes: the profile page's Notes tab embeds a full Feed,
    /// so its rows ARE the timeline's rows (avatar, media, reactions) with
    /// the same endless scroll — scoped to the author's pubkey.
    pub fn author(core: Arc<Core>, who: PublicKey, cx: &mut Context<Self>) -> Self {
        Self::with_mode(core, FeedMode::Profile(who), cx)
    }

    /// The Library pane's Bookmarks tab (D38): the merged own bookmark set
    /// as timeline rows. One shot — the list is finite, no Older rounds.
    pub fn bookmarks(core: Arc<Core>, cx: &mut Context<Self>) -> Self {
        Self::with_mode(core, FeedMode::Bookmarks, cx)
    }

    fn with_mode(core: Arc<Core>, mode: FeedMode, cx: &mut Context<Self>) -> Self {
        let feed = Self {
            liked: core.liked_set(),
            bookmarked: core.bookmarked_set(),
            reposted: core.reposted_set(),
            zapped: core.zapped_set(),
            zap_pending: HashSet::new(),
            wallet_ready: core.wallet().is_ready(),
            core,
            notes: vec![],
            profiles: HashMap::new(),
            parents: HashMap::new(),
            parents_asked: HashSet::new(),
            live_new: 0,
            quotes: HashMap::new(),
            poll_tallies: HashMap::new(),
            poll_sel: HashMap::new(),
            voted: HashSet::new(),
            rsvped: HashMap::new(),
            status: "connecting to the relays…".into(),
            error: None,
            has_more: true,
            paging: false,
            oldest_ts: None,
            snap: None,
            snap_dirty: true,
            mode,
            note_ids: HashSet::new(),
            last_visible_end: 0,
            // j/k's row cursor (feed.rs key_nav) — re-anchored by every
            // scroll event, so mouse and keys never disagree for long.
            cursor: 0,
            viewport_gen: 0,
            deep: false,
            list: ListState::new(0, ListAlignment::Top, px(400.)),
        };
        // Endless scroll: the list's own scroll stream orders the next page
        // (the GTK app prefetched at 70% scroll — same idea). The handler
        // runs on &mut App; WeakEntity::update accepts that.
        let this = cx.entity().downgrade();
        feed.list.set_scroll_handler(move |ev: &ListScrollEvent, _w, cx| {
            let _ = this.update(cx, |feed, cx| {
                feed.last_visible_end = ev.visible_range.end;
                // j/k's cursor rides the scroll stream: keys move it one
                // row, the wheel re-anchors it to the top visible row.
                feed.cursor = ev.visible_range.start;
                // Deep-scroll whisper: the shell floats a back-to-top button
                // once the reader is a few items down — a dozen was too
                // deep (the owner kept scrolling before thinking "where is
                // it"). Emit on change only.
                let deep = ev.visible_range.start >= 3;
                if feed.deep != deep {
                    feed.deep = deep;
                    cx.emit(Nav::PaneDeep);
                }
                feed.maybe_older(feed.last_visible_end, cx);
                // The trim can only fire once the reader is back above the
                // trim line — the scroll stream is where that happens.
                feed.trim_window(cx);
                // WS2: the visible window moved — re-scope the engagement
                // subscription (debounced in viewport_pulse).
                feed.viewport_pulse(cx);
            });
        });
        // Live push: fresh follow posts bump the new-notes pill (the list
        // itself never moves on its own — the reader clicks when ready).
        let this = cx.entity().downgrade();
        let mut live_rx = feed.core.subscribe_live();
        cx.spawn(async move |_, cx| {
            loop {
                match live_rx.recv().await {
                    Ok(Live::Post) => {
                        let _ = this.update(cx, |feed, cx| {
                            // A follow's fresh post needn't carry this
                            // hashtag — no new-notes pill on hashtag feeds.
                            // Author feeds likewise: the pill would sit
                            // under the profile's own header, noise. The
                            // Library's bookmarks (D38) too: a follow's
                            // post is not a new bookmark.
                            if matches!(
                                feed.mode,
                                FeedMode::Hashtag(_)
                                    | FeedMode::Profile(_)
                                    | FeedMode::Bookmarks
                            ) {
                                return;
                            }
                            feed.live_new += 1;
                            cx.notify();
                        });
                    }
                    // WS2: engagement on a viewport note arrived live —
                    // the event is in the store (the SDK saves on
                    // receive); re-seed the own-engagement mirrors and
                    // repaint. No new rendering path: this is the same
                    // snapshot invalidation every data change rides.
                    Ok(Live::Engagement) => {
                        let _ = this.update(cx, |feed, cx| feed.on_engagement(cx));
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(_) => continue, // lagged: still true that there's news
                }
            }
        })
        .detach();
        // The wallet broadcast (D20): bolt glyphs follow every zap —
        // theirs or the sheet's — wherever it started.
        let this = cx.entity().downgrade();
        let mut wallet_rx = feed.core.subscribe_wallet();
        cx.spawn(async move |_, cx| {
            loop {
                match wallet_rx.recv().await {
                    Ok(ev) => {
                        let _ = this.update(cx, |feed, cx| feed.on_wallet(ev, cx));
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(_) => continue, // lagged: the next event re-syncs
                }
            }
        })
        .detach();
        // Hashtag feeds have no follow set to deep-fetch — the tag filter
        // IS the query (profile()-shaped); the rest bootstrap as usual.
        let boot = match &feed.mode {
            FeedMode::Hashtag(tag) => feed.core.hashtag(tag.clone()),
            FeedMode::Profile(who) => feed.core.profile(*who),
            FeedMode::Bookmarks => feed.core.bookmarked_notes(),
            _ => feed.core.bootstrap(feed.mode.kinds()),
        };
        feed.pump(boot, cx);
        feed
    }

    /// The shell header's title — the raw-cased tag on hashtag panes.
    pub fn header(&self) -> String {
        match &self.mode {
            FeedMode::Hashtag(tag) => format!("#{tag}"),
            FeedMode::Timeline => "Feed".into(),
            FeedMode::Articles => "Articles".into(),
            FeedMode::Bookmarks => "Bookmarks".into(),
            // Author feeds nest inside the profile pane, which carries its
            // own header — this title never renders.
            FeedMode::Profile(_) => "Profile".into(),
        }
    }

    /// The list's item count: one row per note + the end cap.
    fn list_len(&self) -> usize {
        self.notes.len() + 1
    }

    /// Back to the top (rail re-click, ensō click, the floating whisper):
    /// home is item zero. The remembered scroll end resets too, or the next
    /// landing page would chain a prefetch for a reader sitting at the top.
    pub fn scroll_top(&mut self) {
        self.last_visible_end = 0;
        self.deep = false;
        self.cursor = 0;
        self.list.scroll_to(ListOffset {
            item_ix: 0,
            offset_in_item: px(0.),
        });
    }

    /// WS2: j/k moves the cursor within the buffer — the window is
    /// unchanged unless the anchor scrolls, and scroll_to fires the scroll
    /// stream (which pulses) whenever it does.
    pub fn key_nav(&mut self, delta: isize) {
        let len = self.notes.len();
        if len == 0 {
            return;
        }
        let next = (self.cursor as isize + delta).clamp(0, len as isize - 1) as usize;
        self.cursor = next;
        self.list.scroll_to(ListOffset {
            item_ix: next,
            offset_in_item: px(0.),
        });
    }

    /// ←/→ (shell.rs key handler): step the cursor row's multi-image
    /// carousel — the same selection the ‹ › arrows move (content.rs
    /// carousel_key_step re-derives the carousel's seg id). False = the
    /// row has no carousel; the shell treats the key as unhandled.
    pub fn carousel_nav(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        let Some(ev) = self.notes.get(self.cursor) else {
            return false;
        };
        let media = media::Snapshot::new(cx);
        let stepped = content::carousel_key_step(ev, &media, delta as i64);
        if stepped {
            cx.notify();
        }
        stepped
    }

    /// Near the bottom, order the next older page. The scroll handler calls
    /// this on every wheel tick — `paging` keeps it one round trip at a time.
    /// The lookahead is generous (a third of a page): the order goes out
    /// while the reader is still several screens away, so the relay round
    /// trip (up to READ_BUDGET) overlaps reading instead of parking the
    /// reader at the end cap.
    fn maybe_older(&mut self, visible_end: usize, cx: &mut Context<Self>) {
        const LOOKAHEAD: usize = 12;
        if !self.has_more || self.paging || self.notes.is_empty() {
            return;
        }
        // The bookmark list is finite (D38) — the one-shot load never pages.
        if matches!(self.mode, FeedMode::Bookmarks) {
            self.has_more = false;
            return;
        }
        if visible_end + LOOKAHEAD < self.notes.len() {
            return;
        }
        self.paging = true;
        // The anchor, not the window's last row: the RAM window may have
        // trimmed the tail since this page was ordered.
        let oldest = self.oldest_ts.unwrap_or(0);
        let rx = match &self.mode {
            FeedMode::Hashtag(tag) => self.core.hashtag_older(tag.clone(), oldest),
            FeedMode::Profile(who) => self.core.profile_older(*who, oldest),
            _ => self.core.older(oldest, self.mode.kinds()),
        };
        self.pump(rx, cx);
        cx.notify();
    }

    /// The feed's RAM window. Notes are KB-scale events and the timeline
    /// only ever grows (`absorb` never clears — dedup makes reloads
    /// idempotent), so a long reading session used to hold every note ever
    /// fetched — and `FeedSnap` deep-clones the Vec on every data change,
    /// so unbounded `notes` also unbounded the per-frame rebuild cost.
    /// Past the ceiling the OLDEST tail is dropped, but only when the
    /// reader sits well above the trim line: splice preserves heights (and
    /// the scroll anchor) for rows above the removed range, and the paging
    /// anchor (`oldest_ts`) keeps `older()` streaming from where the stream
    /// ended, not where the Vec ends. `note_ids` keeps the dropped ids —
    /// a reload overlapping the trimmed range must dedup (a re-added note
    /// would land at the bottom, out of order).
    fn trim_window(&mut self, cx: &mut Context<Self>) {
        let Some(to) = trim_target(self.notes.len(), self.last_visible_end) else {
            return;
        };
        let old_len = self.list_len();
        // Shed the trimmed notes' dedup entries too (audit perf L2): the
        // window may hold 1000 while the visible target is 400 - every
        // scrolled-away note carried its id in note_ids for the session.
        // A trimmed note re-delivered by the live bell re-enters as fresh
        // content, which is correct: nothing of it is on screen.
        for note in self.notes.drain(to..) {
            self.note_ids.remove(&note.id);
        }
        // The end cap replaces the dropped tail: range to..old_len becomes
        // one row. Rows above `to` (the reader's anchor) keep their heights.
        self.list.splice(to..old_len, 1);
        self.touch();
        cx.notify();
    }

    /// The GPUI twin of app/bridge.rs `drain`: core returns receivers, the UI
    /// pumps them. tokio's receiver is executor-agnostic, so gpui's executor
    /// awaits it directly — no tokio context anywhere in the UI.
    fn pump(&self, mut rx: FeedRx, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.recv().await {
                let _ = this.update(cx, |this, cx| this.on_event(ev, cx));
            }
        })
        .detach();
    }

    /// Any state mutation goes through here so the render snapshot knows to
    /// rebuild. (on_event covers the event-driven arms wholesale.)
    fn touch(&mut self) {
        self.snap_dirty = true;
    }

    /// §4.3 Retry: re-ask what this pane asks on open, per mode. The strip's
    /// Retry action only re-runs a relay round trip — it never pages.
    fn retry_load(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        let rx = match &self.mode {
            FeedMode::Hashtag(tag) => self.core.hashtag(tag.clone()),
            FeedMode::Profile(who) => self.core.profile(*who),
            FeedMode::Bookmarks => self.core.bookmarked_notes(),
            _ => self.core.refresh(self.mode.kinds()),
        };
        self.pump(rx, cx);
        cx.notify();
    }

    fn on_event(&mut self, ev: FeedEvent, cx: &mut Context<Self>) {
        self.touch();
        enum Reflow {
            Reset,
            Append(usize),
            None,
        }
        let mut reflow = Reflow::Reset;
        let before = self.list_len();
        match ev {
            FeedEvent::Loaded(page) => {
                self.absorb(page.notes, page.profiles, cx);
                self.want_parents(cx);
                self.status.clear();
                self.error = None;
                // The bookmark list is finite (D38) — never page it.
                self.has_more = !matches!(self.mode, FeedMode::Bookmarks);
            }
            FeedEvent::Older(page, more) => {
                self.has_more = more;
                self.paging = false;
                let old = self.notes.len();
                self.absorb(page.notes, page.profiles, cx);
                self.want_parents(cx);
                if self.notes.len() > old {
                    // Appends keep the reader's place: reset() clears the
                    // scroll anchor and would yank the pane to the top,
                    // while splice() leaves logical_scroll_top alone (the
                    // anchor survives a non-overlapping splice).
                    reflow = Reflow::Append(old);
                } else {
                    reflow = Reflow::None;
                }
                // Chain: if the reader is (still) at the end cap — the scroll
                // handler fired its last wheel tick before this page existed —
                // order the next page now instead of waiting for a tick
                // that may never come. Pages served from the store are
                // near-instant, so a dry-spell relay trip deepens the store
                // once and the chain runs free afterwards.
                if self.has_more {
                    self.maybe_older(self.last_visible_end, cx);
                }
            }
            FeedEvent::Profiles(profiles) => self.absorb_profiles(profiles, cx),
            FeedEvent::Parents(p) => {
                // Height-changing patches (the whisper appears) — the list
                // re-measures; no reset, the anchor survives small shifts.
                // The whisper's NAME resolves through the same profile map
                // as the header — but only note authors and mentions were
                // ever asked for, so every reply whisper fell back to raw
                // hex forever. Ask for the parent authors we can't name.
                let mut seen = HashSet::new();
                let missing: Vec<PublicKey> = p
                    .values()
                    .map(|pn| pn.author.clone())
                    .filter(|a| !self.profiles.contains_key(a) && seen.insert(a.clone()))
                    .filter_map(|a| PublicKey::from_hex(&a).ok())
                    .collect();
                self.parents.extend(p);
                if !missing.is_empty() {
                    let rx = self.core.ensure_profiles(missing);
                    self.pump(rx, cx);
                }
                cx.notify();
            }
            FeedEvent::Failed(e) => {
                // §4.3: the kinded strip, not a raw "… {e}" status dump —
                // the leading ellipsis collided with the loading convention.
                self.error = Some(states::PaneError::transient("The feed", &e));
                // A failed round trip must not wedge the end cap.
                self.paging = false;
            }
            _ => {}
        }
        // Reset only when the item count changed: gpui's List drops every
        // scroll event while a reset is pending, so resetting on profile
        // resolutions (constant during a busy feed) ate wheel input mid-scroll
        // and wedged the pane blank. Appends go through splice() instead (the
        // list learns the new count without losing the anchor); same-height
        // patches need neither (list.rs: "elements outside the scrolled area
        // do not change their height").
        match reflow {
            Reflow::Reset if self.list_len() != before => self.list.reset(self.list_len()),
            Reflow::Append(old) => {
                // splice inserts `count` items at `old`, keeping the suffix:
                // total = old + count + (state_total − old). The state was in
                // sync (state_total = before), so count is the DELTA — the
                // new-total-shaped call double-counted (the state grew by a
                // full list length per append; probed against gpui 0.2.2).
                let added = self.list_len() - before;
                self.list.splice(old..old, added);
            }
            _ => {}
        }
        // A landing can cross the ceiling on its own (a big bootstrap); the
        // reader is above the trim line in that case almost by definition.
        self.trim_window(cx);
        // WS2: a landing changed which notes sit under the viewport.
        self.viewport_pulse(cx);
        cx.notify();
    }

    fn absorb(
        &mut self,
        notes: Vec<Event>,
        profiles: Vec<Profile>,
        cx: &mut Context<Self>,
    ) {
        self.absorb_profiles(profiles, cx);
        // Mention candidates from the notes actually INSERTED (the loop
        // dedups): re-scanning the whole window re-classified every token
        // of every note on every landing (audit perf M6).
        let mut mention_candidates: Vec<PublicKey> = Vec::new();
        for note in notes {
            // The paging anchor advances on EVERY incoming note — even a
            // deduped repeat or a filtered kind marks where the stream has
            // reached; the window may already have trimmed the tail this
            // note belongs to.
            let ts = note.created_at.as_secs();
            self.oldest_ts = Some(self.oldest_ts.map_or(ts, |o: u64| o.min(ts)));
            // Registry kinds render as rich cards (cards.rs card_for port).
            if note.kind != Kind::TextNote && !cards::is_feed_kind(note.kind) {
                continue;
            }
            // Amethyst's repost guard: a repost is not content of its own —
            // if the boosted kind has no card here, the row never renders
            // (a kind-6 payload is the original event's JSON; raw text
            // would leak it).
            if u16::from(note.kind) == 6 && repost_inner(&note).is_none() {
                continue;
            }
            if self.note_ids.contains(&note.id) {
                continue;
            }
            self.want_images(&note, cx);
            self.want_quotes(&note, cx);
            if u16::from(note.kind) == 1068 {
                self.want_poll(&note, cx);
            }
            mention_candidates.push(note.pubkey);
            mention_candidates.extend(content::mention_pks(&note.content));
            self.note_ids.insert(note.id);
            self.notes.push(note);
        }
        self.notes.sort_by_key(|n| std::cmp::Reverse(n.created_at));
        // Ask core for any authors we still can't name — the note's author
        // plus mentioned pubkeys, so a mention renders @name instead of
        // @npub1… (TODO(port): content.ts mentionPubkeys — hydrate before
        // render).
        let missing: Vec<PublicKey> = {
            let mut seen = HashSet::new();
            mention_candidates
                .into_iter()
                .filter(|pk| {
                    !self.profiles.contains_key(&pk.to_string()) && seen.insert(pk.to_string())
                })
                .collect()
        };
        if !missing.is_empty() {
            let rx = self.core.ensure_profiles(missing);
            self.pump(rx, cx);
        }
    }

    /// Collect the page's unresolved reply parents and ask core for them
    /// (store first, one by-id relay trip for the misses). Capped per ask.
    fn want_parents(&mut self, cx: &mut Context<Self>) {
        let mut ids: Vec<String> = Vec::new();
        for note in &self.notes {
            if let Some((id, _relays)) = nip10::reply_parent(note) {
                if self.parents.contains_key(&id) || self.parents_asked.contains(&id) {
                    continue;
                }
                self.parents_asked.insert(id.clone());
                ids.push(id);
                if ids.len() >= 60 {
                    break;
                }
            }
        }
        if ids.is_empty() {
            return;
        }
        let rx = self.core.load_parents(ids);
        let mut rx = rx;
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.recv().await {
                let live = this
                    .update(cx, |f, cx| {
                        f.on_event(ev, cx);
                        true // keep draining until the channel closes
                    })
                    .unwrap_or(false);
                if !live {
                    break;
                }
            }
        })
        .detach();
    }

    fn absorb_profiles(&mut self, profiles: Vec<Profile>, cx: &mut Context<Self>) {
        for p in profiles {
            self.want_avatar(&p, cx);
            self.profiles.insert(p.pubkey.clone(), p);
        }
    }

    /// Queue core fetches for every media URL we haven't got yet. Results
    /// land in the shared media cache (media.rs) — a `cx.notify()` re-render
    /// picks them up via the snapshot.
    fn want_images(&mut self, note: &Event, cx: &mut Context<Self>) {
        for url in content::media_urls_cached(note).iter().cloned() {
            media::fetch(&self.core, url, cx.entity_id(), cx);
        }
    }

    /// One quote-reference resolution, routed off `on_event` (quote replies
    /// must not touch the feed list).
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
                        this.quotes
                            .insert(bech.clone(), Some(content::Embed { event: target, name }));
                        cx.notify();
                    });
                }
            }
        })
        .detach();
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

    /// Tallies for a poll card (cards.rs poll_card): core drains the kind
    /// 1018 votes; the id rides the pump closure (PollVotes carries none).
    fn want_poll(&mut self, note: &Event, cx: &mut Context<Self>) {
        if self.poll_tallies.contains_key(&note.id) {
            return;
        }
        // Reserve so re-renders don't re-request while loading.
        self.poll_tallies.insert(note.id, vec![]);
        let id = note.id;
        let mut rx = self.core.poll_votes(id);
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.recv().await {
                if let FeedEvent::PollVotes(t) = ev {
                    let _ = this.update(cx, |this, cx| {
                        this.poll_tallies.insert(id, t);
                        cx.notify();
                    });
                }
            }
        })
        .detach();
        cx.notify();
    }

    /// Ballot selection (cards.rs poll_card): replace for single-choice,
    /// toggle for multiple.
    pub(crate) fn toggle_poll_sel(&mut self, ev: &Event, opt: String, multiple: bool, cx: &mut Context<Self>) {
        self.touch();
        let sel = self.poll_sel.entry(ev.id).or_default();
        if sel.contains(&opt) {
            sel.retain(|o| *o != opt);
        } else if multiple {
            sel.push(opt);
        } else {
            *sel = vec![opt];
        }
        cx.notify();
    }

    /// Cast the ballot: publish the vote (kind 1018), mark voted on commit,
    /// re-drain the tallies.
    pub(crate) fn cast_vote(&mut self, ev: &Event, cx: &mut Context<Self>) {
        let Some(opts) = self.poll_sel.get(&ev.id).cloned() else {
            return;
        };
        if opts.is_empty() {
            return;
        }
        let id = ev.id;
        let mut rx = self.core.poll_vote(ev, opts);
        cx.spawn(async move |this, cx| {
            while let Some(pv) = rx.recv().await {
                if let PublishEvent::Committed { .. } = pv {
                    let _ = this.update(cx, |this, cx| {
                        this.voted.insert(id);
                        this.touch();
                        // Fresh tally including our own vote.
                        let mut rx = this.core.poll_votes(id);
                        cx.spawn(async move |this, cx| {
                            while let Some(ev) = rx.recv().await {
                                if let FeedEvent::PollVotes(t) = ev {
                                    let _ = this.update(cx, |this, cx| {
                                        this.poll_tallies.insert(id, t);
                                        this.touch();
                                        cx.notify();
                                    });
                                }
                            }
                        })
                        .detach();
                        cx.notify();
                    });
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    /// RSVP to a calendar event (NIP-52 kind 31925) — engaged on commit.
    pub(crate) fn send_rsvp(&mut self, ev: &Event, status: &'static str, cx: &mut Context<Self>) {
        let id = ev.id;
        let mut rx = self.core.rsvp(ev, status);
        cx.spawn(async move |this, cx| {
            while let Some(pv) = rx.recv().await {
                if let PublishEvent::Committed { .. } = pv {
                    let _ = this.update(cx, |this, cx| {
                        this.rsvped.insert(id, status.into());
                        this.touch();
                        cx.notify();
                    });
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    /// Queue the profile picture fetch (core's guarded, disk-cached path).
    fn want_avatar(&mut self, profile: &Profile, cx: &mut Context<Self>) {
        let url = profile.picture.clone();
        if url.is_empty() {
            return;
        }
        media::fetch(&self.core, url, cx.entity_id(), cx);
    }

    /// The bolt's tap: 21 sats, no dialog — the money rule. No wallet
    /// behind it? The dim glyph opens the wallet pane instead. Errors and
    /// settle flow back on the wallet broadcast (on_wallet).
    fn zap_tap(&mut self, ev: &Event, cx: &mut Context<Self>) {
        if !self.wallet_ready {
            cx.emit(Nav::OpenWallet);
            return;
        }
        // One zap in flight per note (the Feather rule; core enforces too).
        if !self.zap_pending.insert(ev.id.to_string()) {
            return;
        }
        self.touch();
        self.core.zap(ev.clone(), super::zap::TAP_SATS * 1000);
        cx.notify();
    }

    /// WS2 (viewport.rs): the settled viewport pulse. The scroll stream
    /// calls this on every wheel tick and every landing; only the LAST
    /// pulse inside the settle window reaches core (the generation guard
    /// drops superseded timers) — fast scrolling coalesces into one
    /// `set_viewport`, no subscription churn (D24).
    fn viewport_pulse(&mut self, cx: &mut Context<Self>) {
        self.viewport_gen += 1;
        let pulse_id = self.viewport_gen;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(
                    satori_core::viewport::SETTLE_MS,
                ))
                .await;
            let _ = this.update(cx, |feed, cx| {
                if feed.viewport_gen != pulse_id {
                    return; // a newer scroll superseded this pulse
                }
                feed.push_viewport(cx);
            });
        })
        .detach();
    }

    /// The settled window → core: the visible range (+ buffer, capped)
    /// becomes the live engagement id set. The per-paint/store loads stay
    /// as the fallback for distant rows — this only ADDS liveness.
    fn push_viewport(&mut self, _cx: &mut Context<Self>) {
        if self.notes.is_empty() {
            self.core.set_viewport(Vec::new());
            return;
        }
        let end = self.last_visible_end.max(self.cursor);
        let (start, stop) = satori_core::viewport::window(
            self.notes.len(),
            self.cursor,
            end,
            satori_core::viewport::VIEWPORT_BUFFER,
            satori_core::viewport::MAX_LIVE_IDS,
        );
        let ids = satori_core::viewport::ids_in_window(&self.notes, start, stop);
        self.core.set_viewport(ids);
    }

    /// A viewport engagement event landed in the store (WS2): re-seed the
    /// own-engagement mirrors (they are the glyphs' render state) and
    /// repaint. The same snapshot invalidation every data change rides —
    /// no new rendering path.
    fn on_engagement(&mut self, cx: &mut Context<Self>) {
        self.liked = self.core.liked_set();
        self.reposted = self.core.reposted_set();
        self.zapped = self.core.zapped_set();
        self.touch();
        cx.notify();
    }

    /// The wallet broadcast → glyph states + the quiet status whisper.
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
                self.status = "zap state unknown — check your wallet before retrying".into();
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
        self.touch();
        cx.notify();
    }

    /// Optimistic like, per §1.2: mutate state first (glyph flips vermilion
    /// now), reconcile against core's publish events later.
    fn toggle_like(&mut self, ev: &Event, cx: &mut Context<Self>) {
        let id = ev.id.to_string();
        if !self.liked.insert(id) {
            return;
        }
        self.touch();
        let mut rx = self.core.react(ev);
        cx.spawn(async move |_, _| {
            while rx.recv().await.is_some() {}
        })
        .detach();
    }

    /// Optimistic repost (NIP-18 kind 6): the glyph flips vermilion now, the
    /// event waits out the undo window in core (the shell's toast carries
    /// Undo), and a failure — or an undo — rolls the glyph back.
    fn toggle_repost(&mut self, ev: &Event, cx: &mut Context<Self>) {
        let id = ev.id.to_string();
        if !self.reposted.insert(id.clone()) {
            return;
        }
        self.touch();
        let mut rx = self.core.repost(ev);
        cx.spawn(async move |this, cx| {
            while let Some(pv) = rx.recv().await {
                match pv {
                    PublishEvent::Failed { .. } | PublishEvent::Undone { .. } => {
                        let _ = this.update(cx, |this, cx| {
                            this.reposted.remove(&id);
                            this.status = "repost didn't go through — try again".into();
                            this.touch();
                            cx.notify();
                        });
                        return;
                    }
                    _ => {}
                }
            }
        })
        .detach();
    }

    /// Bookmark toggle (D38): a real toggle now. The star fills for either
    /// list (private 10003 / public 30001 — the settings pref picks which
    /// one a star WRITES); a second click unbookmarks through the RMW that
    /// clears both. Optimistic, with rollback (the old add-only star made
    /// a second click a no-op).
    fn toggle_bookmark(&mut self, ev: &Event, cx: &mut Context<Self>) {
        let id = ev.id.to_string();
        let ev_id = ev.id;
        let adding = self.bookmarked.insert(id.clone());
        if !adding {
            self.bookmarked.remove(&id);
        }
        // The scope read rides the click: a mid-session pref flip applies
        // to the NEXT star, the running one keeps its list (prefs.rs's
        // mtime cache makes this a file stat).
        let public = prefs::get_bool("bookmarkPublic");
        self.touch();
        let mut rx = if adding {
            self.core.bookmark(ev, public)
        } else {
            self.core.unbookmark(ev_id)
        };
        cx.spawn(async move |this, cx| {
            while let Some(pv) = rx.recv().await {
                match pv {
                    PublishEvent::Failed { .. } => {
                        let _ = this.update(cx, |this, cx| {
                            if adding {
                                this.bookmarked.remove(&id);
                            } else {
                                this.bookmarked.insert(id.clone());
                            }
                            this.status = "bookmark didn't go through — try again".into();
                            this.touch();
                            cx.notify();
                        });
                    }
                    PublishEvent::Committed { .. } if !adding => {
                        // Unbookmarking from the Library's list drops the
                        // row (D38); everywhere else only the star unfills.
                        let _ = this.update(cx, |this, cx| {
                            if matches!(this.mode, FeedMode::Bookmarks) {
                                this.notes.retain(|n| n.id != ev_id);
                                this.note_ids.remove(&ev_id);
                                this.list.reset(this.list_len());
                                this.touch();
                                cx.notify();
                            }
                        });
                    }
                    _ => {}
                }
            }
        })
        .detach();
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        // WS2 (viewport.rs): the pane is gone — its viewport ids go with
        // it. The manager's task plans a Drop and closes the subscription;
        // nothing keeps a REQ alive for a feed nobody renders.
        self.core.set_viewport(Vec::new());
    }
}

/// How long a bolt press reads as "deliberate" (the sheet, not the tap).
const ZAP_HOLD: std::time::Duration = std::time::Duration::from_millis(500);

/// The chip's label — the count, capped at 99+ (a four-digit count would
/// set the chip's width, and nobody reads the tail of a stampede). What
/// the click does is the tooltip's job, not the label's.
fn new_notes_label(n: usize) -> String {
    if n == 1 {
        "1 new note".to_string()
    } else if n > 99 {
        "99+ new notes".to_string()
    } else {
        format!("{n} new notes")
    }
}

// The card takes the full render-time snapshot — one struct would hide the
// shape, and GPUI's no-entity-reads-during-render rule makes them explicit.
#[allow(clippy::too_many_arguments)]
fn note_card(
    idx: usize,
    ev: &Event,
    profiles: &HashMap<String, Profile>,
    parents: &HashMap<String, ParentNote>,
    liked: &HashSet<String>,
    bookmarked: &HashSet<String>,
    reposted: &HashSet<String>,
    zapped: &HashSet<String>,
    zap_pending: &HashSet<String>,
    wallet_ready: bool,
    reduced_motion: bool,
    media: &media::Snapshot,
    quotes: &content::Quotes,
    cards: &cards::CardState,
    // The display timestamp under the current timefmt mode (the item
    // closure computes it — this fn has no cx to read the mode with).
    time: String,
    handle: Entity<Feed>,
) -> impl IntoElement {
    // TODO(port): note_card.rs layout — P0 minimal card; name fallback chain
    // matches Profile::display (render/util.ts style).
    // One owned Arc up front: the eight action closures below each need the
    // event, and cloning the whole Event per closure was eight deep copies
    // of every visible row, every frame — Arc bumps now. (The list hands
    // out &Event; the single clone here is the row's only deep copy.)
    let ev = std::sync::Arc::new(ev.clone());
    let id = ev.id.to_string();
    let key = ev.pubkey.to_string();
    let profile = profiles.get(&key);
    let name = profile
        .map(|p| p.display())
        .unwrap_or_else(|| format!("{}…", &key[..8]));
    let picture = profile.map(|p| p.picture.clone()).unwrap_or_default();
    let engaged = liked.contains(&id);
    let saved = bookmarked.contains(&id);
    let shared = reposted.contains(&id);
    let zapped = zapped.contains(&id);
    let zapping = zap_pending.contains(&id);
    let ev_for_open = ev.clone();
    let ev_for_profile = ev.clone();
    let ev_for_like = ev.clone();
    let ev_for_bookmark = ev.clone();
    let ev_for_reply = ev.clone();
    let ev_for_repost = ev.clone();
    let ev_for_quote = ev.clone();
    let ev_for_zap = ev.clone();
    let handle_open = handle.clone();
    let handle_profile = handle.clone();
    let handle_like = handle.clone();
    let handle_bookmark = handle.clone();
    let handle_reply = handle.clone();
    let handle_repost = handle.clone();
    let handle_quote = handle.clone();
    let handle_zap = handle.clone();

    // No card box: posts sit directly on the washi, separated by whitespace
    // alone (sumi-e — the empty space carries the composition).
    // §2.1 anatomy: py S4 (12px/side, not 16), and the head→body gap (S2,
    // 4px) is TIGHTER than body→actions (S3, 8px) — hierarchy via one
    // exception here, not per-call nudges.
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .px_4()
        .py(px(theme::S4))
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(media::avatar(media, &picture, &name, 28.0))
                .child(
                    // Clicking the author opens their profile (nav via shell).
                    div()
                        .id(SharedString::from(format!("author-{idx}")))
                        .cursor_pointer()
                        .flex_1()
                        .text_size(px(theme::T_BODY))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::ink()))
                        .on_click(move |_, _, cx| {
                            let ev = ev_for_profile.clone();
                            handle_profile.update(cx, |_feed, cx| {
                                cx.emit(Nav::OpenProfile(ev.pubkey));
                            });
                        })
                        .child(name),
                )
                .child(
                    // The timestamp is the third mode toggle (timefmt.rs):
                    // tap cycles relative → date → block height.
                    div()
                        .id(SharedString::from(format!("time-{idx}")))
                        .cursor_pointer()
                        .text_size(px(theme::T_SMALL))
                        .text_color(rgb(theme::muted()))
                        .hover(|s| s.text_color(rgb(theme::ink())))
                        .on_click(|_, _, cx| crate::timefmt::cycle(cx))
                        .child(time),
                ),
        )
        // The reply context (nip10.ts replyContext, option B): the parent's
        // author + a snippet, fetched by id once and cached in the feed.
        // A whisper, not a box — one muted line with the elbow glyph; click
        // opens the parent's thread (the thread pane anchors by id, so this
        // works even before the parent resolves).
        .children(reply_whisper(idx, &ev, parents, profiles, &handle))
        .child(
            // Clicking the note body opens its thread (nav via the shell).
            div()
                .id(SharedString::from(format!("open-{idx}")))
                .cursor_pointer()
                .on_click(move |_, _, cx| {
                    let ev = ev_for_open.clone();
                    handle_open.update(cx, |_feed, cx| {
                        // Articles open the reader (the card body is the
                        // door — the "Read" seal is a second one, for the
                        // summary card's tail). Everything else threads —
                        // at the conversation's ROOT when the note is a
                        // reply (the reference never links the parent).
                        if u16::from(ev.kind) == 30023 {
                            cx.emit(Nav::OpenArticle((*ev).clone()));
                        } else {
                            let root = satori_core::nip::nip10::reply_root(&ev)
                                .and_then(|(id, _)| EventId::from_hex(&id).ok())
                                .unwrap_or(ev.id);
                            cx.emit(Nav::OpenThread(root));
                        }
                    });
                })
                .child(cards::row_body(
                    &ev,
                    &content::Body { media, quotes, profiles },
                    &handle,
                    cards,
                )),
        )
        .child(
            // §2.1: body→actions sits at S3 (8px) — the extra S1 on top of
            // the card's S2 gap.
            div()
                .flex()
                .gap_5()
                .items_center()
                .mt_1()
                // The reply door (thread.rs reply-to): opens the composer
                // carrying this note as the parent.
                .child(
                    div()
                        .id(SharedString::from(format!("reply-{idx}")))
                        .cursor_pointer()
                        .on_click(move |_, _, cx| {
                            let ev = ev_for_reply.clone();
                            handle_reply.update(cx, |_, cx| {
                                cx.emit(Nav::OpenComposer(Some((*ev).clone())));
                            });
                        })
                        .hover(|s| s.bg(theme::hover_wash()))
                        .child(icons::bubble_icon(theme::muted())),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("like-{idx}")))
                        .cursor_pointer()
                        .on_click(move |_, _, cx| {
                            let ev = ev_for_like.clone();
                            handle_like.update(cx, |feed, cx| feed.toggle_like(&ev, cx));
                        })
                        .hover(|s| s.bg(theme::hover_wash()))
                        .child(icons::heart_icon(if engaged {
                            theme::vermilion()
                        } else {
                            theme::muted()
                        })),
                )
                // The bolt (NIP-57, D20): tap = 21 sats, no dialog; hold
                // (~500ms) or right-click = the amount sheet. No wallet
                // behind the tap → the dim glyph opens the wallet pane.
                // States: dim (no wallet) / muted (idle) / amber breathing
                // (in flight) / vermilion (settled).
                .child({
                    let color = if zapping {
                        theme::warn()
                    } else if zapped {
                        theme::vermilion()
                    } else if !wallet_ready {
                        theme::hairline()
                    } else {
                        theme::muted()
                    };
                    let start = std::rc::Rc::new(std::cell::Cell::new(
                        Option::<std::time::Instant>::None,
                    ));
                    let bolt = div()
                        .id(SharedString::from(format!("zap-{idx}")))
                        .cursor_pointer()
                        .hover(|s| s.bg(theme::hover_wash()))
                        // Right-click opens the sheet at once.
                        .on_mouse_down(gpui::MouseButton::Right, {
                            let ev = ev_for_zap.clone();
                            let handle = handle_zap.clone();
                            move |_, _, cx| {
                                handle.update(cx, |_, cx| {
                                    cx.emit(Nav::OpenZapSheet((*ev).clone()));
                                });
                            }
                        })
                        // Left: remember when the press began; the release
                        // decides tap vs hold (no timer machinery, the
                        // decision is honest at release).
                        .on_mouse_down(gpui::MouseButton::Left, {
                            let start = start.clone();
                            move |_, _, _| start.set(Some(std::time::Instant::now()))
                        })
                        .on_mouse_up(gpui::MouseButton::Left, {
                            let start = start.clone();
                            let ev = ev_for_zap.clone();
                            let handle = handle_zap.clone();
                            move |_, _, cx| {
                                let Some(t0) = start.get() else { return };
                                start.set(None);
                                let held = t0.elapsed() >= ZAP_HOLD;
                                handle.update(cx, |feed, cx| {
                                    if held {
                                        cx.emit(Nav::OpenZapSheet((*ev).clone()));
                                    } else {
                                        feed.zap_tap(&ev, cx);
                                    }
                                });
                            }
                        })
                        .child(icons::bolt_icon(color));
                    // In flight and motion allowed: breathe (a slow opacity
                    // triangle — payment pending, nothing more).
                    if zapping && !reduced_motion {
                        bolt.with_animation(
                            SharedString::from(format!("zap-pulse-{idx}")),
                            gpui::Animation::new(std::time::Duration::from_millis(900)).repeat(),
                            |el, delta| el.opacity(0.55 - 0.25 * (2.0 * delta - 1.0).abs()),
                        )
                        .into_any_element()
                    } else if zapping {
                        // Reduced motion: a steady mid-amber, no pulsing.
                        div().opacity(0.7).child(bolt).into_any_element()
                    } else {
                        bolt.into_any_element()
                    }
                })
                .child(
                    div()
                        .id(SharedString::from(format!("repost-{idx}")))
                        .cursor_pointer()
                        .on_click(move |_, _, cx| {
                            let ev = ev_for_repost.clone();
                            handle_repost.update(cx, |feed, cx| feed.toggle_repost(&ev, cx));
                        })
                        .hover(|s| s.bg(theme::hover_wash()))
                        .child(icons::repost_icon(if shared {
                            theme::vermilion()
                        } else {
                            theme::muted()
                        })),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("quote-{idx}")))
                        .cursor_pointer()
                        .on_click(move |_, _, cx| {
                            let ev = ev_for_quote.clone();
                            handle_quote.update(cx, |_, cx| {
                                cx.emit(Nav::OpenQuote((*ev).clone()));
                            });
                        })
                        .hover(|s| s.bg(theme::hover_wash()))
                        .child(icons::quote_icon(theme::muted())),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("bookmark-{idx}")))
                        .cursor_pointer()
                        .on_click(move |_, _, cx| {
                            let ev = ev_for_bookmark.clone();
                            handle_bookmark.update(cx, |feed, cx| feed.toggle_bookmark(&ev, cx));
                        })
                        .hover(|s| s.bg(theme::hover_wash()))
                        .child(icons::bookmark_icon(if saved {
                            theme::vermilion()
                        } else {
                            theme::muted()
                        })),
                ),
        )
}

impl Render for Feed {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let handle = cx.entity();
        let status = self.status.clone();
        // Rebuild the render snapshot only when the DATA changed (any
        // mutation sets snap_dirty). Media is NOT in the snapshot: render
        // takes the memoized cache snapshot fresh each frame (same epoch =
        // refcount bump), so a landed image repaints its card without
        // deep-copying a thousand events.
        // The pending bolt's breathing honors the desktop's motion pref
        // (desktop.rs: the portal's prefers-reduced-motion).
        let reduced_motion = crate::desktop::Desktop::get(cx).reduced_motion;
        if self.snap_dirty || self.snap.is_none() {
            self.snap = Some(FeedSnap {
                notes: std::sync::Arc::new(self.notes.clone()),
                profiles: std::sync::Arc::new(self.profiles.clone()),
                parents: std::sync::Arc::new(self.parents.clone()),
                liked: std::sync::Arc::new(self.liked.clone()),
                bookmarked: std::sync::Arc::new(self.bookmarked.clone()),
                reposted: std::sync::Arc::new(self.reposted.clone()),
                zapped: std::sync::Arc::new(self.zapped.clone()),
                zap_pending: std::sync::Arc::new(self.zap_pending.clone()),
                wallet_ready: self.wallet_ready,
                quotes: std::sync::Arc::new(self.quotes.clone()),
                cards: std::sync::Arc::new(cards::CardState {
                    poll_tallies: self.poll_tallies.clone(),
                    poll_sel: self.poll_sel.clone(),
                    voted: self.voted.clone(),
                    rsvped: self.rsvped.clone(),
                }),
            });
            self.snap_dirty = false;
        }
        let media = media::Snapshot::new(cx);
        let snap = self.snap.as_ref().unwrap();
        let empty = snap.notes.is_empty();

        div()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            // (The 16px top band lives in the shell's content column —
            // every page wears the same padding.)
            .bg(rgb(theme::washi()))
            .text_color(rgb(theme::ink()))
            .font_family(theme::SANS)
            .flex()
            .flex_col()
            // Relay status line — the shell header carries the title now, so
            // this only speaks when there's something to say (loading; the
            // errors have their own strip below, §4.3).
            .when(!status.is_empty(), {
                let status = status.clone();
                move |d| {
                    d.child(
                        div()
                            .px_4()
                            .py_2()
                            .text_size(px(theme::T_SMALL))
                            .text_color(rgb(theme::muted()))
                            .border_b_1()
                            .border_color(rgb(theme::hairline()))
                            .child(status),
                    )
                }
            })
            // The kinded error strip (§4.3): dismissible, retryable.
            .when_some(self.error.clone(), |d, err| {
                let retry = handle.clone();
                let dismiss = handle.clone();
                d.child(states::error_strip(
                    &err,
                    // Retry: re-ask, per mode (retry_load).
                    move |_, _, cx| {
                        retry.update(cx, |feed, cx| feed.retry_load(cx));
                    },
                    // Dismiss: quiet, no reload.
                    move |_, _, cx| {
                        dismiss.update(cx, |feed, cx| {
                            feed.error = None;
                            cx.notify();
                        });
                    },
                ))
            })
            // The new-notes pill (the live subscription's push signal):
            // a small centered chip on the washi — content-sized, not a
            // pane-wide bar. Nothing else moves; the reader decides when
            // the list turns over.
            .when(self.live_new > 0 && !empty, |d| {
                let n = self.live_new;
                let handle = handle.clone();
                d.child(
                    div()
                        .w_full()
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .id("new-posts-pill")
                                .cursor_pointer()
                                .flex_none()
                                .mb_1()
                                .px_4()
                                .py_1p5()
                                .rounded_full()
                                .border_1()
                                .border_color(rgb(theme::hairline()))
                                .text_size(px(theme::T_SMALL))
                                .text_color(rgb(theme::vermilion()))
                                .hover(|s| {
                                    s.border_color(rgb(theme::vermilion()))
                                        .text_color(rgb(theme::ink()))
                                })
                                .tooltip(super::shell::tip("Load new notes"))
                                .on_click(move |_, _, cx| {
                                    handle.update(cx, |feed, cx| {
                                        feed.live_new = 0;
                                        feed.scroll_top();
                                        let rx = feed.core.refresh(feed.mode.kinds());
                                        feed.pump(rx, cx);
                                        cx.notify();
                                    });
                                })
                                .child(new_notes_label(n)),
                        ),
                )
            })
            .child(
                // The centered timeline column (the web app's reading width).
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .justify_center()
                    .child(if empty {
                        // §4.1: "the relays are still answering" was wrong for
                        // a persistently empty feed — it never resolves. Split:
                        // first load pending → the loading line; loaded, zero
                        // events → the honest quiet.
                        if !status.is_empty() {
                            states::empty(&status, None)
                        } else {
                            states::empty(
                                "Quiet here.",
                                Some("The relays remember nothing from this view."),
                            )
                        }
                    } else {
                        // GPUI rule (D13): an entity can't be read while it renders,
                        // so snapshot the data into the item closure; only the click
                        // handlers touch the entity, later, legally. Arc clones.
                        let notes = snap.notes.clone();
                        let profiles = snap.profiles.clone();
                        let parents = snap.parents.clone();
                        let liked = snap.liked.clone();
                        let reposted = snap.reposted.clone();
                        let bookmarked = snap.bookmarked.clone();
                        let zapped = snap.zapped.clone();
                        let zap_pending = snap.zap_pending.clone();
                        let wallet_ready = snap.wallet_ready;
                        let quotes = snap.quotes.clone();
                        let cards = snap.cards.clone();
                        let media = media.clone();
                        let paging = self.paging;
                        let has_more = self.has_more;
                        let core = self.core.clone();
                        list(self.list.clone(), move |idx, _w, cx| {
                            // Re-request what this VISIBLE card paints but
                            // the cache dropped — eviction is the RAM
                            // budget working, and this is what brings an
                            // evicted avatar back the moment its card
                            // scrolls into view. Ready/not-media are silent
                            // no-ops; only a landing notifies, so this
                            // can't loop a render.
                            if let Some(ev) = notes.get(idx) {
                                for url in card_media_urls(ev, &profiles) {
                                    media::fetch(&core, url, handle.entity_id(), cx);
                                }
                            }
                            match notes.get(idx) {
                                Some(ev) if u16::from(ev.kind) == 6 => {
                                    // The stamp reads the current mode +
                                    // blocktime cache per frame; ensure
                                    // refines Block mode from the bridge.
                                    let secs = ev.created_at.as_secs();
                                    let time = crate::timefmt::stamp(secs, cx);
                                    crate::timefmt::ensure(secs, cx);
                                    repost_card(
                                        ev, &profiles, &media, &quotes, &cards,
                                        time, handle.clone(),
                                    )
                                    .into_any_element()
                                }
                                Some(ev) => {
                                    let secs = ev.created_at.as_secs();
                                    let time = crate::timefmt::stamp(secs, cx);
                                    crate::timefmt::ensure(secs, cx);
                                    note_card(
                                        idx, ev, &profiles, &parents, &liked, &bookmarked,
                                        &reposted, &zapped, &zap_pending, wallet_ready,
                                        reduced_motion, &media,
                                        &quotes, &cards, time, handle.clone(),
                                    )
                                    .into_any_element()
                                }
                                None => end_cap(paging, has_more).into_any_element(),
                            }
                        })
                        // list() has no intrinsic height — it must be told to fill
                        // its container or it lays out at 0px and paints nothing.
                        .w_full()
                        .max_w(px(content::COL_W))
                        .h_full()
                        .into_any_element()
                    }),
            )
    }
}

/// Every media URL one card paints: the author's avatar, the body's
/// pictures, and for a repost both the booster's line and the boosted
/// card (which paints ITS author too). The lazy list renders only the
/// visible window, so this doubles as the visible re-request set —
/// evicted entries come back exactly when their card is seen.
fn card_media_urls(ev: &Event, profiles: &HashMap<String, Profile>) -> Vec<String> {
    fn one(note: &Event, profiles: &HashMap<String, Profile>, urls: &mut Vec<String>) {
        if let Some(p) = profiles.get(&note.pubkey.to_string()) {
            urls.push(p.picture.clone());
        }
        urls.extend(content::media_urls_cached(note).iter().cloned());
    }
    let mut urls: Vec<String> = Vec::new();
    if u16::from(ev.kind) == 6 {
        one(ev, profiles, &mut urls);
        if let Some(inner) = repost_inner(ev) {
            one(&inner, profiles, &mut urls);
        }
    } else {
        one(ev, profiles, &mut urls);
    }
    urls
}

/// The boosted event inside a kind-6 payload (NIP-18: the content is the
/// original event's JSON). Only kinds with a card here pass the guard.
fn repost_inner(ev: &Event) -> Option<Event> {
    let inner = serde_json::from_str::<Event>(&ev.content).ok()?;
    (inner.kind == Kind::TextNote || cards::is_feed_kind(inner.kind)).then_some(inner)
}

/// A repost row (NIP-18 kind 6): the reposter in the header slot, the
/// boosted note's card as the body. No action row — the boosted note
/// carries its own engagement where it was posted; clicking the body
/// opens the note (thread, or reader for articles).
fn repost_card(
    ev: &Event,
    profiles: &HashMap<String, Profile>,
    media: &media::Snapshot,
    quotes: &content::Quotes,
    cards: &cards::CardState,
    time: String,
    handle: Entity<Feed>,
) -> impl IntoElement {
    let key = ev.pubkey.to_string();
    let name = profiles
        .get(&key)
        .map(|p| p.display())
        .unwrap_or_else(|| format!("{}…", &key[..8]));
    let picture = profiles
        .get(&key)
        .map(|p| p.picture.clone())
        .unwrap_or_default();
    let inner = repost_inner(ev);
    let open = inner.clone();
    let open_handle = handle.clone();
    inner.map_or_else(
        || {
            // The absorb guard keeps these out of the list; a defensive
            // quiet line in case one slips through a race.
            div()
                .w_full()
                .px_4()
                .py_3()
                .text_size(px(theme::T_SMALL))
                .text_color(rgb(theme::muted()))
                .child(format!("{name} boosted"))
        },
        |note| {
            let is_article = u16::from(note.kind) == 30023;
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .px_4()
                .py_4()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(media::avatar(media, &picture, &name, 28.0))
                .child(
                    // The timestamp is the third mode toggle (timefmt.rs):
                    // tap cycles relative → date → block height.
                    div()
                        .id(SharedString::from(format!("boost-time-{}", ev.id)))
                        .cursor_pointer()
                        .text_size(px(theme::T_SMALL))
                        .text_color(rgb(theme::muted()))
                        .hover(|s| s.text_color(rgb(theme::ink())))
                        .on_click(|_, _, cx| crate::timefmt::cycle(cx))
                        .child(format!("{name} boosted · {time}")),
                ),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("repost-{}", ev.id)))
                        .cursor_pointer()
                        .on_click(move |_, _, cx| {
                            let note = open.clone().expect("inner held");
                            open_handle.update(cx, |_feed, cx| {
                                if is_article {
                                    cx.emit(Nav::OpenArticle(note.clone()));
                                } else {
                                    let root = satori_core::nip::nip10::reply_root(&note)
                                        .and_then(|(id, _)| EventId::from_hex(&id).ok())
                                        .unwrap_or(note.id);
                                    cx.emit(Nav::OpenThread(root));
                                }
                            });
                        })
                        .child(cards::row_body(
                            &note,
                            &content::Body {
                                media,
                                quotes,
                                profiles,
                            },
                            &handle,
                            cards,
                        )),
                )
        },
    )
}

/// The reply-context whisper: `↳ replying to Name — snippet…`, clickable to
/// the parent's thread. None for non-replies (nip10: quotes/mentions don't
/// count). The name resolves through the same profile map as the header.
fn reply_whisper(
    idx: usize,
    ev: &Event,
    parents: &HashMap<String, ParentNote>,
    profiles: &HashMap<String, Profile>,
    handle: &Entity<Feed>,
) -> Option<gpui::AnyElement> {
    // Replies are a kind-1 concern; articles/highlights carry their own
    // context headers and comment parents are NIP-22 (not on this feed).
    if u16::from(ev.kind) != 1 {
        return None;
    }
    let (parent_id, _relays) = nip10::reply_parent(ev)?;
    let parent = parents.get(&parent_id);
    // The name waits for the parent author's profile (the Parents event
    // hydrates it — see on_event); while it's away the whisper stays
    // quiet "↳ reply" — raw hex was never text anyone reads.
    let name = parent
        .as_ref()
        .and_then(|p| profiles.get(&p.author))
        .map(|pr| pr.display());
    let snippet = parent.and_then(|p| (!p.snippet.is_empty()).then(|| p.snippet.clone()));
    let parent_for_open = parent_id.clone();
    let handle = handle.clone();
    Some(
        div()
            // NOT "reply-{idx}": the action row's reply button owns that id
            // one sibling away, and two elements sharing an id path clobber
            // each other's interaction state — the owner's reply clicks
            // died exactly here (only reply cards have a whisper).
            .id(SharedString::from(format!("parent-{idx}")))
            .cursor_pointer()
            .pl(px(36.0)) // under the note's avatar column
            .flex()
            .flex_col()
            .gap_0p5()
            .on_click(move |_, _, cx| {
                let id = parent_for_open.clone();
                handle.update(cx, |_feed, cx| {
                    if let Ok(event_id) = EventId::from_hex(&id) {
                        cx.emit(Nav::OpenThread(event_id));
                    }
                });
            })
            // Two voices, no box: the addressed name in ink, the parent's
            // words in serif italic beneath — the quoted speaker gets their
            // own texture (sumi-e: whitespace and weight, not rules).
            .children({
                let mut rows: Vec<gpui::AnyElement> = Vec::new();
                match &name {
                    Some(name) => rows.push(
                        div()
                            .text_size(px(theme::T_SMALL))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(theme::ink()))
                            .child(format!("↳ replying to {name}"))
                            .into_any_element(),
                    ),
                    None => rows.push(
                        div()
                            .text_size(px(theme::T_SMALL))
                            .text_color(rgb(theme::muted()))
                            .child("↳ reply")
                            .into_any_element(),
                    ),
                }
                if let Some(snippet) = snippet {
                    // Even before the name lands, the parent's own words
                    // are worth their line.
                    rows.push(
                        div()
                            .font_family(theme::SERIF)
                            .italic()
                            .text_size(px(theme::T_UI))
                            .text_color(rgb(theme::muted()))
                            .child(snippet.clone())
                            .into_any_element(),
                    );
                }
                rows
            })
            .into_any_element(),
    )
}

/// The list's last row: the round-trip note while paging, the caught-up seal
/// when the relays remember nothing older (the GTK app's caught_up box), and
/// nothing visible while more pages wait quietly. The seal wears a short
/// hairline — the boundary is felt, not announced.
fn end_cap(paging: bool, more: bool) -> Div {
    let words = if paging {
        "… fetching older posts"
    } else if more {
        ""
    } else {
        "Caught up."
    };
    let mut cap = div()
        .w_full()
        .py_4()
        .flex()
        .flex_col()
        .items_center()
        .gap_2();
    if !words.is_empty() {
        cap = cap
            .child(div().w(px(48.)).border_t_1().border_color(rgb(theme::hairline())))
            .child(
                div()
                    .font_family(theme::SERIF)
                    .text_size(px(theme::T_UI))
                    .line_height(relative(1.2))
                    .text_color(rgb(theme::muted()))
                    .child(words.to_string()),
            );
    }
    cap
}

/// The window decision, pure for the tests: the index to truncate the
/// timeline to, or None while it's under the ceiling — or while the reader
/// sits too close to the trim line (their anchor row must stay above the
/// removed range, or the splice would yank them).
///
/// CEIL/KEEP: the window oscillates 1000 → 400 notes (~KB each; the snap
/// rebuild clones it). MARGIN: a wheel tick past the trigger can't shrink
/// the visible range retroactively — 80 rows of headroom is generous.
fn trim_target(len: usize, visible_end: usize) -> Option<usize> {
    const CEIL: usize = 1000;
    const KEEP: usize = 400;
    const MARGIN: usize = 80;
    if len <= CEIL {
        return None;
    }
    let to = len - KEEP;
    (visible_end + MARGIN <= to).then_some(to)
}

#[cfg(test)]
mod tests {
    use super::trim_target;

    #[test]
    fn window_stays_while_under_the_ceiling() {
        assert_eq!(trim_target(0, 0), None);
        assert_eq!(trim_target(999, 998), None);
        assert_eq!(trim_target(1000, 999), None);
    }

    #[test]
    fn window_trims_only_when_the_reader_is_clear_of_the_line() {
        // At the top (or anywhere below row 720): 1200 - 400 = 800.
        assert_eq!(trim_target(1200, 0), Some(800));
        // Exactly at the margin's edge (720 + 80 = 800): still clear.
        assert_eq!(trim_target(1200, 720), Some(800));
        // Deep reader — no trim, or the anchor would cross the range.
        assert_eq!(trim_target(1200, 721), None);
        assert_eq!(trim_target(1200, 1199), None);
    }
}
