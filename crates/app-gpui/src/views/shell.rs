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
    div, prelude::*, px, rgb, App, Context, Entity, FocusHandle, Focusable, KeyDownEvent, Render,
    Window,
};
use nostr::prelude::*;
use satori_core::manager::Core;

use crate::theme;
use super::{composer::Composer, feed::Feed, nav::Nav, profile::ProfileView, settings::SettingsView, thread::ThreadView};

pub enum Pane {
    Feed(Entity<Feed>),
    Thread(Entity<ThreadView>),
    Profile(Entity<ProfileView>),
    Settings(Entity<SettingsView>),
    Composer(Entity<Composer>),
}

pub struct Shell {
    core: Arc<Core>,
    focus: FocusHandle,
    stack: Vec<Pane>,
}

impl Shell {
    pub fn new(core: Arc<Core>, cx: &mut Context<Self>) -> Self {
        let feed = cx.new(|cx| Feed::new(core.clone(), cx));
        cx.subscribe(&feed, |this, _feed, ev: &Nav, cx| match ev {
            Nav::OpenThread(id) => this.open_thread(*id, cx),
            Nav::OpenProfile(who) => this.open_profile(*who, cx),
            Nav::OpenComposer(reply) => this.open_composer(reply.clone(), cx),
            Nav::Pop => this.back(cx),
        })
        .detach();
        let mut shell = Self {
            core,
            focus: cx.focus_handle(),
            stack: vec![],
        };
        shell.stack.push(Pane::Feed(feed));
        shell
    }

    fn subscribe_nav<V: EventEmitter<Nav> + 'static>(&self, view: &Entity<V>, cx: &mut Context<Self>) {
        cx.subscribe(view, |this, _view, ev: &Nav, cx| match ev {
            Nav::OpenThread(id) => this.open_thread(*id, cx),
            Nav::OpenProfile(who) => this.open_profile(*who, cx),
            Nav::OpenComposer(reply) => this.open_composer(reply.clone(), cx),
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

    fn back(&mut self, cx: &mut Context<Self>) {
        if self.stack.len() > 1 {
            self.stack.pop();
            cx.notify();
        }
    }

    fn title(&self) -> &'static str {
        match self.stack.last() {
            Some(Pane::Feed(_)) => "Feed",
            Some(Pane::Thread(_)) => "Thread",
            Some(Pane::Profile(_)) => "Profile",
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
}

impl Focusable for Shell {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Shell {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let depth = self.stack.len();
        let title = self.title();
        let pane = self.stack.last().unwrap();

        div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|shell, ev: &KeyDownEvent, _, cx| {
                // TODO(port): crates/app/src/main.rs escape-to-back — P1.
                if ev.keystroke.key == "escape" {
                    shell.back(cx);
                    return;
                }
                // TODO(port): crates/app/src/main.rs — "N composes" (app.compose
                // accel). GTK's TextViews consume plain keys, so the accel only
                // fires outside text; here the guard is "not already composing"
                // (an open composer is the only text surface).
                if ev.keystroke.key == "n"
                    && !ev.keystroke.modifiers.modified()
                    && !matches!(shell.stack.last(), Some(Pane::Composer(_)))
                {
                    shell.open_composer(None, cx);
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
                        h.child(self.compose_seal(cx).into_any_element())
                    })
                    .child(self.gear(cx).into_any_element()),
            )
            .child(match pane {
                Pane::Feed(view) => view.clone().into_any_element(),
                Pane::Thread(view) => view.clone().into_any_element(),
                Pane::Profile(view) => view.clone().into_any_element(),
                Pane::Settings(view) => view.clone().into_any_element(),
                Pane::Composer(view) => view.clone().into_any_element(),
            })
    }
}
