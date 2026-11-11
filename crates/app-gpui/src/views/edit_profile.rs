//! The profile edit form (D38): name / display name / about / website /
//! NIP-05 as input rows, picture and banner through the composer's rfd +
//! blossom-upload path, Save riding `Core::set_profile` (kind 0
//! read-modify-write through commit() — the outbox retries it).
//!
//! Prefill comes from the RAW kind:0 content (`Core::own_metadata`), not
//! the parsed Profile: the parsed struct drops display_name/website, and
//! a form save writes EVERY displayed field — a missing prefill would
//! silently clobber them with empty strings.

use gpui::{
    App, Context, Entity, EventEmitter, Focusable, FontWeight, ObjectFit, Render, SharedString,
    Window, div, img, prelude::*, px, rgb,
};
use nostr::prelude::*;
use satori_core::manager::{Core, ProfileEdit, PublishEvent};

use std::sync::Arc;

use super::nav::Nav;
use crate::{input::Input, media, theme};

/// One labelled input row's geometry: the label column is fixed so the
/// fields align; the editor fills the rest.
const LABEL_W: f32 = 92.0;

pub(crate) struct EditProfileView {
    core: Arc<Core>,
    me: PublicKey,
    name: Entity<Input>,
    display_name: Entity<Input>,
    about: Entity<Input>,
    website: Entity<Input>,
    nip05: Entity<Input>,
    /// Landed blossom urls (uploads in flight read as "uploading…").
    picture: Option<String>,
    banner: Option<String>,
    /// Per-picker feedback ("uploading…", the error line).
    pic_status: String,
    banner_status: String,
    saving: bool,
    status: String,
}

impl EventEmitter<Nav> for EditProfileView {}

fn single_line(cx: &mut App) -> Entity<Input> {
    cx.new(|cx| {
        let mut input = Input::new(cx);
        input.multiline = false;
        input
    })
}

fn prefill(field: &Entity<Input>, text: &str, cx: &mut App) {
    field.update(cx, |i, cx| i.set_text(text, cx));
}

impl EditProfileView {
    pub(crate) fn new(core: Arc<Core>, me: PublicKey, cx: &mut Context<Self>) -> Self {
        crate::theme::watch(cx);
        let name = single_line(cx);
        let display_name = single_line(cx);
        let about = cx.new(Input::new); // multiline (the bio)
        let website = single_line(cx);
        let nip05 = single_line(cx);

        // Raw kind:0 prefill, async (the store read rides the core
        // runtime; the fields land within a tick or two of the pane).
        let mut rx = core.own_metadata();
        cx.spawn(async move |this, cx| {
            if let Some(raw) = rx.recv().await {
                let _ = this.update(cx, |this, cx| {
                    let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
                    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
                    prefill(&this.name, &s("name"), cx);
                    prefill(&this.display_name, &s("display_name"), cx);
                    prefill(&this.about, &s("about"), cx);
                    prefill(&this.website, &s("website"), cx);
                    prefill(&this.nip05, &s("nip05"), cx);
                    this.picture = Some(s("picture")).filter(|u| !u.is_empty());
                    this.banner = Some(s("banner")).filter(|u| !u.is_empty());
                    cx.notify();
                });
            }
        })
        .detach();

        Self {
            core,
            me,
            name,
            display_name,
            about,
            website,
            nip05,
            picture: None,
            banner: None,
            pic_status: String::new(),
            banner_status: String::new(),
            saving: false,
            status: String::new(),
        }
    }

