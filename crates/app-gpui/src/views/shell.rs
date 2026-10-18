//! The window shell: header + navigation stack over the views.
//!
//! TODO(port): crates/app/src/shell.rs (AdwNavigationSplitView + header bar)
//! and main.rs Escape-to-back — P1. The GTK sidebar collapses for now; the
//! sumi-e header carries back/title until P1's sidebar pass. Escape-to-back is
//! ported as a root key-down listener (gpui actions land with the composer in
//! P2, when there are more keys to bind).

use std::sync::Arc;

use gpui::{
    EventEmitter,
    div, img, prelude::*, px, rgb, App, Context, Entity, FocusHandle, Focusable, KeyDownEvent,
    ObjectFit, Render, Window,
};
use nostr::prelude::*;
use satori_core::manager::{Core, FeedEvent};

use crate::{media, theme};
use super::{
    composer::{Draft, Composer},
    feed::Feed,
    nav::Nav,
    notifications::NotificationsView,
    profile::ProfileView,
    search::SearchView,
    settings::SettingsView,
    thread::ThreadView,
};

pub enum Pane {
    Feed(Entity<Feed>),
    Thread(Entity<ThreadView>),
    Profile(Entity<ProfileView>),
    Notifications(Entity<NotificationsView>),
    Search(Entity<SearchView>),
    Settings(Entity<SettingsView>),
    Composer(Entity<Composer>),
}

pub struct Shell {
    core: Arc<Core>,
    focus: FocusHandle,
    /// First render claims keyboard focus for the shell handle — without it
    /// nothing is focused and the N/escape shortcuts are dead until a click.
    focused_once: bool,
    /// Set when a pane was popped: the popped pane's Input may still hold
    /// window focus, dead-keying the shell until the next render refocuses.
    refocus: bool,
    /// The lightbox overlay — full-screen image viewer over any pane
    /// (lightbox.rs: Esc/‹/› keys, click closes).
    lightbox: Option<(Vec<String>, usize)>,
    /// Bell dot: anything newer than the read-state high-water? Polled every
    /// 90s like the GTK shell (poll_notif); cleared when the pane opens.
    notif_unread: bool,
    stack: Vec<Pane>,
}

impl Shell {
    pub fn new(core: Arc<Core>, cx: &mut Context<Self>) -> Self {
        let feed = cx.new(|cx| Feed::new(core.clone(), cx));
        cx.subscribe(&feed, |this, _feed, ev: &Nav, cx| match ev {
            Nav::OpenThread(id) => this.open_thread(*id, cx),
            Nav::OpenProfile(who) => this.open_profile(*who, cx),
            Nav::OpenComposer(reply) => this.open_composer(reply.clone(), cx),
            Nav::OpenLightbox(urls, idx) => this.open_lightbox(urls.clone(), *idx, cx),
            Nav::Pop => this.back(cx),
        })
        .detach();
        let mut shell = Self {
            core,
            focus: cx.focus_handle(),
            focused_once: false,
            refocus: false,
            lightbox: None,
            notif_unread: false,
            stack: vec![],
        };
        shell.stack.push(Pane::Feed(feed));
        shell.watch_notif(cx);
        shell
    }

