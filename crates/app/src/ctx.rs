//! Shared UI context — the handle every view/widget gets. Widgets are
//! projections over core state (DESIGN.md §1.2); this carries the projection
//! machinery (navigation, toasts) plus the core handle.

use std::sync::Arc;

use satori_core::manager::Core;

#[derive(Clone)]
pub struct Ctx {
    pub core: Arc<Core>,
    pub nav: adw::NavigationView,
    pub overlay: adw::ToastOverlay,
}

impl Ctx {
    pub fn toast(&self, title: &str) {
        let t = adw::Toast::new(title);
        self.overlay.add_toast(t);
    }
}
