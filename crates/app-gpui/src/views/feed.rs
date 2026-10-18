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
    div, list, prelude::*, px, rgb, Context, Entity, FontWeight, ListAlignment, ListState, Render,
    SharedString, Window,
};
use nostr::prelude::*;
use satori_core::manager::{Core, FeedEvent, FeedRx, Profile};

use crate::{icons, theme};

pub struct Feed {
    core: Arc<Core>,
    notes: Vec<Event>,
    profiles: HashMap<String, Profile>,
    liked: HashSet<String>,
    status: String,
    list: ListState,
}

impl Feed {
    pub fn new(core: Arc<Core>, cx: &mut Context<Self>) -> Self {
        let feed = Self {
            core,
            notes: vec![],
            profiles: HashMap::new(),
            liked: HashSet::new(),
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
            FeedEvent::Profiles(profiles) => self.absorb_profiles(profiles),
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
        self.absorb_profiles(profiles);
        for note in notes {
            // TODO(port): note_card.rs renders all kinds via the manifest
            // registry; the GPUI port starts with text notes only (P2/P3).
            if note.kind != Kind::TextNote {
                continue;
            }
            if self.notes.iter().any(|n| n.id == note.id) {
                continue;
            }
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

    fn absorb_profiles(&mut self, profiles: Vec<Profile>) {
        for p in profiles {
            self.profiles.insert(p.pubkey.clone(), p);
        }
    }

    /// Optimistic like, per §1.2: mutate state first (glyph flips vermilion
    /// now), reconcile against core's publish events later.
    /// TODO(port): note_card.rs `toggle_like` with rollback — P2. The spike
    /// drains publish events without reconcile.
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
}

fn rel_time(secs: u64) -> String {
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

fn note_card(
    idx: usize,
    ev: &Event,
    profiles: &HashMap<String, Profile>,
    liked: &HashSet<String>,
    handle: Entity<Feed>,
) -> impl IntoElement {
    // TODO(port): note_card.rs layout — P0 minimal card; name fallback chain
    // matches Profile::display (render/util.ts style).
    let key = ev.pubkey.to_string();
    let name = profiles
        .get(&key)
        .map(|p| p.display())
        .unwrap_or_else(|| format!("{}…", &key[..8]));
    let time = rel_time(ev.created_at.as_secs());
    let content = ev.content.clone();
    let engaged = liked.contains(&ev.id.to_string());
    let ev_for_click = ev.clone();

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
                .items_baseline()
                .justify_between()
                .child(
                    div()
                        .text_size(px(13.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::INK))
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
            div()
                .font_family(theme::SERIF)
                .text_size(px(15.5))
                .text_color(rgb(theme::INK))
                .child(content),
        )
        .child(
            div()
                .flex()
                .gap_5()
                .items_center()
                .child(
                    div()
                        .id(SharedString::from(format!("like-{idx}")))
                        .cursor_pointer()
                        .on_click(move |_, _, cx| {
                            let ev = ev_for_click.clone();
                            handle.update(cx, |feed, _| feed.toggle_like(&ev));
                        })
                        .child(icons::heart_icon(if engaged {
                            theme::VERMILION
                        } else {
                            theme::MUTED
                        })),
                )
                .child(icons::bubble_icon(theme::MUTED))
                .child(icons::bookmark_icon(theme::MUTED)),
        )
}

impl Render for Feed {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let handle = cx.entity();
        let status = self.status.clone();
        let empty = self.notes.is_empty();

        div()
            .size_full()
            .bg(rgb(theme::WASHI))
            .text_color(rgb(theme::INK))
            .font_family(theme::SANS)
            .flex()
            .flex_col()
            .child(
                div()
                    .px_4()
                    .py_2()
                    .text_size(px(12.0))
                    .text_color(rgb(theme::MUTED))
                    .border_b_1()
                    .border_color(rgb(theme::HAIRLINE))
                    .child(if status.is_empty() {
                        "Satori".to_string()
                    } else {
                        status
                    }),
            )
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
                    list(self.list.clone(), move |idx, _w, _cx| {
                        match notes.get(idx) {
                            Some(ev) => {
                                note_card(idx, ev, &profiles, &liked, handle.clone())
                                    .into_any_element()
                            }
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
