//! The home feed, GPUI-rendered.
//!
//! TODO(port): crates/app/src/feed.rs + note_card.rs (and src/feed.js) — P0.
//! Read-only following feed with optimistic like; the receiver-pumping seam is
//! the GPUI twin of `bridge.rs`'s `drain`. Rich cards for the manifest
//! registry kinds render via cards.rs (P3).

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use gpui::{
    div, list, prelude::*, px, relative, rgb, Context, Entity, EventEmitter, FontWeight,
    ListAlignment,
    ListOffset, ListScrollEvent, ListState, Render, SharedString, Window, Div,
};
use nostr::prelude::*;
use satori_core::manager::{Core, FeedEvent, FeedRx, Live, ParentNote, Profile, PublishEvent};
use satori_core::nip::nip10;

use crate::{icons, media, theme};
use super::{content, nav::Nav, cards};

/// Which surface a feed pane carries: the timeline (the visible catalog)
/// or the articles shelf (kind 30023 from the same follows).
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum FeedMode {
    Timeline,
    Articles,
}
impl FeedMode {
    pub(crate) fn kinds(&self) -> Vec<Kind> {
        match self {
            FeedMode::Timeline => satori_core::store::visible_kinds(),
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
    quotes: content::Quotes,
    // Rich-card interaction state (cards.rs): poll tallies + ballots, and
    // my RSVP per calendar event.
    poll_tallies: HashMap<EventId, Vec<(String, u32)>>,
    poll_sel: HashMap<EventId, Vec<String>>,
    voted: HashSet<EventId>,
    rsvped: HashMap<EventId, String>,
    status: String,
    /// Endless scroll: the relays may have more; a round trip is in flight.
    has_more: bool,
    paging: bool,
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
    /// Deep-scroll state, emitted to the shell (Nav::FeedDeep) so the
    /// floating back-to-top whisper knows when to appear.
    deep: bool,
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
    quotes: std::sync::Arc<content::Quotes>,
    cards: std::sync::Arc<cards::CardState>,
    media: std::sync::Arc<media::Snapshot>,
    media_epoch: u64,
}

impl EventEmitter<Nav> for Feed {}

impl Feed {
    pub fn new(core: Arc<Core>, cx: &mut Context<Self>) -> Self {
        crate::theme::watch(cx); // repaint on theme change
        Self::with_mode(core, FeedMode::Timeline, cx)
    }

    /// The articles shelf: same follows, same paging, kind 30023 only.
    pub fn articles(core: Arc<Core>, cx: &mut Context<Self>) -> Self {
        Self::with_mode(core, FeedMode::Articles, cx)
    }

    fn with_mode(core: Arc<Core>, mode: FeedMode, cx: &mut Context<Self>) -> Self {
        let feed = Self {
            liked: core.liked_set(),
            bookmarked: core.bookmarked_set(),
            reposted: core.reposted_set(),
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
            has_more: true,
            paging: false,
            snap: None,
            snap_dirty: true,
            mode,
            note_ids: HashSet::new(),
            last_visible_end: 0,
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
                // Deep-scroll whisper: the shell floats a back-to-top button
                // once the reader is a dozen items down. Emit on change only.
                let deep = ev.visible_range.start >= 10;
                if feed.deep != deep {
                    feed.deep = deep;
                    cx.emit(Nav::FeedDeep(deep));
                }
                feed.maybe_older(feed.last_visible_end, cx);
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
                            feed.live_new += 1;
                            cx.notify();
                        });
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(_) => continue, // lagged: still true that there's news
                }
            }
        })
        .detach();
        let boot = feed.core.bootstrap(mode.kinds());
        feed.pump(boot, cx);
        feed
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
        self.list.scroll_to(ListOffset {
            item_ix: 0,
            offset_in_item: px(0.),
        });
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
        if visible_end + LOOKAHEAD < self.notes.len() {
            return;
        }
        self.paging = true;
        let oldest = self
            .notes
            .last()
            .map(|n| n.created_at.as_secs())
            .unwrap_or(0);
        self.pump(self.core.older(oldest, self.mode.kinds()), cx);
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
                self.has_more = true;
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
                self.parents.extend(p);
                cx.notify();
            }
            FeedEvent::Failed(e) => {
                self.status = format!("… {e}");
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
            Reflow::Append(old) => self.list.splice(old..old, self.list_len()),
            _ => {}
        }
        cx.notify();
    }

    fn absorb(
        &mut self,
        notes: Vec<Event>,
        profiles: Vec<Profile>,
        cx: &mut Context<Self>,
    ) {
        self.absorb_profiles(profiles, cx);
        for note in notes {
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
            self.note_ids.insert(note.id);
            self.notes.push(note);
        }
        self.notes.sort_by_key(|n| std::cmp::Reverse(n.created_at));
        // Ask core for any authors we still can't name — plus mentioned
        // pubkeys, so a mention renders @name instead of @npub1…
        // (TODO(port): content.ts mentionPubkeys — hydrate before render).
        let missing: Vec<PublicKey> = {
            let mut seen = HashSet::new();
            self.notes
                .iter()
                .flat_map(|n| {
                    let mut pks = vec![n.pubkey];
                    pks.extend(content::mention_pks(&n.content));
                    pks
                })
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
        for url in content::media_urls(note) {
            media::fetch(&self.core, url, cx, |_, _, _, cx| cx.notify());
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
        media::fetch(&self.core, url, cx, |_, _, _, cx| cx.notify());
    }

    /// Optimistic like, per §1.2: mutate state first (glyph flips vermilion
    /// now), reconcile against core's publish events later.
    fn toggle_like(&mut self, ev: &Event) {
        let id = ev.id.to_string();
        if !self.liked.insert(id) {
            return;
        }
        self.touch();
        let mut rx = self.core.react(ev);
        std::thread::spawn(move || {
            while rx.blocking_recv().is_some() {}
        });
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

    /// Optimistic bookmark (private kind:10003 list) — engaged at once,
    /// rolled back if core reports the publish failed.
    fn toggle_bookmark(&mut self, ev: &Event, cx: &mut Context<Self>) {
        let id = ev.id.to_string();
        if !self.bookmarked.insert(id.clone()) {
            return;
        }
        self.touch();
        let mut rx = self.core.bookmark(ev);
        cx.spawn(async move |this, cx| {
            while let Some(pv) = rx.recv().await {
                if let PublishEvent::Failed { .. } = pv {
                    let _ = this.update(cx, |this, cx| {
                        this.bookmarked.remove(&id);
                        this.status = "bookmark didn't go through — try again".into();
                        this.touch();
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }
}

pub(crate) fn rel_time(secs: u64) -> String {
    // TODO(port): render/util.ts relative time — P0 minimal form.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let d = now.saturating_sub(secs);
    match d {
        0..60 => "now".into(),
        _ if d < 3600 => format!("{}m", d / 60),
        _ if d < 86400 => format!("{}h", d / 3600),
        _ if d < 604_800 => format!("{}d", d / 86400),
        _ => format!("{}w", d / 604_800),
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
    media: &media::Snapshot,
    quotes: &content::Quotes,
    cards: &cards::CardState,
    handle: Entity<Feed>,
) -> impl IntoElement {
    // TODO(port): note_card.rs layout — P0 minimal card; name fallback chain
    // matches Profile::display (render/util.ts style).
    let key = ev.pubkey.to_string();
    let profile = profiles.get(&key);
    let name = profile
        .map(|p| p.display())
        .unwrap_or_else(|| format!("{}…", &key[..8]));
    let picture = profile.map(|p| p.picture.clone()).unwrap_or_default();
    let time = rel_time(ev.created_at.as_secs());
    let engaged = liked.contains(&ev.id.to_string());
    let saved = bookmarked.contains(&ev.id.to_string());
    let shared = reposted.contains(&ev.id.to_string());
    let ev_for_open = ev.clone();
    let ev_for_profile = ev.clone();
    let ev_for_like = ev.clone();
    let ev_for_bookmark = ev.clone();
    let ev_for_reply = ev.clone();
    let ev_for_repost = ev.clone();
    let ev_for_quote = ev.clone();
    let handle_open = handle.clone();
    let handle_profile = handle.clone();
    let handle_like = handle.clone();
    let handle_bookmark = handle.clone();
    let handle_reply = handle.clone();
    let handle_repost = handle.clone();
    let handle_quote = handle.clone();

    // No card box: posts sit directly on the washi, separated by whitespace
    // alone (sumi-e — the empty space carries the composition).
    div()
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
                    // Clicking the author opens their profile (nav via shell).
                    div()
                        .id(SharedString::from(format!("author-{idx}")))
                        .cursor_pointer()
                        .flex_1()
                        .text_size(px(13.0))
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
                    div()
                        .text_size(px(12.0))
                        .text_color(rgb(theme::muted()))
                        .child(time),
                ),
        )
        // The reply context (nip10.ts replyContext, option B): the parent's
        // author + a snippet, fetched by id once and cached in the feed.
        // A whisper, not a box — one muted line with the elbow glyph; click
        // opens the parent's thread (the thread pane anchors by id, so this
        // works even before the parent resolves).
        .children(reply_whisper(idx, ev, parents, profiles, &handle))
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
                            cx.emit(Nav::OpenArticle(ev.clone()));
                        } else {
                            let root = satori_core::nip::nip10::reply_root(&ev)
                                .and_then(|(id, _)| EventId::from_hex(&id).ok())
                                .unwrap_or(ev.id);
                            cx.emit(Nav::OpenThread(root));
                        }
                    });
                })
                .child(cards::row_body(
                    ev,
                    &content::Body { media, quotes, profiles },
                    &handle,
                    cards,
                )),
        )
        .child(
            div()
                .flex()
                .gap_5()
                .items_center()
                // The reply door (thread.rs reply-to): opens the composer
                // carrying this note as the parent.
                .child(
                    div()
                        .id(SharedString::from(format!("reply-{idx}")))
                        .cursor_pointer()
                        .on_click(move |_, _, cx| {
                            let ev = ev_for_reply.clone();
                            handle_reply.update(cx, |_, cx| {
                                cx.emit(Nav::OpenComposer(Some(ev)));
                            });
                        })
                        .child(icons::bubble_icon(theme::muted())),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("like-{idx}")))
                        .cursor_pointer()
                        .on_click(move |_, _, cx| {
                            let ev = ev_for_like.clone();
                            handle_like.update(cx, |feed, _| feed.toggle_like(&ev));
                        })
                        .child(icons::heart_icon(if engaged {
                            theme::vermilion()
                        } else {
                            theme::muted()
                        })),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("repost-{idx}")))
                        .cursor_pointer()
                        .on_click(move |_, _, cx| {
                            let ev = ev_for_repost.clone();
                            handle_repost.update(cx, |feed, cx| feed.toggle_repost(&ev, cx));
                        })
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
                                cx.emit(Nav::OpenQuote(ev));
                            });
                        })
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
        // Rebuild the render snapshot only when something actually changed
        // (any mutation sets snap_dirty; the media cache bumps its epoch on
        // every render-visible image arrival/eviction). Frames in between —
        // and there are many, since every view notify repaints the shell —
        // just bump refcounts.
        let media_epoch = media::generation(cx);
        let stale = self.snap_dirty
            || self.snap.as_ref().is_none_or(|s| s.media_epoch != media_epoch);
        if stale {
            self.snap = Some(FeedSnap {
                notes: std::sync::Arc::new(self.notes.clone()),
                profiles: std::sync::Arc::new(self.profiles.clone()),
                parents: std::sync::Arc::new(self.parents.clone()),
                liked: std::sync::Arc::new(self.liked.clone()),
                bookmarked: std::sync::Arc::new(self.bookmarked.clone()),
                reposted: std::sync::Arc::new(self.reposted.clone()),
                quotes: std::sync::Arc::new(self.quotes.clone()),
                cards: std::sync::Arc::new(cards::CardState {
                    poll_tallies: self.poll_tallies.clone(),
                    poll_sel: self.poll_sel.clone(),
                    voted: self.voted.clone(),
                    rsvped: self.rsvped.clone(),
                }),
                media: std::sync::Arc::new(media::Snapshot::new(cx)),
                media_epoch,
            });
            self.snap_dirty = false;
        }
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
            // this only speaks when there's something to say.
            .when(!status.is_empty(), |d| {
                d.child(
                    div()
                        .px_4()
                        .py_2()
                        .text_size(px(12.0))
                        .text_color(rgb(theme::muted()))
                        .border_b_1()
                        .border_color(rgb(theme::hairline()))
                        .child(status),
                )
            })
            // The new-notes pill (the live subscription's push signal):
            // quiet, centered, only when there's news. Nothing else moves —
            // the reader decides when the list turns over.
            .when(self.live_new > 0 && !empty, |d| {
                let n = self.live_new;
                let handle = handle.clone();
                d.child(
                    div()
                        .id("new-posts-pill")
                        .cursor_pointer()
                        .mb_1()
                        .px_4()
                        .py_1p5()
                        .rounded_full()
                        .border_1()
                        .border_color(rgb(theme::hairline()))
                        .text_size(px(12.0))
                        .text_color(rgb(theme::muted()))
                        .on_click(move |_, _, cx| {
                            handle.update(cx, |feed, cx| {
                                feed.live_new = 0;
                                feed.scroll_top();
                                let rx = feed.core.refresh(feed.mode.kinds());
                                feed.pump(rx, cx);
                                cx.notify();
                            });
                        })
                        .child(if n == 1 {
                            "1 new note — load it".to_string()
                        } else {
                            format!("{n} new notes — load them")
                        }),
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
                        div()
                            .w_full()
                            .max_w(px(content::COL_W))
                            .p_8()
                            .text_color(rgb(theme::muted()))
                            .child("Nothing here yet — the relays are still answering.")
                            .into_any_element()
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
                        let quotes = snap.quotes.clone();
                        let cards = snap.cards.clone();
                        let media = snap.media.clone();
                        let paging = self.paging;
                        let has_more = self.has_more;
                        list(self.list.clone(), move |idx, _w, _cx| {
                            match notes.get(idx) {
                                Some(ev) if u16::from(ev.kind) == 6 => repost_card(
                                    ev, &profiles,  &media, &quotes, &cards,
                                    handle.clone(),
                                )
                                .into_any_element(),
                                Some(ev) => note_card(
                                    idx, ev, &profiles, &parents, &liked, &bookmarked,
                                    &reposted, &media,
                                    &quotes, &cards, handle.clone(),
                                )
                                .into_any_element(),
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
    let time = rel_time(ev.created_at.as_secs());
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
                .text_size(px(12.0))
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
                .py_3()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(media::avatar(media, &picture, &name, 28.0))
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(rgb(theme::muted()))
                                .child(format!("{name} boosted · {time}")),
                        ),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("repost-{}", ev.id.to_string())))
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
    let name = parent.as_ref().map(|p| {
        profiles
            .get(&p.author)
            .map(|pr| pr.display())
            .unwrap_or_else(|| format!("{}…", &p.author[..8]))
    });
    let snippet = parent.and_then(|p| (!p.snippet.is_empty()).then(|| p.snippet.clone()));
    let parent_for_open = parent_id.clone();
    let handle = handle.clone();
    Some(
        div()
            .id(SharedString::from(format!("reply-{idx}")))
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
            .children(match (&name, &snippet) {
                (Some(name), Some(snippet)) => vec![
                    div()
                        .text_size(px(12.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::ink()))
                        .child(format!("↳ replying to {name}")),
                    div()
                        .font_family(theme::SERIF)
                        .italic()
                        .text_size(px(12.5))
                        .text_color(rgb(theme::muted()))
                        .child(snippet.clone()),
                ]
                .into_iter()
                .map(|d| d.into_any_element())
                .collect::<Vec<_>>(),
                (Some(name), None) => {
                    vec![div()
                        .text_size(px(12.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::ink()))
                        .child(format!("↳ replying to {name}"))
                        .into_any_element()]
                }
                _ => vec![div()
                    .text_size(px(12.0))
                    .text_color(rgb(theme::muted()))
                    .child("↳ reply")
                    .into_any_element()],
            })
            .into_any_element(),
    )
}

/// The list's last row: the round-trip note while paging, the caught-up seal
/// when the relays remember nothing older (the GTK app's caught_up box), and
/// nothing visible while more pages wait quietly.
fn end_cap(paging: bool, more: bool) -> Div {
    let words = if paging {
        "… fetching older posts"
    } else if more {
        ""
    } else {
        "Caught up."
    };
    div()
        .w_full()
        .py_4()
        .flex()
        .justify_center()
        .child(
            div()
                .font_family(theme::SERIF)
                .text_size(px(13.0))
                .line_height(relative(1.2))
                .text_color(rgb(theme::muted()))
                .child(words.to_string()),
        )
}
