//! The window shell: header + navigation stack over the views.
//!
//! TODO(port): crates/app/src/shell.rs (AdwNavigationSplitView + header bar)
//! and main.rs Escape-to-back — P1. The GTK sidebar collapses for now; the
//! sumi-e header carries back/title until P1's sidebar pass. Escape-to-back is
//! ported as a root key-down listener (gpui actions land with the composer in
//! P2, when there are more keys to bind).

use std::sync::Arc;

use gpui::{
    ClickEvent, Div, EventEmitter,
    div, img, prelude::*, px, rgb, App, Context, Entity, FocusHandle, Focusable, KeyDownEvent,
    AnyView, ObjectFit, Render, SharedString, Stateful, Window,
};
use nostr::prelude::*;
use satori_core::manager::{Core, FeedEvent};

use crate::{icons, media, theme};
use super::{
    composer::{Draft, Composer},
    content,
    feed::Feed,
    nav::Nav,
    notifications::NotificationsView,
    profile::ProfileView,
    reader::ArticleView,
    search::SearchView,
    settings::SettingsView,
    thread::ThreadView,
};

#[derive(Clone)]
pub enum Pane {
    Feed(Entity<Feed>),
    Thread(Entity<ThreadView>),
    Profile(Entity<ProfileView>),
    Notifications(Entity<NotificationsView>),
    Search(Entity<SearchView>),
    Settings(Entity<SettingsView>),
    Article(Entity<ArticleView>),
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
    /// The feed's deep-scroll state (Nav::FeedDeep): the floating
    /// back-to-top whisper only shows once the reader is well below the top.
    feed_deep: bool,
    stack: Vec<Pane>,
}

impl Shell {
    pub fn new(core: Arc<Core>, cx: &mut Context<Self>) -> Self {
        let feed = cx.new(|cx| Feed::new(core.clone(), cx));
        cx.subscribe(&feed, |this, _feed, ev: &Nav, cx| match ev {
            Nav::OpenThread(id) => this.open_thread(*id, cx),
            Nav::OpenProfile(who) => this.open_profile(*who, cx),
            Nav::OpenComposer(reply) => this.open_composer(reply.clone(), cx),
            Nav::OpenArticle(ev) => this.open_article(ev.clone(), cx),
            Nav::OpenLightbox(urls, idx) => this.open_lightbox(urls.clone(), *idx, cx),
            Nav::FeedDeep(deep) => {
                this.feed_deep = *deep;
                cx.notify();
            }
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
            feed_deep: false,
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
            Nav::OpenArticle(ev) => this.open_article(ev.clone(), cx),
            Nav::OpenLightbox(urls, idx) => this.open_lightbox(urls.clone(), *idx, cx),
            Nav::FeedDeep(deep) => {
                this.feed_deep = *deep;
                cx.notify();
            }
            Nav::Pop => this.back(cx),
        })
        .detach();
    }

    fn open_thread(&mut self, id: EventId, cx: &mut Context<Self>) {
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

    fn open_article(&mut self, ev: Event, cx: &mut Context<Self>) {
        let view = cx.new(|cx| ArticleView::new(self.core.clone(), ev, cx));
        self.subscribe_nav(&view, cx);
        self.stack.push(Pane::Article(view));
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
            Some(Pane::Article(_)) => "Read",
            Some(Pane::Composer(_)) => "Compose",
            None => "Satori",
        }
    }

}

/// A rail tooltip: ink chip, washi text, nothing more. gpui paints these
/// itself near the cursor after a hover (window.rs prepaint_tooltip).
struct Tip {
    label: &'static str,
}
impl Render for Tip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded(px(3.0))
            .bg(rgb(theme::INK))
            .text_color(rgb(theme::WASHI))
            .text_size(px(11.5))
            .font_family(theme::SANS)
            .child(self.label)
    }
}
/// The `.tooltip(...)` builder for a static label.
fn tip(label: &'static str) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    move |_, cx| cx.new(|_| Tip { label }).into()
}

// --- the rail (P1's sidebar pass) ------------------------------------------

