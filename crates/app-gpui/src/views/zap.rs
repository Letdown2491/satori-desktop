//! The zap sheet — the bolt's deliberate path (NIP-57, D20/D92/D93).
//!
//! TODO(port): feather AGENTS.md — NWC zaps; dark-wisp ZapDialog.kt — the
//! sheet shape (0.11.0, D92). The hero amount IS the input: one big
//! number seeded from the chosen preset, select-all on focus so the
//! first keystroke replaces the seed. Preset pills SEED rather than send
//! (a preset's optional message autofills only an empty message field);
//! the current amount + message save as a new preset (right-click
//! deletes). The privacy row picks the 9734's author (attributed through
//! the signer, or a fresh ephemeral key); the instant row owns the
//! tap-sends-21-sats pref. The author's NIP-A3 payment targets render as
//! amount-less hand-off chips ("other ways to pay") — and carry the
//! no-dead-end rule: with no wallet connected they are the sheet's
//! working answer, not an error dead end (D93). The feed's bolt glyph
//! (amber in flight, vermilion settled) and its quiet status whisper
//! carry the outcome.

use std::sync::Arc;

use gpui::{
    App, Context, Div, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString,
    Stateful, Window, div, prelude::*, px, rgb,
};
use nostr::prelude::*;
use satori_core::manager::Core;

use super::content;
use super::nav::Nav;
use crate::{input::Input, prefs, theme};

/// How many recent amounts the ledger seeds (deduped).
const RECENT_SEEDS: usize = 3;
/// The strip's cap (prefs::save_zap_presets enforces it too).
const PRESET_CAP: usize = 8;

/// What this sheet zaps: a note (the e-tag zap) or a profile (the
/// no-e-tag variant, the header's Zap button — D92).
enum ZapTarget {
    Note(Event),
    Profile {
        who: PublicKey,
        /// The cached lightning pointer (read at open; None = none known).
        lud: Option<String>,
    },
}

