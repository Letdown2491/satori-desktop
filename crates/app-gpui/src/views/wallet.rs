//! The NWC wallet pane — connect a `nostr+walletconnect://` URI (D20).
//!
//! TODO(port): feather AGENTS.md — NWC zaps. The wallet is a capability,
//! not an identity: the URI's secret rides the secret store, the
//! secret-free record in wallet.json rebuilds the client across restarts.
//! This pane is the connect/disconnect door; the settings section links
//! here, and so does a bolt tap with no wallet behind it.

use std::sync::Arc;

use gpui::{
    App, Context, Div, Entity, EventEmitter, FocusHandle, Focusable, Render, Window, div,
    prelude::*, px, rgb,
};
use satori_core::manager::Core;
use satori_core::wallet::{self, Status};

use crate::{input::Input, theme};

use super::nav::Nav;

pub struct WalletView {
    core: Arc<Core>,
    focus: FocusHandle,
    uri: Entity<Input>,
    /// Mirrored wallet status (the broadcast is the truth; this pane is
    /// one viewer of it).
    status: Status,
    /// The last connect failure (cleared on the next status change).
    err: Option<String>,
    /// Paste-time relay screening refused a private relay (issue #11):
    /// the full verdicts — the banner line and the hosts the "allow"
    /// affordance consents to derive from them (review Tier 2.5).
    /// Cleared on the next status change or allow-and-retry.
    screened: Option<Vec<satori_core::net_guard::RelayVerdict>>,
}

impl Focusable for WalletView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl EventEmitter<Nav> for WalletView {}

impl WalletView {
    pub fn new(core: Arc<Core>, cx: &mut Context<Self>) -> Self {
        crate::theme::watch(cx);
        let mut this = Self {
            focus: cx.focus_handle(),
            uri: cx.new(|cx| {
                let mut i = Input::new(cx);
                i.multiline = false;
                i.placeholder("nostr+walletconnect://…")
            }),
            status: core.wallet().status(),
            err: None,
            screened: None,
            core,
        };
        this.pump_wallet(cx);
        this
    }