    fn subscribe_nav<V: EventEmitter<Nav> + 'static>(&self, view: &Entity<V>, cx: &mut Context<Self>) {
        cx.subscribe(view, |this, _view, ev: &Nav, cx| match ev {
            Nav::OpenThread(id) => this.open_thread(*id, cx),
            Nav::OpenProfile(who) => this.open_profile(*who, cx),
            Nav::OpenComposer(reply) => this.open_composer(reply.clone(), cx),
            Nav::OpenLightbox(urls, idx) => this.open_lightbox(urls.clone(), *idx, cx),
            Nav::Pop => this.back(cx),
        })
        .detach();
    }

    fn open_thread(&mut self, id: EventId, cx: &mut Context<Self>) {
        // TODO(port): crates/app/src/thread.rs thread_page_by_id — flat replies
        // done; reply nesting (nest_replies, depth <= 3) still pending.
        let view = cx.new(|cx| ThreadView::new(self.core.clone(), id, cx));
        self.subscribe_nav(&view, cx);
        self.stack.push(Pane::Thread(view));
        cx.notify();
    }

    fn open_profile(&mut self, who: PublicKey, cx: &mut Context<Self>) {
        let view = cx.new(|cx| ProfileView::new(self.core.clone(), who, cx));
        self.subscribe_nav(&view, cx);
        self.stack.push(Pane::Profile(view));
        cx.notify();
    }

    fn open_composer(&mut self, reply_to: Option<Event>, cx: &mut Context<Self>) {
        let view = cx.new(|cx| Composer::new(self.core.clone(), reply_to, cx));
        self.subscribe_nav(&view, cx);
        self.stack.push(Pane::Composer(view));
        cx.notify();
    }

    fn open_settings(&mut self, cx: &mut Context<Self>) {
        let view = cx.new(|cx| SettingsView::new(self.core.clone(), cx));
        self.stack.push(Pane::Settings(view));
        cx.notify();
    }

    /// The notifications pane. Opening marks seen (the view constructor does
    /// the read-state advance); the dot goes quiet at once and the next poll
    /// confirms.
    fn open_notifications(&mut self, cx: &mut Context<Self>) {
        let view = cx.new(|cx| NotificationsView::new(self.core.clone(), cx));
        self.subscribe_nav(&view, cx);
        self.stack.push(Pane::Notifications(view));
        self.notif_unread = false;
        cx.notify();
    }

    fn open_search(&mut self, cx: &mut Context<Self>) {
        let view = cx.new(|cx| SearchView::new(self.core.clone(), cx));
        self.subscribe_nav(&view, cx);
        self.stack.push(Pane::Search(view));
        cx.notify();
    }

    /// Bell-dot poll, every 90s (shell.rs poll_notif): one notifications()
    /// drain, kept until the NotifDot verdict.
    fn watch_notif(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(90))
                    .await;
                let _ = this.update(cx, |shell, cx| shell.check_notif_dot(cx));
            }
        })
        .detach();
    }

    fn check_notif_dot(&mut self, cx: &mut Context<Self>) {
        let mut rx = self.core.notifications();
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.recv().await {
                match ev {
                    FeedEvent::NotifDot(unread) => {
                        let _ = this.update(cx, |shell, cx| {
                            shell.notif_unread = unread;
                            cx.notify();
                        });
                        break;
                    }
                    FeedEvent::Failed(_) => break,
                    _ => {}
                }
            }
        })
        .detach();
    }

    /// The lightbox: show `urls[idx]` full-screen (lightbox.rs open). Every
    /// URL is pre-joined into the shared media cache so ‹/› are instant once
    /// tiles have loaded.
    fn open_lightbox(&mut self, urls: Vec<String>, idx: usize, cx: &mut Context<Self>) {
        for url in &urls {
            media::fetch(&self.core, url.clone(), cx, |_, _, _, cx| cx.notify());
        }
        self.lightbox = Some((urls, idx));
        cx.notify();
    }

    fn lightbox_nav(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some((urls, idx)) = self.lightbox.as_mut()
            && !urls.is_empty()
        {
            *idx = (*idx as isize + delta).rem_euclid(urls.len() as isize) as usize;
            cx.notify();
        }
    }

    fn back(&mut self, cx: &mut Context<Self>) {
        if self.stack.len() > 1 {
            // Closing a composer stashes the top-level buffer (composer.rs:288);
            // reply composers leave the draft untouched (`draft_candidate`).
            if let Some(Pane::Composer(c)) = self.stack.last()
                && let Some(text) = c.read(cx).draft_candidate(cx)
            {
                cx.default_global::<Draft>().0 = Some(text);
            }
            self.stack.pop();
            self.refocus = true;
            cx.notify();
        }
    }

    fn title(&self) -> &'static str {
        match self.stack.last() {
            Some(Pane::Feed(_)) => "Feed",
            Some(Pane::Thread(_)) => "Thread",
            Some(Pane::Profile(_)) => "Profile",
            Some(Pane::Notifications(_)) => "Notifications",
            Some(Pane::Search(_)) => "Search",
            Some(Pane::Settings(_)) => "Settings",
            Some(Pane::Composer(_)) => "Compose",
            None => "Satori",
        }
    }

    fn gear(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("open-settings")
            .cursor_pointer()
            .p_1()
            .text_size(px(13.0))
            .text_color(rgb(theme::MUTED))
            .on_click(cx.listener(|shell, _, _, cx| shell.open_settings(cx)))
            .child("Settings")
    }

    /// The compose seal: a quiet header affordance, the only way in.
    fn compose_seal(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("open-compose")
            .cursor_pointer()
            .p_1()
            .px_2()
            .text_size(px(13.0))
            .text_color(rgb(theme::VERMILION))
            .on_click(cx.listener(|shell, _, _, cx| shell.open_composer(None, cx)))
            .child("Compose")
    }

    /// The notifications seal with its bell dot (layout.ts notifBell — no
    /// counts, no badges, just a dot).
    fn notif_seal(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("open-notifications")
            .cursor_pointer()
            .flex()
            .items_center()
            .gap_1()
            .p_1()
            .on_click(cx.listener(|shell, _, _, cx| shell.open_notifications(cx)))
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(rgb(theme::MUTED))
                    .child("Notifs"),
            )
            .when(self.notif_unread, |d| {
                d.child(
                    div()
                        .size(px(6.))
                        .rounded_full()
                        .bg(rgb(theme::VERMILION)),
                )
            })
    }
}

