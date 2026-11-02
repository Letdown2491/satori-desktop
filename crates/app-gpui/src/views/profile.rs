//! Profile view: an author's metadata + their notes.
//!
//! TODO(port): crates/app/src/profile.rs — P1. Name/about + note list; banner
//! and avatar images wait for the P2 image pipeline (core fetch → gpui `img`).
//! Follow/mute management ports with the rest of the settings scope (P3).

use std::{collections::HashMap, sync::Arc};

use gpui::{
    div, img, list, prelude::*, px, rgb, AnyElement, Context, Div, Entity, EventEmitter,
    FontWeight, ListAlignment, ListState, ObjectFit, Render, SharedString, Window,
};
use nostr::prelude::*;
use satori_core::manager::{Core, FeedEvent, FeedRx, Profile};

use crate::{media, theme};
use super::{content, nav::Nav};

/// Which profile surface is up: the notes body or the media strips
/// (Phase 2 §4.3 — Photos/Videos, three-column thumbnails).
#[derive(Clone, Copy, PartialEq)]
enum ProfileTab {
    Notes,
    Photos,
    Videos,
}

/// One list row of the active tab (render-time projection).
enum Row {
    Note(Event),
    /// Up to three thumbnails: (url, index into the tab's lightbox list).
    Pics(Vec<(String, usize)>),
    Vid(String, Option<String>),
}

pub struct ProfileView {
    core: Arc<Core>,
    who: PublicKey,
    profile: Option<Profile>,
    notes: Vec<Event>,
    /// The active surface (notes body / media strips — Phase 2 §4.3).
    tab: ProfileTab,
    following: bool,
    /// Known profiles — mention names in note bodies (word_flow).
    profiles: HashMap<String, Profile>,
    quotes: content::Quotes,
    status: String,
    list: ListState,
}

impl EventEmitter<Nav> for ProfileView {}

