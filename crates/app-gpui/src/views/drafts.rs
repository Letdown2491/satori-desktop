//! The Drafts page (D38's drafts slice, D81 split out of the retired
//! Library pane): the synced encrypted drafts (NIP-78) as rows — a tap
//! spends the entry into the composer, the x deletes it.

use gpui::{
    AnyElement, Context, EventEmitter, Render, SharedString, Window, div, prelude::*, px, rgb,
};
use nostr::prelude::*;
use satori_core::manager::{Core, DraftEntry, PublishEvent};
use std::sync::Arc;

use super::{content, nav::Nav};
use crate::theme;

pub(crate) struct DraftsView {
    core: Arc<Core>,
}

impl EventEmitter<Nav> for DraftsView {}

impl DraftsView {
    pub(crate) fn new(core: Arc<Core>, cx: &mut Context<Self>) -> Self {
        crate::theme::watch(cx); // repaint on theme change
        Self { core }
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
        // composer is up either way; this keeps the page honest).
        let mut rx = match d.id {
            Some(id) => self.core.drop_draft(id),
            None => self
                .core
                .save_draft(reply_id, quote_id, String::new(), Vec::new(), false),
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

    /// Delete a synced draft from the page (the x): the spend is
    /// optimistic — the row drops on the notify — and a failed publish
    /// (entry restored) re-renders it back via the drain.
    fn delete_draft(&self, d: &DraftEntry, cx: &mut Context<Self>) {
        let reply_id = d.reply_to.as_ref().and_then(|h| EventId::from_hex(h).ok());
        let quote_id = d.quote.as_ref().and_then(|h| EventId::from_hex(h).ok());
        let mut rx = match d.id.clone() {
            Some(id) => self.core.drop_draft(id),
            None => self
                .core
                .save_draft(reply_id, quote_id, String::new(), Vec::new(), false),
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
        let snippet = d
            .text
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .to_string();
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
}

impl Render for DraftsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("drafts")
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::washi()))
            .child({
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
                        .children(
                            drafts
                                .iter()
                                .enumerate()
                                .map(|(i, d)| self.draft_row(i, d, cx)),
                        )
                        .into_any_element()
                }
            })
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
