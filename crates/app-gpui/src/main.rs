//! Satori's GPUI front-end (D13). One window, sumi-e ink on washi, driven by
//! the untouched `crates/core`.
//!
//! NOTE: shares the LMDB store with the GTK app — run one at a time until P4.

mod icons;
mod theme;
mod views;

use std::sync::Arc;

use gpui::{prelude::*, px, App, Application, Bounds, WindowBounds, WindowOptions, size};
use satori_core::manager::Core;

use crate::views::feed::Feed;

fn main() {
    Application::new().run(|cx: &mut App| {
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
            |_, cx| cx.new(|cx| Feed::new(core, cx)),
        )
        .expect("failed to open Satori window");
        cx.activate(true);
    });
}
