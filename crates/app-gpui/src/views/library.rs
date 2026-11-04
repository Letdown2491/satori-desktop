//! The Library pane (D38): one place for the user's own stuff — Bookmarks,
//! Drafts, Lists. Bookmarks is live this slice: the merged own set
//! (private kind 10003 + public kind 30001 d="bookmarks") as timeline rows,
//! an embedded Feed so the rows ARE the feed's rows. Drafts (synced
//! encrypted, NIP-78) and Lists (NIP-51) land in their own slices.

use gpui::{div, prelude::*, px, rgb, AnyElement, Context, Entity, EventEmitter, Render,
           SharedString, Window};
use nostr::prelude::*;
use std::sync::Arc;
use satori_core::manager::{Core, DraftEntry};

use crate::theme;
use super::{content, feed::Feed, nav::Nav};

pub(crate) struct LibraryView {
    core: Arc<Core>,
    tab: LibTab,
    /// The Bookmarks tab's rows — a full Feed in Bookmarks mode.
    bookmarks: Entity<Feed>,
    /// Mirror of the child feed's deep-scroll state (profile.rs's pattern —
    /// the shell reads the pane view's field for the back-to-top whisper).
    pub(crate) deep: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum LibTab {
    Bookmarks,
    Drafts,
    Lists,
}

impl EventEmitter<Nav> for LibraryView {}

impl LibraryView {
    pub(crate) fn new(core: Arc<Core>, cx: &mut Context<Self>) -> Self {
        crate::theme::watch(cx); // repaint on theme change
        let bookmarks = cx.new(|cx| Feed::bookmarks(core.clone(), cx));
        // Forward the child feed's navs upward (the shell only subscribes
        // to the pane view) and mirror the deep flag.
        cx.subscribe(&bookmarks, |this, feed, ev: &Nav, cx| {
            if matches!(ev, Nav::PaneDeep) {
                this.deep = feed.read(cx).deep;
            }
            cx.emit(ev.clone());
        })
        .detach();
        Self {
            core,
            tab: LibTab::Bookmarks,
            bookmarks,
            deep: false,
        }
    }

    /// Rail re-click / the back-to-top whisper: the active tab's list is
    /// the child feed's (only it scrolls).
    pub fn scroll_top(&mut self, cx: &mut Context<Self>) {
        self.deep = false;
        self.bookmarks.update(cx, |feed, _| feed.scroll_top());
    }

    /// Tap a synced draft (D38): spend the slot now (the composer's close
    /// re-saves whatever the user does with it), resolve the reply/quote
    /// context events best-effort, then open the composer prefilled.
    fn open_draft(&self, d: DraftEntry, cx: &mut Context<Self>) {
        let reply_id = d.reply_to.as_ref().and_then(|h| EventId::from_hex(h).ok());
        let quote_id = d.quote.as_ref().and_then(|h| EventId::from_hex(h).ok());
        let mut rx = self.core.save_draft(reply_id, quote_id, String::new());
        let core = self.core.clone();
        let text = d.text.clone();
        cx.spawn(async move |this, cx| {
            // The slot's spend lands when the publish stream closes — then
            // the tab re-renders without the row.
            while rx.recv().await.is_some() {}
            let _ = this.update(cx, |_this, cx| cx.notify());
            let reply = match reply_id {
                Some(id) => {
                    let mut rx = core.note_by_id(id);
                    rx.recv().await.unwrap_or(None)
                }
                None => None,
            };
            let quote = match quote_id {
                Some(id) => {
                    let mut rx = core.note_by_id(id);
                    rx.recv().await.unwrap_or(None)
                }
                None => None,
            };
            let _ = this.update(cx, |_this, cx| {
                cx.emit(Nav::OpenDraft(text, reply.map(Box::new), quote.map(Box::new)))
            });
        })
        .detach();
    }

    /// Delete a synced draft from the tab (the x): spend the slot; the row
    /// drops when the save stream closes and the tab re-renders from the
    /// fresh list.
    fn delete_draft(&self, d: &DraftEntry, cx: &mut Context<Self>) {
        let reply_id = d.reply_to.as_ref().and_then(|h| EventId::from_hex(h).ok());
        let quote_id = d.quote.as_ref().and_then(|h| EventId::from_hex(h).ok());
        let mut rx = self.core.save_draft(reply_id, quote_id, String::new());
        cx.spawn(async move |this, cx| {
            while rx.recv().await.is_some() {}
            let _ = this.update(cx, |_, cx| cx.notify());
        })
        .detach();
    }

