//! Satori — a quiet Nostr client (native GTK4/libadwaita).
//!
//! M0: sumi-e theme, shell chrome, login (bunker + nsec), session resume.
//! The real following feed lands next (DESIGN.md §9).

mod login;
mod shell;
mod theme;

use std::str::FromStr;
use std::sync::Arc;

use adw::prelude::*;
use gtk::gio;
use gtk::glib;
use nostr::prelude::*;
use satori_core::manager::Core;

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

    let (main_page, avatar) = shell::build_main();

    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .build();
    stack.add_named(&main_page, Some("main"));

    // Login page: on success, show the identity in the account avatar and
    // crossfade into the main shell.
    let login_page = login::LoginPage::new(
        core.clone(),
        {
            let stack = stack.clone();
            let avatar = avatar.clone();
            move |pk| {
                shell::update_avatar(&avatar, pk);
                stack.set_visible_child_full("main", gtk::StackTransitionType::Crossfade);
            }
        },
    );
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
                shell::update_avatar(&avatar, pk);
            }
            stack.set_visible_child_full("main", gtk::StackTransitionType::Crossfade);
        }
        None => {
            stack.set_visible_child_full("login", gtk::StackTransitionType::Crossfade);
        }
    }
}
