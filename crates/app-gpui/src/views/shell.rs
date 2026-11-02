//! The window shell: header + navigation stack over the views.
//!
//! TODO(port): crates/app/src/shell.rs (AdwNavigationSplitView + header bar)
//! and main.rs Escape-to-back — P1. The GTK sidebar collapses for now; the
//! sumi-e header carries back/title until P1's sidebar pass. Escape-to-back is
//! ported as a root key-down listener (gpui actions land with the composer in
//! P2, when there are more keys to bind).

use std::str::FromStr;
use std::sync::Arc;

use gpui::{
    ClickEvent, Div, EventEmitter,
    div, img, prelude::*, px, rgb, App, Context, Entity, FocusHandle, Focusable, KeyDownEvent,
    AnyView, ObjectFit, Render, SharedString, Stateful, Window,
};
use nostr::prelude::*;
use satori_core::manager::{Core, FeedEvent, Profile, PublishEvent};

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
    wallet::WalletView,
    zap::ZapSheet,
};

#[derive(Clone)]
pub enum Pane {
    Feed(Entity<Feed>),
    /// The articles shelf — a second Feed entity in articles mode.
    Articles(Entity<Feed>),
    /// A hashtag timeline — a third Feed entity in hashtag mode (Phase 2 §4.2).
    Hashtag(Entity<Feed>),
    Thread(Entity<ThreadView>),
    Profile(Entity<ProfileView>),
    Notifications(Entity<NotificationsView>),
    Search(Entity<SearchView>),
    Settings(Entity<SettingsView>),
    Article(Entity<ArticleView>),
    Composer(Entity<Composer>),
    /// The zap amount sheet (bolt long-press).
    ZapSheet(Entity<ZapSheet>),
    /// The NWC wallet connect pane (D20).
    Wallet(Entity<WalletView>),
}

/// Shell-to-root events. The root subscribes at both shell-creation sites
/// (startup session resume and Login::Approved) — today only the sign-out
/// handoff, which swaps the window back to the login doors.
#[derive(Debug)]
pub enum ShellEvent {
    SignedOut,
}

/// State of the shell's undo toast: the held token + its deadline, or the
/// terminal whisper (undone) / sticky error.
struct HoldToast {
    token: u64,
    deadline: std::time::Instant,
    undone: bool,
    err: Option<String>,
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
    /// The video overlay (Phase 3) — full-screen player over any pane:
    /// the URL + its imeta mime (the inline gate rode the Nav).
    video_overlay: Option<(String, Option<String>)>,
    /// The undo window's floating toast (compose.ts undo-toast): the
    /// composer pops on Held, so the countdown lives HERE, over any pane.
    /// Undone flashes briefly; a Failed error sticks until the next try.
    hold: Option<HoldToast>,
    /// Bell dot: anything newer than the read-state high-water? Polled every
    /// 90s like the GTK shell (poll_notif); cleared when the pane opens.
    notif_unread: bool,
    /// The feed's deep-scroll state (Nav::FeedDeep): the floating
    /// back-to-top whisper only shows once the reader is well below the top.
    feed_deep: bool,
    stack: Vec<Pane>,
    /// The signed-in identity: the rail's top mark is the user's avatar
    /// (replacing the ensō as the home glyph).
    me: Option<PublicKey>,
    me_profile: Option<Profile>,
    /// Snapshot memo for the one rail avatar (media::Snapshot::new clones the
    /// cache map — not per-frame work).
    rail_snap: Option<media::Snapshot>,
    rail_epoch: u64,
}

