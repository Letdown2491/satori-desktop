//! Notifications — port of crates/app/src/notifications.rs (and
//! src/routes/notifications.ts): replies + mentions newest-first as compact
//! cards, mark-seen on open, the New/Earlier split at the pre-visit
//! high-water, "Nothing new. Sit quietly." when empty. The header bell dot
//! polls every 90s from the shell (shell.rs poll_notif).

use std::{collections::HashMap, sync::Arc};

use gpui::{div, list, prelude::*, px, rgb, Context, EventEmitter, ListAlignment, ListState, Render, Window};
use nostr::prelude::*;
use satori_core::manager::{Core, FeedEvent, FeedRx, Profile};

use crate::{media, theme};
use super::{content, nav::Nav};

/// One list row: a section label (New / Earlier) or a notification card.
#[derive(Clone)]
enum Row {
    Label(&'static str),
    Note(Event),
}

pub struct NotificationsView {
    core: Arc<Core>,
    rows: Vec<Row>,
    profiles: HashMap<String, Profile>,
    quotes: content::Quotes,
    status: String,
    list: ListState,
}

impl EventEmitter<Nav> for NotificationsView {}

impl NotificationsView {
    /// Fetched at construction — a GPUI pane is built when shown, so this is
    /// the GTK page's fetch-on-show hook. Opening also marks seen: the
    /// pre-visit high-water splits the page into New / Earlier.
    pub fn new(core: Arc<Core>, cx: &mut Context<Self>) -> Self {
        let prev = satori_core::read_state::load().notif;
        core.mark_notifications_seen();
        let view = Self {
            core,
            rows: vec![],
            profiles: HashMap::new(),
            quotes: HashMap::new(),
            status: "Gathering…".into(),
            list: ListState::new(0, ListAlignment::Top, px(400.)),
        };
        view.pump_at(prev, cx);
        view
    }

    fn pump_at(&self, prev_high_water: u64, cx: &mut Context<Self>) {
        let mut rx = self.core.notifications();
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.recv().await {
                let _ = this.update(cx, |this, cx| this.on_event(ev, prev_high_water, cx));
            }
        })
        .detach();
    }

    fn on_event(&mut self, ev: FeedEvent, prev: u64, cx: &mut Context<Self>) {
        match ev {
            FeedEvent::Notifications(notifs, profiles) => {
                // Each event is a full page (cached-first, then fresh) —
                // rebuild, never append. The relay phase asks for TextNote
                // only, but the cached store page can surface reactions
                // (kind 7, usually empty content) — keep the page to the
                // slice-1 spec: replies + mentions.
                let notifs: Vec<Event> = notifs
                    .into_iter()
                    .filter(|e| e.kind == Kind::TextNote && !e.content.trim().is_empty())
                    .collect();
                for p in profiles {
                    self.profiles.insert(p.pubkey.clone(), p);
                }
                for n in &notifs {
                    self.want_media(n, cx);
                    self.want_quotes(n, cx);
                }
                self.rows = sectioned(prev, &notifs);
                self.status.clear();
            }
            FeedEvent::Failed(e) => self.status = format!("Could not load notifications: {e}"),
            _ => {}
        }
        self.list.reset(self.rows.len());
        cx.notify();
    }

    fn want_media(&mut self, note: &Event, cx: &mut Context<Self>) {
        for url in content::media_urls(note) {
            media::fetch(&self.core, url, cx, |_, _, _, cx| cx.notify());
        }
    }

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
}

/// New / Earlier section labels at the high-water boundary
/// (routes/notifications.ts) — only when the page straddles it.
fn sectioned(prev: u64, notifs: &[Event]) -> Vec<Row> {
    let new_count = notifs
        .iter()
        .filter(|n| n.created_at.as_secs() > prev)
        .count();
    let split = (new_count > 0 && new_count < notifs.len()).then_some(new_count);
    notifs
        .iter()
        .enumerate()
        .flat_map(|(i, n)| {
            let label = match split {
                Some(at) if i == at => Some("Earlier"),
                _ if split.is_some() && i == 0 => Some("New"),
                _ => None,
            };
            let label = label.map(Row::Label);
            label.into_iter().chain(std::iter::once(Row::Note(n.clone())))
        })
        .collect()
}

impl Render for NotificationsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let handle = cx.entity();
        let status = self.status.clone();
        let rows = self.rows.clone();
        let profiles = self.profiles.clone();
        let media = media::Snapshot::new(cx);
        let quotes = self.quotes.clone();
        let empty = rows.is_empty();

        div()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .bg(rgb(theme::WASHI))
            .text_color(rgb(theme::INK))
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
                            .text_color(rgb(theme::MUTED))
                            .child(if status.is_empty() {
                                "Nothing new. Sit quietly.".to_string()
                            } else {
                                status
                            })
                            .into_any_element()
                    } else {
                        list(self.list.clone(), move |idx, _w, _cx| {
                            match rows.get(idx) {
                                Some(Row::Label(text)) => div()
                                    .w_full()
                                    .max_w(px(content::COL_W))
                                    .px_4()
                                    .pt_3()
                                    .text_size(px(12.0))
                                    .font_family(theme::SERIF)
                                    .text_color(rgb(theme::MUTED))
                                    .child(text.to_string())
                                    .into_any_element(),
                                Some(Row::Note(ev)) => content::compact_card(
                                    idx, "notif", ev, &profiles, &media, &quotes, handle.clone(),
                                ),
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
