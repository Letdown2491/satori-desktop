//! The composer: text, multi-image attach, undo window, draft persistence.
//!
//! TODO(port): crates/app/src/composer.rs (Adw dialog) — P2. Same write path:
//! attachments upload through `blossom_upload` one by one, then the note goes
//! out via `post_note` (text/reply) or `post_picture` (kind 20 with imetas),
//! and the undo window surfaces as an inline row while Held. Draft semantics
//! match the GTK composer: a top-level compose restores whatever was stashed,
//! and closing with un-published text stashes it again (replies get neither).

use std::sync::Arc;

use gpui::{
    actions, div, img, prelude::*, px, rgb, App, Context, Entity, EventEmitter, FocusHandle, Focusable,
    Global, Render, SharedString, Window,
};
use nostr::prelude::*;
use satori_core::manager::{Core, Profile, PublishEvent};

use crate::{input::Input, theme};
use super::nav::Nav;

actions!(composer, [Publish]);

/// `ctrl-enter` publishes — the web composer's keybinding (composer.rs:307).
/// Bound on the "Input" context, where the composer's focus lives; the action
/// bubbles up the focus path to this view.
pub fn bind_composer_keys(cx: &mut gpui::App) {
    cx.bind_keys([gpui::KeyBinding::new("ctrl-enter", Publish, Some("Input"))]);
}

/// The stashed top-level draft (composer.rs's DRAFT thread-local, session
/// scope like the web app's note.ts saveDraft — one draft at a time).
#[derive(Default)]
pub struct Draft(pub Option<String>);
impl Global for Draft {}

fn draft_take(cx: &mut App) -> Option<String> {
    cx.default_global::<Draft>().0.take()
}

struct Attachment {
    name: String,
    content_type: String,
    bytes: Vec<u8>,
    /// The tile preview: the ORIGINAL encoded bytes handed to gpui (it
    /// decodes on its own asset thread) — the note's gallery, live.
    image: Option<gpui::Image>,
}

pub struct Composer {
    core: Arc<Core>,
    input: Entity<Input>,
    /// @-mentions: the pubkeys whose p tags ride the publish (content
    /// keeps the human-readable @Name).
    mentions: Vec<PublicKey>,
    /// The live @-suggestions (strip open when non-empty) + the token
    /// being completed.
    suggest: Vec<Profile>,
    mention_token: Option<String>,
    reply_to: Option<Event>,
    attachments: Vec<Attachment>,
    posting: bool,
    /// (undo token, hold_secs) while the signed note waits out its window.
    undo: Option<(u64, u64)>,
    status: String,
    autofocused: bool,
    focus: FocusHandle,
}

impl EventEmitter<Nav> for Composer {}

impl Composer {
    pub fn new(core: Arc<Core>, reply_to: Option<Event>, cx: &mut Context<Self>) -> Self {
        crate::theme::watch(cx); // repaint on theme change
        let input = cx.new(|cx| {
            let i = Input::new(cx);
            if reply_to.is_some() {
                i.placeholder("Write a reply — ctrl-enter sends it.")
            } else {
                i.placeholder("Speak. The post holds a few seconds — ctrl-enter sends it.")
            }
        });
        let focus = input.read(cx).focus_handle(cx);
        // Draft restore: a top-level compose reopens with whatever was typed
        // and closed before (composer.rs:114 — the take spends the stash).
        if reply_to.is_none()
            && let Some(text) = draft_take(cx).filter(|t| !t.trim().is_empty())
        {
            input.update(cx, |i, cx| i.set_text(&text, cx));
        }
        // @-mention detection: watch the buffer, find an active `@token`.
        cx.observe(&input, |this, _input, cx| this.detect_mention(cx))
            .detach();
        Self {
            core,
            input,
            mentions: vec![],
            suggest: vec![],
            mention_token: None,
            reply_to,
            attachments: vec![],
            posting: false,
            undo: None,
            status: String::new(),
            autofocused: false,
            focus,
        }
    }