    /// The wallet broadcast → this pane's mirror (status + connect errors).
    fn pump_wallet(&mut self, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        let mut rx = self.core.subscribe_wallet();
        cx.spawn(async move |_, cx| {
            loop {
                match rx.recv().await {
                    Ok(wallet::WalletEvent::Status(s)) => {
                        if this
                            .update(cx, |this, cx| {
                                this.status = s;
                                this.err = None;
                                this.screened = None;
                                cx.notify();
                            })
                            .is_err()
                        {
                            break; // dead pane
                        }
                    }
                    Ok(wallet::WalletEvent::RelayScreened { verdicts }) => {
                        // The verdicts cross the core boundary whole; the
                        // dialog derives the banner and the allow list from
                        // them (review Tier 2.5).
                        if this
                            .update(cx, |this, cx| {
                                this.screened = Some(verdicts);
                                cx.notify();
                            })
                            .is_err()
                        {
                            break; // dead pane
                        }
                    }
                    Ok(wallet::WalletEvent::ConnectFailed(e)) => {
                        if this
                            .update(cx, |this, cx| {
                                this.err = Some(e);
                                cx.notify();
                            })
                            .is_err()
                        {
                            break; // dead pane
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(_) => continue, // lagged: the next event re-syncs
                }
            }
        })
        .detach();
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        let uri = self.uri.read(cx).content.trim().to_string();
        if uri.is_empty() {
            self.err = Some("paste the URI your wallet app shows".into());
            cx.notify();
            return;
        }
        self.core.connect_wallet(uri);
        cx.notify();
    }

    /// The status line: what the wallet answered about itself, or the
    /// honest absence of one.
    fn status_line(&self) -> Div {
        let (text, color) = match &self.status {
            Status::Disconnected => ("no wallet connected".to_string(), theme::muted()),
            Status::Connecting => ("reaching the wallet…".to_string(), theme::muted()),
            Status::Ready(info) => {
                let mut line = if info.alias.is_empty() {
                    "connected".to_string()
                } else {
                    format!("connected, {}", info.alias)
                };
                if let Some(msat) = info.balance_msat {
                    line.push_str(&format!(" · {} sats", msat / 1000));
                }
                (line, theme::ok())
            }
        };
        if let Some(err) = &self.err {
            return div()
                .text_size(px(theme::T_SMALL))
                .text_color(rgb(theme::err()))
                .child(err.clone());
        }
        div()
            .text_size(px(theme::T_SMALL))
            .text_color(rgb(color))
            .child(text)
    }
}

impl Render for WalletView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ready = matches!(self.status, Status::Ready(_));
        let connecting = matches!(self.status, Status::Connecting);

        div()
            .id("wallet-root")
            .track_focus(&self.focus)
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_4()
            .py_3()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_size(px(theme::T_UI))
                    .text_color(rgb(theme::ink()))
                    .child("Wallet"),
            )
            .child(self.status_line())
            // The connect row: the URI paste + the seal. Hidden while the
            // wallet answers (disconnect takes its place).
            .when(!ready && !connecting, |d| {
                d.child(
                    div().flex().items_center().gap_2().child(
                        // The height is definite — the Input fills its
                        // parent (relative 1.0), which collapses in a
                        // content-sized chain (login.rs field()).
                        div()
                            .id("wallet-uri")
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .h(px(34.))
                            .px_2()
                            .pt_1()
                            .rounded(px(theme::R_EMBED))
                            .bg(rgb(theme::panel()))
                            .border_1()
                            .border_color(rgb(theme::hairline()))
                            .cursor(gpui::CursorStyle::IBeam)
                            .on_click(cx.listener(|this, _, window, cx| {
                                let focus = this.uri.read(cx).focus_handle(cx);
                                window.focus(&focus, cx);
                            }))
                            .child(self.uri.clone()),
                    ),
                )
                .child(
                    div().flex().child(
                        div()
                            .id("wallet-connect")
                            .cursor_pointer()
                            .px_2()
                            .py_1()
                            .text_size(px(theme::T_UI))
                            .text_color(rgb(theme::vermilion()))
                            .child("Connect")
                            .on_click(cx.listener(|this, _, _, cx| this.connect(cx))),
                    ),
                )
                // The issue-#11 screen refused a private relay in the
                // pasted URI: name it, offer the consent toggle. Allowing
                // writes the SAME ledger the relay allowlist uses, then
                // retries (the input still holds the URI).
                .when(self.screened.is_some(), |d| {
                    let Some(verdicts) = self.screened.clone() else {
                        return d;
                    };
                    let notice = satori_core::net_guard::relay_screen_notice(&verdicts);
                    d.child(
                        div()
                            .text_size(px(theme::T_SMALL))
                            .text_color(rgb(theme::err()))
                            .child(notice),
                    )
                    .child(div().flex().child(
                        div()
                            .id("wallet-allow-relay")
                            .cursor_pointer()
                            .px_2()
                            .py_1()
                            .text_size(px(theme::T_UI))
                            .text_color(rgb(theme::vermilion()))
                            .child("Allow & retry")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                satori_core::net_guard::allow_direct_relays(
                                    verdicts.iter().map(|v| v.host.as_str()),
                                );
                                this.screened = None;
                                this.err = None;
                                this.connect(cx);
                            })),
                    ))
                })
            })
            .when(connecting, |d| {
                d.child(
                    div()
                        .text_size(px(theme::T_SMALL))
                        .text_color(rgb(theme::muted()))
                        .child("approving in your wallet app…"),
                )
            })
            .when(ready, |d| {
                d.child(
                    div()
                        .text_size(px(theme::T_SMALL))
                        .text_color(rgb(theme::muted()))
                        .child("Zaps go out as public NIP-57 requests; the wallet pays the invoice."),
                )
                .child(
                    div().flex().child(
                        div()
                            .id("wallet-disconnect")
                            .cursor_pointer()
                            .px_2()
                            .py_1()
                            .text_size(px(theme::T_UI))
                            .text_color(rgb(theme::err()))
                            .child("Disconnect")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.core.disconnect_wallet();
                                cx.notify();
                            })),
                    ),
                )
            })
            .child(
                div()
                    .text_size(px(theme::T_SMALL))
                    .text_color(rgb(theme::muted()))
                    .child("The connection secret stays in this device's secret store, never in the event cache, never in a backup."),
            )
    }
}
