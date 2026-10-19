//! Login — port of crates/app/src/login.rs (the /login page, minus NIP-07),
//! re-doored per D17: the app never handles the master key. Two doors —
//! bunker:// (paste the URI your signer shows) and nostrconnect:// (the app
//! advertises a QR; the signer scans and approves). The waiting state
//! breathes the ensō at the web app's 3.4s period — or shows the QR when a
//! nostrconnect handshake is live. Success swaps the window root to the
//! shell (main.rs Root).

use std::sync::Arc;

use gpui::{
    div, img, prelude::*, px, rgb, Animation, AnimationExt, App, Context, Entity, EventEmitter,
    FocusHandle, Focusable, Image, ImageFormat, KeyDownEvent, Render, TextAlign, Window,
};
use satori_core::manager::{ConnectEvent, ConnectRx, Core};
use std::time::Duration;

use crate::{icons, input::Input, theme};

/// Login outcome — the root drops the login view and mounts the shell.
pub enum Login {
    Approved,
}

pub struct LoginView {
    core: Arc<Core>,
    bunker: Entity<Input>,
    focus: FocusHandle,
    /// Form, or the waiting page with its status + hint.
    waiting: Option<(String, String)>,
    /// Live nostrconnect handshake: the advertised URI + its QR (rendered
    /// once at Uri, not per frame).
    qr_uri: Option<String>,
    qr: Option<Arc<Image>>,
    /// Click-the-URI-to-copy feedback (brief "Copied" swap).
    copied: bool,
    /// NIP-46 out-of-band approval link (login.rs AuthUrl — GTK printed it
    /// to stderr behind a TODO; here it's a clickable chip).
    auth_url: Option<String>,
    error: String,
    /// Each connect run invalidates the previous pump (same seam as search).
    generation: u64,
    autofocused: bool,
}

impl EventEmitter<Login> for LoginView {}

impl Focusable for LoginView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl LoginView {
    pub fn new(core: Arc<Core>, resume_error: Option<String>, cx: &mut Context<Self>) -> Self {
        let bunker = cx.new(|cx| {
            let mut i = Input::new(cx);
            i.multiline = false; // Enter submits, no newline
            i.placeholder("bunker://…")
        });
        let focus = bunker.read(cx).focus_handle(cx);
        let mut view = Self {
            core,
            bunker,
            focus,
            waiting: None,
            qr_uri: None,
            qr: None,
            copied: false,
            auth_url: None,
            error: resume_error.unwrap_or_default(),
            generation: 0,
            autofocused: false,
        };
        view.start_nostrconnect(cx);
        view
    }

    /// The nostrconnect handshake runs from the moment the doors show —
    /// the QR is form furniture, not a flow you start. A fresh URI per
    /// LoginView (each resume/reject regenerates one).
    fn start_nostrconnect(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.qr_uri = None;
        self.qr = None;
        let rx = self.core.connect_nostrconnect("wss://relay.nip46.com".into());
        let mut rx = rx;
        eprintln!("[login] nostrconnect pump armed");
        cx.spawn(async move |this, cx| {
            eprintln!("[login] nostrconnect pump first poll");
            while let Some(ev) = rx.recv().await {
                eprintln!("[login] pump got event");
                let again = this
                    .update(cx, |this, cx| this.on_event(ev, generation, cx))
                    .is_ok_and(|more| more);
                if !again {
                    break;
                }
            }
        })
        .detach();
    }