    fn pick_attachments(&mut self, cx: &mut Context<Self>) {
        if self.posting {
            return;
        }
        self.status = "choosing files…".into();
        cx.spawn(async move |this, cx| {
            // Blocking portal dialog on the background executor; one thread
            // parked while the user picks is fine.
            let picked = cx
                .background_executor()
                .spawn(async move {
                    rfd::FileDialog::new()
                        .add_filter("Images", &["png", "jpg", "jpeg", "webp", "gif"])
                        .pick_files()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.status.clear();
                for path in picked.into_iter().flatten() {
                    let ext = path
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_lowercase();
                    let content_type = match ext.as_str() {
                        "png" => "image/png",
                        "jpg" | "jpeg" => "image/jpeg",
                        "webp" => "image/webp",
                        "gif" => "image/gif",
                        _ => continue,
                    };
                    match std::fs::read(&path) {
                        Ok(bytes) => {
                            let format = match content_type {
                                "image/png" => gpui::ImageFormat::Png,
                                "image/jpeg" => gpui::ImageFormat::Jpeg,
                                "image/webp" => gpui::ImageFormat::Webp,
                                _ => gpui::ImageFormat::Gif,
                            };
                            let image = gpui::Image::from_bytes(format, bytes.clone());
                            this.attachments.push(Attachment {
                                name: path
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_default(),
                                content_type: content_type.into(),
                                bytes,
                                image: Some(image),
                            })
                        }
                        Err(e) => this.status = format!("couldn't read {}: {e}", path.display()),
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The @-token under the caret, if any: `@handle` with no whitespace
    /// between the @ and the cursor.
    fn mention_under_caret(&self, cx: &mut Context<Self>) -> Option<String> {
        let i = self.input.read(cx);
        let text = &i.content;
        let cursor = i.cursor_offset().min(text.len());
        let before = &text[..cursor];
        let at = before.rfind('@')?;
        let token = &before[at + 1..];
        (token.len() <= 30 && !token.contains(char::is_whitespace))
            .then(|| token.to_string())
    }

    fn detect_mention(&mut self, cx: &mut Context<Self>) {
        let token = self.mention_under_caret(cx);
        match &token {
            Some(q) if !q.is_empty() => {
                let candidates = self.core.mention_candidates(q);
                if candidates.is_empty() {
                    self.mention_token = None;
                    self.suggest.clear();
                } else {
                    self.mention_token = Some(q.clone());
                    self.suggest = candidates;
                }
            }
            _ => {
                self.mention_token = None;
                self.suggest.clear();
            }
        }
        cx.notify();
    }

    /// A suggestion was picked: replace the @token in the buffer with the
    /// chosen name, remember the pubkey for the p tags.
    fn pick_mention(&mut self, idx: usize, cx: &mut Context<Self>) {
        let Some(profile) = self.suggest.get(idx).cloned() else {
            return;
        };
        if let Some(q) = self.mention_token.clone() {
            let i = self.input.read(cx);
            let text = i.content.clone();
            let cursor = i.cursor_offset().min(text.len());
            let needle = format!("@{q}");
            let at = text[..cursor].rfind(&needle).unwrap_or(cursor);
            let mut next = String::with_capacity(text.len());
            next.push_str(&text[..at]);
            next.push_str(&format!("@{} ", profile.display()));
            next.push_str(&text[cursor..]);
            drop(i);
            self.input.update(cx, |i, cx| i.set_text(&next, cx));
        }
        let pk_hex = profile.pubkey.clone();
        if let Ok(pk) = PublicKey::from_hex(&pk_hex)
            && !self.mentions.contains(&pk)
        {
            self.mentions.push(pk);
        }
        self.suggest.clear();
        self.mention_token = None;
        cx.notify();
    }

    fn post(&mut self, cx: &mut Context<Self>) {
        if self.posting {
            return;
        }
        let text = self.input.read(cx).content.trim().to_string();
        if text.is_empty() && self.attachments.is_empty() {
            self.status = "nothing to say yet".into();
            cx.notify();
            return;
        }
        self.posting = true;
        self.status.clear();
        let reply = self.reply_to.clone();
        // A top-level publish consumes the buffer: the draft is spent
        // (composer.rs:476). If the send fails, `draft_text` re-stashes on
        // close.
        if reply.is_none() {
            cx.default_global::<Draft>().0 = None;
        }
        let core = self.core.clone();
        let this_mentions = std::mem::take(&mut self.mentions);
        let atts: Vec<(String, Vec<u8>)> = self
            .attachments
            .iter()
            .map(|a| (a.content_type.clone(), a.bytes.clone()))
            .collect();
        cx.notify();

        cx.spawn(async move |this, cx| {
            // Upload chain: one blob at a time, collecting imeta tags.
            let mut imetas: Vec<Vec<String>> = vec![];
            for (content_type, bytes) in &atts {
                let mut rx = core.blossom_upload(bytes.clone(), content_type.clone());
                let mut uploaded = false;
                while let Some(ev) = rx.recv().await {
                    match ev {
                        PublishEvent::Uploaded { imeta, .. } => {
                            imetas.push(imeta);
                            uploaded = true;
                            break;
                        }
                        PublishEvent::Failed { error, .. } => {
                            let _ = this.update(cx, |this, cx| {
                                this.posting = false;
                                this.status = format!("upload failed: {error}");
                                cx.notify();
                            });
                            return;
                        }
                        _ => {}
                    }
                }
                if !uploaded {
                    let _ = this.update(cx, |this, cx| {
                        this.posting = false;
                        this.status = "upload cut short".into();
                        cx.notify();
                    });
                    return;
                }
            }

            // Publish: pictures when there are imetas, notes otherwise.
            // Reply routing follows Amethyst's migration rule: kind-1 roots
            // keep kind-1 replies + NIP-10 tags (every client reads those);
            // every other root (articles, pictures, highlights…) goes out as
            // a NIP-22 kind-1111 comment with scope tags.
            let mentions = this_mentions;
            let mut rx = if !imetas.is_empty() {
                core.post_picture(String::new(), text, imetas, None)
            } else if reply.as_ref().is_some_and(|r| u16::from(r.kind) != 1) {
                let r = reply.expect("checked");
                core.comment(
                    text,
                    satori_core::nip::comment::CommentTarget::for_event(&r),
                    Some(r),
                    mentions,
                )
            } else {
                core.post_note(text, reply, mentions)
            };
            while let Some(ev) = rx.recv().await {
                match ev {
                    PublishEvent::Held { token, hold_secs, .. } => {
                        let _ = this.update(cx, |this, cx| {
                            this.undo = Some((token, hold_secs));
                            cx.notify();
                        });
                    }
                    PublishEvent::Undone { .. } => {
                        let _ = this.update(cx, |this, cx| {
                            // Kept on the stack with a quiet word — the draft
                            // is still there if the user wants it.
                            this.undo = None;
                            this.posting = false;
                            this.status = "undone — nothing was sent".into();
                            cx.notify();
                        });
                        return;
                    }
                    PublishEvent::Committed { .. } => {
                        let _ = this.update(cx, |_this, cx| cx.emit(Nav::Pop));
                        return;
                    }
                    PublishEvent::Failed { error, .. } => {
                        let _ = this.update(cx, |this, cx| {
                            this.posting = false;
                            this.undo = None;
                            this.status = format!("failed: {error}");
                            cx.notify();
                        });
                        return;
                    }
                    _ => {}
                }
            }
        })
        .detach();
    }

    fn undo_now(&mut self, cx: &mut Context<Self>) {
        if let Some((token, _)) = self.undo {
            self.core.undo(token);
        }
        cx.notify();
    }

    /// Stash candidate, GTK close-handler semantics (composer.rs:509): the
    /// whole buffer of a top-level compose that never sent — possibly empty,
    /// so closing empty clears a stale draft. None = don't touch the draft
    /// (replies, a send in flight, an undo window pending).
    pub fn draft_candidate(&self, cx: &App) -> Option<String> {
        if self.reply_to.is_some() || self.posting || self.undo.is_some() {
            return None;
        }
        Some(self.input.read(cx).content.trim().to_string())
    }
}

impl Focusable for Composer {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.autofocused {
            self.autofocused = true;
            window.focus(&self.focus);
        }
        let replying = self.reply_to.is_some();
        let status = self.status.clone();
        let undo = self.undo.map(|(_, secs)| secs);
        let input = self.input.clone();
        let posting = self.posting;
        let suggest = self.suggest.clone();

        div()
            .id("composer")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Publish, _, cx| this.post(cx)))
            .on_click(cx.listener(|this, _, window, _cx| {
                // Clicking anywhere in the pane hands the caret to the field.
                window.focus(&this.focus);
            }))
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .flex()
            .flex_col()
            .font_family(theme::SANS)
            .text_color(rgb(theme::ink()))
            .gap_2()
            .p_4()
            .child(
                div()
                    .id("composer-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .rounded(px(6.))
                    .border_1()
                    .border_color(rgb(theme::hairline()))
                    .bg(rgb(theme::panel()))
                    .p_2()
                    .child(input),
            )
            .when(!suggest.is_empty(), |d| {
                d.child(
                    div()
                        .w_full()
                        .flex()
                        .flex_wrap()
                        .gap_1()
                        .children(suggest.iter().enumerate().map(|(i, p)| {
                            div()
                                .id(SharedString::from(format!("mention-{i}")))
                                .cursor_pointer()
                                .px_2()
                                .py_0p5()
                                .rounded(px(3.))
                                .border_1()
                                .border_color(rgb(theme::hairline()))
                                .text_size(px(12.))
                                .text_color(rgb(theme::ink()))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.pick_mention(i, cx);
                                }))
                                .child(format!("@{}", p.display()))
                        })),
                )
            })
            .child(self.attachment_row(cx))
            .child(match (undo, posting) {
                (Some(secs), _) => div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .text_size(px(12.5))
                    .text_color(rgb(theme::muted()))
                    .child(format!("publishing in {secs}s"))
                    .child(
                        div()
                            .id("undo")
                            .cursor_pointer()
                            .px_3()
                            .py_1()
                            .rounded(px(4.))
                            .border_1()
                            .border_color(rgb(theme::hairline()))
                            .on_click(cx.listener(|this, _, _, cx| this.undo_now(cx)))
                            .child("Undo"),
                    ),
                _ => div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .id("attach")
                            .cursor_pointer()
                            .px_3()
                            .py_1()
                            .text_size(px(13.))
                            .text_color(rgb(theme::muted()))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.pick_attachments(cx);
                            }))
                            .child("+ attach images"),
                    )
                    .child(
                        div()
                            .id("send")
                            .cursor_pointer()
                            .px_4()
                            .py_1()
                            .rounded(px(4.))
                            .border_1()
                            .border_color(rgb(theme::vermilion()))
                            .text_size(px(13.5))
                            .text_color(rgb(theme::vermilion()))
                            .on_click(cx.listener(|this, _, _, cx| this.post(cx)))
                            .child(if replying { "Reply" } else { "Publish" }),
                    ),
            })
            .when(!status.is_empty(), |d| {
                d.child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(theme::muted()))
                        .child(status),
                )
            })
    }
}