impl Focusable for Shell {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let depth = self.stack.len();
        let title = self.title();
        let pane = self.stack.last().unwrap();
        if !self.focused_once || self.refocus {
            self.focused_once = true;
            self.refocus = false;
            window.focus(&self.focus);
        }

        div()
            .relative()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|shell, ev: &KeyDownEvent, _, cx| {
                // The lightbox owns the keyboard while it's up: Esc closes it
                // instead of popping the pane; arrows page through.
                if shell.lightbox.is_some() {
                    match ev.keystroke.key.as_str() {
                        "escape" => shell.lightbox = None,
                        "left" => shell.lightbox_nav(-1, cx),
                        "right" => shell.lightbox_nav(1, cx),
                        _ => {}
                    }
                    cx.notify();
                    return;
                }
                // TODO(port): crates/app/src/main.rs escape-to-back — P1.
                if ev.keystroke.key == "escape" {
                    shell.back(cx);
                    return;
                }
                // TODO(port): crates/app/src/main.rs — "N composes" (app.compose
                // accel). GTK's TextViews consume plain keys, so the accel only
                // fires outside text; here the guard is "a text surface is
                // open". The Input handles characters via the IME path and
                // lets the key events bubble, so anything typed into the
                // search field would also fire these bindings (an "n" in
                // "nostr" opened the composer) — the composer guard is its
                // own pane, the search field needs the same.
                let composing = matches!(
                    shell.stack.last(),
                    Some(Pane::Composer(_)) | Some(Pane::Search(_)) | Some(Pane::Settings(_))
                );
                if !ev.keystroke.modifiers.modified() && !composing {
                    if ev.keystroke.key == "n" {
                        shell.open_composer(None, cx);
                    }
                    // No GTK accel — the header seals are unclickable when the
                    // window is CSD-tiled (§9.1 finding), so the keyboard is
                    // the reliable way in.
                    if ev.keystroke.key == "u" {
                        shell.open_notifications(cx);
                    }
                    // TODO(port): crates/app/src/main.rs — "/" opens search
                    // (app.goto-search accel).
                    if ev.keystroke.key == "/" {
                        shell.open_search(cx);
                    }
                    // No GTK accel — same CSD story: the header Settings seal
                    // needs a keyboard way in.
                    if ev.keystroke.key == "s" {
                        shell.open_settings(cx);
                    }
                }
            }))
            .size_full()
            .bg(rgb(theme::WASHI))
            .text_color(rgb(theme::INK))
            .font_family(theme::SANS)
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .py_2()
                    .border_b_1()
                    .border_color(rgb(theme::HAIRLINE))
                    .child(if depth > 1 {
                        div()
                            .id("back")
                            .cursor_pointer()
                            .text_size(px(13.0))
                            .text_color(rgb(theme::MUTED))
                            .on_click(cx.listener(|shell, _, _, cx| shell.back(cx)))
                            .child("‹ Back")
                            .into_any_element()
                    } else {
                        div()
                            .text_size(px(13.0))
                            .text_color(rgb(theme::MUTED))
                            .child("Satori")
                            .into_any_element()
                    })
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(13.0))
                            .text_color(rgb(theme::INK))
                            .child(title),
                    )
                    .when(!matches!(pane, Pane::Composer(_)), |h| {
                        h.child(self.notif_seal(cx).into_any_element())
                            .child(self.compose_seal(cx).into_any_element())
                    })
                    .child(self.gear(cx).into_any_element()),
            )
            .child(match pane {
                Pane::Feed(view) => view.clone().into_any_element(),
                Pane::Thread(view) => view.clone().into_any_element(),
                Pane::Profile(view) => view.clone().into_any_element(),
                Pane::Notifications(view) => view.clone().into_any_element(),
                Pane::Search(view) => view.clone().into_any_element(),
                Pane::Settings(view) => view.clone().into_any_element(),
                Pane::Composer(view) => view.clone().into_any_element(),
            })
            .when_some(self.lightbox.clone(), |d, (urls, idx)| {
                d.child(lightbox_overlay(&urls, idx, cx))
            })
    }
}

