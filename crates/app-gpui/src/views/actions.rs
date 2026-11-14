//! The action row: reply · like · zap · repost · quote · bookmark.
//!
//! TODO(port): src/render/actions.ts (and the GTK note_card's action row) —
//! the timeline's row, extracted (#16) so the thread pane's notes wear the
//! same affordances instead of the reply-only button. One row, many
//! surfaces; notifications compact cards can adopt it later.
//!
//! The row is state-blind (§1.2: views are projections of state; the
//! mutation is the host's business). The glyph states ride in as
//! [`ActionState`]; the toggles go out through the [`ActionHost`] trait —
//! `Feed` keeps its optimistic mirrors, `ThreadView` keeps its own, both
//! funnel publishes through `core` (commit()).

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnimationExt, Context, Div, Entity, EventEmitter, SharedString, div, prelude::*, px, rgb,
};
use nostr::prelude::*;

use super::nav::Nav;
use crate::desktop;
use crate::{icons, theme};

/// Hold (~500ms) or right-click = the amount sheet; tap = 21 sats.
const ZAP_HOLD: Duration = Duration::from_millis(500);

/// What the row's toggles DO on their host. The host owns the optimistic
/// mirrors and the core publishes; the row only calls.
pub trait ActionHost: EventEmitter<Nav> + Sized + 'static {
    fn toggle_like(&mut self, ev: &Event, cx: &mut Context<Self>);
    /// The heart's right-click: open/close the row's reaction quick-set.
    fn toggle_reaction_picker(&mut self, id: &str, cx: &mut Context<Self>);
    /// A picked quick-set reaction (positive unicode glyphs; the heart
    /// stays the plain click).
    fn react_with(&mut self, ev: &Event, content: &str, cx: &mut Context<Self>);
    fn toggle_repost(&mut self, ev: &Event, cx: &mut Context<Self>);
    fn toggle_bookmark(&mut self, ev: &Event, cx: &mut Context<Self>);
    fn zap_tap(&mut self, ev: &Event, cx: &mut Context<Self>);
}

/// The glyph states at paint time (the host's mirrors, snapshotted per row).
#[derive(Clone, Copy)]
pub struct ActionState {
    pub liked: bool,
    pub reposted: bool,
    pub bookmarked: bool,
    pub zapped: bool,
    /// A zap is in flight on this note (the Feather one-per-note rule).
    pub zapping: bool,
    /// No wallet behind the tap → the dim glyph opens the wallet pane.
    pub wallet_ready: bool,
    /// Comment/reply count for the bubble (0 = bare glyph).
    pub replies: usize,
    /// The reaction quick-set is open on this row (the heart's
    /// right-click).
    pub picker_open: bool,
    pub reduced_motion: bool,
}

/// The reaction quick-set (the heart's right-click): fixed, positive —
/// the house posture never sends a negative reaction, and the heart
/// itself stays the plain click's default. Content `"+"` = the heart
/// (NIP-25's like marker, what every compatible client already counts);
/// the rest ride as their unicode body.
const QUICKSET: &[(&str, &str)] = &[
    ("👍", "thumbs up"),
    ("❤️", "heart"),
    ("😂", "laugh"),
    ("😮", "wow"),
    ("😢", "sad"),
];

