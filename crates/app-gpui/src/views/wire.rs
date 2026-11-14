//! Wire — the live-wire relay column (D68's kind expansion): ONE relay,
//! subscribed directly (manager::subscribe_relay_wire, live-wire v1 — the
//! owner's call). Notes and long-form arrive since launch only: no
//! history, the column starts empty and fills with what the relay pushes
//! while it's open. Newest-first, bounded buffer. The receiver's lifetime
//! IS the subscription: when this view drops, the core task's next send
//! fails and it unsubscribes — a closed column stops paying for its
//! socket.

use std::{collections::HashMap, sync::Arc};

use gpui::{
    Context, Entity, EventEmitter, ListAlignment, ListOffset, ListScrollEvent, ListState, Render,
    SharedString, Window, div, list, prelude::*, px, rgb,
};
use nostr::prelude::*;
use satori_core::manager::{Core, FeedEvent, Profile};

use super::{actions, content, engagement, nav, nav::Nav, quotes, states};
use crate::{media, theme};

/// The newest-first buffer's cap: a busy relay can outpace reading; the
/// oldest notes drop past this.
const WIRE_CAP: usize = 200;

pub struct WireView {
    core: Arc<Core>,
    /// The relay this wire listens to (the status whisper's host).
    relay: String,
    rows: Vec<Arc<Event>>,
    profiles: HashMap<String, Profile>,
    quotes: content::Quotes,
    engagement: engagement::Engagement,
    /// The quiet status whisper (the failure voice).
    status: String,
    list: ListState,
    deep: bool,
}

impl EventEmitter<Nav> for WireView {}

impl WireView {
    pub fn new(core: Arc<Core>, relay: String, cx: &mut Context<Self>) -> Self {
        crate::theme::watch(cx); // repaint on theme change
        crate::timefmt::watch(cx); // repaint on mode/height change
        crate::video::watch(cx); // repaint on player open/toggle/close
        let engagement = engagement::Engagement::new(&core);
        let mut view = Self {
            core,
            relay,
            rows: vec![],
            profiles: HashMap::new(),
            quotes: HashMap::new(),
            engagement,
            status: states::loading("Listening…"),
            list: ListState::new(0, ListAlignment::Top, px(400.)),
            deep: false,
        };
        view.pump(cx);
        // Deep-scroll whisper: notifications.rs's shape — emit on change
        // only; the shell's floating back-to-top covers this pane too.
        let this = cx.entity().downgrade();
        view.list
            .set_scroll_handler(move |ev: &ListScrollEvent, _w, cx| {
                let _ = this.update(cx, |n, cx| {
                    let deep = ev.visible_range.start >= nav::PANE_DEEP_AT;
                    if n.deep != deep {
                        n.deep = deep;
                        cx.emit(Nav::PaneDeep);
                    }
                });
            });
        // The wallet broadcast (D20): the rows' bolts follow every zap.
        engagement::pump_wallet(&view.core, &cx.entity(), cx);
        view
    }

    /// Back to the top (rail re-click, the floating whisper).
    pub fn scroll_top(&mut self) {
        self.deep = false;
        self.list.scroll_to(ListOffset {
            item_ix: 0,
            offset_in_item: px(0.),
        });
    }

    fn pump(&mut self, cx: &mut Context<Self>) {
        let mut rx = self.core.subscribe_relay_wire(self.relay.clone());
        cx.spawn(async move |this, cx| {
            // Lagged (the buffer outran us) skips ahead, not out: the wire
            // is a live feed, the missed notes are gone by choice.
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        let _ = this.update(cx, |this, cx| this.on_note(ev, cx));
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        })
        .detach();
    }

    fn on_note(&mut self, ev: Event, cx: &mut Context<Self>) {
        self.status.clear();
        for url in content::media_urls_cached(&ev).iter().cloned() {
            media::fetch(&self.core, url, cx.entity_id(), cx);
        }
        quotes::want_quotes(self, &ev, cx);
        let pk = ev.pubkey;
        self.rows.insert(0, Arc::new(ev));
        self.rows.truncate(WIRE_CAP);
        self.list.reset(self.rows.len());
        // The author's face: profiles hydrate as they land (one pump per
        // unknown author; the channel closes when the service is done).
        if !self.profiles.contains_key(&pk.to_string()) {
            let mut rx = self.core.ensure_profiles(vec![pk]);
            cx.spawn(async move |this, cx| {
                while let Some(fe) = rx.recv().await {
                    let _ = this.update(cx, |this, cx| {
                        if let FeedEvent::Profiles(profs) = fe {
                            for p in profs {
                                this.profiles.insert(p.pubkey.clone(), p);
                            }
                            cx.notify();
                        }
                    });
                }
            })
            .detach();
        }
        cx.notify();
    }
}