impl ProfileView {
    pub fn new(core: Arc<Core>, who: PublicKey, cx: &mut Context<Self>) -> Self {
        crate::theme::watch(cx); // repaint on theme change
        let view = Self {
            following: core.is_following(&who),
            who,
            profile: None,
            notes: vec![],
            tab: ProfileTab::Notes,
            profiles: HashMap::new(),
            quotes: HashMap::new(),
            status: "loading…".into(),
            list: ListState::new(0, ListAlignment::Top, px(400.)),
            core,
        };
        let rx = view.core.profile(view.who);
        view.pump(rx, cx);
        view
    }

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
                for p in &page.profiles {
                    self.profiles.insert(p.pubkey.clone(), p.clone());
                }
                self.profile = page
                    .profiles
                    .iter()
                    .find(|p| p.pubkey == self.who.to_string())
                    .cloned();
                if let Some(p) = self.profile.clone() {
                    self.want_avatar(&p, cx);
                }
                // All visible kinds land; each tab filters (the strips read
                // kind 20 + imeta-bearing kind 1 — Phase 2 §4.3).
                self.notes = page.notes;
                for note in self.notes.clone() {
                    self.want_media(&note, cx);
                    self.want_quotes(&note, cx);
                }
                self.status.clear();
            }
            FeedEvent::Failed(e) => self.status = format!("… {e}"),
            _ => {}
        }
        self.list.reset(self.row_count());
        cx.notify();
    }

    // --- the strips' projections (Phase 2 §4.3) -------------------------------

    /// The Notes tab's rows: text + picture notes.
    fn note_rows(&self) -> Vec<Event> {
        self.notes
            .iter()
            .filter(|n| n.kind == Kind::TextNote || u16::from(n.kind) == 20)
            .cloned()
            .collect()
    }

    /// The Photos strip: every picture URL in timeline order (kind 20 +
    /// imeta-bearing kind 1), flat — the grid chunks it into threes.
    fn photo_urls(&self) -> Vec<String> {
        self.notes
            .iter()
            .filter(|n| u16::from(n.kind) == 20 || n.kind == Kind::TextNote)
            .flat_map(content::media_assets)
            .filter_map(|a| match a {
                content::MediaAsset::Picture { url } => Some(url),
                _ => None,
            })
            .collect()
    }

    /// The Videos strip: one tile per video-bearing note (kind 21/22, plus
    /// imeta/link videos on kind 1) — url + poster. Video files themselves
    /// are never fetched (the tile opens the browser).
    fn video_rows(&self) -> Vec<(String, Option<String>)> {
        self.notes
            .iter()
            .filter(|n| matches!(u16::from(n.kind), 1 | 21 | 22))
            .filter_map(|n| {
                content::media_assets(n).into_iter().find_map(|a| match a {
                    content::MediaAsset::Video { url, poster } => Some((url, poster)),
                    _ => None,
                })
            })
            .collect()
    }

    /// The active tab's list-row count (list.reset wants it at event time).
    fn row_count(&self) -> usize {
        match self.tab {
            ProfileTab::Notes => self.note_rows().len(),
            ProfileTab::Photos => self.photo_urls().len().div_ceil(3),
            ProfileTab::Videos => self.video_rows().len(),
        }
    }

    /// Queue the profile picture fetch (core's guarded, disk-cached path).
    fn want_avatar(&mut self, profile: &Profile, cx: &mut Context<Self>) {
        let url = profile.picture.clone();
        if url.is_empty() {
            return;
        }
        media::fetch(&self.core, url, cx.entity_id(), cx);
    }

    /// Queue core fetches for every media URL in a note (avatars share the
    /// same cache; URLs are unique so there's no clash).
    fn want_media(&mut self, note: &Event, cx: &mut Context<Self>) {
        for url in content::media_urls(note) {
            media::fetch(&self.core, url, cx.entity_id(), cx);
        }
    }

    /// One quote-reference resolution, routed off `on_event` (its `Loaded`
    /// arm is the profile page itself).
    fn pump_quote(&self, bech: String, mut rx: FeedRx, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.recv().await {
                if let FeedEvent::One(Some(target), prof) = ev {
                    let _ = this.update(cx, |this, cx| {
                        let key = target.pubkey.to_string();
                        let name = prof
                            .as_ref()
                            .map(|p| p.display())
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

    /// One strip tab: a quiet label, the active one ink + underlined.
    fn tab_btn(
        &self,
        which: ProfileTab,
        label: &'static str,
        active: ProfileTab,
        cx: &Context<Self>,
    ) -> AnyElement {
        let on = which == active;
        div()
            .id(SharedString::from(format!("tab-{label}")))
            .cursor_pointer()
            .text_size(px(12.5))
            .text_color(rgb(if on { theme::ink() } else { theme::muted() }))
            .when(on, |d| d.underline())
            .child(label)
            .on_click(cx.listener(move |view, _, _, cx| {
                if view.tab != which {
                    view.tab = which;
                    view.list.reset(view.row_count());
                    cx.notify();
                }
            }))
            .into_any_element()
    }
}

impl Render for ProfileView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let who_hex = self.who.to_string();
        let name = self
            .profile
            .as_ref()
            .map(|p| p.display())
            .unwrap_or_else(|| format!("{}…", &who_hex[..8]));
        let about = self.profile.as_ref().map(|p| p.about.clone()).unwrap_or_default();
        let picture = self.profile.as_ref().map(|p| p.picture.clone()).unwrap_or_default();
        let tab = self.tab;
        let rows: Vec<Row> = match tab {
            ProfileTab::Notes => self.note_rows().into_iter().map(Row::Note).collect(),
            ProfileTab::Photos => self
                .photo_urls()
                .chunks(3)
                .enumerate()
                .map(|(r, chunk)| {
                    Row::Pics(
                        chunk
                            .iter()
                            .enumerate()
                            .map(|(i, u)| (u.clone(), r * 3 + i))
                            .collect(),
                    )
                })
                .collect(),
            ProfileTab::Videos => self
                .video_rows()
                .into_iter()
                .map(|(u, p)| Row::Vid(u, p))
                .collect(),
        };
        // The Photos strip's lightbox list (tile taps index into it).
        let pics_all = self.photo_urls();
        let empty = rows.is_empty();
        let following = self.following;
        let media = media::Snapshot::new(cx);
        // Evicted media comes back on render — the profile header avatar,
        // known-author avatars, and the notes' pictures (fetch is a silent
        // no-op for entries the cache holds).
        let viewer = cx.entity_id();
        if let Some(p) = self.profile.as_ref() {
            media::fetch(&self.core, p.picture.clone(), viewer, cx);
        }
        for p in self.profiles.values() {
            media::fetch(&self.core, p.picture.clone(), viewer, cx);
        }
        for ev in &self.notes {
            for url in content::media_urls(ev) {
                media::fetch(&self.core, url, viewer, cx);
            }
        }
        let profiles = self.profiles.clone();
        let quotes = self.quotes.clone();
        let handle = cx.entity();
        let follow_handle = handle.clone();

        div()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(
                // Header: name, about, follow seal — inner content rides the
                // same centered column as the notes below.
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .border_b_1()
                    .border_color(rgb(theme::hairline()))
                    .bg(rgb(theme::panel()))
                    .child(
                        div()
                            .w(px(content::COL_W))
                            .flex()
                            .flex_col()
                            .gap_2()
                            .p_4()
                            .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_3()
                                    .child(media::avatar(&media, &picture, &name, 48.0))
                                    .child(
                                        div()
                                            .text_size(px(17.0))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(rgb(theme::ink()))
                                            .child(name),
                                    ),
                            )
                            .child(
                                div()
                                    .id("follow")
                                    .cursor_pointer()
                                    .px_2()
                                    .py_1()
                                    .rounded(px(4.))
                                    .text_size(px(12.0))
                                    .on_click(move |_, _, cx| {
                                        let h = follow_handle.clone();
                                        h.update(cx, |view, cx| {
                                            view.following = !view.following;
                                            // Optimistic flip; publish events are
                                            // drained and ignored (P1).
                                            let mut rx = view
                                                .core
                                                .set_following(view.who, view.following);
                                            std::thread::spawn(move || {
                                                while rx.blocking_recv().is_some() {}
                                            });
                                            cx.notify();
                                        });
                                    })
                                    .when(following, |d| {
                                        d.text_color(rgb(theme::muted()))
                                            .child("Following")
                                    })
                                    .when(!following, |d| {
                                        d.text_color(rgb(theme::vermilion()))
                                            .child("Follow")
                                    }),
                            ),
                    )
                    .when(!about.is_empty(), |d| {
                        d.child(
                            div()
                                .font_family(theme::SERIF)
                                .text_size(px(14.0))
                                .text_color(rgb(theme::muted()))
                                .child(about),
                        )
                    }),
                        ),
            )
            .child(
                // The strips' tabs (Phase 2 §4.3): quiet labels, the active
                // one ink + underlined.
                div()
                    .w_full()
                    .border_b_1()
                    .border_color(rgb(theme::hairline()))
                    .bg(rgb(theme::panel()))
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .w(px(content::COL_W))
                            .flex()
                            .gap_4()
                            .px_4()
                            .py_2()
                            .child(self.tab_btn(ProfileTab::Notes, "Notes", tab, cx))
                            .child(self.tab_btn(ProfileTab::Photos, "Photos", tab, cx))
                            .child(self.tab_btn(ProfileTab::Videos, "Videos", tab, cx)),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .justify_center()
                    .child(if empty {
                        div()
                            .w_full()
                            .max_w(px(content::COL_W))
                            .p_4()
                            .text_color(rgb(theme::muted()))
                            .child(if self.status.is_empty() {
                                match tab {
                                    ProfileTab::Notes => "No notes here yet.",
                                    ProfileTab::Photos => "No pictures yet.",
                                    ProfileTab::Videos => "No videos yet.",
                                }
                                .to_string()
                            } else {
                                self.status.clone()
                            })
                            .into_any_element()
                    } else {
                        list(self.list.clone(), move |idx, _w, _cx| {
                    match rows.get(idx) {
                        Some(Row::Note(ev)) => {
                            let time = super::feed::rel_time(ev.created_at.as_secs());
                            let ev_for_click = ev.clone();
                            let body_handle = handle.clone();
                            div()
                                .id(SharedString::from(format!("note-{idx}")))
                                .cursor_pointer()
                                .w_full()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .px_4()
                                .py_3()
                                .border_b_1()
                                .border_color(rgb(theme::hairline()))
                                .child(
                                    div()
                                        .text_size(px(11.5))
                                        .text_color(rgb(theme::muted()))
                                        .child(time),
                                )
                                .child(
                                    div()
                                        .id(SharedString::from(format!("note-open-{idx}")))
                                        .cursor_pointer()
                                        .on_click(move |_, _, cx| {
                                            let ev = ev_for_click.clone();
                                            body_handle.update(cx, |_, cx| {
                                                cx.emit(Nav::OpenThread(ev.id))
                                            });
                                        })
                                        .child(content::body(
                                            ev,
                                            &content::Body {
                                                media: &media,
                                                quotes: &quotes,
                                                profiles: &profiles,
                                            },
                                            handle.clone(),
                                        )),
                                )
                                .into_any_element()
                        }
                        Some(Row::Pics(tiles)) => {
                            let mut grid = div()
                                .w_full()
                                .flex()
                                .gap_1()
                                .px_4()
                                .py_1()
                                .border_b_1()
                                .border_color(rgb(theme::hairline()));
                            for (url, gix) in tiles {
                                grid = grid.child(thumb_tile(
                                    &media,
                                    SharedString::from(format!("pic-{gix}")),
                                    url,
                                    Some((&pics_all, *gix)),
                                    None,
                                    handle.clone(),
                                ));
                            }
                            grid.into_any_element()
                        }
                        Some(Row::Vid(url, poster)) => {
                            let mut grid = div()
                                .w_full()
                                .flex()
                                .gap_1()
                                .px_4()
                                .py_1()
                                .border_b_1()
                                .border_color(rgb(theme::hairline()));
                            // One tile per video note: the poster thumb when
                            // the bytes landed, the quiet play frame otherwise.
                            grid = grid.child(thumb_tile(
                                &media,
                                SharedString::from(format!("vid-{idx}")),
                                poster.as_deref().unwrap_or(""),
                                None,
                                Some(url),
                                handle.clone(),
                            ));
                            grid.into_any_element()
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

/// One 3-column strip thumbnail (Phase 2 §4.3): the cached bytes
/// cover-cropped to a square, a quiet frame until they land. Photos tap
/// into the lightbox at their strip index; videos open the browser (the
/// play badge marks them, the poster is the thumb).
fn thumb_tile(
    snap: &media::Snapshot,
    key: SharedString,
    thumb: &str,
    lightbox: Option<(&[String], usize)>,
    video_url: Option<&str>,
    handle: Entity<ProfileView>,
) -> AnyElement {
    let size = (content::COL_W - 32.0 - 8.0) / 3.0;
    let video = video_url.is_some();
    let target = video_url.unwrap_or_default().to_string();
    let urls: Vec<String> = lightbox.map(|(l, _)| l.to_vec()).unwrap_or_default();
    let idx = lightbox.map(|(_, i)| i).unwrap_or(0);
    let frame = div()
        .id(key)
        .cursor_pointer()
        .flex_none()
        .overflow_hidden()
        .rounded(px(4.))
        .border_1()
        .border_color(rgb(theme::hairline()))
        .bg(rgb(theme::panel()))
        .size(px(size))
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            if video {
                cx.open_url(&target);
            } else {
                let urls = urls.clone();
                handle.update(cx, |_, cx| cx.emit(Nav::OpenLightbox(urls, idx)));
            }
        });
    let badge = |f: Div| {
        if video {
            f.child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(22.0))
                    .text_color(rgb(theme::vermilion()))
                    .child("▶"),
            )
        } else {
            f
        }
    };
    match snap.png(thumb) {
        Some(png) => frame
            .child(
                div()
                    .relative()
                    .size_full()
                    .child(img(png).size_full().object_fit(ObjectFit::Cover))
                    .map(badge),
            )
            .into_any_element(),
        None => frame
            .child(
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(if video { 22.0 } else { 11.0 }))
                    .text_color(rgb(if video {
                        theme::vermilion()
                    } else {
                        theme::muted()
                    }))
                    .child(if video { "▶" } else { "…" }),
            )
            .into_any_element(),
    }
}