/// The lightbox overlay (lightbox.rs): dark full-screen viewer, fit-to-screen
/// image, ‹ › pager zones, Esc/arrow keys, click-to-advance on the picture.
fn lightbox_overlay(urls: &[String], idx: usize, cx: &mut Context<Shell>) -> impl IntoElement {
    let snap = media::Snapshot::new(cx);
    let n = urls.len();
    let png = urls.get(idx).and_then(|u| snap.png(u));
    let caption = format!("{} / {}", idx + 1, n);

    div()
        .absolute()
        .inset_0()
        .bg(rgb(0x000000))
        .opacity(0.94)
        .flex()
        .flex_col()
        .font_family(theme::SANS)
        // Top bar: index + close.
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .px_4()
                .py_2()
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(rgb(0xb8b2a6))
                        .child(caption),
                )
                .child(
                    div()
                        .id("lb-close")
                        .cursor_pointer()
                        .px_2()
                        .text_size(px(13.0))
                        .text_color(rgb(0xb8b2a6))
                        .on_click(cx.listener(|shell, _, _, cx| {
                            shell.lightbox = None;
                            cx.notify();
                        }))
                        .child("✕"),
                ),
        )
        // Middle: pager zones flanking the fit-to-screen image.
        .child(
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .child(
                    div()
                        .id("lb-prev")
                        .cursor_pointer()
                        .w(px(64.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .on_click(cx.listener(|shell, _, _, cx| shell.lightbox_nav(-1, cx)))
                        .child(
                            div()
                                .text_size(px(22.0))
                                .text_color(rgb(0xb8b2a6))
                                .child("‹"),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .p_2()
                        .child(match png {
                            Some(p) => img(p)
                                .size_full()
                                .object_fit(ObjectFit::Contain)
                                .into_any_element(),
                            None => div()
                                .text_size(px(12.0))
                                .text_color(rgb(0xb8b2a6))
                                .child("…")
                                .into_any_element(),
                        }),
                )
                .child(
                    div()
                        .id("lb-next")
                        .cursor_pointer()
                        .w(px(64.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .on_click(cx.listener(|shell, _, _, cx| shell.lightbox_nav(1, cx)))
                        .child(
                            div()
                                .text_size(px(22.0))
                                .text_color(rgb(0xb8b2a6))
                                .child("›"),
                        ),
                ),
        )
        .child(
            // Clicking anywhere else in the dark closes (lightbox.rs Esc).
            div()
                .id("lb-backdrop")
                .flex_1()
                .cursor_pointer()
                .on_click(cx.listener(|shell, _, _, cx| {
                    shell.lightbox = None;
                    cx.notify();
                })),
        )
}
