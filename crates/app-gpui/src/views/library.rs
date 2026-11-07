//! The Library pane (D38): one place for the user's own stuff — Bookmarks,
//! Drafts, Lists. Bookmarks is live this slice: the merged own set
//! (private kind 10003 + public kind 30001 d="bookmarks") as timeline rows,
//! an embedded Feed so the rows ARE the feed's rows. Drafts (synced
//! encrypted, NIP-78) and Lists (NIP-51) land in their own slices.

use gpui::{div, prelude::*, px, rgb, AnyElement, Context, Entity, EventEmitter, FontWeight,
           Render, SharedString, Window};
use nostr::prelude::*;
use std::{collections::HashMap, sync::Arc};
use satori_core::manager::{Core, DraftEntry, FeedEvent, FeedRx, NostrList, Profile, PublishEvent};

use crate::{media, theme};
use super::{content, feed::Feed, nav::Nav};

pub(crate) struct LibraryView {
    core: Arc<Core>,
    tab: LibTab,
    /// The Bookmarks tab's rows — a full Feed in Bookmarks mode.
    bookmarks: Entity<Feed>,
    /// The Lists tab (D38): the own NIP-51 lists, re-queried on every
    /// tab visit (the fresh result replaces whatever showed).
    lists: Vec<NostrList>,
    /// The open list's detail (in-tab second level): the list + its
    /// members' resolved profiles.
    open_list: Option<NostrList>,
    members: HashMap<String, Profile>,
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
            lists: Vec::new(),
            open_list: None,
            members: HashMap::new(),
            deep: false,
        }
    }

    /// Rail re-click / the back-to-top whisper: the active tab's list is
    /// the child feed's (only it scrolls).
    pub fn scroll_top(&mut self, cx: &mut Context<Self>) {
        self.deep = false;
        self.bookmarks.update(cx, |feed, _| feed.scroll_top());
    }

    /// A Lists tab visit (D38 v1.2: EVERY visit, not just the first —
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
                            this.members.insert(p.pubkey.clone(), p);
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
                                        this.members.insert(p.pubkey.clone(), p);
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
        let p = self.members.get(&hex);
        let (name, picture, nip05) = match p {
            Some(p) => (
                p.display(),
                p.picture.clone(),
                p.nip05.clone(),
            ),
            None => (hex.clone(), String::new(), String::new()),
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

    /// Tap a synced draft (D38): spend the slot (an imported id-marked
    /// entry instead drops its NIP-37 wrap — the conversion moment,
    /// v1.1), resolve the reply/quote context events best-effort, then
    /// open the composer prefilled. The spend is OPTIMISTIC (the core's
    /// in-memory array updates synchronously) — only the context reads
    /// stand between the tap and the composer; the publishes ride
    /// behind, a failed one restores the entry (the drain re-renders).
    fn open_draft(&self, d: DraftEntry, cx: &mut Context<Self>) {
        let reply_id = d.reply_to.as_ref().and_then(|h| EventId::from_hex(h).ok());
        let quote_id = d.quote.as_ref().and_then(|h| EventId::from_hex(h).ok());
        let core = self.core.clone();
        let text = d.text.clone();
        let attachments = d.attachments.clone();
        cx.notify();
        // A failed publish restored the entry — the row comes back (the
        // composer is up either way; this keeps the tab honest).
        let mut rx = match d.id {
            Some(id) => self.core.drop_draft(id),
            None => self.core.save_draft(reply_id, quote_id, String::new(), Vec::new()),
        };
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.recv().await {
                if matches!(ev, PublishEvent::Failed { .. }) {
                    let _ = this.update(cx, |_, cx| cx.notify());
                }
            }
        })
        .detach();
        // The context reads are local-store fast; the composer opens as
        // soon as they land (the publishes never block it).
        cx.spawn(async move |this, cx| {
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
                cx.emit(Nav::OpenDraft(
                    text,
                    attachments.clone(),
                    reply.map(Box::new),
                    quote.map(Box::new),
                ))
            });
        })
        .detach();
    }

    /// Delete a synced draft from the tab (the x): the spend is
    /// optimistic — the row drops on the notify — and a failed publish
    /// (entry restored) re-renders it back via the drain.
    fn delete_draft(&self, d: &DraftEntry, cx: &mut Context<Self>) {
        let reply_id = d.reply_to.as_ref().and_then(|h| EventId::from_hex(h).ok());
        let quote_id = d.quote.as_ref().and_then(|h| EventId::from_hex(h).ok());
        let mut rx = match d.id.clone() {
            Some(id) => self.core.drop_draft(id),
            None => self.core.save_draft(reply_id, quote_id, String::new(), Vec::new()),
        };
        cx.notify();
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.recv().await {
                if matches!(ev, PublishEvent::Failed { .. }) {
                    let _ = this.update(cx, |_, cx| cx.notify());
                }
            }
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
                    .text_size(px(theme::T_SMALL))
                    .text_color(rgb(theme::vermilion()))
                    .child(label),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(theme::T_UI))
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
                    .text_size(px(theme::T_SMALL))
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
            .text_size(px(theme::T_UI))
            .text_color(rgb(if on { theme::ink() } else { theme::muted() }))
            .when(on, |d| d.underline())
            .child(label)
            .on_click(cx.listener(move |view, _, _, cx| {
                if view.tab != which {
                    view.tab = which;
                    view.deep = false;
                    // Leaving the detail behind — tabs are flat surfaces.
                    view.open_list = None;
                    // Every Lists visit re-queries (see load_lists).
                    if which == LibTab::Lists {
                        view.load_lists(cx);
                    }
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
                    // Newest first: the wrap's created_at (imports, v1.1)
                    // or the last save, whichever the entry carries.
                    let mut drafts = self.core.own_draft_list();
                    drafts.sort_by_key(|d| std::cmp::Reverse(d.at));
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
                // The Lists tab (D38): the own NIP-51 named lists
                // (kind 30000), read-only — the detail is an in-tab second
                // level, the member rows are profile links (search.rs's).
                LibTab::Lists => match self.open_list.clone() {
                    None => {
                        let lists = self.lists.clone();
                        if lists.is_empty() {
                            placeholder(
                                "Lists",
                                "Your NIP-51 lists will show here — none in the store yet.",
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
                },
            })
    }
}

impl LibraryView {
    /// A list's members: a back row, the list's name + count, the people
    /// (search.rs's person rows — avatars re-fetch on render, evicted
    /// entries come back silently).
    fn list_detail(&mut self, l: &NostrList, cx: &mut Context<Self>) -> AnyElement {
        let media = media::Snapshot::new(cx);
        let viewer = cx.entity_id();
        for p in self.members.values() {
            media::fetch(&self.core, p.picture.clone(), viewer, cx);
        }
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
