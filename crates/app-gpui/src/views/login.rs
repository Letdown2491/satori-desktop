//! Login — port of crates/app/src/login.rs (the /login page, minus NIP-07):
//! two doors matching the product's key posture (DESIGN.md D2) — bunker
//! (default, the key never touches us) and nsec (explicit opt-in). The
//! waiting state breathes the ensō at the web app's 3.4s period. Success
//! swaps the window root to the shell (main.rs Root).

use std::sync::Arc;

use gpui::{
    div, prelude::*, px, rgb, Animation, AnimationExt, App, Context, Entity, EventEmitter,
    FocusHandle, Focusable, KeyDownEvent, Render, TextAlign, Window,
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
    nsec: Entity<Input>,
    focus: FocusHandle,
    /// Form, or the waiting page with its status + hint (they differ per
    /// flow: nsec never needs approval).
    waiting: Option<(String, String)>,
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
        let nsec = cx.new(|cx| {
            let mut i = Input::new(cx);
            i.multiline = false;
            i.placeholder("nsec1…")
        });
        let focus = bunker.read(cx).focus_handle(cx);
        Self {
            core,
            bunker,
            nsec,
            focus,
            waiting: None,
            auth_url: None,
            error: resume_error.unwrap_or_default(),
            generation: 0,
            autofocused: false,
        }
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
            ConnectEvent::Approved(_) => {
                cx.emit(Login::Approved);
                return false;
            }
            ConnectEvent::Failed(msg) => {
                self.error = msg;
                self.waiting = None;
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

    fn login_nsec(&mut self, cx: &mut Context<Self>) {
        let secret = self.nsec.read(cx).content.trim().to_string();
        if secret.is_empty() {
            return;
        }
        let rx = self.core.login_nsec(secret);
        self.begin(
            "Unsealing your key…",
            "Signing locally — nothing to approve.",
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
        let nsec = self.nsec.clone();

        div()
            .id("login")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                // Enter submits whichever door is focused (single-line Inputs
                // don't bind enter, so it bubbles here).
                if ev.keystroke.key == "enter" {
                    if this.nsec.read(cx).focus_handle(cx).is_focused(window) {
                        this.login_nsec(cx);
                    } else {
                        this.connect_bunker(cx);
                    }
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
                            // The breath animates the wrapper's opacity —
                            // canvas glyphs aren't styleable directly.
                            div()
                                .child(icons::enso_icon(theme::INK, 120.0))
                                .with_animation(
                                    "enso-breath",
                                    Animation::new(Duration::from_millis(3_400)).repeat(),
                                    |enso, t| {
                                        // 0.4→1.0 opacity, one breath per cycle
                                        // (the web app's enso-breath, login.rs).
                                        let phase = 0.5 + 0.5 * (t * std::f32::consts::TAU).sin();
                                        enso.opacity(0.4 + 0.6 * phase)
                                    },
                                )
                                .into_any_element(),
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
                            div()
                                .w_full()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(field(nsec))
                                .child(
                                    div()
                                        .id("login-nsec")
                                        .cursor_pointer()
                                        .p_1()
                                        .px_2()
                                        .text_size(px(13.0))
                                        .text_color(rgb(theme::MUTED))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.login_nsec(cx)
                                        }))
                                        .child("Sign in with nsec"),
                                )
                                .into_any_element(),
                            div()
                                .w_full()
                                .text_size(px(11.0))
                                .text_color(rgb(theme::MUTED))
                                .text_align(TextAlign::Center)
                                .child(
                                    "Uses a local key, stored in your Secret Service. Only \
                                     do this if you understand the trade-off.",
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