/// Which rail spot (if any) the active pane corresponds to — the tick marks
/// the active spot, and re-clicking it is an affordance (feed: back to top).
#[derive(Clone, Copy, PartialEq)]
enum Spot {
    Feed,
    Search,
    Notif,
    Compose,
    Settings,
}

impl Shell {
    fn active_spot(&self) -> Option<Spot> {
        match self.stack.last() {
            Some(Pane::Feed(_)) => Some(Spot::Feed),
            Some(Pane::Search(_)) => Some(Spot::Search),
            Some(Pane::Notifications(_)) => Some(Spot::Notif),
            Some(Pane::Composer(_)) => Some(Spot::Compose),
            Some(Pane::Settings(_)) => Some(Spot::Settings),
            _ => None,
        }
    }

    /// Home: back to the feed, unstacking whatever depth we're at (back()
    /// handles each pop, including the composer draft stash).
    fn go_home(&mut self, cx: &mut Context<Self>) {
        while self.stack.len() > 1 {
            self.back(cx);
        }
    }

    /// The feed pane's scroll-to-top (rail re-click, ensō, the whisper).
    fn feed_to_top(&self, cx: &mut Context<Self>) {
        if let Some(Pane::Feed(view)) = self.stack.last().cloned() {
            view.update(cx, |feed, _| feed.scroll_top());
        }
    }

    fn rail(&self, cx: &mut Context<Self>) -> Div {
        let active = self.active_spot();
        let ink = |on: bool| if on { theme::INK } else { theme::MUTED };
        let feed_active = active == Some(Spot::Feed);

        div()
            .flex_none()
            .h_full()
            .w(px(52.0))
            .flex()
            .flex_col()
            .items_center()
            .pt(px(14.0))
            .gap_1()
            // The ensō: home. Same as the feed mark, drawn heavier.
            .child(self.rail_button(
                // The ensō is the logo that happens to lead home — it never
                // wears the active tick.
                "rail-home",
                "Home",
                icons::enso_icon(ink(feed_active), 22.0),
                false,
                cx.listener(move |shell, _, _, cx| {
                    if feed_active {
                        shell.feed_to_top(cx);
                    } else {
                        shell.go_home(cx);
                    }
                }),
            ))
            .child(self.rail_button(
                "rail-feed",
                "Feed",
                icons::stream_icon(ink(feed_active)),
                feed_active,
                cx.listener(move |shell, _, _, cx| {
                    if feed_active {
                        // Re-click the active spot: back to the top.
                        shell.feed_to_top(cx);
                    } else {
                        shell.go_home(cx);
                    }
                }),
            ))
            .child(self.rail_button(
                "rail-search",
                "Search",
                icons::search_icon(ink(active == Some(Spot::Search))),
                active == Some(Spot::Search),
                cx.listener(move |shell, _, _, cx| {
                    if shell.active_spot() != Some(Spot::Search) {
                        shell.open_search(cx);
                    }
                }),
            ))
            .child(self.rail_button(
                "rail-notifications",
                "Notifications",
                div()
                    .relative()
                    .child(icons::bell_icon(ink(active == Some(Spot::Notif))))
                    .when(self.notif_unread, |d| {
                        d.child(
                            div()
                                .absolute()
                                .top(px(-1.))
                                .right(px(-1.))
                                .size(px(6.))
                                .rounded_full()
                                .bg(rgb(theme::VERMILION)),
                        )
                    }),
                active == Some(Spot::Notif),
                cx.listener(move |shell, _, _, cx| {
                    if shell.active_spot() != Some(Spot::Notif) {
                        shell.open_notifications(cx);
                    }
                }),
            ))
            // Compose speaks (it broadcasts) — the pen wears vermilion.
            .child(self.rail_button(
                "rail-compose",
                "Compose",
                icons::pen_icon(theme::VERMILION),
                active == Some(Spot::Compose),
                cx.listener(move |shell, _, _, cx| {
                    // Never clobber an open composer (its draft is live).
                    if shell.active_spot() != Some(Spot::Compose) {
                        shell.open_composer(None, cx);
                    }
                }),
            ))
            .child(self.rail_button(
                "rail-settings",
                "Settings",
                icons::gear_icon(ink(active == Some(Spot::Settings))),
                active == Some(Spot::Settings),
                cx.listener(move |shell, _, _, cx| {
                    if shell.active_spot() != Some(Spot::Settings) {
                        shell.open_settings(cx);
                    }
                }),
            ))
    }