impl Shell {
    pub fn new(core: Arc<Core>, cx: &mut Context<Self>) -> Self {
        crate::theme::watch(cx); // repaint on theme change
        crate::video::watch(cx); // repaint on player open/toggle/close
        let feed = cx.new(|cx| Feed::new(core.clone(), cx));
        cx.subscribe(&feed, |this, _feed, ev: &Nav, cx| match ev {
            Nav::OpenThread(id) => this.open_thread(*id, cx),
            Nav::OpenProfile(who) => this.open_profile(*who, cx),
            Nav::OpenHashtag(tag) => this.open_hashtag(tag.clone(), cx),
            Nav::OpenComposer(reply) => this.open_composer(reply.clone(), None, cx),
            Nav::OpenQuote(ev) => this.open_composer(None, Some(ev.clone()), cx),
            Nav::OpenArticle(ev) => this.open_article(ev.clone(), cx),
            Nav::OpenLightbox(urls, idx) => this.open_lightbox(urls.clone(), *idx, cx),
            Nav::OpenVideo(url, mime) => this.open_video(url.clone(), mime.clone(), cx),
            Nav::OpenZapSheet(ev) => this.open_zap_sheet(ev.clone(), cx),
            Nav::OpenWallet => this.open_wallet(cx),
            Nav::FeedDeep(deep) => {
                this.feed_deep = *deep;
                cx.notify();
            }
            Nav::Pop => this.back(cx),
            Nav::SetTheme(dark) => this.set_theme(*dark, cx),
            Nav::SignOut => this.sign_out(cx),
        })
        .detach();
        let mut shell = Self {
            core,
            focus: cx.focus_handle(),
            focused_once: false,
            refocus: false,
            lightbox: None,
            video_overlay: None,
            hold: None,
            notif_unread: false,
            feed_deep: false,
            stack: vec![],
            me: None,
            me_profile: None,
            rail_snap: None,
            rail_epoch: 0,
        };
        shell.stack.push(Pane::Feed(feed));
        // The rail avatar: session pubkey synchronously, profile via the
        // usual ensure_profiles pump (name/picture arrive from the store or
        // the relays a beat later).
        shell.me = satori_core::session::load()
            .map(|s| s.pubkey)
            .and_then(|hex| PublicKey::from_str(&hex).ok());
        shell.want_avatar(cx);
        shell.watch_notif(cx);
        shell
    }

