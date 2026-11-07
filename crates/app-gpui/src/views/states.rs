//! Shared pane states — empty / loading / error / offline — defined once
//! per class (0.2 design spec §4): every list surface renders these from
//! one helper, only the copy varies. No spinners anywhere (§4.2: the
//! ellipsis is enough; the ensō is the only animation). No new color, no
//! new motion — the strip is a state, it does not travel.

use gpui::{div, prelude::*, px, rgb, AnyElement, ClickEvent, FontWeight, Window};

use crate::theme;

/// §4.2: loading copy, normalized — lowercase first letter, exactly one
/// trailing ellipsis (notifications' "Gathering…" was the only capitalized
/// line; deficiency 10).
pub fn loading(msg: &str) -> String {
    let t = msg
        .trim()
        .trim_end_matches('…')
        .trim_end_matches('.')
        .trim_end();
    let mut chars = t.chars();
    match chars.next() {
        Some(first) => format!("{}{}…", first.to_lowercase(), chars.as_str()),
        None => "…".into(),
    }
}

/// §4.1: one quiet centered line + one serif whisper where the moment
/// deserves it. T_UI, muted, generous padding; no icon, no illustration,
/// no inline retry (retry is the error strip's job).
pub fn empty(line: &str, whisper: Option<&str>) -> AnyElement {
    div()
        .w_full()
        .p_8()
        .text_color(rgb(theme::muted()))
        .child(
            div()
                .text_size(px(theme::T_UI))
                .child(line.to_string()),
        )
        .children(whisper.map(|w| {
            div()
                .font_family(theme::SERIF)
                .text_size(px(17.0))
                .mt_2()
                .child(w.to_string())
        }))
        .into_any_element()
}

/// §4.3 — a kinded pane error (D22 kinds media; this kinds failures the
/// same way): `transient` offers Retry (a relay round trip can succeed on
/// a second ask), `verdict` doesn't (not found is not retryable). The
/// copy pattern is "<What> didn't answer." — the raw error string is a
/// secondary whisper, not the headline.
#[derive(Clone)]
pub struct PaneError {
    pub what: String,
    pub detail: String,
    pub retryable: bool,
}

impl PaneError {
    pub fn transient(what: &str, detail: &str) -> Self {
        Self {
            what: what.into(),
            detail: detail.into(),
            retryable: true,
        }
    }
    #[allow(dead_code)] // verdict errors arrive with WS4's kinded surfaces
    pub fn verdict(what: &str, detail: &str) -> Self {
        Self {
            what: what.into(),
            detail: detail.into(),
            retryable: false,
        }
    }
}

/// The dismissible inline strip on `panel()` with `err()` text and a quiet
/// Retry action (§4.3). Retryability comes from the [`PaneError`]'s kind;
/// `on_retry`/`on_dismiss` are the view's `cx.listener` closures.
pub fn error_strip(
    err: &PaneError,
    on_retry: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
    on_dismiss: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> AnyElement {
    div()
        .w_full()
        .px_4()
        .py_3()
        .child(
            div()
                .max_w(px(640.0)) // the reading column (content::COL_W)
                .rounded(px(theme::R_EMBED))
                .border_1()
                .border_color(rgb(theme::hairline()))
                .bg(rgb(theme::panel()))
                .px_4()
                .py_3()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(theme::T_UI))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(rgb(theme::err()))
                                .child(format!("{} didn't answer.", err.what)),
                        )
                        .child(
                            div()
                                .id("error-dismiss")
                                .cursor_pointer()
                                .text_size(px(theme::T_SMALL))
                                .text_color(rgb(theme::muted()))
                                .hover(|s| s.text_color(rgb(theme::ink())))
                                .on_click(on_dismiss)
                                .child("×"),
                        ),
                )
                .child(
                    div()
                        .text_size(px(theme::T_SMALL))
                        .text_color(theme::ink_faint())
                        .child(err.detail.clone()),
                )
                .when(err.retryable, |d| {
                    d.child(
                        div()
                            .id("error-retry")
                            .cursor_pointer()
                            .text_size(px(theme::T_SMALL))
                            .child(theme::pressable(div().child("Retry")))
                            .on_click(on_retry),
                    )
                }),
        )
        .into_any_element()
}

/// §4.4 — the offline / degraded state: a pill-shaped status on `warn()`
/// amber. States, not errors (D26's split): Tor-mode warm-up and the
/// outbox's "delivering…" honesty state render through this. (Wired by
/// WS4's shell work — rollout step 2 absorbs those surfaces.)
#[allow(dead_code)]
pub fn offline_pill(text: &str) -> AnyElement {
    div()
        .w_full()
        .flex()
        .justify_center()
        .child(
            div()
                .flex_none()
                .px_4()
                .py_1p5()
                .rounded_full()
                .border_1()
                .border_color(rgb(theme::warn()))
                .text_size(px(theme::T_SMALL))
                .text_color(rgb(theme::warn()))
                .child(text.to_string()),
        )
        .into_any_element()
}
