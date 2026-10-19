//! Search — port of crates/app/src/search.rs (slice 1: notes only, NIP-50
//! `search` filter): one field, Enter or the seal runs it, results as compact
//! cards. People results and the operator grammar (`by:`, `p:`, `has:`,
//! `since:`, `site:`) land with the rich-cards milestone.

use std::{collections::HashMap, sync::Arc};

use gpui::{
    div, list, prelude::*, px, rgb, Context, Entity, EventEmitter, FocusHandle, Focusable,
    KeyDownEvent, ListAlignment, ListState, Render, Window,
};
use nostr::prelude::*;
use satori_core::manager::{Core, FeedEvent, Profile};

use crate::{input::Input, media, theme};
use super::{content, nav::Nav};

pub struct SearchView {
    core: Arc<Core>,
    input: Entity<Input>,
    focus: FocusHandle,
    results: Vec<Event>,
    profiles: HashMap<String, Profile>,
    quotes: content::Quotes,
    status: String,
    /// Each run invalidates the previous pump — a slow answer to an old
    /// question must not overwrite the new one.
    generation: u64,
    list: ListState,
    autofocused: bool,
}

impl EventEmitter<Nav> for SearchView {}

impl SearchView {
    pub fn new(core: Arc<Core>, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            let mut i = Input::new(cx);
            i.multiline = false; // Enter runs the question, no newline
            i.placeholder("Search notes…")
        });
        let focus = input.read(cx).focus_handle(cx);
        Self {
            core,
            input,
            focus,
            results: vec![],
            profiles: HashMap::new(),
            quotes: HashMap::new(),
            status: String::new(),
            generation: 0,
            list: ListState::new(0, ListAlignment::Top, px(400.)),
            autofocused: false,
        }
    }

    fn run(&mut self, cx: &mut Context<Self>) {
        let q = self.input.read(cx).content.trim().to_string();
        if q.is_empty() {
            return;
        }
        // New question, clean answer (search.rs:41).
        self.generation += 1;
        let generation = self.generation;
        self.results.clear();
        self.quotes.clear();
        self.list.reset(0);
        self.status = "Searching…".into();
        let mut rx = self.core.search(q);
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.recv().await {
                let _ = this.update(cx, |this, cx| this.on_event(ev, generation, cx));
            }
        })
        .detach();
        cx.notify();
    }

    fn on_event(&mut self, ev: FeedEvent, generation: u64, cx: &mut Context<Self>) {
        if generation != self.generation {
            return; // stale answer
        }
        match ev {
            FeedEvent::Search(results, profiles) => {
                for p in profiles {
                    let url = p.picture.clone();
                    if !url.is_empty() {
                        media::fetch(&self.core, url, cx, |_, _, _, cx| cx.notify());
                    }
                    self.profiles.insert(p.pubkey.clone(), p);
                }
                for n in &results {
                    for url in content::media_urls(n) {
                        media::fetch(&self.core, url, cx, |_, _, _, cx| cx.notify());
                    }
                    self.want_quotes(n, cx);
                }
                self.status = if results.is_empty() {
                    "Nothing found.".into()
                } else {
                    String::new()
                };
                self.results = results;
                self.list.reset(self.results.len());
            }
            FeedEvent::Failed(m) => self.status = format!("Search failed: {m}"),
            _ => {}
        }
        cx.notify();
    }

    fn pump_quote(&self, bech: String, mut rx: satori_core::manager::FeedRx, cx: &mut Context<Self>) {
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

impl Render for SearchView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.autofocused {
            self.autofocused = true;
            window.focus(&self.focus);
        }
        let handle = cx.entity();
        let status = self.status.clone();
        let results = self.results.clone();
        let profiles = self.profiles.clone();
        let media = media::Snapshot::new(cx);
        let quotes = self.quotes.clone();
        let input = self.input.clone();

        div()
            .id("search-pane")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                // Enter runs the question; the Input doesn't bind it, so it
                // bubbles here from the focused field.
                if ev.keystroke.key == "enter" {
                    this.run(cx);
                }
            }))
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .bg(rgb(theme::WASHI))
            .text_color(rgb(theme::INK))
            .font_family(theme::SANS)
            .flex()
            .flex_col()
            .child(
                // The question row (search.rs:18): field + seal.
                div()
                    .w_full()
                    .max_w(px(content::COL_W))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .pt_3()
                    .pb_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .bg(rgb(theme::PANEL))
                            .border_1()
                            .border_color(rgb(theme::HAIRLINE))
                            .child(input),
                    )
                    .child(
                        div()
                            .id("run-search")
                            .cursor_pointer()
                            .px_2()
                            .py_1()
                            .text_size(px(13.0))
                            .text_color(rgb(theme::VERMILION))
                            .on_click(cx.listener(|this, _, _, cx| this.run(cx)))
                            .child("Search"),
                    ),
            )
            .child(div().w_full().max_w(px(content::COL_W)).flex().flex_col().child(
                div()
                    .px_4()
                    .text_size(px(12.0))
                    .text_color(rgb(theme::MUTED))
                    .when(!status.is_empty(), |d| d.child(status)),
            ))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .justify_center()
                    .child(
                        list(self.list.clone(), move |idx, _w, _cx| {
                            match results.get(idx) {
                                Some(ev) => content::compact_card(
                                    idx,
                                    "search",
                                    ev,
                                    None,
                                    &profiles, &media, &quotes, handle.clone(),
                                ),
                                None => div().into_any_element(),
                            }
                        })
                        .w_full()
                        .max_w(px(content::COL_W))
                        .h_full(),
                    ),
            )
    }
}
