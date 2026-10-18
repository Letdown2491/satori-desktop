//! Thread view: the anchor note + its replies.
//!
//! TODO(port): crates/app/src/thread.rs — P1. Flat reply list; the GTK app's
//! `nest_replies` (parent = last `e` tag present in the reply set, depth <= 3)
//! lands in P2. Replies open their own threads on click.

use std::{collections::HashMap, sync::Arc};

use gpui::{
    div, list, prelude::*, px, rgb, Context, Entity, EventEmitter, FontWeight, ListAlignment,
    ListState, Render, SharedString, Window, Image,
};
use nostr::prelude::*;
use satori_core::manager::{Core, FeedEvent, Profile};
use satori_core::nip::imeta;

use crate::{icons, media, theme};
use super::{content, nav::Nav};

pub struct ThreadView {
    core: Arc<Core>,
    anchor: Option<Event>,
    replies: Vec<Event>,
    profiles: HashMap<String, Profile>,
    images: HashMap<String, Arc<Image>>,
    status: String,
    list: ListState,
}

impl EventEmitter<Nav> for ThreadView {}

impl ThreadView {
    pub fn new(core: Arc<Core>, id: EventId, cx: &mut Context<Self>) -> Self {
        let view = Self {
            core,
            anchor: None,
            replies: vec![],
            profiles: HashMap::new(),
            images: HashMap::new(),
            status: "loading the thread…".into(),
            list: ListState::new(0, ListAlignment::Top, px(400.)),
        };
        // The anchor rides in with its author's profile; replies follow.
        view.pump(view.core.event_by_id(id), cx);
        view.pump(view.core.thread(id), cx);
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
            FeedEvent::One(Some(note), profile) => {
                self.anchor = Some(note.clone());
                if let Some(p) = profile {
                    self.profiles.insert(p.pubkey.clone(), p);
                }
                self.want_images(&note, cx);
            }
            FeedEvent::One(None, _) => self.status = "this note is gone or unreachable".into(),
            FeedEvent::Replies(replies, profiles) => {
                for note in &replies {
                    self.want_images(note, cx);
                }
                self.replies = replies;
                for p in profiles {
                    self.profiles.insert(p.pubkey.clone(), p);
                }
                self.status.clear();
            }
            FeedEvent::Failed(e) => self.status = format!("… {e}"),
            _ => {}
        }
        self.list.reset(self.replies.len());
        cx.notify();
    }

    /// Queue core fetches for every imeta URL we haven't got yet.
    fn want_images(&mut self, note: &Event, cx: &mut Context<Self>) {
        for m in imeta::parse(note) {
            let url = m.url;
            if url.is_empty() || self.images.contains_key(&url) {
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

    fn name_for(&self, ev: &Event) -> String {
        let key = ev.pubkey.to_string();
        self.profiles
            .get(&key)
            .map(|p| p.display())
            .unwrap_or_else(|| format!("{}…", &key[..8]))
    }
}

fn reply_row(
    idx: usize,
    ev: &Event,
    name: String,
    images: &HashMap<String, Arc<Image>>,
    handle: Entity<ThreadView>,
) -> impl IntoElement {
    let time = super::feed::rel_time(ev.created_at.as_secs());
    let ev_for_click = ev.clone();
    let ev_for_reply = ev.clone();
    let open_handle = handle.clone();
    let reply_handle = handle.clone();
    let body_handle = handle.clone();

    div()
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
                .flex()
                .items_baseline()
                .justify_between()
                .child(
                    div()
                        .text_size(px(12.5))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::INK))
                        .child(name),
                )
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(rgb(theme::MUTED))
                        .child(time),
                ),
        )
        .child(
            div()
                .id(SharedString::from(format!("reply-{idx}")))
                .cursor_pointer()
                .on_click(move |_, _, cx| {
                    let ev = ev_for_click.clone();
                    open_handle.update(cx, |_view, cx| cx.emit(Nav::OpenThread(ev.id)));
                })
                .child(content::body(ev, images, body_handle)),
        )
        .child(
            div()
                .flex()
                .gap_4()
                .items_center()
                .child(
                    div()
                        .id(SharedString::from(format!("reply-to-{idx}")))
                        .cursor_pointer()
                        .flex()
                        .items_center()
                        .gap_1()
                        .text_size(px(11.5))
                        .text_color(rgb(theme::MUTED))
                        .on_click(move |_, _, cx| {
                            let ev = ev_for_reply.clone();
                            reply_handle.update(cx, |_view, cx| {
                                cx.emit(Nav::OpenComposer(Some(ev)));
                            });
                        })
                        .child(icons::bubble_icon(theme::MUTED))
                        .child("Reply"),
                ),
        )
}

impl Render for ThreadView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let handle = cx.entity();
        let status = self.status.clone();
        let replies_empty = self.replies.is_empty();
        let replies = self.replies.clone();
        let profiles = self.profiles.clone();
        let images = self.images.clone();
        let anchor = self.anchor.clone();
        let anchor_handle = handle.clone();
        let reply_all_handle = handle.clone();

        div()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(match anchor {
                Some(note) => {
                    let anchor_reply = note.clone();
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
                                .text_size(px(13.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(theme::INK))
                                .child(self.name_for(&note)),
                        )
                        .child(content::body(&note, &images, handle))
                        .child(
                            div()
                                .flex()
                                .gap_5()
                                .items_center()
                                .child(
                                    div()
                                        .id("anchor-reply")
                                        .cursor_pointer()
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        .text_size(px(11.5))
                                        .text_color(rgb(theme::MUTED))
                                        .on_click(move |_, _, cx| {
                                            let ev = anchor_reply.clone();
                                            reply_all_handle.update(cx, |_view, cx| {
                                                cx.emit(Nav::OpenComposer(Some(ev)));
                                            });
                                        })
                                        .child(icons::bubble_icon(theme::MUTED))
                                        .child("Reply"),
                                ),
                        )
                        .into_any_element()
                }
                None => div()
                    .p_4()
                    .text_color(rgb(theme::MUTED))
                    .child(status.clone())
                    .into_any_element(),
            })
            .child(if replies_empty {
                div()
                    .p_4()
                    .text_color(rgb(theme::MUTED))
                    .child(if status.is_empty() {
                        "No replies yet.".to_string()
                    } else {
                        status.clone()
                    })
                    .into_any_element()
            } else {
                list(self.list.clone(), move |idx, _w, _cx| {
                    match replies.get(idx) {
                        Some(ev) => {
                            let name = profiles
                                .get(&ev.pubkey.to_string())
                                .map(|p| p.display())
                                .unwrap_or_else(|| format!("{}…", &ev.pubkey.to_string()[..8]));
                            reply_row(idx, ev, name, &images, anchor_handle.clone())
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
