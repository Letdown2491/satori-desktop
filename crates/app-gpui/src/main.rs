//! Satori's GPUI front-end (D13). One window, sumi-e ink on washi, driven by
//! the untouched `crates/core`.
//!
//! NOTE: shares the LMDB store with the GTK app — run one at a time until P4.

mod icons;
mod input;
mod media;
mod markdown;
mod prefs;
mod theme;
mod views;

use std::sync::Arc;

use gpui::{
    div, prelude::*, px, App, Application, Bounds, Context, Entity, Render, WindowBounds,
    WindowOptions, size,
};
use satori_core::manager::Core;

use crate::{
    input::bind_input_keys,
    views::{
        composer::bind_composer_keys, login::{Login, LoginView}, shell::Shell,
    },
};

/// The window root: the login doors until a session exists, then the shell.
/// The swap happens on Login::Approved — the shell focuses itself on first
/// render (shell.rs focused_once).
struct Root {
    core: Arc<Core>,
    login: Option<Entity<LoginView>>,
    shell: Option<Entity<Shell>>,
}

impl Root {
    fn new(
        core: Arc<Core>,
        session: bool,
        resume_error: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let (login, shell) = match session {
            true => (None, Some(cx.new(|cx| Shell::new(core.clone(), cx)))),
            false => {
                // Resume failures surface on the doors — the user must know
                // why they're back here (login.rs set_resume_error).
                (Some(cx.new(|cx| LoginView::new(core.clone(), resume_error, cx))), None)
            }
        };
        if let Some(login) = login.clone() {
            cx.subscribe(&login, |this, _, _: &Login, cx| {
                this.login = None;
                this.shell = Some(cx.new(|cx| Shell::new(this.core.clone(), cx)));
                cx.notify();
            })
            .detach();
        }
        Self { core, login, shell }
    }
}

impl Render for Root {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        if let Some(login) = self.login.clone() {
            login.into_any_element()
        } else if let Some(shell) = self.shell.clone() {
            shell.into_any_element()
        } else {
            div().into_any_element()
        }
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        bind_input_keys(cx);
        bind_composer_keys(cx);
        // Shared decoded-image cache (media.rs) — avatars, tiles and the
        // lightbox all key off it.
        cx.default_global::<media::Cache>();
        let core = Arc::new(Core::new());
        let (session, err) = core.init();
        if err.is_some() {
            eprintln!("[satori-gpui] resume error (surfaced on the login doors)");
        }

        // TODO(port): crates/app/src/shell.rs window geometry + state — P1.
        let bounds = Bounds::centered(None, size(px(460.), px(760.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("Satori".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_window, cx| {
                let session_ok = session.is_some();
                cx.new(|cx| Root::new(core, session_ok, err, cx))
            },
        )
        .expect("failed to open Satori window");
        cx.activate(true);
    });
}