    /// One picture picker: rfd on the background executor (composer.rs's
    /// pattern), the bytes up the blossom ladder, the landed url into the
    /// patch slot (`which`: "picture" | "banner").
    fn pick_image(&mut self, which: &'static str, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        match which {
            "picture" => self.pic_status.clear(),
            _ => self.banner_status.clear(),
        }
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let picked = cx
                .background_executor()
                .spawn(async {
                    rfd::FileDialog::new()
                        .add_filter("Images", &["png", "jpg", "jpeg", "webp", "gif"])
                        .pick_file()
                })
                .await;
            let Some(path) = picked else { return };
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            let content_type = match ext.as_str() {
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "webp" => "image/webp",
                _ => "application/octet-stream",
            };
            let Ok(bytes) = std::fs::read(&path) else {
                let _ = this.update(cx, |this, cx| {
                    match which {
                        "picture" => this.pic_status = "couldn't read that file".into(),
                        _ => this.banner_status = "couldn't read that file".into(),
                    }
                    cx.notify();
                });
                return;
            };
            let _ = this.update(cx, |this, cx| {
                match which {
                    "picture" => this.pic_status = "uploading…".into(),
                    _ => this.banner_status = "uploading…".into(),
                }
                cx.notify();
            });
            let mut rx = core.blossom_upload(bytes, content_type.into());
            while let Some(ev) = rx.recv().await {
                match ev {
                    PublishEvent::Uploaded { url, .. } => {
                        let _ = this.update(cx, |this, cx| {
                            match which {
                                "picture" => {
                                    this.picture = Some(url);
                                    this.pic_status.clear();
                                }
                                _ => {
                                    this.banner = Some(url);
                                    this.banner_status.clear();
                                }
                            }
                            cx.notify();
                        });
                        break;
                    }
                    PublishEvent::Failed { error, .. } => {
                        let _ = this.update(cx, |this, cx| {
                            match which {
                                "picture" => this.pic_status = error,
                                _ => this.banner_status = error,
                            }
                            cx.notify();
                        });
                        break;
                    }
                    _ => {}
                }
            }
        })
        .detach();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        self.saving = true;
        self.status = "saving…".into();
        let patch = ProfileEdit {
            name: Some(self.name.read(cx).content.trim().to_string()),
            display_name: Some(self.display_name.read(cx).content.trim().to_string()),
            about: Some(self.about.read(cx).content.trim().to_string()),
            website: Some(self.website.read(cx).content.trim().to_string()),
            nip05: Some(self.nip05.read(cx).content.trim().to_string()),
            picture: Some(self.picture.clone().unwrap_or_default()),
            banner: Some(self.banner.clone().unwrap_or_default()),
        };
        let mut rx = self.core.set_profile(patch);
        cx.spawn(async move |this, cx| {
            let mut outcome = String::new();
            while let Some(ev) = rx.recv().await {
                match ev {
                    PublishEvent::Committed { .. } => {
                        outcome = String::new(); // saved — pop
                        break;
                    }
                    PublishEvent::Failed { error, .. } => {
                        outcome = error;
                        break;
                    }
                    _ => {}
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.saving = false;
                this.status = outcome;
                if this.status.is_empty() {
                    cx.emit(Nav::Pop);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The field host (settings.rs `field`'s shape): click-to-focus is
    /// explicit — the Input's own hitbox doesn't win focus transfer here.
    fn field_row(
        &self,
        id: &'static str,
        label: &'static str,
        field: &Entity<Input>,
    ) -> impl IntoElement {
        let focus_target = field.clone();
        div()
            .flex()
            .items_start()
            .gap_3()
            .child(
                div()
                    .flex_none()
                    .w(px(LABEL_W))
                    .pt(px(7.))
                    .text_size(px(theme::T_SMALL))
                    .text_color(rgb(theme::muted()))
                    .child(label),
            )
            .child(
                div()
                    .id(SharedString::from(format!("{id}-field")))
                    .cursor_text()
                    .flex_1()
                    .min_w_0()
                    .min_h(px(30.))
                    .px_2()
                    .py_1()
                    .rounded(px(theme::R_EMBED))
                    .bg(rgb(theme::panel()))
                    .border_1()
                    .border_color(rgb(theme::hairline()))
                    .child(field.clone())
                    .on_click(move |_, window, cx| {
                        let fh = focus_target.read(cx).focus_handle(cx);
                        window.focus(&fh, cx);
                    }),
            )
    }

    /// The picture/banner row: a small thumbnail (the landed url, or the
    /// stored one once prefill lands), the picker button, the status line.
    fn image_row(
        &self,
        id: &'static str,
        label: &'static str,
        url: &Option<String>,
        status: &str,
        which: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let snap = media::Snapshot::new(cx);
        let tile: gpui::AnyElement = match url {
            Some(u) if !u.is_empty() => {
                media::fetch(&self.core, u.clone(), cx.entity_id(), cx);
                if let Some(png) = snap.png(u) {
                    div()
                        .size(px(40.))
                        .rounded(px(theme::R_EMBED))
                        .overflow_hidden()
                        .child(img(png).size_full().object_fit(ObjectFit::Cover))
                        .into_any_element()
                } else {
                    div()
                        .size(px(40.))
                        .rounded(px(theme::R_EMBED))
                        .border_1()
                        .border_color(rgb(theme::hairline()))
                        .bg(rgb(theme::panel()))
                        .into_any_element()
                }
            }
            _ => div()
                .size(px(40.))
                .rounded(px(theme::R_EMBED))
                .border_1()
                .border_color(rgb(theme::hairline()))
                .bg(rgb(theme::panel()))
                .into_any_element(),
        };
        drop(snap);
        div()
            .flex()
            .items_center()
            .gap_3()
            .child(
                div()
                    .flex_none()
                    .w(px(LABEL_W))
                    .text_size(px(theme::T_SMALL))
                    .text_color(rgb(theme::muted()))
                    .child(label),
            )
            .child(tile)
            .child(
                div()
                    .id(SharedString::from(format!("{id}-pick")))
                    .cursor_pointer()
                    .px_2()
                    .py_1()
                    .rounded(px(theme::R_EMBED))
                    .border_1()
                    .border_color(rgb(theme::hairline()))
                    .hover(|s| s.border_color(rgb(theme::muted())))
                    .text_size(px(theme::T_SMALL))
                    .on_click(cx.listener(move |this, _, _, cx| this.pick_image(which, cx)))
                    .child("choose image…"),
            )
            .when(!status.is_empty(), |d| {
                d.child(
                    div()
                        .text_size(px(theme::T_SMALL))
                        .text_color(rgb(theme::muted()))
                        .child(status.to_string()),
                )
            })
    }
}

impl Render for EditProfileView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("edit-profile")
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::panel()))
            .child(
                // The header: title + close.
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_4()
                    .py_3()
                    .border_b_1()
                    .border_color(rgb(theme::hairline()))
                    .child(
                        div()
                            .text_size(px(theme::T_READER))
                            .font_weight(FontWeight::MEDIUM)
                            .child("Edit profile"),
                    )
                    .child(
                        div()
                            .id("edit-close")
                            .cursor_pointer()
                            .text_size(px(theme::T_SMALL))
                            .text_color(rgb(theme::muted()))
                            .hover(|s| s.text_color(rgb(theme::ink())))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(Nav::Pop)))
                            .child("close"),
                    ),
            )
            .child(
                div()
                    .id("edit-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .px_4()
                            .py_4()
                            .max_w(px(560.))
                            .mx_auto()
                            .child(self.image_row(
                                "row-picture",
                                "Picture",
                                &self.picture,
                                &self.pic_status,
                                "picture",
                                cx,
                            ))
                            .child(self.image_row(
                                "row-banner",
                                "Banner",
                                &self.banner,
                                &self.banner_status,
                                "banner",
                                cx,
                            ))
                            .child(self.field_row("row-name", "Name", &self.name))
                            .child(self.field_row(
                                "row-display",
                                "Display name",
                                &self.display_name,
                            ))
                            .child(self.field_row("row-about", "About", &self.about))
                            .child(self.field_row("row-website", "Website", &self.website))
                            .child(self.field_row("row-nip05", "NIP-05", &self.nip05))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_3()
                                    .pl(px(LABEL_W + 12.))
                                    .child(
                                        div()
                                            .id("edit-save")
                                            .cursor_pointer()
                                            .px_3()
                                            .py_1()
                                            .rounded(px(theme::R_EMBED))
                                            .border_1()
                                            .border_color(rgb(if self.saving {
                                                theme::hairline()
                                            } else {
                                                theme::vermilion()
                                            }))
                                            .text_color(rgb(if self.saving {
                                                theme::muted()
                                            } else {
                                                theme::vermilion()
                                            }))
                                            .text_size(px(theme::T_SMALL))
                                            .on_click(cx.listener(|this, _, _, cx| this.save(cx)))
                                            .child(if self.saving { "saving…" } else { "Save" }),
                                    )
                                    .when(!self.status.is_empty(), |d| {
                                        d.child(
                                            div()
                                                .text_size(px(theme::T_SMALL))
                                                .text_color(rgb(theme::vermilion()))
                                                .child(self.status.clone()),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .pl(px(LABEL_W + 12.))
                                    .text_size(px(theme::T_SMALL))
                                    .text_color(rgb(theme::muted()))
                                    .child(format!(
                                        "signs as {}…, saves go out through the outbox",
                                        &self.me.to_string()[..8]
                                    )),
                            ),
                    ),
            )
    }
}
