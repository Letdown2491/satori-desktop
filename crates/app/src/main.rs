//! Satori — a quiet Nostr client (native GTK4/libadwaita).
//!
//! M0: sumi-e theme, login (bunker + nsec), session resume, following feed,
//! threads, notifications, posting with the undo window (DESIGN.md §9).

mod bridge;
mod cards;
mod comments;
mod composer;
mod content;
mod ctx;
mod feed;
mod icons;
mod lightbox;
mod login;
mod markdown;
mod media_cache;
mod motion;
mod note_card;
mod notifications;
mod prefs;
mod profile;
mod search;
mod settings;
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
use satori_core::manager::Core;

fn main() -> glib::ExitCode {
    // GL renderer: the Vulkan swapchain on tiling compositors (niri) spams
    // VK_SUBOPTIMAL_KHR resize warnings — harmless, but we want a clean log.
    // Safe: main runs before any thread spawn.
    unsafe { std::env::set_var("GSK_RENDERER", "gl") };
    // One targeted suppression: GTK's gsettings portal maps
    // color-scheme=prefer-dark onto gtk-application-prefer-dark-theme and
    // libadwaita warns on every notify of it. We never use that property
    // (AdwStyleManager::color-scheme is the supported path), so the warning
    // is pure noise from the host theme — drop exactly that message.
    glib::log_set_writer_func(|level, fields| {
        let msg = fields
            .iter()
            .find(|f| f.key() == "MESSAGE")
            .and_then(|f| f.value_str())
            .unwrap_or_default();
        let domain = fields
            .iter()
            .find(|f| f.key() == "GLIB_DOMAIN")
            .and_then(|f| f.value_str())
            .unwrap_or_default();
        if domain == "Adwaita" && msg.contains("gtk-application-prefer-dark-theme") {
            return glib::LogWriterOutput::Handled;
        }
        glib::log_writer_default(level, fields);
        glib::LogWriterOutput::Handled
    });

    let app = adw::Application::builder()
        .application_id(satori_core::config::APP_ID)
        // NON_UNIQUE while in development: a relaunch must run the NEW
        // binary, not remote-activate a stale process (the classic "my fix
        // doesn't work" trap). Single-instance focus behavior returns in M5.
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &adw::Application) {
    theme::load_current();
    icons::install();
    let core = Arc::new(Core::new());

    // The main shell: a sidebar + section stack (DESIGN: the sidebar IS the
    // chrome); toasts overlay the whole main area.
    let overlay = adw::ToastOverlay::new();
    let ctx = ctx::Ctx::new(core.clone(), overlay.clone());
    let feed = feed::FeedView::new(&ctx);
    let (main_shell, handles) = shell::build_main(&ctx, Rc::clone(&feed));

    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .build();
    stack.add_named(&main_shell, Some("main"));

    let login_page = login::LoginPage::new(core.clone(), {
        let stack = stack.clone();
        let handles2 = shell::ShellHandles {
            home_dot: handles.home_dot.clone(),
            notif_dot: handles.notif_dot.clone(),
            avatar: handles.avatar.clone(),
            account_label: handles.account_label.clone(),
        };
        let ctx = ctx.clone();
        let feed_login = Rc::clone(&feed);
        move |_pk| {
            feed_login.bootstrap();
            shell::poll_notif(&ctx, &handles2.notif_dot);
            shell::poll_feed_dot(&ctx, &handles2.home_dot);
            crate::shell::poll_time_ticks();
            stack.set_visible_child_full("main", gtk::StackTransitionType::Crossfade);
        }
    });
    stack.add_named(login_page.widget(), Some("login"));

    // Settings: an AdwPreferencesWindow (account menu).
    let settings_action = gio::SimpleAction::new("settings", None);
    {
        let ctx = ctx.clone();
        settings_action.connect_activate(move |_, _| {
            crate::settings::open_settings(&ctx);
        });
    }
    app.add_action(&settings_action);

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

    gtk::Window::set_default_icon_name("satori-logo");

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Satori")
        .default_width(880)
        .default_height(760)
        .content(&stack)
        .build();
    window.present();

    // Resume a persisted session, else first-run: the login page. Resume
    // failures are surfaced on the login page, never silent.
    let (resumed, resume_error) = core.init();
    match resumed {
        Some(session) => {
            if let Ok(pk) = PublicKey::from_str(&session.pubkey) {
                shell::update_identity(&handles, &ctx, pk);
            }
            feed.bootstrap();
            shell::poll_notif(&ctx, &handles.notif_dot);
            shell::poll_feed_dot(&ctx, &handles.home_dot);
            crate::shell::poll_time_ticks();
            stack.set_visible_child_full("main", gtk::StackTransitionType::Crossfade);
        }
        None => {
            if let Some(err) = resume_error {
                login_page.set_resume_error(&err);
            }
            stack.set_visible_child_full("login", gtk::StackTransitionType::Crossfade);
        }
    }
}