    fn rail_button(
        &self,
        id: &'static str,
        label: &'static str,
        glyph: impl IntoElement + 'static,
        active: bool,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Stateful<Div> {
        div()
            .id(SharedString::from(id))
            .relative()
            .cursor_pointer()
            .w(px(52.0))
            .py(px(9.0))
            .flex()
            .justify_center()
            .tooltip(tip(label))
            .on_click(on_click)
            // The active tick: a small vermilion brush accent, not a pill.
            .when(active, |d| {
                d.child(
                    div()
                        .absolute()
                        .left(px(11.0))
                        .top(px(11.0))
                        .bottom(px(11.0))
                        .w(px(2.0))
                        .rounded_full()
                        .bg(rgb(theme::VERMILION)),
                )
            })
            .child(glyph)
    }

    /// The deep panes' quiet top row: back + title. The feed (depth 1) has
    /// none — the rail's ensō is home and the stream needs no label.
    fn title_row(&self, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .items_center()
            .gap_3()
            .px_4()
            .pt(px(8.0))
            .pb(px(4.0))
            .child(
                div()
                    .id("back")
                    .cursor_pointer()
                    .text_size(px(13.0))
                    .text_color(rgb(theme::MUTED))
                    .on_click(cx.listener(|shell, _, _, cx| shell.back(cx)))
                    .child("‹ Back"),
            )
            .child(
                div()
                    .flex_1()
                    .text_size(px(13.0))
                    .text_color(rgb(theme::INK))
                    .child(self.title()),
            )
    }

    /// The floating back-to-top whisper — bottom-right of the content
    /// column, only when the feed is scrolled well down.
    fn floating_top(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        div()
            .id("back-to-top")
            .absolute()
            .bottom(px(20.0))
            .right(px(24.0))
            .size(px(38.0))
            .rounded_full()
            .cursor_pointer()
            .border_1()
            .border_color(rgb(theme::HAIRLINE))
            .bg(rgb(theme::WASHI))
            .flex()
            .items_center()
            .justify_center()
            .tooltip(tip("Back to top"))
            .on_click(cx.listener(|shell, _, _, cx| {
                shell.feed_to_top(cx);
                shell.feed_deep = false;
                cx.notify();
            }))
            .child(icons::arrow_up_icon(theme::MUTED))
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
                    | Some(Pane::Article(_))
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
            // The rail + content row. The pair is centered as a group —
            // the rail stands to the left of the reading column, not glued
            // to the window edge. The rail sits outside every pane's
            // scroll container, so anchoring is free; the deep panes carry
            // their back+title row above the pane (the feed carries none).
            .flex()
            .flex_row()
            .items_start()
            .justify_center()
            .child(self.rail(cx))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .max_w(px(content::COL_W))
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .when(depth > 1, |d| d.child(self.title_row(cx)))
                    .child(match pane {
                        Pane::Feed(view) => view.clone().into_any_element(),
                        Pane::Thread(view) => view.clone().into_any_element(),
                        Pane::Profile(view) => view.clone().into_any_element(),
                        Pane::Notifications(view) => view.clone().into_any_element(),
                        Pane::Search(view) => view.clone().into_any_element(),
                        Pane::Settings(view) => view.clone().into_any_element(),
                        Pane::Article(view) => view.clone().into_any_element(),
                        Pane::Composer(view) => view.clone().into_any_element(),
                    })
                    .when(
                        self.feed_deep && matches!(pane, Pane::Feed(_)),
                        |d| d.child(self.floating_top(cx)),
                    ),
            )
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
