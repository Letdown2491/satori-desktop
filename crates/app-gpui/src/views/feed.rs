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
    ListScrollEvent, ListState, Render, SharedString, Window, Div,
};
use nostr::prelude::*;
use satori_core::manager::{Core, FeedEvent, FeedRx, Profile, PublishEvent};

use crate::{icons, media, theme};
use super::{content, nav::Nav, cards};

pub struct Feed {
    core: Arc<Core>,
    notes: Vec<Event>,
    profiles: HashMap<String, Profile>,
    liked: HashSet<String>,
    bookmarked: HashSet<String>,
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
    list: ListState,
}

impl EventEmitter<Nav> for Feed {}

impl Feed {
    pub fn new(core: Arc<Core>, cx: &mut Context<Self>) -> Self {
        let feed = Self {
            core,
            notes: vec![],
            profiles: HashMap::new(),
            liked: HashSet::new(),
            bookmarked: HashSet::new(),
            quotes: HashMap::new(),
            poll_tallies: HashMap::new(),
            poll_sel: HashMap::new(),
            voted: HashSet::new(),
            rsvped: HashMap::new(),
            status: "connecting to the relays…".into(),
            has_more: true,
            paging: false,
            list: ListState::new(0, ListAlignment::Top, px(400.)),
        };
        // Endless scroll: the list's own scroll stream orders the next page
        // (the GTK app prefetched at 70% scroll — same idea). The handler
        // runs on &mut App; WeakEntity::update accepts that.
        let this = cx.entity().downgrade();
        feed.list.set_scroll_handler(move |ev: &ListScrollEvent, _w, cx| {
            let _ = this.update(cx, |feed, cx| feed.maybe_older(ev.visible_range.end, cx));
        });
        let boot = feed.core.bootstrap();
        feed.pump(boot, cx);
        feed
    }

    /// The list's item count: one row per note + the end cap.
    fn list_len(&self) -> usize {
        self.notes.len() + 1
    }

    /// Near the bottom, order the next older page. The scroll handler calls
    /// this on every wheel tick — `paging` keeps it one round trip at a time.
    fn maybe_older(&mut self, visible_end: usize, cx: &mut Context<Self>) {
        const LOOKAHEAD: usize = 6;
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
        self.pump(self.core.older(oldest), cx);
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

    fn on_event(&mut self, ev: FeedEvent, cx: &mut Context<Self>) {
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
                self.status.clear();
                self.has_more = true;
            }
            FeedEvent::Older(page, more) => {
                self.has_more = more;
                self.paging = false;
                let old = self.notes.len();
                self.absorb(page.notes, page.profiles, cx);
                if self.notes.len() > old {
                    // Appends keep the reader's place: reset() clears the
                    // scroll anchor and would yank the pane to the top,
                    // while splice() leaves logical_scroll_top alone (the
                    // anchor survives a non-overlapping splice).
                    reflow = Reflow::Append(old);
                } else {
                    reflow = Reflow::None;
                }
            }
            FeedEvent::Profiles(profiles) => self.absorb_profiles(profiles, cx),
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
            if self.notes.iter().any(|n| n.id == note.id) {
                continue;
            }
            self.want_images(&note, cx);
            self.want_quotes(&note, cx);
            if note.kind == Kind::Custom(1068) {
                self.want_poll(&note, cx);
            }
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
                        // Fresh tally including our own vote.
                        let mut rx = this.core.poll_votes(id);
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
        let mut rx = self.core.react(ev);
        std::thread::spawn(move || {
            while rx.blocking_recv().is_some() {}
        });
    }

    /// Optimistic bookmark (private kind:10003 list) — engaged at once,
    /// rolled back if core reports the publish failed.
    fn toggle_bookmark(&mut self, ev: &Event, cx: &mut Context<Self>) {
        let id = ev.id.to_string();
        if !self.bookmarked.insert(id.clone()) {
            return;
        }
        let mut rx = self.core.bookmark(ev);
        cx.spawn(async move |this, cx| {
            while let Some(pv) = rx.recv().await {
                if let PublishEvent::Failed { .. } = pv {
                    let _ = this.update(cx, |this, cx| {
                        this.bookmarked.remove(&id);
                        this.status = "bookmark didn't go through — try again".into();
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
    liked: &HashSet<String>,
    bookmarked: &HashSet<String>,
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
    let ev_for_open = ev.clone();
    let ev_for_profile = ev.clone();
    let ev_for_like = ev.clone();
    let ev_for_bookmark = ev.clone();
    let handle_open = handle.clone();
    let handle_profile = handle.clone();
    let handle_like = handle.clone();
    let handle_bookmark = handle.clone();

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
                        .text_color(rgb(theme::INK))
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
                        .text_color(rgb(theme::MUTED))
                        .child(time),
                ),
        )
        .child(
            // Clicking the note body opens its thread (nav via the shell).
            div()
                .id(SharedString::from(format!("open-{idx}")))
                .cursor_pointer()
                .on_click(move |_, _, cx| {
                    let ev = ev_for_open.clone();
                    handle_open.update(cx, |_feed, cx| cx.emit(Nav::OpenThread(ev.id)));
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
                .child(icons::bubble_icon(theme::MUTED))
                .child(
                    div()
                        .id(SharedString::from(format!("like-{idx}")))
                        .cursor_pointer()
                        .on_click(move |_, _, cx| {
                            let ev = ev_for_like.clone();
                            handle_like.update(cx, |feed, _| feed.toggle_like(&ev));
                        })
                        .child(icons::heart_icon(if engaged {
                            theme::VERMILION
                        } else {
                            theme::MUTED
                        })),
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
                            theme::VERMILION
                        } else {
                            theme::MUTED
                        })),
                ),
        )
}

impl Render for Feed {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let handle = cx.entity();
        let status = self.status.clone();
        let empty = self.notes.is_empty();
        let media = media::Snapshot::new(cx);

        div()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .bg(rgb(theme::WASHI))
            .text_color(rgb(theme::INK))
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
                        .text_color(rgb(theme::MUTED))
                        .border_b_1()
                        .border_color(rgb(theme::HAIRLINE))
                        .child(status),
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
                            .text_color(rgb(theme::MUTED))
                            .child("Nothing here yet — the relays are still answering.")
                            .into_any_element()
                    } else {
                        // GPUI rule (D13): an entity can't be read while it renders,
                        // so snapshot the data into the item closure; only the click
                        // handlers touch the entity, later, legally.
                        let notes = self.notes.clone();
                        let profiles = self.profiles.clone();
                        let liked = self.liked.clone();
                        let bookmarked = self.bookmarked.clone();
                        let quotes = self.quotes.clone();
                        let cards = cards::CardState {
                            poll_tallies: self.poll_tallies.clone(),
                            poll_sel: self.poll_sel.clone(),
                            voted: self.voted.clone(),
                            rsvped: self.rsvped.clone(),
                        };
                        let paging = self.paging;
                        let has_more = self.has_more;
                        list(self.list.clone(), move |idx, _w, _cx| {
                            match notes.get(idx) {
                                Some(ev) => note_card(
                                    idx, ev, &profiles, &liked, &bookmarked, &media, &quotes,
                                    &cards, handle.clone(),
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
                .text_color(rgb(theme::MUTED))
                .child(words.to_string()),
        )
}
