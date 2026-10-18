//! Profile view: an author's metadata + their notes.
//!
//! TODO(port): crates/app/src/profile.rs — P1. Name/about + note list; banner
//! and avatar images wait for the P2 image pipeline (core fetch → gpui `img`).
//! Follow/mute management ports with the rest of the settings scope (P3).

use std::{collections::HashMap, sync::Arc};

use gpui::{
    div, list, prelude::*, px, rgb, Context, EventEmitter, FontWeight, ListAlignment,
    ListState, Render, SharedString, Window, Image,
};
use nostr::prelude::*;
use satori_core::manager::{Core, FeedEvent, Profile};

use crate::{media, theme};
use super::nav::Nav;

pub struct ProfileView {
    core: Arc<Core>,
    who: PublicKey,
    profile: Option<Profile>,
    notes: Vec<Event>,
    following: bool,
    images: HashMap<String, Arc<Image>>,
    status: String,
    list: ListState,
}

impl EventEmitter<Nav> for ProfileView {}

impl ProfileView {
    pub fn new(core: Arc<Core>, who: PublicKey, cx: &mut Context<Self>) -> Self {
        let view = Self {
            following: core.is_following(&who),
            who,
            profile: None,
            notes: vec![],
            images: HashMap::new(),
            status: "loading…".into(),
            list: ListState::new(0, ListAlignment::Top, px(400.)),
            core,
        };
        let rx = view.core.profile(view.who);
        view.pump(rx, cx);
        view
    }

    fn pump(&self, mut rx: satori_core::manager::FeedRx, cx: &mut Context<Self>) {
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
                self.profile = page
                    .profiles
                    .iter()
                    .find(|p| p.pubkey == self.who.to_string())
                    .cloned();
                if let Some(p) = self.profile.clone() {
                    self.want_avatar(&p, cx);
                }
                // TODO(port): profile.rs renders all visible kinds; P1 is text
                // notes only.
                self.notes = page
                    .notes
                    .into_iter()
                    .filter(|n| n.kind == Kind::TextNote)
                    .collect();
                self.status.clear();
            }
            FeedEvent::Failed(e) => self.status = format!("… {e}"),
            _ => {}
        }
        self.list.reset(self.notes.len());
        cx.notify();
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
        let notes = self.notes.clone();
        let empty = notes.is_empty();
        let following = self.following;
        let images = self.images.clone();
        let handle = cx.entity();
        let follow_handle = handle.clone();

        div()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(
                // Header: name, about, follow seal.
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_4()
                    .border_b_1()
                    .border_color(rgb(theme::HAIRLINE))
                    .bg(rgb(theme::PANEL))
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
                                    .child(media::avatar(&images, &picture, &name, 48.0))
                                    .child(
                                        div()
                                            .text_size(px(17.0))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(rgb(theme::INK))
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
                                        d.text_color(rgb(theme::MUTED))
                                            .child("Following")
                                    })
                                    .when(!following, |d| {
                                        d.text_color(rgb(theme::VERMILION))
                                            .child("Follow")
                                    }),
                            ),
                    )
                    .when(!about.is_empty(), |d| {
                        d.child(
                            div()
                                .font_family(theme::SERIF)
                                .text_size(px(14.0))
                                .text_color(rgb(theme::MUTED))
                                .child(about),
                        )
                    }),
            )
            .child(if empty {
                div()
                    .p_4()
                    .text_color(rgb(theme::MUTED))
                    .child(if self.status.is_empty() {
                        "No notes here yet.".to_string()
                    } else {
                        self.status.clone()
                    })
                    .into_any_element()
            } else {
                list(self.list.clone(), move |idx, _w, _cx| {
                    match notes.get(idx) {
                        Some(ev) => {
                            let time = super::feed::rel_time(ev.created_at.as_secs());
                            let content = ev.content.clone();
                            let ev_for_click = ev.clone();
                            let note_handle = handle.clone();
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
                                .border_color(rgb(theme::HAIRLINE))
                                .child(
                                    div()
                                        .text_size(px(11.5))
                                        .text_color(rgb(theme::MUTED))
                                        .child(time),
                                )
                                .child(
                                    div()
                                        .id(SharedString::from(format!("note-open-{idx}")))
                                        .cursor_pointer()
                                        .font_family(theme::SERIF)
                                        .text_size(px(14.5))
                                        .text_color(rgb(theme::INK))
                                        .on_click(move |_, _, cx| {
                                            let ev = ev_for_click.clone();
                                            note_handle.update(cx, |_, cx| {
                                                cx.emit(Nav::OpenThread(ev.id))
                                            });
                                        })
                                        .child(content),
                                )
                                .into_any_element()
                        }
                        None => div().into_any_element(),
                    }
                })
                .w_full()
                .h_full()
                .into_any_element()
            })
    }
}