impl Composer {
    fn attachment_row(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let tiles = self.attachments.len();
        div()
            .w_full()
            .flex()
            .flex_wrap()
            .gap_2()
            .children(self.attachments.iter().enumerate().map(|(i, a)| {
                let remove = cx.listener(move |this, _, _, cx| {
                    if !this.posting {
                        this.attachments.remove(i);
                        cx.notify();
                    }
                });
                div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .child(
                        div()
                            .size(px(72.))
                            .rounded(px(4.))
                            .border_1()
                            .border_color(rgb(theme::hairline()))
                            .overflow_hidden()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(rgb(theme::panel()))
                            .child(match &a.image {
                                Some(image) => img(std::sync::Arc::new(image.clone()))
                                    .size_full()
                                    .object_fit(gpui::ObjectFit::Cover)
                                    .into_any_element(),
                                None => div()
                                    .text_size(px(11.))
                                    .text_color(rgb(theme::muted()))
                                    .child("…")
                                    .into_any_element(),
                            }),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("rm-att-{i}")))
                            .cursor_pointer()
                            .text_size(px(11.))
                            .text_color(rgb(theme::muted()))
                            .on_click(remove)
                            .when(i < tiles, |d| {
                                d.child(format!("× {}", truncate_name(&a.name)))
                            }),
                    )
            }))
    }
}

/// Trim a file name to a tile label without splitting a glyph.
fn truncate_name(name: &str) -> String {
    if name.chars().count() <= 18 {
        return name.to_string();
    }
    let cut: String = name.chars().take(15).collect();
    format!("{cut}…")
}
