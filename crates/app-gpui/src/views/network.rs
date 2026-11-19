//! The Network page (D83): who you follow, who follows you, who you've
//! muted — one me-menu row, one pane, three segments (the search.rs tab
//! pattern). The row actions are the profile page's Follow/Mute buttons
//! per row, optimistic with the publishes drained and ignored (P1).
//!
//! Honesty note baked into the Followers header: Nostr has no followers
//! event — the list is a reverse lookup over the contact lists the
//! relays have delivered, so it says "seen", not global truth.

use gpui::{
    AnyElement, Context, EventEmitter, Render, SharedString, Window, div, prelude::*, px, rgb,
    uniform_list,
};
use nostr::prelude::*;
use satori_core::manager::{Core, FeedEvent, FeedRx, Profile};
use std::{collections::HashMap, sync::Arc};

use super::{content, nav::Nav};
use crate::{media, theme};

pub(crate) struct NetworkView {
    core: Arc<Core>,
    tab: Tab,
    /// People the store has SEEN following me (the reverse lookup).
    followers: Vec<PublicKey>,
    /// The own follow set (the cached contact list's p-keys).
    follows: Vec<PublicKey>,
    /// The own mute set (kind:10000, decrypted at session start).
    mutes: Vec<PublicKey>,
    /// Resolved profiles for every pk on any segment.
    profiles: HashMap<String, Profile>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Following,
    Followers,
    Muted,
}

impl EventEmitter<Nav> for NetworkView {}

impl NetworkView {
    pub(crate) fn new(core: Arc<Core>, cx: &mut Context<Self>) -> Self {
        crate::theme::watch(cx); // repaint on theme change
        let mut this = Self {
            core: core.clone(),
            tab: Tab::Following,
            followers: Vec::new(),
            follows: core.follows(),
            mutes: core.mutes(),
            profiles: HashMap::new(),
        };
        this.load(cx);
        this
    }

