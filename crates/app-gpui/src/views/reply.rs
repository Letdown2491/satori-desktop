//! The reply sheet (#21): a floating card over the current pane —
//! replying never costs you your place. The action row's bubble opens it
//! (shell.rs overlay layer, like the zap sheet); the thread view keeps
//! its own inline box at the bottom where the conversation lives. Tag
//! building is core's (manager.rs note_tags / nip::comment reply_tags):
//! the sheet is only the text box and the publish pump.

use std::sync::Arc;

use gpui::{
    Context, Entity, EventEmitter, Focusable, FontWeight, KeyDownEvent, Render, Window, div,
    prelude::*, px, rgb,
};
use nostr::prelude::*;
use satori_core::manager::{Core, PublishEvent, PublishRx};

use crate::{input::Input, theme, views::nav::Nav};

pub struct ReplySheet {
    core: Arc<Core>,
    parent: Event,
    input: Entity<Input>,
    focus: gpui::FocusHandle,
    autofocus: bool,
    posting: bool,
    status: String,
}
impl Focusable for ReplySheet {
    fn focus_handle(&self, _cx: &gpui::App) -> gpui::FocusHandle {
        self.focus.clone()
    }
}

impl EventEmitter<Nav> for ReplySheet {}

impl ReplySheet {
    pub fn new(core: Arc<Core>, parent: Event, cx: &mut Context<Self>) -> Self {
        crate::theme::watch(cx);
        let input = cx.new(|cx| Input::new(cx).placeholder("Write a reply…"));
        Self {
            core,
            parent,
            input,
            focus: cx.focus_handle(),
            autofocus: false,
            posting: false,
            status: String::new(),
        }
    }

    fn who(&self) -> String {
        self.core
            .profile_of(&self.parent.pubkey)
            .map(|p| p.display())
            .unwrap_or_else(|| format!("{}…", &self.parent.pubkey.to_string()[..8]))
    }

    /// The parent's first line, whisper-trimmed: enough to recognize
    /// WHAT you're replying to, never the whole note.
    fn snippet(&self) -> String {
        let s: String = self
            .parent
            .content
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let mut snippet: String = s.chars().take(120).collect();
        if snippet.len() < s.len() {
            snippet.push('…');
        }
        snippet
    }

    /// Publish: kind-1 roots keep kind-1 replies + NIP-10 tags (every
    /// client reads those); every other root goes out as a NIP-22
    /// kind-1111 comment (the composer's Amethyst migration rule).
    /// Parent/root author p-tags are core's job (note_tags) — the sheet
    /// carries no tag logic.
    fn publish(&mut self, cx: &mut Context<Self>) {
        if self.posting {
            return;
        }
        let text = self.input.read(cx).content.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.posting = true;
        self.status.clear();
        cx.notify();
        let core = self.core.clone();
        let parent = self.parent.clone();
        let rx = if u16::from(parent.kind) != 1 {
            core.comment(
                text,
                satori_core::nip::comment::CommentTarget::for_event(&parent),
                Some(parent),
                Vec::new(),
                Vec::new(),
            )
        } else {
            core.post_note(text, Some(parent), Vec::new(), None, Vec::new())
        };
        cx.spawn(async move |this, cx| pump(this, rx, cx).await)
            .detach();
    }
}

/// The publish pump (composer.rs's shape): Held/Committed pop the sheet
/// (the undo toast floats in the shell off core.subscribe_hold, whatever
/// pane published); Failed sticks as the sheet's status line.
async fn pump(this: gpui::WeakEntity<ReplySheet>, mut rx: PublishRx, cx: &mut gpui::AsyncApp) {
    while let Some(ev) = rx.recv().await {
        match ev {
            PublishEvent::Held { .. } | PublishEvent::Committed { .. } => {
                let _ = this.update(cx, |_this, cx| cx.emit(Nav::Pop));
                return;
            }
            PublishEvent::Failed { error, .. } => {
                let _ = this.update(cx, |this, cx| {
                    this.posting = false;
                    this.status = format!("failed: {error}");
                    cx.notify();
                });
                return;
            }
            _ => {}
        }
    }
}

impl Render for ReplySheet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.autofocus {
            self.autofocus = true;
            window.focus(&self.input.read(cx).focus_handle(cx), cx);
        }
        let who = self.who();
        let snippet = self.snippet();
        let posting = self.posting;
        let status = self.status.clone();

        div()
            .id("reply-sheet")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                // Ctrl+Enter fires (Enter stays a newline, the composer's
                // convention — replies run multi-line too).
                if ev.keystroke.key == "enter" && ev.keystroke.modifiers.control {
                    this.publish(cx);
                }
            }))
            .px_4()
            .py_3()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_size(px(theme::T_UI))
                    .text_color(rgb(theme::ink()))
                    .child(format!("Reply to {who}")),
            )
            .child(
                div()
                    .text_size(px(theme::T_SMALL))
                    .text_color(rgb(theme::muted()))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(snippet),
            )
            .child(
                // The text field: definite height so the Input fills it
                // (login.rs field()'s rule — a content-sized chain
                // collapses it).
                div()
                    .id("reply-field")
                    .min_w_0()
                    .h(px(64.))
                    .overflow_y_scroll()
                    .px_2()
                    .py_1()
                    .rounded(px(theme::R_EMBED))
                    .bg(rgb(theme::panel()))
                    .border_1()
                    .border_color(rgb(theme::hairline()))
                    .cursor(gpui::CursorStyle::IBeam)
                    .on_click(cx.listener(|this, _, window, cx| {
                        let focus = this.input.read(cx).focus_handle(cx);
                        window.focus(&focus, cx);
                    }))
                    .child(self.input.clone()),
            )
            .child(
                div().flex().items_center().justify_end().gap_2().child(
                    div()
                        .id("reply-send")
                        .cursor_pointer()
                        .px_3()
                        .py_1()
                        .rounded(px(theme::R_EMBED))
                        .bg(rgb(theme::vermilion()))
                        .text_size(px(theme::T_UI))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::washi()))
                        .on_click(cx.listener(|this, _, _, cx| this.publish(cx)))
                        .child(if posting { "…" } else { "Reply" }),
                ),
            )
            .when(!status.is_empty(), |d| {
                d.child(
                    div()
                        .text_size(px(theme::T_SMALL))
                        .text_color(rgb(theme::muted()))
                        .child(status),
                )
            })
    }
}