    fn begin(
        &mut self,
        status: &str,
        hint: &str,
        rx: ConnectRx,
        cx: &mut Context<Self>,
    ) {
        self.generation += 1;
        let generation = self.generation;
        self.error.clear();
        self.auth_url = None;
        self.qr_uri = None;
        self.qr = None;
        self.waiting = Some((status.into(), hint.into()));
        let mut rx = rx;
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.recv().await {
                let again = this
                    .update(cx, |this, cx| this.on_event(ev, generation, cx))
                    .is_ok_and(|more| more);
                if !again {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    /// Returns false when the flow is over (approved, failed or superseded).
    fn on_event(&mut self, ev: ConnectEvent, generation: u64, cx: &mut Context<Self>) -> bool {
        if generation != self.generation {
            return false; // superseded by a newer attempt or a cancel
        }
        match ev {
            ConnectEvent::Waiting => {}
            ConnectEvent::AuthUrl(url) => self.auth_url = Some(url),
            ConnectEvent::Uri(uri) => {
                self.qr = qr_image(&uri);
                self.qr_uri = Some(uri);
            }
            ConnectEvent::Approved(_) => {
                cx.emit(Login::Approved);
                return false;
            }
            ConnectEvent::Failed(msg) => {
                self.error = msg;
                self.waiting = None;
                // A dead handshake leaves a stale QR on the doors — drop it;
                // the URI strip now reads "generating…" and re-arms on click.
                self.qr_uri = None;
                self.qr = None;
                self.core.cancel_login();
                return false;
            }
        }
        cx.notify();
        true
    }

    fn connect_bunker(&mut self, cx: &mut Context<Self>) {
        let uri = self.bunker.read(cx).content.trim().to_string();
        if uri.is_empty() {
            return;
        }
        let rx = self.core.connect_bunker(uri);
        self.begin(
            "Waiting for approval…",
            "Approve the request in your bunker app.",
            rx,
            cx,
        );
    }


    fn cancel(&mut self, cx: &mut Context<Self>) {
        self.generation += 1; // invalidate the pump: failures stay quiet
        self.core.cancel_login();
        self.waiting = None;
        self.auth_url = None;
        cx.notify();
    }
}

impl Render for LoginView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.autofocused {
            self.autofocused = true;
            window.focus(&self.focus);
        }
        let error = self.error.clone();
        let waiting = self.waiting.clone();
        let auth_url = self.auth_url.clone();
        let bunker = self.bunker.clone();
        let qr = self.qr.clone();
        let qr_uri = self.qr_uri.clone();

        div()
            .id("login")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                // Enter submits whichever door is focused (single-line Inputs
                // don't bind enter, so it bubbles here).
                if ev.keystroke.key == "enter" {
                    this.connect_bunker(cx);
                }
                // Esc backs out of a running flow — the waiting page's
                // keyboard Cancel (matches the shell's Esc-to-back).
                if ev.keystroke.key == "escape" && this.waiting.is_some() {
                    this.cancel(cx);
                }
            }))
            // Window root, not a pane: a definite size, so the doors center.
            .size_full()
            .overflow_hidden()
            .bg(rgb(theme::WASHI))
            .text_color(rgb(theme::INK))
            .font_family(theme::SANS)
            .flex()
            .justify_center()
            .items_center()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .w_full()
                    .max_w(px(420.))
                    .px_6()
                    .items_center()
                    .children(if let Some((status, hint)) = waiting {
                        // The waiting page: breathing ensō + status + cancel.
                        vec![
                            // nostrconnect: the QR is the waiting mark. Bunker:
                            // the breathing ensō (the breath animates the
                            // wrapper's opacity — canvas glyphs aren't
                            // styleable directly).
                            if let Some(code) = qr {
                                img(code)
                                    .size(px(280.0))
                                    .rounded_sm()
                                    .into_any_element()
                            } else {
                                div()
                                    .child(icons::enso_icon(theme::INK, 120.0))
                                    .with_animation(
                                        "enso-breath",
                                        Animation::new(Duration::from_millis(3_400)).repeat(),
                                        |enso, t| {
                                            // 0.4→1.0 opacity, one breath per cycle
                                            // (the web app's enso-breath, login.rs).
                                            let phase =
                                                0.5 + 0.5 * (t * std::f32::consts::TAU).sin();
                                            enso.opacity(0.4 + 0.6 * phase)
                                        },
                                    )
                                    .into_any_element()
                            },
                            div()
                                .font_family(theme::SERIF)
                                .text_size(px(16.0))
                                .child(status)
                                .into_any_element(),
                            div()
                                .text_size(px(12.0))
                                .text_color(rgb(theme::MUTED))
                                .child(hint)
                                .into_any_element(),
                            // The URI in the clear, for signers that can't
                            // scan (typed by hand into the signer app).
                            qr_uri
                                .map(|uri| {
                                    div()
                                        .id("qr-uri")
                                        .text_size(px(11.0))
                                        .text_color(rgb(theme::MUTED))
                                        .overflow_x_scroll()
                                        .max_w(px(360.))
                                        .child(uri)
                                        .into_any_element()
                                })
                                .unwrap_or_else(|| div().into_any_element()),
                            auth_url
                                .map(|url| {
                                    let open = url.clone();
                                    div()
                                        .id("login-auth-url")
                                        .cursor_pointer()
                                        .text_size(px(12.0))
                                        .text_color(rgb(theme::LIVE))
                                        .overflow_x_scroll()
                                        .max_w(px(360.))
                                        .on_click(move |_, _, cx| cx.open_url(&open))
                                        .child(url)
                                        .into_any_element()
                                })
                                .unwrap_or_else(|| div().into_any_element()),
                            div()
                                .id("login-cancel")
                                .cursor_pointer()
                                .p_1()
                                .px_2()
                                .text_size(px(13.0))
                                .text_color(rgb(theme::MUTED))
                                .on_click(cx.listener(|this, _, _, cx| this.cancel(cx)))
                                .child("Cancel")
                                .into_any_element(),
                        ]
                    } else {
                        // The form: two doors (login.rs form page).
                        vec![
                            div()
                                .font_family(theme::SERIF)
                                .text_size(px(28.0))
                                .child("Satori")
                                .into_any_element(),
                            div()
                                .w_full()
                                .text_size(px(12.0))
                                .text_color(rgb(theme::MUTED))
                                .text_align(TextAlign::Center)
                                .child("A quiet Nostr client. Your key never has to touch it.")
                                .into_any_element(),
                            div()
                                .w_full()
                                .h(px(1.))
                                .bg(rgb(theme::HAIRLINE))
                                .into_any_element(),
                            div()
                                .when(!error.is_empty(), |d| {
                                    d.child(
                                        div()
                                            .text_size(px(12.0))
                                            .text_color(rgb(theme::ERR))
                                            .text_align(TextAlign::Center)
                                            .child(error),
                                    )
                                })
                                .into_any_element(),
                            div()
                                .w_full()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(field(bunker))
                                .child(
                                    div()
                                        .id("connect-bunker")
                                        .cursor_pointer()
                                        .p_1()
                                        .px_2()
                                        .text_size(px(13.0))
                                        .text_color(rgb(theme::VERMILION))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.connect_bunker(cx)
                                        }))
                                        .child("Connect"),
                                )
                                .into_any_element(),
                            div()
                                .w_full()
                                .text_size(px(11.0))
                                .text_color(rgb(theme::MUTED))
                                .text_align(TextAlign::Center)
                                .child(
                                    "Sign in with a remote signer (NIP-46). The app holds \
                                     only a transport secret.",
                                )
                                .into_any_element(),
                            div()
                                .w_full()
                                .h(px(1.))
                                .bg(rgb(theme::HAIRLINE))
                                .into_any_element(),
                            // The nostrconnect door, always on: the QR is
                            // generated the moment the doors show — click the
                            // URI below to copy it into a signer that can't
                            // scan its own screen.
                            div()
                                .w_full()
                                .flex()
                                .flex_col()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .font_family(theme::SERIF)
                                        .text_size(px(15.0))
                                        .child("Scan with your signer"),
                                )
                                .child(
                                    match &qr {
                                        Some(code) => div()
                                            .child(img(code.clone()).size(px(240.0)))
                                            .into_any_element(),
                                        None => div()
                                            .child(icons::enso_icon(theme::MUTED, 60.0))
                                            .with_animation(
                                                "qr-breath",
                                                Animation::new(Duration::from_millis(3_400))
                                                    .repeat(),
                                                |enso, t| {
                                                    let phase = 0.5
                                                        + 0.5 * (t * std::f32::consts::TAU).sin();
                                                    enso.opacity(0.4 + 0.6 * phase)
                                                },
                                            )
                                            .into_any_element(),
                                    },
                                )
                                .child(
                                    div()
                                        .id("qr-uri-copy")
                                        .cursor_pointer()
                                        .p_1()
                                        .px_2()
                                        .rounded_sm()
                                        .border_1()
                                        .border_color(rgb(theme::HAIRLINE))
                                        .text_size(px(11.0))
                                        .text_color(rgb(theme::MUTED))
                                        .overflow_x_scroll()
                                        .max_w(px(360.))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            // Handshake dead (timeout, relay loss)?
                                            // The click re-arms it.
                                            if this.qr_uri.is_none() {
                                                this.start_nostrconnect(cx);
                                                return;
                                            }
                                            let Some(uri) = this.qr_uri.clone() else {
                                                return;
                                            };
                                            cx.write_to_clipboard(
                                                gpui::ClipboardItem::new_string(uri),
                                            );
                                            this.copied = true;
                                            cx.notify();
                                            let reset = cx.spawn(async move |this, cx| {
                                                cx.background_executor()
                                                    .timer(Duration::from_millis(2_500))
                                                    .await;
                                                let _ = this.update(cx, |this, cx| {
                                                    this.copied = false;
                                                    cx.notify();
                                                });
                                            });
                                            reset.detach();
                                        }))
                                        .child(match (&qr_uri, self.copied) {
                                            (_, true) => "Copied — paste it in your signer".to_string(),
                                            (Some(uri), _) => uri.clone(),
                                            (None, _) => "generating…".to_string(),
                                        }),
                                )
                                .into_any_element(),
                            div()
                                .w_full()
                                .text_size(px(11.0))
                                .text_color(rgb(theme::MUTED))
                                .text_align(TextAlign::Center)
                                .child(
                                    "Approve in your signer app — the key never touches \
                                     Satori.",
                                )
                                .into_any_element(),
                        ]
                    }),
            )
    }
}

