//! The NIP-A3 payment-target surface (D93): one home for the chip, its
//! label, and the fetch-once scaffold. The sheet and the profile header
//! both render "other ways to pay" — born as verbatim twins in the zap
//! milestone, extracted at the D94 audit before they could drift (one
//! already had: the text sizes).

use std::sync::Arc;

use gpui::{Context, Div, SharedString, Stateful, div, prelude::*, px, rgb};
use nostr::prelude::PublicKey;
use satori_core::manager::Core;

use crate::theme;

/// The chip's label: the rail type, capitalized ("venmo" → "Venmo").
pub(crate) fn label(t: &satori_core::nip::payto::PaymentTarget) -> String {
    let mut chars = t.target_type.chars();
    match chars.next() {
        Some(first) => {
            let rest: String = chars.collect();
            first.to_uppercase().collect::<String>() + &rest
        }
        None => t.target_type.clone(),
    }
}

/// The hand-off chip: click opens the URI, right-click copies the
/// address (the two affordances travel together — no amount is ever
/// attached, D93). `text_size` is the host row's scale (the sheet's
/// pills run T_UI, the profile header's row T_SMALL).
pub(crate) fn chip(t: &satori_core::nip::payto::PaymentTarget, text_size: f32) -> Stateful<Div> {
    let uri = satori_core::nip::payto::uri_for(t);
    div()
        .id(SharedString::from(format!("payto-{}", t.target_type)))
        .cursor_pointer()
        .px_2()
        .py_0p5()
        .rounded(px(theme::R_EMBED))
        .border_1()
        .border_color(rgb(theme::hairline()))
        .hover(|s| s.border_color(rgb(theme::muted())))
        .text_size(px(text_size))
        .text_color(rgb(theme::muted()))
        .on_click({
            let uri = uri.clone();
            move |_, _, cx| {
                cx.open_url(&uri);
            }
        })
        .on_mouse_down(gpui::MouseButton::Right, {
            let uri = uri.clone();
            move |_, _, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(uri.clone()));
            }
        })
        .child(label(t))
}

/// One cold fetch at view construction (store-first inside core; the
/// session negative cache makes repeats free). `store` lands the
/// targets on the view; a view that dies before the fetch lands just
/// drops the result (the failed weak update).
pub(crate) fn want_targets<V: 'static>(
    core: &Arc<Core>,
    who: PublicKey,
    cx: &mut Context<V>,
    store: impl Fn(&mut V, Vec<satori_core::nip::payto::PaymentTarget>, &mut Context<V>) + 'static,
) {
    let core = Arc::clone(core);
    let this = cx.entity().downgrade();
    cx.spawn(async move |_, cx| {
        let targets = core.payment_targets(who).await;
        let _ = this.update(cx, |view, cx| store(view, targets, cx));
    })
    .detach();
}
