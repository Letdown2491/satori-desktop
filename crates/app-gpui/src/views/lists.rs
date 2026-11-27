//! The Lists page (D38's lists slice, D81 split out of the retired
//! Library pane): the own NIP-51 named lists (kind 30000), read-only —
//! the detail is an in-page second level, the member rows are profile
//! links (search.rs's).

use gpui::{
    AnyElement, Context, EventEmitter, FontWeight, Render, SharedString, Window, div, prelude::*,
    px, rgb,
};
use nostr::prelude::*;
use satori_core::manager::{Core, FeedEvent, FeedRx, NostrList, Profile};
use std::{collections::HashMap, sync::Arc};

use super::{content, nav::Nav};
use crate::{media, theme};

pub(crate) struct ListsView {
    core: Arc<Core>,
    /// The own NIP-51 lists, re-queried on every visit (the fresh result
    /// replaces whatever showed).
    lists: Vec<NostrList>,
    /// The open list's detail (in-page second level): the list + its
    /// members' resolved profiles.
    open_list: Option<NostrList>,
    members: HashMap<PublicKey, Profile>,
}

impl EventEmitter<Nav> for ListsView {}

impl ListsView {
    pub(crate) fn new(core: Arc<Core>, cx: &mut Context<Self>) -> Self {
        crate::theme::watch(cx); // repaint on theme change
        let mut this = Self {
            core,
            lists: Vec::new(),
            open_list: None,
            members: HashMap::new(),
        };
        this.load_lists(cx);
        this
    }

