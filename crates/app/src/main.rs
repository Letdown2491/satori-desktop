//! Satori — a quiet Nostr client (native GTK4/libadwaita).
//!
//! M0: sumi-e theme, login (bunker + nsec), session resume, following feed,
//! threads, notifications, posting with the undo window (DESIGN.md §9).

mod bridge;
mod composer;
mod content;
mod ctx;
mod feed;
mod login;
mod note_card;
mod notifications;
mod shell;
mod theme;
mod thread;

use std::rc::Rc;
use std::str::FromStr;
use std::sync::Arc;

use adw::prelude::*;
use gtk::gio;
use gtk::glib;
use nostr::prelude::*;
use satori_core::manager::{Core, FeedEvent};

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id(satori_core::config::APP_ID)
        .build();
    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &adw::Application) {
    theme::load_current();
    let core = Arc::new(Core::new());

    // The main shell: nav (feed → thread → notifications) under a toast
    // overlay (the undo window surface), wrapped in the header bar.
    let overlay = adw::ToastOverlay::new();
    let nav = adw::NavigationView::new();
    let ctx = ctx::Ctx { core: core.clone(), nav: nav.clone(), overlay: overlay.clone() };
    let feed = feed::FeedView::new(&ctx);
    nav.add(&feed.page);
    overlay.set_child(Some(&nav));

    let (main_shell, handles) = shell::build_main(&ctx, Rc::clone(&feed), overlay.upcast());

    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .build();
    stack.add_named(&main_shell, Some("main"));

    let login_page = login::LoginPage::new(core.clone(), {
        let stack = stack.clone();
        let handles = handles.clone();
        let ctx = ctx.clone();
        move |_pk| {
            shell::poll_notif(&ctx, &handles.bell_dot);
            stack.set_visible_child_full("main", gtk::StackTransitionType::Crossfade);
        }
    });
    stack.add_named(login_page.widget(), Some("login"));

    // Log out: wipe the session (secrets included) and return to the login page.
    let logout = gio::SimpleAction::new("logout", None);
    {
        let core = Arc::clone(&core);
        let stack = stack.clone();
        let login = login_page.clone();
        logout.connect_activate(move |_, _| {
            core.logout();
            login.reset();
            stack.set_visible_child_full("login", gtk::StackTransitionType::Crossfade);
        });
    }
    app.add_action(&logout);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Satori")
        .default_width(880)
        .default_height(760)
        .content(&stack)
        .build();
    window.present();

    // Resume a persisted session, else first-run: the login page.
    match core.init() {
        Some(session) => {
            if let Ok(pk) = PublicKey::from_str(&session.pubkey) {
                shell::update_avatar(&handles.avatar, pk);
            }
            shell::poll_notif(&ctx, &handles.bell_dot);
            stack.set_visible_child_full("main", gtk::StackTransitionType::Crossfade);
        }
        None => {
            stack.set_visible_child_full("login", gtk::StackTransitionType::Crossfade);
        }
    }
}
