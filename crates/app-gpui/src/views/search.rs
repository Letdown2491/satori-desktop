//! Search — port of crates/app/src/search.rs (slice 1: notes, NIP-50
//! `search` filter): one field, Enter or the seal runs it, results as
//! compact cards under a Notes / People tab pair (profile.rs's underline
//! tabs). Notes is the relay search; People is the hydrated profile-cache
//! scan — a search that answers no notes but matches people lands there.
//! Above the tabs: a direct-open row when the query is a NIP-21 entity
//! (uris.rs). The operator grammar (`by:`, `p:`, `has:`, `since:`,
//! `site:`) is relay-side — the hint row names it, relays without NIP-50
//! search just answer empty.

use std::{collections::HashMap, sync::Arc};

use gpui::{
    div, list, prelude::*, px, rgb, AnyElement, Context, Entity, EventEmitter, FocusHandle,
    Focusable, KeyDownEvent, ListAlignment, ListState, Render, SharedString, Window,
};
use nostr::prelude::*;
use satori_core::manager::{Core, FeedEvent, Profile};

use crate::{input::Input, media, theme, uris};
use super::{content, nav::Nav};

/// The results pane's tab.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SearchTab {
    /// The relay search (NIP-50 `search`).
    Notes,
    /// The profile-cache scan (`Core::search_people`).
    People,
}

pub struct SearchView {
    core: Arc<Core>,
    input: Entity<Input>,
    focus: FocusHandle,
    results: Vec<Event>,
    profiles: HashMap<String, Profile>,
    quotes: content::Quotes,
    status: String,
    /// The question as typed — the nothing-found state reads it back.
    query: String,
    /// People matches (pubkeys, row order; the display data lives in
    /// `profiles`) and the direct-open target when the query parses as a
    /// NIP-21 entity. Both computed at run time, cleared on the next run.
    people: Vec<String>,
    target: Option<uris::UriTarget>,
    tab: SearchTab,
    /// The picture urls pinned for THIS query's People rows. The pane
    /// stops rendering while idle, render touches stop with it, and
    /// small-class pressure then evicts the on-screen thumbs — the
    /// blinking avatar. Pinned rows never evict; the next run unpins
    /// these before pinning its own, so the set stays bounded.
    pinned: Vec<String>,
    /// Each run invalidates the previous pump — a slow answer to an old
    /// question must not overwrite the new one. Also the "never searched"
    /// flag (0 = the empty state's cue).
    generation: u64,
    list: ListState,
    autofocused: bool,
}

impl EventEmitter<Nav> for SearchView {}

