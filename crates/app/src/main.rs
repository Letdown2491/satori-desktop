//! Satori — a quiet Nostr client (native GTK4/libadwaita).
//!
//! M0 skeleton: sumi-e theme, shell chrome (bar, FAB, caught-up clearing),
//! placeholder feed. Login + real feed wiring land next (DESIGN.md §9).

mod shell;
mod theme;

use adw::prelude::*;

use crate::shell::build_shell;

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id(satori_core::config::APP_ID)
        .build();
    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &adw::Application) {
    theme::load_current();

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Satori")
        .default_width(880)
        .default_height(760)
        .content(&build_shell())
        .build();
    window.present();
}

use gtk::glib;