    /// The three lists. The own sets are sync mirrors (one read each);
    /// the followers reverse-lookup is a store scan (one-shot), and the
    /// profiles resolve reactively over the union (lists.rs's pump).
    fn load(&mut self, cx: &mut Context<Self>) {
        self.follows = self.core.follows();
        self.mutes = self.core.mutes();

        // The one profiles pump, shared (D84 audit): batches land in the
        // map until the channel closes (single batch by construction —
        // ProfileService resolves each waiter exactly once).
        let mut all: Vec<PublicKey> = self.follows.clone();
        all.extend(self.mutes.iter().copied());
        let weak = cx.entity().downgrade();
        let rx = self.core.ensure_profiles(all);
        cx.spawn(async move |_, cx| Self::pump_body(weak, rx, cx).await)
            .detach();

        // The followers scan: one-shot receiver; its batch joins the
        // same pump (the scan lands once, so this spawns once).
        let core = self.core.clone();
        let mut rx = self.core.followers_seen();
        cx.spawn(async move |this, cx| {
            while let Some(followers) = rx.recv().await {
                let batch = core.ensure_profiles(followers.clone());
                Self::pump_body(this.clone(), batch, cx).await;
                let _ = this.update(cx, |this, cx| {
                    this.followers = followers;
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// The one profiles pump: batch-and-notify until the channel closes.
    /// Two load() sites want it (the union seed, the followers batch) —
    /// one body, named (D84 audit).
    async fn pump_body(this: gpui::WeakEntity<Self>, mut rx: FeedRx, cx: &mut gpui::AsyncApp) {
        while let Some(ev) = rx.recv().await {
            if let FeedEvent::Profiles(ps) = ev {
                let _ = this.update(cx, |this, cx| {
                    for p in ps {
                        this.profiles.insert(p.pubkey.clone(), p);
                    }
                    cx.notify();
                });
            }
        }
    }

    // The row actions: optimistic flips over the mirrors, publish events
    // drained and ignored (profile.rs's Follow/Mute pattern).

    fn set_follow(&mut self, who: PublicKey, follow: bool, cx: &mut Context<Self>) {
        if follow {
            if !self.follows.contains(&who) {
                self.follows.push(who);
            }
        } else {
            self.follows.retain(|f| f != &who);
        }
        let mut rx = self.core.set_following(who, follow);
        cx.spawn(async move |_, _| while rx.recv().await.is_some() {})
            .detach();
        cx.notify();
    }

    fn set_mute(&mut self, who: PublicKey, mute: bool, cx: &mut Context<Self>) {
        if mute {
            self.mutes.push(who);
        } else {
            self.mutes.retain(|m| m != &who);
        }
        let mut rx = self.core.set_muted(who, mute);
        cx.spawn(async move |_, _| while rx.recv().await.is_some() {})
            .detach();
        cx.notify();
    }

    /// One segment tab (search.rs's quiet label, active one underlined).
    fn tab_btn(&self, which: Tab, label: String, cx: &Context<Self>) -> AnyElement {
        let on = self.tab == which;
        div()
            .id(SharedString::from(format!("tab-{label}")))
            .cursor_pointer()
            .text_size(px(theme::T_UI))
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

    /// One person row: avatar, name, NIP-05 — a profile link — with the
    /// segment's actions on the right (cards.rs's stop_propagation keeps
    /// a button tap from also opening the profile).
    fn person_row(
        &self,
        i: usize,
        media: &media::Snapshot,
        pk: &PublicKey,
        actions: Vec<(&'static str, bool, Action)>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let hex = pk.to_string();
        let p = self.profiles.get(&hex);
        let (name, picture, nip05) = match p {
            Some(p) => (p.display(), p.picture.clone(), p.nip05.clone()),
            None => (content::short_bech32(&hex), String::new(), String::new()),
        };
        let who = *pk;
        let mut row = div()
            .id(SharedString::from(format!("person-{i}")))
            .w_full()
            .flex()
            .items_center()
            .gap_2()
            .py_1p5()
            .px_4()
            .cursor_pointer()
            .border_b_1()
            .border_color(rgb(theme::hairline()))
            .hover(|s| s.bg(rgb(theme::panel())))
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(Nav::OpenProfile(who))))
            .child(media::avatar(media, &picture, &name, 26.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(theme::T_BODY))
                    .text_color(rgb(theme::ink()))
                    .text_ellipsis()
                    .child(name),
            );
        if !nip05.is_empty() {
            row = row.child(
                div()
                    .flex_none()
                    .text_size(px(theme::T_SMALL))
                    .text_color(rgb(theme::muted()))
                    .child(nip05),
            );
        }
        for (label, vermilion, action) in actions {
            let (who, handle) = (*pk, cx.entity());
            row = row.child(
                div()
                    .id(SharedString::from(format!("{i}-{action:?}")))
                    .cursor_pointer()
                    .flex_none()
                    .px_2()
                    .py_1()
                    .rounded(px(theme::R_EMBED))
                    .border_1()
                    .border_color(rgb(theme::hairline()))
                    .hover(|s| s.border_color(rgb(theme::muted())))
                    .text_size(px(theme::T_SMALL))
                    .text_color(rgb(if vermilion {
                        theme::vermilion()
                    } else {
                        theme::ink()
                    }))
                    .on_click(move |_, _, cx| {
                        // A button tap is NOT a row tap.
                        cx.stop_propagation();
                        handle.update(cx, |this, cx| match action {
                            Action::Follow => this.set_follow(who, true, cx),
                            Action::Unfollow => this.set_follow(who, false, cx),
                            Action::Mute => this.set_mute(who, true, cx),
                            Action::Unmute => this.set_mute(who, false, cx),
                        });
                    })
                    .child(label),
            );
        }
        row.into_any_element()
    }
}

/// A row action's kind — the id needs it, the click needs the dispatch.
#[derive(Debug, Clone, Copy)]
enum Action {
    Follow,
    Unfollow,
    Mute,
    Unmute,
}

impl Render for NetworkView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewer = cx.entity_id();

        // Membership as sets: the per-row Vec::contains scans were
        // O(n²) per paint (D84 audit H1). Sets rebuilt per render —
        // bounded by the mirrors' sizes, still trivial.
        let is_follow: std::collections::HashSet<PublicKey> =
            self.follows.iter().copied().collect();
        let is_muted: std::collections::HashSet<PublicKey> = self.mutes.iter().copied().collect();

        // The ACTIVE tab's list only, with each name resolved once (a
        // key per element, then a borrow sort — the old path re-ran an
        // allocating key per comparison, on all three clones, every
        // paint; D84 audit M1).
        let name_of = |pk: &PublicKey| -> String {
            match self.profiles.get(&pk.to_string()) {
                Some(p) => p.display(),
                None => String::new(),
            }
        };
        let mut list: Vec<PublicKey> = match self.tab {
            Tab::Followers => self.followers.clone(),
            Tab::Following => self.follows.clone(),
            Tab::Muted => self.mutes.clone(),
        };
        let mut keyed: Vec<(String, PublicKey)> =
            list.drain(..).map(|pk| (name_of(&pk), pk)).collect();
        keyed.sort_by(|a, b| b.0.cmp(&a.0));

        // Repin + LRU touch happen INSIDE the virtual list below: only
        // rows actually painted touch the eviction window (D60's
        // discipline; the D84 fix narrowed the crawl from three tabs to
        // one — the virtual list narrows it to the visible window).

        // Rows are VIRTUALIZED (uniform_list): every scroll tick used to
        // rebuild + lay out every row of the segment (the "janky scroll"
        // the 0.10.0 test round reported) — now only the visible window
        // exists per frame. The sort above stays per-paint (it feeds the
        // window offsets), but it's a borrow over pre-built keys.
        let count = keyed.len();
        let note: &str = match self.tab {
            Tab::Followers if count > 0 => "seen through your relays' contact lists",
            _ => "",
        };
        let tab = self.tab;
        let list: AnyElement = if count == 0 {
            placeholder(match tab {
                Tab::Followers => "No followers seen yet.",
                Tab::Following => "You aren't following anyone yet.",
                Tab::Muted => "Your mute list is empty.",
            })
        } else {
            let this = cx.entity();
            uniform_list("net-list", count, move |range, _window, cx| {
                this.update(cx, |this, cx| {
                    let media = media::Snapshot::new(cx);
                    let mut painted: Vec<String> = Vec::new();
                    let mut seen = std::collections::HashSet::new();
                    let mut rows: Vec<AnyElement> = Vec::new();
                    for (i, (_, pk)) in keyed
                        .iter()
                        .enumerate()
                        .skip(range.start)
                        .take(range.end.saturating_sub(range.start))
                    {
                        if let Some(p) = this.profiles.get(&pk.to_string()) {
                            media::fetch(&this.core, p.picture.clone(), viewer, cx);
                            if !p.picture.is_empty() && seen.insert(p.picture.clone()) {
                                painted.push(p.picture.clone());
                            }
                        }
                        // Per-segment rows: Followers (Follow?+Mute),
                        // Following (Unfollow+Mute), Muted (Unmute).
                        let mut actions: Vec<(&'static str, bool, Action)> = Vec::new();
                        match tab {
                            Tab::Followers => {
                                if !is_muted.contains(pk) {
                                    actions.push(("Mute", false, Action::Mute));
                                }
                                if is_follow.contains(pk) {
                                    actions.push(("Following", false, Action::Unfollow));
                                } else {
                                    actions.push(("Follow", true, Action::Follow));
                                }
                            }
                            Tab::Following => {
                                if !is_muted.contains(pk) {
                                    actions.push(("Mute", false, Action::Mute));
                                } else {
                                    actions.push(("Unmute", false, Action::Unmute));
                                }
                                actions.push(("Unfollow", false, Action::Unfollow));
                            }
                            Tab::Muted => {
                                actions.push(("Unmute", false, Action::Unmute));
                            }
                        }
                        rows.push(this.person_row(i, &media, pk, actions, cx));
                    }
                    media::repin(viewer, painted, cx);
                    rows
                })
            })
            .w_full()
            .h_full()
            .into_any_element()
        };

        div()
            .id("following")
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::washi()))
            .child(
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
                    .child(self.tab_btn(
                        Tab::Following,
                        format!("Following {}", self.follows.len()),
                        cx,
                    ))
                    .child(self.tab_btn(
                        Tab::Followers,
                        format!("Followers {}", self.followers.len()),
                        cx,
                    ))
                    .child(self.tab_btn(Tab::Muted, format!("Muted {}", self.mutes.len()), cx)),
            )
            .when(!note.is_empty() && count > 0, |d| {
                d.child(
                    div()
                        .px_4()
                        .pt_2()
                        .text_size(px(theme::T_SMALL))
                        .text_color(rgb(theme::muted()))
                        .child(note),
                )
            })
            .child(
                div()
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .max_w(px(content::COL_W))
                    .flex()
                    .flex_col()
                    .child(list),
            )
    }
}

/// The quiet centered empty state (lists.rs's placeholder, inlined).
fn placeholder(what: &str) -> AnyElement {
    div()
        .flex_1()
        .min_h_0()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(theme::T_SMALL))
        .text_color(rgb(theme::muted()))
        .child(what.to_string())
        .into_any_element()
}
