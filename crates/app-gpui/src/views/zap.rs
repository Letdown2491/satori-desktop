//! The zap amount sheet — the bolt's long-press (NIP-57, D20).
//!
//! TODO(port): feather AGENTS.md — NWC zaps. A tap is 21 sats with no
//! dialog; the sheet is the deliberate path: presets, the ledger's recent
//! amounts as seeds, one custom field. Picking an amount starts the zap
//! and pops — the feed's bolt glyph (amber in flight, vermilion settled)
//! and its quiet status whisper carry the outcome.

use std::sync::Arc;

use gpui::{
    App, Context, Div, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString,
    Stateful, Window, div, prelude::*, px, rgb,
};
use nostr::prelude::*;
use satori_core::manager::Core;

use super::nav::Nav;
use crate::{input::Input, theme};

/// The one-tap default (sats).
pub(crate) const TAP_SATS: u64 = 21;
/// The picker's presets (sats).
const PRESETS: [u64; 6] = [21, 100, 500, 1000, 2100, 5000];
/// How many recent amounts the ledger seeds (deduped).
const RECENT_SEEDS: usize = 3;

pub struct ZapSheet {
    core: Arc<Core>,
    note: Event,
    focus: FocusHandle,
    custom: Entity<Input>,
    /// The selected preset (sats); None = the custom field decides.
    sel: Option<u64>,
    /// Recent zap amounts (sats, deduped) from the ledger.
    recent: Vec<u64>,
    status: String,
    status_err: bool,
}

impl Focusable for ZapSheet {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl EventEmitter<Nav> for ZapSheet {}

impl ZapSheet {
    pub fn new(core: Arc<Core>, note: Event, cx: &mut Context<Self>) -> Self {
        crate::theme::watch(cx);
        cx.observe_global::<crate::input::NsecWarn>(|_, cx| cx.notify())
            .detach();
        // The ledger is the seed bed: recent amounts, newest first,
        // deduped (the picker grows around the user's habits).
        let mut seen = std::collections::HashSet::new();
        let recent = satori_core::wallet::ledger_history()
            .into_iter()
            .map(|z| z.msat / 1000)
            .filter(|sats| *sats > 0 && seen.insert(*sats))
            .take(RECENT_SEEDS)
            .collect();
        Self {
            focus: cx.focus_handle(),
            custom: cx.new(|cx| {
                let mut i = Input::new(cx);
                i.multiline = false;
                // D17 tripwire: no reason a private key ever lands here.
                i.tripwire = true;
                i.placeholder("custom, sats")
            }),
            sel: Some(TAP_SATS),
            recent,
            status: String::new(),
            status_err: false,
            core,
            note,
        }
    }

    /// The recipient's display name (best effort — the short npub stands
    /// in until the profile is cached).
    fn who(&self) -> String {
        self.core
            .profile_of(&self.note.pubkey)
            .map(|p| p.display())
            .unwrap_or_else(|| format!("{}…", &self.note.pubkey.to_string()[..8]))
    }

    /// The chosen amount in msats (the custom field wins when no preset
    /// is selected). None = the field didn't parse.
    fn chosen_msat(&self, cx: &App) -> Option<u64> {
        match self.sel {
            Some(sats) => Some(sats * 1000),
            None => self
                .custom
                .read(cx)
                .content
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|s| *s > 0.0 && *s <= 100_000_000.0)
                .map(|sats| (sats * 1000.0) as u64),
        }
    }

    /// Start the zap with the chosen amount and pop. Outcome flows on the
    /// wallet broadcast (the feed's bolt + status whisper own it).
    fn zap(&mut self, cx: &mut Context<Self>) {
        let Some(msat) = self.chosen_msat(cx) else {
            self.status = "how many sats?".into();
            self.status_err = true;
            cx.notify();
            return;
        };
        if !self.core.wallet().is_ready() {
            self.status = "no wallet connected".into();
            self.status_err = true;
            cx.notify();
            return;
        }
        let note = self.note.clone();
        self.core.zap(note, msat);
        cx.emit(Nav::Pop);
    }