/// The row: body→actions at S3 (8px), glyph buttons with the hover wash.
/// `key` seeds the element ids (the note id hex — unique per subtree).
pub(crate) fn action_row<H: ActionHost>(
    key: &str,
    ev: &Arc<Event>,
    st: ActionState,
    handle: Entity<H>,
) -> Div {
    let ev_for_reply = ev.clone();
    let ev_for_like = ev.clone();
    let ev_for_bookmark = ev.clone();
    let ev_for_repost = ev.clone();
    let ev_for_quote = ev.clone();
    let ev_for_zap = ev.clone();
    let handle_reply = handle.clone();
    let handle_like = handle.clone();
    let handle_picker = handle.clone();
    let handle_bookmark = handle.clone();
    let handle_repost = handle.clone();
    let handle_quote = handle.clone();
    let handle_zap = handle.clone();

    let row = div()
        .flex()
        .gap_5()
        .items_center()
        .mt_1()
        // The reply door (thread.rs reply-to): opens the composer carrying
        // this note as the parent.
        .child(
            div()
                .id(SharedString::from(format!("reply-{key}")))
                .cursor_pointer()
                .on_click(move |_, _, cx| {
                    let ev = ev_for_reply.clone();
                    handle_reply.update(cx, |_, cx| {
                        // The reply sheet (#21): replying never navigates
                        // away — a floating card over the current pane.
                        cx.emit(Nav::OpenReply((*ev).clone()));
                    });
                })
                .hover(|s| s.bg(theme::hover_wash()))
                .flex()
                .items_center()
                .gap_1()
                .child(icons::bubble_icon(theme::muted()))
                .children((st.replies > 0).then(|| {
                    div()
                        .text_size(px(theme::T_SMALL))
                        .text_color(rgb(theme::muted()))
                        .child(st.replies.to_string())
                })),
        )
        .child(
            div()
                .id(SharedString::from(format!("like-{key}")))
                .cursor_pointer()
                .on_click(move |_, _, cx| {
                    let ev = ev_for_like.clone();
                    handle_like.update(cx, |host, cx| host.toggle_like(&ev, cx));
                })
                // Right-click = the reaction quick-set (more than just the
                // heart): the bolt's pattern, one pane-local strip.
                .on_mouse_down(gpui::MouseButton::Right, {
                    let key = key.to_string();
                    let handle_picker = handle_picker.clone();
                    move |_, _, cx| {
                        handle_picker.update(cx, |host, cx| host.toggle_reaction_picker(&key, cx));
                    }
                })
                .hover(|s| s.bg(theme::hover_wash()))
                // The heart doubles as the quick-set's door; the click alone
                // doesn't say so.
                .tooltip(super::shell::tip(
                    "React: click to like, right-click for more",
                ))
                .child(icons::heart_icon(if st.liked {
                    theme::vermilion()
                } else {
                    theme::muted()
                })),
        )
        // The bolt (NIP-57, D20): tap = 21 sats, no dialog; hold (~500ms)
        // or right-click = the amount sheet. States: dim (no wallet) /
        // muted (idle) / amber breathing (in flight) / vermilion (settled).
        .child({
            let color = if st.zapping {
                theme::warn()
            } else if st.zapped {
                theme::vermilion()
            } else if !st.wallet_ready {
                theme::hairline()
            } else {
                theme::muted()
            };
            let start = std::rc::Rc::new(std::cell::Cell::new(Option::<std::time::Instant>::None));
            let bolt = div()
                .id(SharedString::from(format!("zap-{key}")))
                .cursor_pointer()
                .hover(|s| s.bg(theme::hover_wash()))
                // Right-click opens the sheet at once.
                .on_mouse_down(gpui::MouseButton::Right, {
                    let ev = ev_for_zap.clone();
                    let handle = handle_zap.clone();
                    move |_, _, cx| {
                        handle.update(cx, |_, cx| {
                            cx.emit(Nav::OpenZapSheet((*ev).clone()));
                        });
                    }
                })
                // Left: remember when the press began; the release decides
                // tap vs hold (no timer machinery, the decision is honest
                // at release).
                .on_mouse_down(gpui::MouseButton::Left, {
                    let start = start.clone();
                    move |_, _, _| start.set(Some(std::time::Instant::now()))
                })
                .on_mouse_up(gpui::MouseButton::Left, {
                    let start = start.clone();
                    let ev = ev_for_zap.clone();
                    let handle = handle_zap.clone();
                    move |_, _, cx| {
                        let Some(t0) = start.get() else { return };
                        start.set(None);
                        let held = t0.elapsed() >= ZAP_HOLD;
                        handle.update(cx, |host, cx| {
                            if held {
                                cx.emit(Nav::OpenZapSheet((*ev).clone()));
                            } else {
                                host.zap_tap(&ev, cx);
                            }
                        });
                    }
                })
                .child(icons::bolt_icon(color));
            // In flight and motion allowed: breathe (a slow opacity
            // triangle — payment pending, nothing more).
            if st.zapping && !st.reduced_motion {
                bolt.with_animation(
                    SharedString::from(format!("zap-pulse-{key}")),
                    gpui::Animation::new(Duration::from_millis(900)).repeat(),
                    |el, delta| el.opacity(0.55 - 0.25 * (2.0 * delta - 1.0).abs()),
                )
                .into_any_element()
            } else if st.zapping {
                // Reduced motion: a steady mid-amber, no pulsing.
                div().opacity(0.7).child(bolt).into_any_element()
            } else {
                bolt.into_any_element()
            }
        })
        .child(
            div()
                .id(SharedString::from(format!("repost-{key}")))
                .cursor_pointer()
                .on_click(move |_, _, cx| {
                    let ev = ev_for_repost.clone();
                    handle_repost.update(cx, |host, cx| host.toggle_repost(&ev, cx));
                })
                .hover(|s| s.bg(theme::hover_wash()))
                .child(icons::repost_icon(if st.reposted {
                    theme::vermilion()
                } else {
                    theme::muted()
                })),
        )
        .child(
            div()
                .id(SharedString::from(format!("quote-{key}")))
                .cursor_pointer()
                .on_click(move |_, _, cx| {
                    let ev = ev_for_quote.clone();
                    handle_quote.update(cx, |_, cx| {
                        cx.emit(Nav::OpenQuote((*ev).clone()));
                    });
                })
                .hover(|s| s.bg(theme::hover_wash()))
                .child(icons::quote_icon(theme::muted())),
        )
        .child(
            div()
                .id(SharedString::from(format!("bookmark-{key}")))
                .cursor_pointer()
                .on_click(move |_, _, cx| {
                    let ev = ev_for_bookmark.clone();
                    handle_bookmark.update(cx, |host, cx| host.toggle_bookmark(&ev, cx));
                })
                .hover(|s| s.bg(theme::hover_wash()))
                .child(icons::bookmark_icon(if st.bookmarked {
                    theme::vermilion()
                } else {
                    theme::muted()
                })),
        );
    // The quick-set strip (the heart's right-click): pane-local, beneath
    // the row — no overlay sheet for a five-glyph choice. A pick closes
    // it (react_with); a second right-click toggles it.
    if st.picker_open {
        let mut strip = div().flex().gap_3().items_center().mt_1();
        for (glyph, label) in QUICKSET {
            let ev = ev.clone();
            let handle = handle.clone();
            strip = strip.child(
                div()
                    .id(SharedString::from(format!("react-{key}-{label}")))
                    .cursor_pointer()
                    .px_1()
                    .rounded(px(theme::R_EMBED))
                    .hover(|s| s.bg(theme::hover_wash()))
                    .on_click(move |_, _, cx| {
                        let ev = ev.clone();
                        let content = (*glyph).to_string();
                        handle.update(cx, |host, cx| host.react_with(&ev, &content, cx));
                    })
                    .text_size(px(theme::T_BODY))
                    // The color emoji face as the run's primary font (see
                    // theme::EMOJI): through the UI face these five paint
                    // as DejaVu/Adwaita monochrome outlines.
                    .font_family(theme::EMOJI)
                    .tooltip(super::shell::tip(label))
                    .child(*glyph),
            );
        }
        div().w_full().flex().flex_col().child(row).child(strip)
    } else {
        div().w_full().flex().flex_col().child(row)
    }
}

/// The paint-time reduced-motion pref (desktop.rs's portal read; the same
/// mirror feed.rs's render uses).
pub(crate) fn reduced_motion(cx: &mut gpui::App) -> bool {
    desktop::Desktop::get(cx).reduced_motion
}