impl SearchView {
    pub fn new(core: Arc<Core>, cx: &mut Context<Self>) -> Self {
        crate::theme::watch(cx); // repaint on theme change
        crate::timefmt::watch(cx); // repaint on mode/height change
        crate::video::watch(cx); // repaint on player open/toggle/close
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
            query: String::new(),
            people: vec![],
            target: None,
            tab: SearchTab::Notes,
            pinned: vec![],
            generation: 0,
            list: ListState::new(0, ListAlignment::Top, px(400.)),
            autofocused: false,
        }
    }

    /// The direct-open row's click: route the parsed NIP-21 target through
    /// the same seams the argv URIs use (main.rs route_target). The naddr
    /// resolves here first (relays, newest-wins), then opens as an article
    /// (30023) or the thread containing it.
    fn open_target(&mut self, target: uris::UriTarget, cx: &mut Context<Self>) {
        match target {
            uris::UriTarget::Profile(who) => cx.emit(Nav::OpenProfile(who)),
            uris::UriTarget::Thread(id) => cx.emit(Nav::OpenThread(id)),
            uris::UriTarget::Article { kind, author, identifier } => {
                let generation = self.generation;
                self.status = "Opening…".into();
                let mut rx = self.core.event_by_address(kind, author, identifier);
                cx.spawn(async move |this, cx| {
                    if let Some(FeedEvent::One(Some(ev), _)) = rx.recv().await {
                        let _ = this.update(cx, |this, cx| {
                            if this.generation != generation {
                                return; // a new question superseded the open
                            }
                            if u16::from(ev.kind) == 30023 {
                                cx.emit(Nav::OpenArticle(ev));
                            } else {
                                cx.emit(Nav::OpenThread(ev.id));
                            }
                        });
                    }
                })
                .detach();
            }
        }
        cx.notify();
    }

    fn run(&mut self, cx: &mut Context<Self>) {        let q = self.input.read(cx).content.trim().to_string();
        if q.is_empty() {
            return;
        }
        // New question, clean answer (search.rs:41) — profiles too: the
        // map used to survive across runs for the pane's lifetime.
        self.generation += 1;
        let generation = self.generation;
        self.results.clear();
        self.quotes.clear();
        self.profiles.clear();
        self.list.reset(0);
        self.query = q.clone();
        self.tab = SearchTab::Notes;
        self.status = "Searching…".into();
        // People from the cache and the direct-open target are per-question
        // too (the clean-answer rule). The rows are on screen while the
        // pane idles — render touches stop with rendering, so the rows'
        // thumbs get pinned (and the previous query's released).
        media::unpin(&self.pinned, cx);
        self.people.clear();
        for p in self.core.search_people(&q) {
            if !p.picture.is_empty() {
                media::pin(&p.picture, cx);
                self.pinned.push(p.picture.clone());
            }
            self.profiles.insert(p.pubkey.clone(), p.clone());
            self.people.push(p.pubkey);
        }
        self.target = uris::uri_target(&q);
        let mut rx = self.core.search(q);
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.recv().await {
                let _ = this.update(cx, |this, cx| this.on_event(ev, generation, cx));
            }
        })
        .detach();
        cx.notify();
    }

    /// One search tab: a quiet label, the active one ink + underlined
    /// (profile.rs's tab pattern).
    fn tab_btn(
        &self,
        which: SearchTab,
        label: &'static str,
        cx: &Context<Self>,
    ) -> AnyElement {
        let on = self.tab == which;
        div()
            .id(SharedString::from(format!("tab-{label}")))
            .cursor_pointer()
            .text_size(px(12.5))
            .text_color(rgb(if on { theme::ink() } else { theme::muted() }))
            .when(on, |d| d.underline())
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.tab != which {
                    this.tab = which;
                    cx.notify();
                }
            }))
            .into_any_element()
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
                        media::fetch(&self.core, url, cx.entity_id(), cx);
                    }
                    self.profiles.insert(p.pubkey.clone(), p);
                }
                for n in &results {
                    for url in content::media_urls_cached(n).iter().cloned() {
                        media::fetch(&self.core, url, cx.entity_id(), cx);
                    }
                    self.want_quotes(n, cx);
                }
                self.status = String::new();
                self.results = results;
                self.list.reset(self.results.len());
                // No notes but people matched: the People tab is the
                // answer (the owner's tab model — notes stay the default).
                if self.results.is_empty() && !self.people.is_empty() {
                    self.tab = SearchTab::People;
                }
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
            window.focus(&self.focus, cx);
        }
        let handle = cx.entity();
        let status = self.status.clone();
        let results = self.results.clone();
        let profiles = self.profiles.clone();
        let media = media::Snapshot::new(cx);
        // Evicted avatars come back on render — search results are bounded
        // content (fetch is a silent no-op for entries the cache holds).
        let viewer = cx.entity_id();
        for p in self.profiles.values() {
            media::fetch(&self.core, p.picture.clone(), viewer, cx);
        }
        let quotes = self.quotes.clone();
        let input = self.input.clone();
        // The Input reports percent height (it fills its parent), which
        // collapses to zero in a content-sized wrapper — the P3 gotcha.
        // One-line field: a definite height of one line + its padding
        // (py_1) + the hairline borders.
        let field_h = window.line_height() + px(10.0);

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
            .bg(rgb(theme::washi()))
            .text_color(rgb(theme::ink()))
            .font_family(theme::SANS)
            .flex()
            .flex_col()
            .child(
                // The question row: field + seal.
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
                            .h(field_h)
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .bg(rgb(theme::panel()))
                            .border_1()
                            .border_color(rgb(theme::hairline()))
                            .child(input),
                    )
                    .child(
                        div()
                            .id("run-search")
                            .cursor_pointer()
                            .px_2()
                            .py_1()
                            .text_size(px(13.0))
                            .text_color(rgb(theme::vermilion()))
                            .on_click(cx.listener(|this, _, _, cx| this.run(cx)))
                            .child("Search"),
                    ),
            )
            .when_some(self.target.clone(), |d, target| {
                // The query is a NIP-21 entity: one quiet action row, the
                // router D27 already trusts (open_thread/open_profile/
                // event_by_address then open_article).
                let label = match &target {
                    uris::UriTarget::Profile(_) => "Open profile",
                    uris::UriTarget::Thread(_) => "Open thread",
                    uris::UriTarget::Article { .. } => "Open article",
                };
                d.child(
                    div()
                        .px_4()
                        .pb_1()
                        .flex()
                        .child(
                            div()
                                .id("open-target")
                                .cursor_pointer()
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .border_1()
                                .border_color(rgb(theme::hairline()))
                                .text_size(px(12.5))
                                .text_color(rgb(theme::ink()))
                                .hover(|s| {
                                    s.border_color(rgb(theme::vermilion()))
                                        .text_color(rgb(theme::vermilion()))
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_target(target.clone(), cx);
                                }))
                                .child(label),
                        ),
                )
            })
            .when(self.generation > 0, |d| {
                // The tab pair: Notes (the relay search) and People (the
                // profile-cache scan) — profile.rs's underline pattern.
                d.child(
                    div()
                        .w_full()
                        .max_w(px(content::COL_W))
                        .flex()
                        .items_baseline()
                        .gap_4()
                        .px_4()
                        .pt_1()
                        .pb_1()
                        .border_b_1()
                        .border_color(rgb(theme::hairline()))
                        .child(self.tab_btn(SearchTab::Notes, "Notes", cx))
                        .child(self.tab_btn(SearchTab::People, "People", cx)),
                )
            })
            .when(!status.is_empty(), |d| {
                // In-flight and failures only — "nothing found" is a state
                // of its own now, not a dangling status line.
                d.child(
                    div()
                        .px_4()
                        .pt_2()
                        .text_size(px(12.0))
                        .text_color(rgb(theme::muted()))
                        .child(status.clone()),
                )
            })
            .child(if self.generation == 0 {
                // The virgin state: what this pane is and what relays can
                // be asked, quiet and centered. Gone at the first run.
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(
                        div()
                            .font_family(theme::SERIF)
                            .text_size(px(15.0))
                            .text_color(rgb(theme::muted()))
                            .child("Search the notes your relays hold."),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(rgb(theme::muted()))
                            .child(
                                "Operators some relays honor: by: p: t: has: since: site:",
                            ),
                    )
                    .into_any_element()
            } else if self.tab == SearchTab::People {
                // The People tab: the cache matches as avatar rows.
                div()
                    .id("search-people")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .when(self.people.is_empty(), |d| {
                        d.child(
                            div()
                                .flex_1()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(px(12.0))
                                .text_color(rgb(theme::muted()))
                                .child("No people matched."),
                        )
                    })
                    .when(!self.people.is_empty(), |d| {
                        let mut rows = d.child(
                            div()
                                .px_4()
                                .pt_2()
                                .text_size(px(11.0))
                                .text_color(rgb(theme::muted()))
                                .child("From the profiles your relays have sent."),
                        );
                        for (i, pk) in self.people.iter().enumerate() {
                            let Some(p) = self.profiles.get(pk) else {
                                continue;
                            };
                            let pk = pk.clone();
                            rows = rows.child(
                                div()
                                    .id(SharedString::from(format!("person-{i}")))
                                    .cursor_pointer()
                                    .w_full()
                                    .max_w(px(content::COL_W))
                                    .px_4()
                                    .py_1p5()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .hover(|s| s.bg(rgb(theme::panel())))
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        // The row is a profile link: the
                                        // same nav the cards' name taps
                                        // use. (Parsed here — the row
                                        // closure owns its pk copy.)
                                        if let Ok(who) = PublicKey::parse(&pk) {
                                            cx.emit(Nav::OpenProfile(who));
                                        }
                                    }))
                                    .child(media::avatar(
                                        &media,
                                        &p.picture,
                                        if p.name.is_empty() { &p.pubkey } else { &p.name },
                                        26.0,
                                    ))
                                    .child(
                                        div()
                                            .text_size(px(13.5))
                                            .text_color(rgb(theme::ink()))
                                            .child(if p.name.is_empty() {
                                                content::short_bech32(&p.pubkey)
                                            } else {
                                                p.name.clone()
                                            }),
                                    )
                                    .when(!p.nip05.is_empty(), |row| {
                                        row.child(
                                            div()
                                                .text_size(px(12.0))
                                                .text_color(rgb(theme::muted()))
                                                .child(p.nip05.clone()),
                                        )
                                    }),
                            );
                        }
                        rows
                    })
                    .into_any_element()
            } else if !self.results.is_empty() {
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .justify_center()
                    .child(
                        list(self.list.clone(), move |idx, _w, cx| {
                            match results.get(idx) {
                                Some(ev) => {
                                    // The stamp reads the current mode +
                                    // blocktime cache per frame; ensure
                                    // refines Block mode from the bridge.
                                    let secs = ev.created_at.as_secs();
                                    let time = crate::timefmt::stamp(secs, cx);
                                    crate::timefmt::ensure(secs, cx);
                                    content::compact_card(
                                        idx, "search", ev, &profiles, &media, &quotes,
                                        time, handle.clone(),
                                    )
                                }
                                None => div().into_any_element(),
                            }
                        })
                        .w_full()
                        .max_w(px(content::COL_W))
                        .h_full(),
                    )
                    .into_any_element()
            } else if status.is_empty() {
                // The search came back empty: a centered answer, not a
                // status line. Relays without NIP-50 search answer
                // nothing — say so once, quietly.
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(
                        div()
                            .font_family(theme::SERIF)
                            .text_size(px(15.0))
                            .text_color(rgb(theme::muted()))
                            .child(format!("Nothing found for “{}”.", self.query)),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(rgb(theme::muted()))
                            .child("Relays without NIP-50 search answer nothing."),
                    )
                    .into_any_element()
            } else {
                div().flex_1().min_h_0().into_any_element()
            })
    }
}