    /// One ensure_profiles pump for the rail avatar. The profiles service
    /// resolves a miss exactly once — and this pump fires at Shell::new, a
    /// beat before the relays connect, so the first ask usually misses.
    /// Retry (2s apart, ~30s ceiling) until the profile lands.
    fn want_avatar(&self, cx: &mut Context<Self>) {
        let Some(me) = self.me else { return };
        let me_hex = me.to_string();
        cx.spawn(async move |this, cx| {
            for _ in 0..15 {
                let mut rx = this
                    .update(cx, |shell, _| {
                        shell.core.clone().ensure_profiles(vec![me])
                    })
                    .expect("shell alive");
                // One resolution per ask (the debouncer drops the waiter
                // after its single send); a miss waits out the 2s, retries.
                if let Some(FeedEvent::Profiles(ps)) = rx.recv().await
                    && let Some(p) = ps.into_iter().find(|p| p.pubkey == me_hex)
                {
                    let _ = this.update(cx, |shell, cx| {
                        shell.me_profile = Some(p);
                        cx.notify();
                    });
                    return;
                }
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(2))
                    .await;
            }
        })
        .detach();
    }

    fn subscribe_nav<V: EventEmitter<Nav> + 'static>(&self, view: &Entity<V>, cx: &mut Context<Self>) {
        cx.subscribe(view, |this, _view, ev: &Nav, cx| match ev {
            Nav::OpenThread(id) => this.open_thread(*id, cx),
            Nav::OpenProfile(who) => this.open_profile(*who, cx),
            Nav::OpenHashtag(tag) => this.open_hashtag(tag.clone(), cx),
            Nav::OpenComposer(reply) => this.open_composer(reply.clone(), None, cx),
            Nav::OpenQuote(ev) => this.open_composer(None, Some(ev.clone()), cx),
            Nav::OpenArticle(ev) => this.open_article(ev.clone(), cx),
            Nav::OpenLightbox(urls, idx) => this.open_lightbox(urls.clone(), *idx, cx),
            Nav::OpenVideo(url, mime) => this.open_video(url.clone(), mime.clone(), cx),
            Nav::OpenZapSheet(ev) => this.open_zap_sheet(ev.clone(), cx),
            Nav::OpenWallet => this.open_wallet(cx),
            Nav::FeedDeep(deep) => {
                this.feed_deep = *deep;
                cx.notify();
            }
            Nav::Pop => this.back(cx),
            Nav::SetTheme(dark) => this.set_theme(*dark, cx),
            Nav::SignOut => this.sign_out(cx),
        })
        .detach();
    }

    /// Live palette switch (settings.rs SetTheme): persist the pref,
    /// select, and wake every watching view (theme.rs ThemeSignal).
    fn set_theme(&mut self, dark: bool, cx: &mut Context<Self>) {
        let mut p = crate::prefs::load();
        let changed = p.dark != Some(dark);
        p.dark = Some(dark);
        crate::prefs::save(&p);
        theme::select(if dark { theme::DARK } else { theme::LIGHT });
        cx.set_global(theme::ThemeSignal);
        cx.notify();
        // NIP-78: sync the theme slot (Phase 2 §4.1) — fire and forget.
        if changed {
            self.core.publish_app_data("dark", serde_json::Value::Bool(dark));
        }
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

    /// The articles shelf: a second feed entity, kind 30023 from the same
    /// follows. It stacks on the timeline (back returns to it).
    fn open_articles(&mut self, cx: &mut Context<Self>) {
        let view = cx.new(|cx| Feed::articles(self.core.clone(), cx));
        self.subscribe_nav(&view, cx);
        self.stack.push(Pane::Articles(view));
        cx.notify();
    }

    /// The hashtag timeline: a Feed entity in hashtag mode — a #tag tap in
    /// a note body lands here (Phase 2 §4.2).
    fn open_hashtag(&mut self, tag: String, cx: &mut Context<Self>) {
        let view = cx.new(|cx| Feed::hashtag(self.core.clone(), tag, cx));
        self.subscribe_nav(&view, cx);
        self.stack.push(Pane::Hashtag(view));
        cx.notify();
    }

    fn open_composer(
        &mut self,
        reply_to: Option<Event>,
        quote: Option<Event>,
        cx: &mut Context<Self>,
    ) {
        let view = cx.new(|cx| Composer::new(self.core.clone(), reply_to, quote, cx));
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

    /// The zap amount sheet (bolt long-press / right-click).
    fn open_zap_sheet(&mut self, ev: Event, cx: &mut Context<Self>) {
        let view = cx.new(|cx| ZapSheet::new(self.core.clone(), ev, cx));
        self.subscribe_nav(&view, cx);
        self.stack.push(Pane::ZapSheet(view));
        cx.notify();
    }

    /// The NWC wallet pane (settings door, or a bolt tap with no wallet).
    fn open_wallet(&mut self, cx: &mut Context<Self>) {
        let view = cx.new(|cx| WalletView::new(self.core.clone(), cx));
        self.subscribe_nav(&view, cx);
        self.stack.push(Pane::Wallet(view));
        cx.notify();
    }

    fn open_settings(&mut self, cx: &mut Context<Self>) {
        let view = cx.new(|cx| SettingsView::new(self.core.clone(), cx));
        self.subscribe_nav(&view, cx);
        self.stack.push(Pane::Settings(view));
        cx.notify();
    }

    /// Settings' sign-out row: clear the session + secrets, then hand the
    /// window back to the login doors (Root swaps on SignedOut).
    fn sign_out(&mut self, cx: &mut Context<Self>) {
        self.core.sign_out();
        cx.emit(ShellEvent::SignedOut);
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
        // The live subscription is the first line (instant); this poll is
        // only a correctness net for a dead stream — 15 minutes, and the
        // full notifications computation it runs is why it must be rare.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(900))
                    .await;
                let _ = this.update(cx, |shell, cx| shell.check_notif_dot(cx));
            }
        })
        .detach();
        // Push: any reply/mention tagging me lights the bell at once.
        let mut live_rx = self.core.subscribe_live();
        cx.spawn(async move |this, cx| {
            loop {
                match live_rx.recv().await {
                    Ok(satori_core::manager::Live::Mention) => {
                        let _ = this.update(cx, |shell, cx| {
                            shell.notif_unread = true;
                            cx.notify();
                        });
                    }
                    Ok(satori_core::manager::Live::PrefsSynced) => {
                        // NIP-78 login sync landed and remote prefs won —
                        // re-read what the shell renders live (the theme).
                        // Not a user action: no publish, no echo.
                        let _ = this.update(cx, |_shell, cx| {
                            if let Some(dark) = crate::prefs::load().dark {
                                theme::select(if dark { theme::DARK } else { theme::LIGHT });
                                cx.set_global(theme::ThemeSignal);
                            }
                            cx.notify();
                        });
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(_) => continue,
                }
            }
        })
        .detach();
        // The undo window's toast: the composer pops on Held, so the
        // countdown floats here — over the feed, like the reference's
        // bottom toast. Undo calls core.undo(token); outcomes resolve it.
        let mut hold_rx = self.core.subscribe_hold();
        cx.spawn(async move |this, cx| {
            loop {
                match hold_rx.recv().await {
                    Ok(PublishEvent::Held { token, hold_secs, .. }) => {
                        let _ = this.update(cx, |shell, cx| {
                            shell.hold = Some(HoldToast {
                                token,
                                deadline: std::time::Instant::now()
                                    + std::time::Duration::from_secs(hold_secs),
                                undone: false,
                                err: None,
                            });
                            cx.notify();
                        });
                        // One tick a second keeps the countdown honest; it
                        // dies when the toast resolves.
                        let weak = this.clone();
                        cx.spawn(async move |cx| {
                            loop {
                                cx.background_executor()
                                    .timer(std::time::Duration::from_secs(1))
                                    .await;
                                let alive = weak
                                    .update(cx, |shell, cx| {
                                        let running = shell
                                            .hold
                                            .as_ref()
                                            .is_some_and(|h| !h.undone && h.err.is_none());
                                        if running {
                                            cx.notify();
                                        }
                                        running
                                    })
                                    .unwrap_or(false);
                                if !alive {
                                    break;
                                }
                            }
                        })
                        .detach();
                    }
                    Ok(PublishEvent::Committed { .. }) => {
                        let _ = this.update(cx, |shell, cx| {
                            shell.hold = None;
                            cx.notify();
                        });
                    }
                    Ok(PublishEvent::Undone { .. }) => {
                        let _ = this.update(cx, |shell, cx| {
                            if let Some(h) = shell.hold.as_mut() {
                                h.undone = true;
                            }
                            cx.notify();
                        });
                        // The undone whisper clears itself.
                        let weak = this.clone();
                        cx.spawn(async move |cx| {
                            cx.background_executor()
                                .timer(std::time::Duration::from_millis(2500))
                                .await;
                            let _ = weak.update(cx, |shell, cx| {
                                if shell.hold.as_ref().is_some_and(|h| h.undone) {
                                    shell.hold = None;
                                    cx.notify();
                                }
                            });
                        })
                        .detach();
                    }
                    Ok(PublishEvent::Failed { error, .. }) => {
                        let _ = this.update(cx, |shell, cx| {
                            match shell.hold.as_mut() {
                                Some(h) => h.err = Some(error),
                                None => {
                                    shell.hold = Some(HoldToast {
                                        token: 0,
                                        deadline: std::time::Instant::now(),
                                        undone: false,
                                        err: Some(error),
                                    })
                                }
                            }
                            cx.notify();
                        });
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(_) => continue,
                }
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
            media::fetch(&self.core, url.clone(), cx.entity_id(), cx);
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

    /// The video overlay (Phase 3): full-screen player when the URL is
    /// inline-able (video.rs gates strict Tor/YouTube), else the browser
    /// link — the honest pre-Phase-3 behavior.
    fn open_video(&mut self, url: String, mime: Option<String>, cx: &mut Context<Self>) {
        if !crate::video::inline_able(&url, mime.as_deref()) {
            cx.open_url(&url);
            return;
        }
        self.video_overlay = Some((url.clone(), mime.clone()));
        crate::video::open(url, mime, cx);
        cx.notify();
    }

    /// The video overlay closes: the player's pipeline tears down with it
    /// (video.rs close — deferred drop, off the UI thread).
    fn close_video(&mut self, cx: &mut Context<Self>) {
        if self.video_overlay.take().is_some() {
            crate::video::close(cx);
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

    fn title(&self, cx: &App) -> SharedString {
        match self.stack.last() {
            Some(Pane::Feed(_)) => "Feed".into(),
            Some(Pane::Articles(_)) => "Articles".into(),
            Some(Pane::Hashtag(f)) => f.read(cx).header().into(),
            Some(Pane::Thread(_)) => "Thread".into(),
            Some(Pane::Profile(_)) => "Profile".into(),
            Some(Pane::Notifications(_)) => "Notifications".into(),
            Some(Pane::Search(_)) => "Search".into(),
            Some(Pane::Settings(_)) => "Settings".into(),
            Some(Pane::Article(_)) => "Read".into(),
            Some(Pane::Composer(_)) => "Compose".into(),
            Some(Pane::ZapSheet(_)) => "Zap".into(),
            Some(Pane::Wallet(_)) => "Wallet".into(),
            None => "Satori".into(),
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
            .bg(rgb(theme::ink()))
            .text_color(rgb(theme::washi()))
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
    Articles,
    Search,
    Notif,
    Compose,
    Settings,
}

impl Shell {
    fn active_spot(&self) -> Option<Spot> {
        match self.stack.last() {
            Some(Pane::Feed(_)) => Some(Spot::Feed),
            Some(Pane::Articles(_)) => Some(Spot::Articles),
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
        match self.stack.last().cloned() {
            Some(Pane::Feed(view)) | Some(Pane::Articles(view)) | Some(Pane::Hashtag(view)) => {
                view.update(cx, |feed, _| feed.scroll_top());
            }
            _ => {}
        }
    }

    fn rail(&mut self, cx: &mut Context<Self>) -> Div {
        let active = self.active_spot();
        let ink = |on: bool| if on { theme::ink() } else { theme::muted() };
        let feed_active = active == Some(Spot::Feed);

        // The avatar's snapshot memoized on the media epoch — Snapshot::new
        // clones the cache map, so it must not run per frame.
        let epoch = media::generation(cx);
        if self.rail_snap.is_none() || self.rail_epoch != epoch {
            self.rail_snap = Some(media::Snapshot::new(cx));
            self.rail_epoch = epoch;
        }
        let (me_pic, me_name) = match (&self.me_profile, self.me) {
            (Some(p), _) => (p.picture.clone(), p.display()),
            (None, Some(pk)) => (String::new(), format!("{}…", &pk.to_string()[..8])),
            _ => (String::new(), "You".to_string()),
        };
        // The rail avatar comes back if the cache evicted it (silent no-op
        // when the cache still holds it).
        if !me_pic.is_empty() {
            media::fetch(&self.core, me_pic.clone(), cx.entity_id(), cx);
        }
        let me_avatar =
            media::avatar(self.rail_snap.as_ref().unwrap(), &me_pic, &me_name, 26.0);

        div()
            .flex_none()
            .h_full()
            .w(px(52.0))
            .flex()
            .flex_col()
            .items_center()
            // Glyph top lands at 19+9=28 — level with the feed's first
            // avatar (16px page band + 12px note-card padding).
            .pt(px(19.0))
            .gap_1()
            // The user's avatar is the home mark now (it replaced the ensō);
            // like the ensō it never wears the active tick.
            .child(self.rail_button(
                "rail-home",
                "Home",
                me_avatar,
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
                "rail-articles",
                "Articles",
                icons::article_icon(ink(active == Some(Spot::Articles))),
                active == Some(Spot::Articles),
                cx.listener(move |shell, _, _, cx| {
                    if shell.active_spot() != Some(Spot::Articles) {
                        shell.open_articles(cx);
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
                                .bg(rgb(theme::vermilion())),
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
            .child(self.rail_button(
                "rail-compose",
                "Compose",
                icons::pen_icon(theme::vermilion()),
                active == Some(Spot::Compose),
                cx.listener(move |shell, _, _, cx| {
                    // Never clobber an open composer (its draft is live).
                    if shell.active_spot() != Some(Spot::Compose) {
                        shell.open_composer(None, None, cx);
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
                        .bg(rgb(theme::vermilion())),
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
            // Title ink at ~28px — level with the feed's first note (the
            // column's 16px band + 10 here ≈ 26px text box, ~2px leading).
            .pt(px(10.0))
            .pb(px(4.0))
            .child(
                div()
                    .id("back")
                    .cursor_pointer()
                    .text_size(px(13.0))
                    .text_color(rgb(theme::muted()))
                    .on_click(cx.listener(|shell, _, _, cx| shell.back(cx)))
                    .child("‹ Back"),
            )
            .child(
                div()
                    .flex_1()
                    .text_size(px(13.0))
                    .text_color(rgb(theme::ink()))
                    .child(self.title(cx)),
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
            .border_color(rgb(theme::hairline()))
            .bg(rgb(theme::washi()))
            .flex()
            .items_center()
            .justify_center()
            .tooltip(tip("Back to top"))
            .on_click(cx.listener(|shell, _, _, cx| {
                shell.feed_to_top(cx);
                shell.feed_deep = false;
                cx.notify();
            }))
            .child(icons::arrow_up_icon(theme::muted()))
    }
}

impl EventEmitter<ShellEvent> for Shell {}

impl Focusable for Shell {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let depth = self.stack.len();
        // Built before the pane borrow: rail() takes &mut self (snapshot
        // memo), and `pane` holds the stack for the rest of the render.
        let rail = self.rail(cx);
        let pane = self.stack.last().unwrap();
        if !self.focused_once || self.refocus {
            self.focused_once = true;
            self.refocus = false;
            window.focus(&self.focus);
        }

        div()
            .relative()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|shell, ev: &KeyDownEvent, window, cx| {
                // The video overlay owns Esc while it's up: Esc closes it
                // (and tears the player down) instead of popping the pane.
                if shell.video_overlay.is_some() {
                    if ev.keystroke.key == "escape" {
                        shell.close_video(cx);
                    }
                    cx.notify();
                    return;
                }
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
                // The single-letter shortcuts are for the SHELL: while any
                // text surface holds focus, typed letters are content, not
                // commands. (The pane-type guard this replaces was whack-
                // a-mole — "an 'n' in 'nostr' opened the composer" via the
                // search field, and D20's wallet URI field relit it. The
                // Input lets plain keys bubble, but it DOES take focus, and
                // that's the honest signal: shortcuts fire only when the
                // shell root itself holds focus — startup, after a pop's
                // refocus, after clicking dead space.)
                if !shell.focus.is_focused(window) {
                    return;
                }
                if !ev.keystroke.modifiers.modified() {
                    if ev.keystroke.key == "n" {
                        shell.open_composer(None, None, cx);
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
                    if ev.keystroke.key == "a" {
                        shell.open_articles(cx);
                    }
                    // No GTK accel — same CSD story: the header Settings seal
                    // needs a keyboard way in.
                    if ev.keystroke.key == "s" {
                        shell.open_settings(cx);
                    }
                }
            }))
            .size_full()
            .bg(rgb(theme::washi()))
            .text_color(rgb(theme::ink()))
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
            .child(rail)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .max_w(px(content::COL_W))
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    // The quiet band every page starts under (uniform top
                    // padding — the feed's first note, the deep panes'
                    // title row: all begin 16px down).
                    .pt(px(16.0))
                    .when(depth > 1, |d| d.child(self.title_row(cx)))
                    .child(match pane {
                        Pane::Feed(view) => view.clone().into_any_element(),
                        Pane::Articles(view) => view.clone().into_any_element(),
                        Pane::Hashtag(view) => view.clone().into_any_element(),
                        Pane::Thread(view) => view.clone().into_any_element(),
                        Pane::Profile(view) => view.clone().into_any_element(),
                        Pane::Notifications(view) => view.clone().into_any_element(),
                        Pane::Search(view) => view.clone().into_any_element(),
                        Pane::Settings(view) => view.clone().into_any_element(),
                        Pane::Article(view) => view.clone().into_any_element(),
                        Pane::Composer(view) => view.clone().into_any_element(),
                        Pane::ZapSheet(view) => view.clone().into_any_element(),
                        Pane::Wallet(view) => view.clone().into_any_element(),
                    })
                    .when(
                        self.feed_deep
                            && matches!(pane, Pane::Feed(_) | Pane::Articles(_) | Pane::Hashtag(_)),
                        |d| d.child(self.floating_top(cx)),
                    ),
            )
            .when(self.hold.is_some(), |d| d.child(self.hold_toast(cx)))
            .when_some(self.lightbox.clone(), |d, (urls, idx)| {
                d.child(lightbox_overlay(&urls, idx, cx))
            })
            .when_some(self.video_overlay.clone(), |d, (url, _mime)| {
                d.child(video_overlay(&url, cx))
            })
    }
}

impl Shell {
    /// The undo-window toast: floats bottom-center over any pane —
    /// countdown + Undo while held, a brief "undone" whisper, a sticky
    /// failure line (cleared by the next publish).
    fn hold_toast(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let held = self.hold.as_ref().unwrap();
        let text = if let Some(e) = &held.err {
            format!("failed: {e}")
        } else if held.undone {
            "undone — nothing was sent".to_string()
        } else {
            let left = held
                .deadline
                .saturating_duration_since(std::time::Instant::now())
                .as_secs();
            format!("publishing in {}s", left.max(1))
        };
        div()
            .absolute()
            .left(px(0.))
            .right(px(0.))
            .bottom(px(16.))
            .flex()
            .justify_center()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .rounded(px(6.))
                    .border_1()
                    .border_color(rgb(theme::hairline()))
                    .bg(rgb(theme::panel()))
                    .px_4()
                    .py_2()
                    .text_size(px(12.5))
                    .text_color(rgb(theme::muted()))
                    .child(text)
                    .when(held.undone || held.err.is_some(), |d| d), // shape only
            )
            .when(!held.undone && held.err.is_none(), |d| {
                d.child(
                    div()
                        .id("hold-undo")
                        .cursor_pointer()
                        .px_2()
                        .py(px(2.))
                        .rounded(px(4.))
                        .border_1()
                        .border_color(rgb(theme::vermilion()))
                        .text_color(rgb(theme::vermilion()))
                        .text_size(px(12.5))
                        .on_click(cx.listener(|shell, _, _, cx| {
                            if let Some(h) = shell.hold.as_ref() {
                                shell.core.undo(h.token);
                            }
                            cx.notify();
                        }))
                        .child("Undo"),
                )
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

/// The video overlay (Phase 3): dark full-screen player mirroring the
/// lightbox — the crate's element aspect-fit inside a centered box, the
/// control row under it (⏸/▶, ↗ browser), ✕/Esc/backdrop close (the
/// pipeline tears down via close_video).
fn video_overlay(url: &str, cx: &mut Context<Shell>) -> impl IntoElement {
    let video = crate::video::active(url);
    let paused_now = video.as_ref().is_some_and(|v| v.paused());
    let browser = url.to_string();

    div()
        .absolute()
        .inset_0()
        .bg(rgb(0x000000))
        .opacity(0.94)
        .flex()
        .flex_col()
        .font_family(theme::SANS)
        // Top bar: the mark + close.
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
                        .child("▶"),
                )
                .child(
                    div()
                        .id("vid-close")
                        .cursor_pointer()
                        .px_2()
                        .text_size(px(13.0))
                        .text_color(rgb(0xb8b2a6))
                        .on_click(cx.listener(|shell, _, _, cx| shell.close_video(cx)))
                        .child("✕"),
                ),
        )
        // Middle: the player centered in the spare space, its control row
        // directly beneath.
        .child(
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .p_2()
                        .child(match video {
                            Some(v) => crate::video::element(&v, content::COL_W - 64.0, 480.0)
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
                        .flex()
                        .items_center()
                        .justify_center()
                        .gap_4()
                        .py_2()
                        .child(
                            div()
                                .id("vid-toggle")
                                .cursor_pointer()
                                .text_size(px(14.0))
                                .text_color(rgb(0xb8b2a6))
                                .on_click(|_, _, cx| {
                                    cx.stop_propagation();
                                    crate::video::toggle(cx);
                                })
                                .child(if paused_now { "▶" } else { "⏸" }),
                        )
                        .child(
                            div()
                                .id("vid-browser")
                                .cursor_pointer()
                                .text_size(px(14.0))
                                .text_color(rgb(0xb8b2a6))
                                .on_click(move |_, _, cx| {
                                    cx.stop_propagation();
                                    cx.open_url(&browser);
                                })
                                .child("↗"),
                        ),
                ),
        )
        .child(
            // Clicking the dark closes (lightbox.rs Esc), pipeline with it.
            div()
                .id("vid-backdrop")
                .flex_1()
                .cursor_pointer()
                .on_click(cx.listener(|shell, _, _, cx| shell.close_video(cx))),
        )
}
