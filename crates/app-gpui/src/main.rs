//! Satori's GPUI front-end (D13). One window, sumi-e ink on washi, driven by
//! the untouched `crates/core`.
//!
//! NOTE: shares the LMDB store with the GTK app — run one at a time until P4.

mod icons;
mod input;
mod media;
mod theme;
mod views;

use std::sync::Arc;

use gpui::{prelude::*, px, App, Application, Bounds, Focusable, WindowBounds, WindowOptions, size};
use satori_core::manager::Core;

use crate::{
    input::bind_input_keys,
    views::{composer::bind_composer_keys, shell::Shell},
};

fn main() {
    Application::new().run(|cx: &mut App| {
        bind_input_keys(cx);
        bind_composer_keys(cx);
        // Shared decoded-image cache (media.rs) — avatars, tiles and the
        // lightbox all key off it.
        cx.default_global::<media::Cache>();
        let core = Arc::new(Core::new());
        let (session, err) = core.init();
        if let Some(e) = err {
            eprintln!("[satori-gpui] resume: {e}");
        }
        if session.is_none() {
            eprintln!("[satori-gpui] no stored session — log in once with the GTK app first");
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
            |window, cx| {
                let shell = cx.new(|cx| Shell::new(core, cx));
                // Key events dispatch through the focus tree — focus the shell
                // so Escape-to-back is live from the first keystroke.
                window.focus(&shell.read(cx).focus_handle(cx));
                shell
            },
        )
        .expect("failed to open Satori window");
        cx.activate(true);
    });
}
