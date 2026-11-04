//! The Library pane (D38): one place for the user's own stuff — Bookmarks,
//! Drafts, Lists. Bookmarks is live this slice: the merged own set
//! (private kind 10003 + public kind 30001 d="bookmarks") as timeline rows,
//! an embedded Feed so the rows ARE the feed's rows. Drafts (synced
//! encrypted, NIP-78) and Lists (NIP-51) land in their own slices.

use gpui::{div, prelude::*, px, rgb, AnyElement, Context, Entity, EventEmitter, Render,
           SharedString, Window};
use std::sync::Arc;
use satori_core::manager::Core;

use crate::theme;
use super::{content, feed::Feed, nav::Nav};

pub(crate) struct LibraryView {
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
                LibTab::Drafts => placeholder(
                    "Drafts",
                    "Composer drafts will sync here, encrypted — not yet.",
                ),
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