    fn pill(id: String, sats: u64, on: bool) -> Stateful<Div> {
        div()
            .id(SharedString::from(id))
            .cursor_pointer()
            .px_2()
            .py_0p5()
            .rounded(px(theme::R_EMBED))
            .border_1()
            .border_color(rgb(if on {
                theme::vermilion()
            } else {
                theme::hairline()
            }))
            .text_size(px(theme::T_UI))
            .text_color(rgb(if on { theme::ink() } else { theme::muted() }))
            .when(on, |d| d.font_weight(gpui::FontWeight::SEMIBOLD))
            .child(format_sats(sats))
    }
}

fn format_sats(sats: u64) -> String {
    format!("{sats} sats")
}

impl Render for ZapSheet {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let who = self.who();
        let sel = self.sel;
        let status = self.status.clone();
        let status_err = self.status_err;

        let preset_row = div().flex().flex_wrap().gap_2().py_1().children(
            PRESETS
                .iter()
                .map(|sats| {
                    let sats = *sats;
                    Self::pill(format!("preset-{sats}"), sats, sel == Some(sats)).on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.sel = Some(sats);
                            this.status.clear();
                            cx.notify();
                        }),
                    )
                })
                .collect::<Vec<_>>(),
        );

        let recent_row = if self.recent.is_empty() {
            None
        } else {
            Some(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .pb_1()
                    .children(self.recent.iter().map(|sats| {
                        let sats = *sats;
                        Self::pill(format!("recent-{sats}"), sats, sel == Some(sats)).on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.sel = Some(sats);
                                this.status.clear();
                                cx.notify();
                            }),
                        )
                    })),
            )
        };

        div()
            .id("zap-sheet")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, window, cx| {
                // Enter fires the zap (the custom field doesn't bind it).
                if ev.keystroke.key == "enter"
                    && this
                        .custom
                        .read(cx)
                        .focus_handle(cx)
                        .is_focused(window)
                {
                    this.zap(cx);
                }
            }))
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
                    .child(format!("Zap {who}")),
            )
            .child(
                div()
                    .text_size(px(theme::T_SMALL))
                    .text_color(rgb(theme::muted()))
                    .child("A public zap (NIP-57) through your wallet, the receipt shows your name."),
            )
            .child(preset_row)
            .children(recent_row)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        // Definite height — the Input fills its parent
                        // (relative 1.0); a content-sized chain collapses
                        // it (login.rs field()).
                        div()
                            .id("zap-custom-field")
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
                                let focus = this.custom.read(cx).focus_handle(cx);
                                window.focus(&focus, cx);
                            }))
                            .child(self.custom.clone()),
                    )
                    .child(
                        div()
                            .id("zap-go")
                            .cursor_pointer()
                            .px_2()
                            .py_1()
                            .text_size(px(theme::T_UI))
                            .text_color(rgb(theme::vermilion()))
                            .child("Zap"),
                    ),
            )
            .child(muted_status(status, status_err))
            .when(
                cx.try_global::<crate::input::NsecWarn>()
                    .is_some_and(|w| w.0),
                |d| {
                    d.child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_2()
                            .rounded(px(theme::R_EMBED))
                            .border_1()
                            .border_color(rgb(theme::err()))
                            .px_3()
                            .py_2()
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(theme::T_SMALL))
                                    .text_color(rgb(theme::err()))
                                    .child(
                                        "That looks like a private key (nsec…). Nothing was inserted, a key has no business in a zap sheet.",
                                    ),
                            )
                            .child(
                                div()
                                    .id("zap-nsec-dismiss")
                                    .cursor_pointer()
                                    .text_size(px(theme::T_SMALL))
                                    .text_color(rgb(theme::muted()))
                                    .hover(|s| s.text_color(rgb(theme::ink())))
                                    .on_click(|_, _, cx| {
                                        cx.set_global(crate::input::NsecWarn(false))
                                    })
                                    .child("×"),
                            ),
                    )
                },
            )
    }
}

/// The quiet status line (a whisper, not a banner).
fn muted_status(status: String, err: bool) -> Div {
    if status.is_empty() {
        return div();
    }
    div()
        .text_size(px(theme::T_SMALL))
        .text_color(rgb(if err { theme::err() } else { theme::muted() }))
        .child(status)
}