    /// A page visit (D38 v1.2: EVERY visit, not just the first —
    /// the one-shot query raced the SDK's saves and pinned the empty
    /// placeholder). Two deliveries apply: the instant store snapshot,
    /// then the post-fetch merge.
    fn load_lists(&mut self, cx: &mut Context<Self>) {
        let mut rx = self.core.own_lists();
        cx.spawn(async move |this, cx| {
            while let Some(lists) = rx.recv().await {
                let _ = this.update(cx, |this, cx| {
                    this.lists = lists;
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Open a list's detail: the members resolve reactively (search.rs's
    /// person rows — ensure_profiles pumps Profiles batches). A private
    /// list's members decrypt here, lazily (v1.2) — the rows never wait
    /// on crypto.
    fn open_list(&mut self, l: NostrList, cx: &mut Context<Self>) {
        if self.open_list.as_ref().is_some_and(|o| o.d == l.d) {
            return;
        }
        self.open_list = Some(l.clone());
        let mut rx: FeedRx = self.core.ensure_profiles(l.members.clone());
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.recv().await {
                if let FeedEvent::Profiles(ps) = ev {
                    let _ = this.update(cx, |this, cx| {
                        for p in ps {
                            this.members.insert(p.pubkey, p);
                        }
                        cx.notify();
                    });
                }
            }
        })
        .detach();
        // The lazy decrypt (v1.2): a private list's p-tags live in the
        // encrypted content — one decrypt per OPEN, the members row in
        // when it lands.
        if l.members.is_empty() && !l.event.content.is_empty() {
            let mut rx = self.core.decrypt_list_members(l.event.clone());
            cx.spawn(async move |this, cx| {
                let decrypted = rx.recv().await.unwrap_or_default();
                let _ = this.update(cx, |this, cx| {
                    if decrypted.is_empty() {
                        return;
                    }
                    let mut rx = this.core.ensure_profiles(decrypted.clone());
                    // Feed the batch pump on this spawn; the detail shows
                    // the people as their profiles resolve.
                    cx.spawn(async move |this, cx| {
                        while let Some(ev) = rx.recv().await {
                            if let FeedEvent::Profiles(ps) = ev {
                                let _ = this.update(cx, |this, cx| {
                                    for p in ps {
                                        this.members.insert(p.pubkey, p);
                                    }
                                    cx.notify();
                                });
                            }
                        }
                    })
                    .detach();
                    if let Some(o) = this.open_list.as_mut() {
                        o.members.extend(decrypted);
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        cx.notify();
    }

    /// One list row: the display name, the member count, a chevron.
    fn list_row(&self, i: usize, l: &NostrList, cx: &mut Context<Self>) -> AnyElement {
        let count = l.members.len();
        let tap = l.clone();
        div()
            .id(SharedString::from(format!("list-{i}")))
            .w_full()
            .flex()
            .items_center()
            .gap_2()
            .py_1p5()
            .px_4()
            .border_b_1()
            .border_color(rgb(theme::hairline()))
            .hover(|s| s.bg(rgb(theme::panel())))
            .on_click(cx.listener(move |this, _, _, cx| this.open_list(tap.clone(), cx)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(theme::T_BODY))
                    .text_color(rgb(theme::ink()))
                    .text_ellipsis()
                    .child(l.display()),
            )
            // The count hides when 0: a private list's members are
            // unknown until the detail's lazy decrypt runs.
            .when(count > 0, |d| {
                d.child(
                    div()
                        .flex_none()
                        .text_size(px(theme::T_SMALL))
                        .text_color(rgb(theme::muted()))
                        .child(if count == 1 {
                            "1 person".to_string()
                        } else {
                            format!("{count} people")
                        }),
                )
            })
            .child(
                div()
                    .flex_none()
                    .text_size(px(theme::T_SMALL))
                    .text_color(rgb(theme::muted()))
                    .child("›"),
            )
            .into_any_element()
    }

    /// One member row (search.rs's person rows): avatar, name, NIP-05 — a
    /// profile link.
    fn member_row(
        &self,
        i: usize,
        media: &media::Snapshot,
        pk: &PublicKey,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let hex = pk.to_string();
        let p = self.members.get(pk);
        let (name, picture, nip05) = match p {
            Some(p) => (p.display(), p.picture.clone(), p.nip05.clone()),
            None => (content::short_bech32(&hex), String::new(), String::new()),
        };
        let who = *pk;
        div()
            .id(SharedString::from(format!("member-{i}")))
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
            )
            .when(!nip05.is_empty(), |row| {
                row.child(
                    div()
                        .flex_none()
                        .text_size(px(theme::T_SMALL))
                        .text_color(rgb(theme::muted()))
                        .child(nip05),
                )
            })
            .into_any_element()
    }
}

impl Render for ListsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("lists")
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::washi()))
            .child(match self.open_list.clone() {
                None => {
                    let lists = self.lists.clone();
                    if lists.is_empty() {
                        placeholder(
                            "Lists",
                            "Your NIP-51 lists will show here, none in the store yet.",
                        )
                    } else {
                        div()
                            .id("lists-list")
                            .w_full()
                            .h_full()
                            .max_w(px(content::COL_W))
                            .flex()
                            .flex_col()
                            .overflow_y_scroll()
                            .children(
                                lists
                                    .iter()
                                    .enumerate()
                                    .map(|(i, l)| self.list_row(i, l, cx)),
                            )
                            .into_any_element()
                    }
                }
                Some(l) => self.list_detail(&l, cx),
            })
    }
}

impl ListsView {
    /// A list's members: a back row, the list's name + count, the people
    /// (search.rs's person rows — avatars re-fetch on render, evicted
    /// entries come back silently).
    fn list_detail(&mut self, l: &NostrList, cx: &mut Context<Self>) -> AnyElement {
        let media = media::Snapshot::new(cx);
        let viewer = cx.entity_id();
        let mut painted: Vec<String> = Vec::new();
        let mut painted_set = std::collections::HashSet::new();
        for p in self.members.values() {
            media::fetch(&self.core, p.picture.clone(), viewer, cx);
            if !p.picture.is_empty() && painted_set.insert(p.picture.clone()) {
                painted.push(p.picture.clone());
            }
        }
        // Pin the avatars this detail paints: the pane idles open (renders
        // stop, only renders touch), so the next landing burst took the
        // avatars on screen (thread.rs render, the same discipline).
        media::repin(cx.entity_id(), painted, cx);
        let name = l.display();
        let count = l.members.len();
        div()
            .id("list-detail")
            .w_full()
            .h_full()
            .max_w(px(content::COL_W))
            .flex()
            .flex_col()
            .overflow_y_scroll()
            .child(
                div()
                    .id("lists-back")
                    .cursor_pointer()
                    .px_4()
                    .py_2()
                    .text_size(px(theme::T_UI))
                    .text_color(rgb(theme::muted()))
                    .hover(|s| s.text_color(rgb(theme::ink())))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_list = None;
                        cx.notify();
                    }))
                    .child("‹ All lists"),
            )
            .child(
                div()
                    .px_4()
                    .pb_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(theme::T_BODY))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(theme::ink()))
                            .child(name),
                    )
                    .child(
                        div()
                            .text_size(px(theme::T_SMALL))
                            .text_color(rgb(theme::muted()))
                            .child(if count == 1 {
                                "1 person".to_string()
                            } else {
                                format!("{count} people")
                            }),
                    ),
            )
            .when(count == 0, |d| {
                d.child(
                    div()
                        .px_4()
                        .py_4()
                        .text_size(px(theme::T_UI))
                        .text_color(rgb(theme::muted()))
                        .child("Nobody in this list yet."),
                )
            })
            .children(
                l.members
                    .iter()
                    .enumerate()
                    .map(|(i, pk)| self.member_row(i, &media, pk, cx)),
            )
            .into_any_element()
    }
}

/// A quiet empty state.
fn placeholder(title: &'static str, note: &'static str) -> AnyElement {
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
                .text_size(px(theme::T_BODY))
                .text_color(rgb(theme::ink()))
                .child(title),
        )
        .child(
            div()
                .text_size(px(theme::T_SMALL))
                .text_color(rgb(theme::muted()))
                .child(note),
        )
        .into_any_element()
}