impl actions::ActionHost for WireView {
    fn toggle_like(&mut self, ev: &Event, cx: &mut Context<Self>) {
        engagement::toggle_like(self, ev, cx)
    }
    fn toggle_reaction_picker(&mut self, id: &str, cx: &mut Context<Self>) {
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

impl engagement::Host for WireView {
    fn engagement(&mut self) -> &mut engagement::Engagement {
        &mut self.engagement
    }
    fn core(&self) -> &Arc<Core> {
        &self.core
    }
    fn whisper(&mut self, msg: String) {
        self.status = msg;
    }
}

impl quotes::Host for WireView {
    fn quotes(&mut self) -> &mut content::Quotes {
        &mut self.quotes
    }
    fn core(&self) -> &Arc<Core> {
        &self.core
    }
    fn profiles(&self) -> &HashMap<String, Profile> {
        &self.profiles
    }
}

/// A wire row: the actor header, the note's body (kind 1) or the
/// article's title + summary card (kind 30023), the timeline's action
/// row beneath (notifications.rs's one-row rule: the row IS a note).
#[allow(clippy::too_many_arguments)]
fn wire_card(
    idx: usize,
    ev: &Arc<Event>,
    st: actions::ActionState,
    profiles: &HashMap<String, Profile>,
    media_snap: &media::Snapshot,
    quotes: &content::Quotes,
    time: String,
    handle: Entity<WireView>,
) -> gpui::AnyElement {
    let key = ev.pubkey.to_string();
    let name = content::display_name(profiles.get(&key), &key);
    let picture = profiles
        .get(&key)
        .map(|p| p.picture.clone())
        .unwrap_or_default();
    let mut row = div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .px_4()
        .py_3()
        .child(content::actor_header(
            None,
            media_snap,
            &picture,
            name,
            None,
            time,
            format!("wire-{idx}"),
            Some(ev.pubkey),
            handle.clone(),
        ));
    let id_hex = ev.id.to_string();
    match u16::from(ev.kind) {
        // The article card: serif title + summary, the reader opens on
        // the click (content.rs article_card's compact shape).
        30023 => {
            let data = content::article_data_cached(ev);
            let title = data
                .as_ref()
                .map(|d| d.title.clone())
                .unwrap_or_else(|| "Article".into());
            let summary = data
                .as_ref()
                .map(|d| {
                    if d.summary.is_empty() {
                        ev.content.chars().take(140).collect::<String>()
                    } else {
                        d.summary.clone()
                    }
                })
                .unwrap_or_default();
            let ev_open = (**ev).clone();
            let handle_open = handle.clone();
            row = row.child(
                div()
                    .id(SharedString::from(format!("wire-open-{idx}")))
                    .cursor_pointer()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .on_click(move |_, _, cx| {
                        let ev = ev_open.clone();
                        handle_open.update(cx, |_, cx| cx.emit(Nav::OpenArticle(ev)));
                    })
                    .child(
                        div()
                            .font_family(theme::SERIF)
                            .text_size(px(theme::T_UI + 3.0))
                            .text_color(rgb(theme::ink()))
                            .child(title),
                    )
                    .when(!summary.is_empty(), |d| {
                        d.child(
                            div()
                                .text_size(px(theme::T_UI))
                                .text_color(rgb(theme::muted()))
                                .child(summary),
                        )
                    }),
            );
        }
        // The note itself: the body pipeline (quotes, media, emoji), the
        // click opens its thread.
        _ => {
            let emoji = content::body_parse(ev).emoji.clone();
            let ctx = content::Body {
                media: media_snap,
                quotes,
                profiles,
                emoji,
            };
            let target = ev.id;
            let handle_body = handle.clone();
            row = row.child(
                div()
                    .id(SharedString::from(format!("wire-open-{idx}")))
                    .child(content::body(ev, &ctx, handle.clone()))
                    .cursor_pointer()
                    .on_click(move |_, _, cx| {
                        handle_body.update(cx, |_, cx| cx.emit(Nav::OpenThread(target)));
                    }),
            );
        }
    }
    row.child(actions::action_row(&id_hex, ev, st, handle))
        .into_any_element()
}

impl Render for WireView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let handle = cx.entity();
        let status = self.status.clone();
        let rows = self.rows.clone();
        let profiles = self.profiles.clone();
        let media_snap = media::Snapshot::new(cx);
        let quotes = self.quotes.clone();
        // The action row's glyph states, snapshotted per render
        // (notifications.rs's inline FeedSnap discipline).
        let liked = self.engagement.liked.clone();
        let reposted = self.engagement.reposted.clone();
        let reaction_picker = self.engagement.reaction_picker.clone();
        let bookmarked = self.engagement.bookmarked.clone();
        let zapped = self.engagement.zapped.clone();
        let zap_pending = self.engagement.zap_pending.clone();
        let wallet_ready = self.engagement.wallet_ready;
        let reduced_motion = crate::desktop::Desktop::get(cx).reduced_motion;
        let empty = rows.is_empty();
        let host = self.relay.clone();

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
                    .child(if empty {
                        div()
                            .w_full()
                            .max_w(px(content::COL_W))
                            .p_8()
                            .text_color(rgb(theme::muted()))
                            .child(if status.is_empty() {
                                format!("Listening to {host}… nothing has arrived yet.")
                            } else {
                                status
                            })
                            .into_any_element()
                    } else {
                        list(self.list.clone(), move |idx, _w, cx| match rows.get(idx) {
                            Some(ev) => {
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
                                    picker_open: reaction_picker.as_deref() == Some(&id_hex),
                                    replies: 0,
                                    reduced_motion,
                                };
                                wire_card(
                                    idx,
                                    ev,
                                    st,
                                    &profiles,
                                    &media_snap,
                                    &quotes,
                                    time,
                                    handle.clone(),
                                )
                            }
                            None => div().into_any_element(),
                        })
                        .into_any_element()
                    }),
            )
    }
}
