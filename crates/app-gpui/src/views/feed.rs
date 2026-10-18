//! The home feed, GPUI-rendered.
//!
//! TODO(port): crates/app/src/feed.rs + note_card.rs (and src/feed.js) — P0.
//! Read-only following feed with optimistic like; the receiver-pumping seam is
//! the GPUI twin of `bridge.rs`'s `drain`. Text notes only so far — rich cards
//! land in P2/P3 per §9.1.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use gpui::{
    div, list, prelude::*, px, rgb, Context, Entity, EventEmitter, FontWeight, Image,
    ListAlignment, ListState, Render, SharedString, Window,
};
use nostr::prelude::*;
use satori_core::manager::{Core, FeedEvent, FeedRx, Profile, PublishEvent};

use crate::{icons, media, theme};
use super::{content, nav::Nav};

pub struct Feed {
    core: Arc<Core>,
    notes: Vec<Event>,
    profiles: HashMap<String, Profile>,
    liked: HashSet<String>,
    bookmarked: HashSet<String>,
    images: HashMap<String, Arc<Image>>,
    quotes: content::Quotes,
    status: String,
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
            images: HashMap::new(),
            quotes: HashMap::new(),
            status: "connecting to the relays…".into(),
            list: ListState::new(0, ListAlignment::Top, px(400.)),
        };
        let boot = feed.core.bootstrap();
        feed.pump(boot, cx);
        feed
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
        match ev {
            FeedEvent::Loaded(page) => {
                self.absorb(page.notes, page.profiles, cx);
                self.status.clear();
            }
            FeedEvent::Older(page, _more) => {
                self.absorb(page.notes, page.profiles, cx);
            }
            FeedEvent::Profiles(profiles) => self.absorb_profiles(profiles, cx),
            FeedEvent::Failed(e) => self.status = format!("… {e}"),
            _ => {}
        }
        self.list.reset(self.notes.len());
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
            // TODO(port): note_card.rs renders all kinds via the manifest
            // registry; the GPUI port covers text + picture notes (P2).
            if note.kind != Kind::TextNote && note.kind != Kind::Custom(20) {
                continue;
            }
            if self.notes.iter().any(|n| n.id == note.id) {
                continue;
            }
            self.want_images(&note, cx);
            self.want_quotes(&note, cx);
            self.notes.push(note);
        }
        self.notes.sort_by_key(|n| std::cmp::Reverse(n.created_at));
        // Ask core for any authors we still can't name.
        let missing: Vec<PublicKey> = {
            let mut seen = HashSet::new();
            self.notes
                .iter()
                .map(|n| n.pubkey)
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

    /// Queue core fetches for every media URL we haven't got yet.
    fn want_images(&mut self, note: &Event, cx: &mut Context<Self>) {
        for url in content::media_urls(note) {
            if self.images.contains_key(&url) {
                continue;
            }
            media::fetch_png(&self.core, url, cx, |this, url, png, cx| {
                if let Some(png) = png {
                    this.images.insert(url, png);
                }
                cx.notify();
            });
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

    /// Queue the profile picture fetch (core's guarded, disk-cached path).
    fn want_avatar(&mut self, profile: &Profile, cx: &mut Context<Self>) {
        let url = profile.picture.clone();
        if url.is_empty() || self.images.contains_key(&url) {
            return;
        }
        media::fetch_png(&self.core, url, cx, |this, url, png, cx| {
            if let Some(png) = png {
                this.images.insert(url, png);
            }
            cx.notify();
        });
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
    images: &HashMap<String, Arc<Image>>,
    quotes: &content::Quotes,
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

    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .p_4()
        .bg(rgb(theme::PANEL))
        .border_b_1()
        .border_color(rgb(theme::HAIRLINE))
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(media::avatar(images, &picture, &name, 28.0))
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
                .child(content::body(ev, images, quotes, handle)),
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
                div().flex_1().min_h_0().child(if empty {
                    div()
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
                    let images = self.images.clone();
                    let quotes = self.quotes.clone();
                    list(self.list.clone(), move |idx, _w, _cx| {
                        match notes.get(idx) {
                            Some(ev) => note_card(
                                idx, ev, &profiles, &liked, &bookmarked, &images, &quotes,
                                handle.clone(),
                            )
                            .into_any_element(),
                            None => div().into_any_element(),
                        }
                    })
                    // list() has no intrinsic height — it must be told to fill
                    // its container or it lays out at 0px and paints nothing.
                    .w_full()
                    .h_full()
                    .into_any_element()
                }),
            )
    }
}