pub struct ZapSheet {
    core: Arc<Core>,
    target: ZapTarget,
    /// The zapped author (the targets fetch + display name).
    who: PublicKey,
    focus: FocusHandle,
    /// The hero: the amount in sats, the sheet's centerpiece.
    hero: Entity<Input>,
    /// The zap message (the 9734's content).
    message: Entity<Input>,
    /// The stored strip (mirrors prefs; save/delete writes through).
    presets: Vec<prefs::ZapPreset>,
    /// Recent zap amounts (sats, deduped) from the ledger.
    recent: Vec<u64>,
    privacy: satori_core::wallet::ZapPrivacy,
    /// The target author's NIP-A3 payment targets (fetched once at open).
    targets: Vec<satori_core::nip::payto::PaymentTarget>,
    /// The instant pref's mirror (the toggle row writes through).
    instant: bool,
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
        let who = note.pubkey;
        let sheet = Self::build(core, ZapTarget::Note(note), who, cx);
        sheet.want_targets(who, cx);
        sheet
    }

    pub fn new_profile(core: Arc<Core>, who: PublicKey, cx: &mut Context<Self>) -> Self {
        // The pointer rides the cached kind-0 (the caller just came from
        // the profile page — it's warm).
        let lud = core
            .profile_of(&who)
            .and_then(|p| if p.lud.is_empty() { None } else { Some(p.lud) });
        let sheet = Self::build(core, ZapTarget::Profile { who, lud }, who, cx);
        sheet.want_targets(who, cx);
        sheet
    }

    fn build(core: Arc<Core>, target: ZapTarget, who: PublicKey, cx: &mut Context<Self>) -> Self {
        crate::theme::watch(cx);
        cx.observe_global::<crate::input::NsecWarn>(|_, cx| cx.notify())
            .detach();
        // The ledger is the seed bed: recent amounts, newest first,
        // deduped (the picker grows around the user's habits).
        let recent = satori_core::wallet::ledger_recent_amounts(RECENT_SEEDS);
        // The hero seeds with the instant amount (the tap pref — the
        // Wallet tab's field owns it), selected — the first keystroke
        // replaces it.
        let instant_sats = prefs::zap_instant_sats();
        let hero = cx.new(|cx| {
            let mut i = Input::single_line("custom, sats", cx);
            i.seed(&instant_sats.to_string(), cx);
            i
        });
        let message = cx.new(|cx| {
            let mut i = Input::single_line("say thanks (optional)", cx);
            // D17 tripwire: no reason a private key ever lands here.
            i.tripwire = true;
            i
        });
        Self {
            focus: cx.focus_handle(),
            presets: prefs::zap_presets(),
            instant: prefs::zap_instant(),
            privacy: satori_core::wallet::ZapPrivacy::Attributed,
            targets: Vec::new(),
            status: String::new(),
            status_err: false,
            core,
            target,
            who,
            hero,
            message,
            recent,
        }
    }

    /// The author's NIP-A3 payment targets (one cold fetch at open;
    /// empty = none known — the chips just don't render).
    fn want_targets(&self, who: PublicKey, cx: &mut Context<Self>) {
        super::payto::want_targets(&self.core, who, cx, |sheet, targets, cx| {
            sheet.targets = targets;
            cx.notify();
        });
    }

    /// The recipient's display name (best effort — the short npub stands
    /// in until the profile is cached).
    fn who(&self) -> String {
        content::display_name(self.core.profile_of(&self.who).as_ref(), &self.who)
    }

    /// The hero's amount in msats. None = the field didn't parse.
    fn chosen_msat(&self, cx: &App) -> Option<u64> {
        self.hero
            .read(cx)
            .content
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|s| *s > 0.0 && *s <= 100_000_000.0)
            .map(|sats| (sats * 1000.0) as u64)
    }

    /// Seed the hero from a pill; a preset's message autofills only an
    /// EMPTY message field (never clobbers typing).
    fn seed_from(&mut self, sats: u64, message: Option<String>, cx: &mut Context<Self>) {
        self.hero.update(cx, |h, cx| h.seed(&sats.to_string(), cx));
        if let Some(msg) = message
            && self.message.read(cx).content.is_empty()
        {
            self.message.update(cx, |m, cx| m.set_text(&msg, cx));
        }
        self.status.clear();
        cx.notify();
    }

    /// The quiet status whisper (settings' set_status shape): one door,
    /// so the err flag and the notify never split.
    fn set_status(&mut self, msg: &str, err: bool, cx: &mut Context<Self>) {
        self.status = msg.to_string();
        self.status_err = err;
        cx.notify();
    }

    /// Save the current hero + message as a preset (same sats replaces).
    fn save_preset(&mut self, cx: &mut Context<Self>) {
        let Some(sats) = self.chosen_msat(cx).map(|m| m / 1000) else {
            self.set_status("how many sats?", true, cx);
            return;
        };
        let message = self.message.read(cx).content.trim().to_string();
        if message.chars().count() > satori_core::wallet::MESSAGE_CAP {
            self.set_status(
                &format!(
                    "message is {} characters max",
                    satori_core::wallet::MESSAGE_CAP
                ),
                true,
                cx,
            );
            return;
        }
        self.presets.retain(|p| p.sats != sats);
        self.presets.insert(0, prefs::ZapPreset { sats, message });
        self.presets.truncate(PRESET_CAP);
        prefs::save_zap_presets(&self.presets);
        self.set_status("preset saved", false, cx);
    }

    fn delete_preset(&mut self, sats: u64, cx: &mut Context<Self>) {
        self.presets.retain(|p| p.sats != sats);
        prefs::save_zap_presets(&self.presets);
        self.set_status("preset removed", false, cx);
    }

    /// Start the zap with the hero's amount and pop. Outcome flows on
    /// the wallet broadcast (the feed's bolt + status whisper own it).
    fn zap(&mut self, cx: &mut Context<Self>) {
        let Some(msat) = self.chosen_msat(cx) else {
            self.set_status("how many sats?", true, cx);
            return;
        };
        let message = self.message.read(cx).content.trim().to_string();
        if message.chars().count() > satori_core::wallet::MESSAGE_CAP {
            self.set_status(
                &format!(
                    "message is {} characters max",
                    satori_core::wallet::MESSAGE_CAP
                ),
                true,
                cx,
            );
            return;
        }
        if !self.core.wallet().is_ready() {
            // No dead end (D93): the targets row below IS the answer.
            self.set_status("no wallet connected", true, cx);
            return;
        }
        match &self.target {
            ZapTarget::Note(note) => {
                self.core.zap(note.clone(), msat, message, self.privacy);
            }
            ZapTarget::Profile { who, lud } => {
                let Some(lud) = lud.clone() else {
                    self.set_status("no lightning address on the profile", true, cx);
                    return;
                };
                self.core
                    .zap_profile(*who, lud, msat, message, self.privacy);
            }
        }
        cx.emit(Nav::Pop);
    }
}