/// A door's field: the Input in the quiet panel skin (search.rs row). The
/// height is definite — the Input element fills its parent (relative 1.0),
/// which collapses in the login's content-sized chain.
fn field(input: Entity<Input>) -> impl IntoElement {
    div()
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .h(px(34.))
        .px_2()
        .pt_1()
        .rounded_sm()
        .bg(rgb(theme::PANEL))
        .border_1()
        .border_color(rgb(theme::HAIRLINE))
        .child(input)
}

/// The nostrconnect URI as a QR image — PNG bytes at 4px/module with the
/// spec's 4-module quiet zone, handed straight to gpui's image pipeline
/// (media.rs's Image::from_bytes path).
fn qr_image(uri: &str) -> Option<Arc<Image>> {
    let code = qrcode::QrCode::new(uri.as_bytes()).ok()?;
    let n = code.width();
    let colors = code.to_colors();
    let scale = 4usize;
    let quiet = 4usize;
    let dim = (n + quiet * 2) * scale;
    let mut buf = vec![255u8; dim * dim];
    for y in 0..n {
        for x in 0..n {
            if colors[y * n + x] == qrcode::Color::Dark {
                for dy in 0..scale {
                    for dx in 0..scale {
                        let py = (y + quiet) * scale + dy;
                        let px = (x + quiet) * scale + dx;
                        buf[py * dim + px] = 0;
                    }
                }
            }
        }
    }
    let luma = image::GrayImage::from_raw(dim as u32, dim as u32, buf)?;
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageLuma8(luma)
        .write_to(&mut png, image::ImageFormat::Png)
        .ok()?;
    Some(Arc::new(Image::from_bytes(ImageFormat::Png, png.into_inner())))
}

#[cfg(test)]
mod scratch {
    use super::*;

    #[test]
    fn uri_and_qr_generate() {
        let keys = nostr::prelude::Keys::generate();
        use nostr::prelude::*;
        let uri = NostrConnectUri::client(
            keys.public_key(),
            [RelayUrl::parse("wss://relay.nip46.com").unwrap()],
            "Satori",
        );
        let s = uri.to_string();
        println!("uri len={} starts={}", s.len(), &s[..40]);
        let code = qrcode::QrCode::new(s.as_bytes());
        match code {
            Ok(c) => println!("qr ok, width={}", c.width()),
            Err(e) => println!("QR ERR: {e}"),
        }
        assert!(qr_image(&s).is_some(), "qr_image returned None");
    }
}