    /// One draft row: context chip, the first line's snippet, the delete x.
    fn draft_row(&self, i: usize, d: &DraftEntry, cx: &mut Context<Self>) -> AnyElement {
        let snippet = d.text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").to_string();
        let label = match (&d.reply_to, &d.quote) {
            (Some(_), _) => "reply",
            (None, Some(_)) => "quote",
            _ => "draft",
        };
        let d_tap = d.clone();
        let d_del = d.clone();
        div()
            .id(SharedString::from(format!("draft-{i}")))
            .w_full()
            .flex()
            .items_center()
            .gap_2()
            .py_1()
            .px_4()
            .border_b_1()
            .border_color(rgb(theme::hairline()))
            .hover(|s| s.bg(rgb(theme::panel())))
            .on_click(cx.listener(move |this, _, _, cx| this.open_draft(d_tap.clone(), cx)))
            .child(
                div()
                    .flex_none()
                    .text_size(px(10.5))
                    .text_color(rgb(theme::vermilion()))
                    .child(label),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(13.))
                    .text_color(rgb(theme::ink()))
                    .text_ellipsis()
                    .child(if snippet.is_empty() {
                        "…".to_string()
                    } else {
                        snippet
                    }),
            )
            .child(
                div()
                    .id(SharedString::from(format!("draft-del-{i}")))
                    .flex_none()
                    .cursor_pointer()
                    .px_2()
                    .text_size(px(11.))
                    .text_color(rgb(theme::muted()))
                    .hover(|s| s.text_color(rgb(theme::err())))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        // The row is tappable too — don't open the draft.
                        cx.stop_propagation();
                        this.delete_draft(&d_del, cx)
                    }))
                    .child("delete"),
            )
            .into_any_element()
    }

    fn tab_btn(
        &self,
        which: LibTab,
        label: &'static str,
        active: LibTab,
        cx: &Context<Self>,
    ) -> AnyElement {
        let on = which == active;
        div()
            .id(SharedString::from(format!("tab-{label}")))
            .cursor_pointer()
            .text_size(px(12.5))
            .text_color(rgb(if on { theme::ink() } else { theme::muted() }))
            .when(on, |d| d.underline())
            .child(label)
            .on_click(cx.listener(move |view, _, _, cx| {
                if view.tab != which {
                    view.tab = which;
                    view.deep = false;
                    cx.notify();
                }
            }))
            .into_any_element()
    }
}

impl Render for LibraryView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tab = self.tab;
        div()
            .id("library")
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::washi()))
            // The tab strip: centered, the profile's rhythm.
            .child(
                div()
                    .bg(rgb(theme::panel()))
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .w(px(content::COL_W))
                            .flex()
                            .gap_4()
                            .px_4()
                            .py_2()
                            .child(self.tab_btn(LibTab::Bookmarks, "Bookmarks", tab, cx))
                            .child(self.tab_btn(LibTab::Drafts, "Drafts", tab, cx))
                            .child(self.tab_btn(LibTab::Lists, "Lists", tab, cx)),
                    ),
            )
            .child(match tab {
                // The Bookmarks tab IS a feed (FeedMode::Bookmarks): the
                // merged own set — private 10003, public 30001 — as the
                // timeline's exact rows, star working as a real toggle.
                LibTab::Bookmarks => div()
                    .w_full()
                    .h_full()
                    .max_w(px(content::COL_W))
                    .flex()
                    .flex_col()
                    .child(self.bookmarks.clone())
                    .into_any_element(),
                // Placeholder slices: the drafts land with the NIP-78 sync,
                // the lists with the NIP-51 pass (D38's queue order).
                LibTab::Drafts => {
                    let drafts = self.core.own_draft_list();
                    if drafts.is_empty() {
                        placeholder(
                            "Drafts",
                            "Drafts you close in the composer sync here, encrypted.",
                        )
                    } else {
                        div()
                            .id("drafts-list")
                            .w_full()
                            .h_full()
                            .max_w(px(content::COL_W))
                            .flex()
                            .flex_col()
                            .overflow_y_scroll()
                            .children(drafts.iter().enumerate().map(|(i, d)| {
                                self.draft_row(i, d, cx)
                            }))
                            .into_any_element()
                    }
                }
                LibTab::Lists => placeholder(
                    "Lists",
                    "Your NIP-51 lists will live here — not yet.",
                ),
            })
    }
}

/// A quiet empty state for tabs whose slice hasn't landed.
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
                .text_size(px(14.))
                .text_color(rgb(theme::ink()))
                .child(title),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(theme::muted()))
                .child(note),
        )
        .into_any_element()
}
