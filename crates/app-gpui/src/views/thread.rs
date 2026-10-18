//! Thread view: the anchor note + its replies.
//!
//! TODO(port): crates/app/src/thread.rs — P1. Flat reply list; the GTK app's
//! `nest_replies` (parent = last `e` tag present in the reply set, depth <= 3)
//! lands in P2. Replies open their own threads on click.

use std::{collections::HashMap, sync::Arc};

use gpui::{
    div, list, prelude::*, px, rgb, Context, Entity, EventEmitter, FontWeight, ListAlignment,
    ListState, Render, SharedString, Window,
};
use nostr::prelude::*;
use satori_core::manager::{Core, FeedEvent, Profile};

use crate::{icons, theme};
use super::nav::Nav;

pub struct ThreadView {
    core: Arc<Core>,
    anchor: Option<Event>,
    replies: Vec<Event>,
    profiles: HashMap<String, Profile>,
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
                self.anchor = Some(note);
                if let Some(p) = profile {
                    self.profiles.insert(p.pubkey.clone(), p);
                }
            }
            FeedEvent::One(None, _) => self.status = "this note is gone or unreachable".into(),
            FeedEvent::Replies(replies, profiles) => {
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
    handle: Entity<ThreadView>,
) -> impl IntoElement {
    let time = super::feed::rel_time(ev.created_at.as_secs());
    let content = ev.content.clone();
    let ev_for_click = ev.clone();

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
                .font_family(theme::SERIF)
                .text_size(px(14.5))
                .text_color(rgb(theme::INK))
                .on_click(move |_, _, cx| {
                    let ev = ev_for_click.clone();
                    handle.update(cx, |_view, cx| cx.emit(Nav::OpenThread(ev.id)));
                })
                .child(content),
        )
}

impl Render for ThreadView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let handle = cx.entity();
        let status = self.status.clone();
        let replies_empty = self.replies.is_empty();
        let replies = self.replies.clone();
        let profiles = self.profiles.clone();
        let anchor = self.anchor.clone();
        let anchor_handle = handle.clone();

        div()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(match anchor {
                Some(note) => div()
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
                    .child(
                        div()
                            .font_family(theme::SERIF)
                            .italic()
                            .text_size(px(16.0))
                            .text_color(rgb(theme::INK))
                            .child(note.content.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_5()
                            .items_center()
                            .child(icons::bubble_icon(theme::MUTED))
                            .child(icons::heart_icon(theme::MUTED))
                            .child(icons::bookmark_icon(theme::MUTED)),
                    )
                    .into_any_element(),
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
                            reply_row(idx, ev, name, anchor_handle.clone()).into_any_element()
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
