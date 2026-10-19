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
    div, list, prelude::*, px, rgb, Context, Entity, EventEmitter, FontWeight, ListAlignment,
    ListState, Render, SharedString, Window,
};
use nostr::prelude::*;
use satori_core::manager::{Core, FeedEvent, FeedRx, Profile};

use crate::{icons, media, theme};
use super::{content, nav::Nav};

pub struct ThreadView {
    core: Arc<Core>,
    anchor: Option<Event>,
    /// Replies flattened pre-order with their NIP-10 depth (nest_replies).
    replies: Vec<(Event, usize)>,
    profiles: HashMap<String, Profile>,
    quotes: content::Quotes,
    status: String,
    list: ListState,
}

impl EventEmitter<Nav> for ThreadView {}

impl ThreadView {
    pub fn new(core: Arc<Core>, id: EventId, cx: &mut Context<Self>) -> Self {
        crate::theme::watch(cx); // repaint on theme change
        let view = Self {
            core,
            anchor: None,
            replies: vec![],
            profiles: HashMap::new(),
            quotes: HashMap::new(),
            status: "loading the thread…".into(),
            list: ListState::new(0, ListAlignment::Top, px(400.)),
        };
        // The anchor rides in with its author's profile; replies follow.
        view.pump(view.core.event_by_id(id), cx);
        view.pump(view.core.thread(id), cx);
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

    /// One quote-reference resolution, kept off `on_event` (its `One` arm
    /// means "the anchor").
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

    /// Hydrate names for note authors + mentioned pubkeys (TODO(port):
    /// content.ts mentionPubkeys) — otherwise mentions degrade to @npub1….
    fn want_names(&self, notes: &[Event], cx: &mut Context<Self>) {
        let mut seen = HashSet::new();
        let missing: Vec<PublicKey> = notes
            .iter()
            .flat_map(|n| {
                let mut pks = vec![n.pubkey];
                pks.extend(content::mention_pks(&n.content));
                pks
            })
            .filter(|pk| {
                !self.profiles.contains_key(&pk.to_string()) && seen.insert(pk.to_string())
            })
            .collect();
        if !missing.is_empty() {
            self.pump(self.core.ensure_profiles(missing), cx);
        }
    }

    fn on_event(&mut self, ev: FeedEvent, cx: &mut Context<Self>) {
        match ev {
            FeedEvent::One(Some(note), profile) => {
                // An article landing as a thread anchor (quote embed, reply
                // target) belongs in the reader, not the raw thread.
                if u16::from(note.kind) == 30023 {
                    cx.emit(Nav::OpenArticle(note.clone()));
                    return;
                }
                self.anchor = Some(note.clone());
                if let Some(p) = profile {
                    self.profiles.insert(p.pubkey.clone(), p);
                }
                self.want_media(&note, cx);
                self.want_quotes(&note, cx);
                self.want_names(&[note], cx);
            }
            FeedEvent::One(None, _) => self.status = "this note is gone or unreachable".into(),
            FeedEvent::Replies(replies, profiles) => {
                for note in &replies {
                    self.want_media(note, cx);
                    self.want_quotes(note, cx);
                }
                self.replies = nest_replies(&replies);
                for p in profiles {
                    self.profiles.insert(p.pubkey.clone(), p);
                }
                self.status.clear();
                self.want_names(&replies, cx);
            }
            FeedEvent::Profiles(profiles) => {
                for p in profiles {
                    self.profiles.insert(p.pubkey.clone(), p);
                }
            }
            FeedEvent::Failed(e) => self.status = format!("… {e}"),
            _ => {}
        }
        self.list.reset(self.replies.len());
        cx.notify();
    }

    /// Queue core fetches for every media URL we haven't got yet (results
    /// land in the shared media cache; notify re-renders pick them up).
    fn want_media(&mut self, note: &Event, cx: &mut Context<Self>) {
        for url in content::media_urls(note) {
            media::fetch(&self.core, url, cx.entity_id(), cx);
        }
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

    fn name_for(&self, ev: &Event) -> String {
        let key = ev.pubkey.to_string();
        self.profiles
            .get(&key)
            .map(|p| p.display())
            .unwrap_or_else(|| format!("{}…", &key[..8]))
    }
}

/// The NIP-10 tree, flattened pre-order with depths (thread.rs nest_replies).
fn nest_replies(replies: &[Event]) -> Vec<(Event, usize)> {
    let ids: HashSet<String> = replies.iter().map(|r| r.id.to_string()).collect();
    let mut children: HashMap<String, Vec<Event>> = HashMap::new();
    let mut roots: Vec<Event> = Vec::new();
    for r in replies {
        // Last e tag = direct parent (NIP-10 marked-e reads right-to-left).
        let parent = r
            .tags
            .clone()
            .event_ids()
            .last()
            .filter(|pid| ids.contains(&pid.to_string()) && *pid != r.id);
        match parent {
            Some(pid) => children.entry(pid.to_string()).or_default().push(r.clone()),
            None => roots.push(r.clone()),
        }
    }
    fn walk(
        ev: &Event,
        children: &HashMap<String, Vec<Event>>,
        depth: usize,
        out: &mut Vec<(Event, usize)>,
    ) {
        out.push((ev.clone(), depth));
        if depth < 3
            && let Some(kids) = children.get(&ev.id.to_string())
        {
            for kid in kids {
                walk(kid, children, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    for root in &roots {
        walk(root, &children, 0, &mut out);
    }
    out
}

// The card takes the full render-time snapshot — one struct would hide the
// shape, and GPUI's no-entity-reads-during-render rule makes them explicit.
#[allow(clippy::too_many_arguments)]
fn reply_row(
    idx: usize,
    ev: &Event,
    depth: usize,
    name: String,
    media: &media::Snapshot,
    quotes: &content::Quotes,
    profiles: &HashMap<String, Profile>,
    handle: Entity<ThreadView>,
) -> impl IntoElement {
    let time = super::feed::rel_time(ev.created_at.as_secs());
    let picture = profiles
        .get(&ev.pubkey.to_string())
        .map(|p| p.picture.clone())
        .unwrap_or_default();
    let ev_for_click = ev.clone();
    let ev_for_reply = ev.clone();
    let open_handle = handle.clone();
    let reply_handle = handle.clone();
    let body_handle = handle.clone();
    // thread.rs comment-children: 20px for the first level, 14px deeper.
    let indent = if depth == 0 { 0.0 } else { 20.0 + 14.0 * (depth - 1) as f32 };

    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .pl(px(indent))
        .pr_4()
        .py_3()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(media::avatar(media, &picture, &name, 24.0))
                        .child(
                            div()
                                .text_size(px(12.5))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(theme::ink()))
                                .child(name),
                        ),
                )
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(rgb(theme::muted()))
                        .child(time),
                ),
        )
        .child(
            div()
                .id(SharedString::from(format!("reply-{idx}")))
                .cursor_pointer()
                .on_click(move |_, _, cx| {
                    let ev = ev_for_click.clone();
                    // A reply opens the conversation at its ROOT (the
                    // reference never links the immediate parent) — unless
                    // we're already reading that root's thread.
                    open_handle.update(cx, |view, cx| {
                        let root = satori_core::nip::nip10::reply_root(&ev)
                            .and_then(|(id, _)| EventId::from_hex(&id).ok())
                            .unwrap_or(ev.id);
                        if view.anchor.as_ref().map(|a| a.id) != Some(root) {
                            cx.emit(Nav::OpenThread(root));
                        }
                    });
                })
                .child(content::body(
                    ev,
                    &content::Body { media, quotes, profiles },
                    body_handle,
                )),
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
                        .text_color(rgb(theme::muted()))
                        .on_click(move |_, _, cx| {
                            let ev = ev_for_reply.clone();
                            reply_handle.update(cx, |_view, cx| {
                                cx.emit(Nav::OpenComposer(Some(ev)));
                            });
                        })
                        .child(icons::bubble_icon(theme::muted()))
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
        let media = media::Snapshot::new(cx);
        // Evicted media comes back on render — a thread is bounded content,
        // so sweep everything it paints (Ready/not-media URLs are silent
        // no-ops in fetch; only a landing notifies, so no render loop).
        let viewer = cx.entity_id();
        for p in self.profiles.values() {
            media::fetch(&self.core, p.picture.clone(), viewer, cx);
        }
        for url in self.anchor.as_ref().into_iter().flat_map(content::media_urls) {
            media::fetch(&self.core, url, viewer, cx);
        }
        for (ev, _) in &self.replies {
            for url in content::media_urls(ev) {
                media::fetch(&self.core, url, viewer, cx);
            }
        }
        let quotes = self.quotes.clone();
        let anchor = self.anchor.clone();
        let anchor_handle = handle.clone();
        let reply_all_handle = handle.clone();

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
                            let anchor_reply = note.clone();
                            div()
                                .w_full()
                                .max_w(px(content::COL_W))
                                .flex()
                                .flex_col()
                                .gap_2()
                                .p_4()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(media::avatar(
                                            &media,
                                            &self
                                                .profiles
                                                .get(&note.pubkey.to_string())
                                                .map(|p| p.picture.clone())
                                                .unwrap_or_default(),
                                            &self.name_for(&note),
                                            28.0,
                                        ))
                                        .child(
                                            div()
                                                .text_size(px(13.0))
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(rgb(theme::ink()))
                                                .child(self.name_for(&note)),
                                        ),
                                )
                                .child(content::body(
                                    &note,
                                    &content::Body {
                                        media: &media,
                                        quotes: &quotes,
                                        profiles: &profiles,
                                    },
                                    handle,
                                ))
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
                                                .text_color(rgb(theme::muted()))
                                                .on_click(move |_, _, cx| {
                                                    let ev = anchor_reply.clone();
                                                    reply_all_handle.update(cx, |_view, cx| {
                                                        cx.emit(Nav::OpenComposer(Some(ev)));
                                                    });
                                                })
                                                .child(icons::bubble_icon(theme::muted()))
                                                .child("Reply"),
                                        ),
                                )
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
                        list(self.list.clone(), move |idx, _w, _cx| {
                            match replies.get(idx) {
                                Some((ev, depth)) => {
                                    let key = ev.pubkey.to_string();
                                    let name = profiles
                                        .get(&key)
                                        .map(|p| p.display())
                                        .unwrap_or_else(|| format!("{}…", &key[..8]));
                                    reply_row(
                                        idx,
                                        ev,
                                        *depth,
                                        name,
                                        &media,
                                        &quotes,
                                        &profiles,
                                        anchor_handle.clone(),
                                    )
                                    .into_any_element()
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
