//! The window shell: header + navigation stack over the views.
//!
//! TODO(port): crates/app/src/shell.rs (AdwNavigationSplitView + header bar)
//! and main.rs Escape-to-back — P1. The GTK sidebar collapses for now; the
//! sumi-e header carries back/title until P1's sidebar pass. Escape-to-back is
//! ported as a root key-down listener (gpui actions land with the composer in
//! P2, when there are more keys to bind).

use std::sync::Arc;

use gpui::{
    div, prelude::*, px, rgb, App, Context, Entity, FocusHandle, Focusable, KeyDownEvent, Render,
    Window,
};
use nostr::prelude::*;
use satori_core::manager::Core;

use crate::theme;
use super::{feed::Feed, nav::Nav, profile::ProfileView, settings::SettingsView, thread::ThreadView};

pub enum Pane {
    Feed(Entity<Feed>),
    Thread(Entity<ThreadView>),
    Profile(Entity<ProfileView>),
    Settings(Entity<SettingsView>),
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

    fn open_thread(&mut self, id: EventId, cx: &mut Context<Self>) {
        // TODO(port): crates/app/src/thread.rs thread_page_by_id — P1 flat
        // replies; reply nesting (nest_replies, depth <= 3) lands in P2.
        let view = cx.new(|cx| ThreadView::new(self.core.clone(), id, cx));
        cx.subscribe(&view, |this, _view, ev: &Nav, cx| match ev {
            Nav::OpenThread(id) => this.open_thread(*id, cx),
            Nav::OpenProfile(who) => this.open_profile(*who, cx),
        })
        .detach();
        self.stack.push(Pane::Thread(view));
        cx.notify();
    }

    fn open_profile(&mut self, who: PublicKey, cx: &mut Context<Self>) {
        let view = cx.new(|cx| ProfileView::new(self.core.clone(), who, cx));
        cx.subscribe(&view, |this, _view, ev: &Nav, cx| match ev {
            Nav::OpenThread(id) => this.open_thread(*id, cx),
            Nav::OpenProfile(who) => this.open_profile(*who, cx),
        })
        .detach();
        self.stack.push(Pane::Profile(view));
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
                    .child(self.gear(cx).into_any_element()),
            )
            .child(match pane {
                Pane::Feed(view) => view.clone().into_any_element(),
                Pane::Thread(view) => view.clone().into_any_element(),
                Pane::Profile(view) => view.clone().into_any_element(),
                Pane::Settings(view) => view.clone().into_any_element(),
            })
    }
}
