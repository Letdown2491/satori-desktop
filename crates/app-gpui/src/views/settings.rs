//! Settings view (P1 slice): relays + media servers, read-only.
//!
//! TODO(port): crates/app/src/settings.rs — P3. The GTK pane's editing (relay
//! add/remove, media server management, appearance/content prefs) ports once
//! forms/inputs exist. P1 shows the live configuration so the route is real.

use std::sync::Arc;

use gpui::{div, prelude::*, px, rgb, Context, Render, Window};
use satori_core::manager::{Core, FeedEvent};

use crate::theme;

pub struct SettingsView {
    core: Arc<Core>,
    read_relays: Vec<String>,
    write_relays: Vec<String>,
    media_servers: Vec<String>,
    status: String,
}

impl SettingsView {
    pub fn new(core: Arc<Core>, cx: &mut Context<Self>) -> Self {
        let (read, write) = core.current_relays();
        let view = Self {
            read_relays: read,
            write_relays: write,
            media_servers: vec![],
            status: String::new(),
            core,
        };
        let rx = view.core.fetch_media_servers();
        view.pump(rx, cx);
        view
    }

    fn pump(&self, mut rx: satori_core::manager::FeedRx, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.recv().await {
                let _ = this.update(cx, |this, cx| this.on_event(ev, cx));
            }
        })
        .detach();
    }

    fn on_event(&mut self, ev: FeedEvent, cx: &mut Context<Self>) {
        match ev {
            FeedEvent::MediaServers(servers) => self.media_servers = servers,
            FeedEvent::Failed(e) => self.status = format!("… {e}"),
            _ => {}
        }
        cx.notify();
    }

    fn section(title: &str, items: &[String], fallback: &str) -> impl IntoElement {
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .p_4()
            .border_b_1()
            .border_color(rgb(theme::HAIRLINE))
            .child(
                div()
                    .text_size(px(12.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(rgb(theme::MUTED))
                    .child(title.to_string()),
            )
            .child(if items.is_empty() {
                div()
                    .text_size(px(13.0))
                    .text_color(rgb(theme::MUTED))
                    .child(fallback.to_string())
            } else {
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(items.iter().map(|item| {
                        div()
                            .text_size(px(13.0))
                            .text_color(rgb(theme::INK))
                            .child(item.clone())
                    }))
            })
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let read = self.read_relays.clone();
        let write = self.write_relays.clone();
        let media = self.media_servers.clone();
        let status = self.status.clone();
        let core_refresh = self.core.clone();

        div()
            .id("settings-root")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .child(Self::section(
                "Read relays",
                &read,
                "none configured",
            ))
            .child(Self::section(
                "Write relays",
                &write,
                "none configured",
            ))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_4()
                    .border_b_1()
                    .border_color(rgb(theme::HAIRLINE))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(theme::MUTED))
                            .child("Media servers (Blossom)"),
                    )
                    .child(if media.is_empty() {
                        div()
                            .text_size(px(13.0))
                            .text_color(rgb(theme::MUTED))
                            .child("default server")
                    } else {
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .children(
                                media.iter().map(|s| {
                                    div()
                                        .text_size(px(13.0))
                                        .text_color(rgb(theme::INK))
                                        .child(s.clone())
                                }),
                            )
                    })
                    .child(
                        div()
                            .id("refresh-media")
                            .cursor_pointer()
                            .mt_1()
                            .text_size(px(12.0))
                            .text_color(rgb(theme::MUTED))
                            .on_click(move |_, _, _cx| {
                                // TODO(port): settings.rs media server editing —
                                // P3. Refresh-only until inputs exist.
                                let mut rx = core_refresh.fetch_media_servers();
                                std::thread::spawn(move || {
                                    while rx.blocking_recv().is_some() {}
                                });
                            })
                            .child("refresh"),
                    ),
            )
            .when(!status.is_empty(), |d| {
                d.child(
                    div()
                        .p_4()
                        .text_size(px(12.0))
                        .text_color(rgb(theme::ERR))
                        .child(status),
                )
            })
    }
}