/// A preset pill's label.
fn format_sats(sats: u64) -> String {
    format!("{sats} sats")
}

impl Render for ZapSheet {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let who = self.who();
        let status = self.status.clone();
        let status_err = self.status_err;
        let anon = self.privacy == satori_core::wallet::ZapPrivacy::Anonymous;
        let instant = self.instant;
        let targets = self.targets.clone();

        let presets = self.presets.clone();
        let preset_row = div()
            .flex()
            .flex_wrap()
            .gap_2()
            .py_1()
            .children(presets.iter().map(|p| {
                let sats = p.sats;
                let message = p.message.clone();
                let sheet = cx.entity().downgrade();
                Self::pill(format!("preset-{sats}"), format_sats(sats))
                    // Right-click deletes the stored preset (a quiet
                    // editor: save and remove both live in the sheet).
                    .on_mouse_down(gpui::MouseButton::Right, move |_, _, cx| {
                        let sheet = sheet.clone();
                        let _ = sheet.update(cx, |this, cx| this.delete_preset(sats, cx));
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.seed_from(sats, Some(message.clone()), cx);
                    }))
            }));

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
                        Self::pill(format!("recent-{sats}"), format_sats(sats)).on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.seed_from(sats, None, cx);
                            }),
                        )
                    })),
            )
        };

        // Other ways to pay (D93): amount-less hand-off chips, always
        // rendered when the author published targets — and the working
        // answer when no wallet is connected (the no-dead-end rule).
        let targets_row = if targets.is_empty() {
            None
        } else {
            Some(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(theme::T_SMALL))
                            .text_color(rgb(theme::muted()))
                            .child(if self.core.wallet().is_ready() {
                                "Other ways to pay"
                            } else {
                                "No wallet connected — other ways to pay:"
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_2()
                            .children(
                                targets.iter().map(|t| super::payto::chip(t, theme::T_UI)),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(theme::T_SMALL))
                            .text_color(rgb(theme::muted()))
                            .child("no amount is attached — the app you open names it. Right-click copies the address."),
                    ),
            )
        };

        div()
            .id("zap-sheet")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, window, cx| {
                // Enter fires the zap (the hero doesn't bind it).
                if ev.keystroke.key == "enter"
                    && this
                        .hero
                        .read(cx)
                        .focus_handle(cx)
                        .is_focused(window)
                {
                    this.zap(cx);
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
                    .child(format!("Zap {who}")),
            )
            .child(
                div()
                    .text_size(px(theme::T_SMALL))
                    .text_color(rgb(theme::muted()))
                    .child(if anon {
                        "The request is signed with a throwaway key — the receipt shows no name."
                    } else {
                        "A public zap (NIP-57) through your wallet, the receipt shows your name."
                    }),
            )
            // The hero: the big number IS the input (D92).
            .child(
                div()
                    .id("zap-hero-field")
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .h(px(46.))
                    .px_3()
                    .pt(px(10.))
                    .rounded(px(theme::R_INPUT))
                    .bg(rgb(theme::panel()))
                    .border_1()
                    .border_color(rgb(theme::hairline()))
                    .text_size(px(20.0))
                    .cursor(gpui::CursorStyle::IBeam)
                    .on_click(cx.listener(|this, _, window, cx| {
                        let focus = this.hero.read(cx).focus_handle(cx);
                        window.focus(&focus, cx);
                    }))
                    .child(self.hero.clone()),
            )
            .child(preset_row)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .children(recent_row)
                    .child(
                        div()
                            .id("zap-save-preset")
                            .cursor_pointer()
                            .px_2()
                            .py_0p5()
                            .rounded(px(theme::R_EMBED))
                            .border_1()
                            .border_color(rgb(theme::hairline()))
                            .hover(|s| s.border_color(rgb(theme::muted())))
                            .text_size(px(theme::T_UI))
                            .text_color(rgb(theme::muted()))
                            .on_click(cx.listener(|this, _, _, cx| this.save_preset(cx)))
                            .child("+ save preset"),
                    ),
            )
            .child(
                div()
                    .id("zap-message-field")
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
                        let focus = this.message.read(cx).focus_handle(cx);
                        window.focus(&focus, cx);
                    }))
                    .child(self.message.clone()),
            )
            .child(toggle_row(
                "zap-anon-toggle",
                "Anonymous zap",
                if anon {
                    "the receipt carries no name"
                } else {
                    "the receipt shows your name"
                },
                anon,
                cx.listener(move |this, _, _, cx| {
                    this.privacy = if anon {
                        satori_core::wallet::ZapPrivacy::Attributed
                    } else {
                        satori_core::wallet::ZapPrivacy::Anonymous
                    };
                    this.status.clear();
                    cx.notify();
                }),
            ))
            .child(toggle_row(
                "zap-instant-toggle",
                "Instant zap on tap",
                &format!(
                    "{sats} sats, no sheet — hold or right-click opens this",
                    sats = prefs::zap_instant_sats()
                ),
                instant,
                cx.listener(move |this, _, _, cx| {
                    this.instant = !this.instant;
                    prefs::set_bool("zap_instant", this.instant);
                    this.status.clear();
                    cx.notify();
                }),
            ))
            .children(targets_row)
            .child(
                div()
                    .flex()
                    .justify_end()
                    .child(
                        div()
                            .id("zap-go")
                            .cursor_pointer()
                            .px_3()
                            .py_1()
                            .rounded(px(theme::R_EMBED))
                            .border_1()
                            .border_color(rgb(theme::vermilion()))
                            .text_size(px(theme::T_UI))
                            .text_color(rgb(theme::vermilion()))
                            .on_click(cx.listener(|this, _, _, cx| this.zap(cx)))
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

impl ZapSheet {
    fn pill(id: String, label: String) -> Stateful<Div> {
        div()
            .id(SharedString::from(id))
            .cursor_pointer()
            .px_2()
            .py_0p5()
            .rounded(px(theme::R_EMBED))
            .border_1()
            .border_color(rgb(theme::hairline()))
            .text_size(px(theme::T_UI))
            .text_color(rgb(theme::muted()))
            .child(label)
    }
}

/// A labeled toggle row (the Dark Wisp switch row, flattened to the app's
/// quiet idiom): label left, helper text under it, state right.
fn toggle_row(
    id: &str,
    label: &str,
    helper: &str,
    on: bool,
    click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    div()
        .id(SharedString::from(id))
        .flex()
        .items_center()
        .justify_between()
        .px_2()
        .py_1p5()
        .rounded(px(theme::R_EMBED))
        .bg(rgb(theme::panel()))
        .cursor_pointer()
        .on_click(click)
        .child(
            div()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_size(px(theme::T_UI))
                        .text_color(rgb(theme::ink()))
                        .child(label.to_string()),
                )
                .child(
                    div()
                        .text_size(px(theme::T_SMALL))
                        .text_color(rgb(theme::muted()))
                        .child(helper.to_string()),
                ),
        )
        .child(
            div()
                .text_size(px(theme::T_SMALL))
                .text_color(rgb(if on {
                    theme::vermilion()
                } else {
                    theme::muted()
                }))
                .child(if on { "on" } else { "off" }),
        )
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
