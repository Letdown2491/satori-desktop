//! Shell — the sidebar layout: wordmark, section rows (Home /
//! Notifications / Search) with quiet dots, Settings, account row. The
//! main area holds the section Stack (each section its own NavigationView)
//! under one toast overlay. The old top bar is gone — the sidebar IS the
//! chrome (the Fractal/Tuba pattern, sumi-e styled).

use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use nostr::prelude::*;

use crate::bridge::drain;
use crate::ctx::Ctx;
use crate::feed::FeedView;

pub struct ShellHandles {
    pub home_dot: gtk::Image,
    pub notif_dot: gtk::Image,
    /// Sidebar identity widget (patched on login/resume).
    pub avatar: adw::Avatar,
    pub account_label: gtk::Label,
    /// Lives in the titlebar (packed by main).
    pub collapse_btn: gtk::Button,
}

fn side_button(icon: &str, label: &str) -> (gtk::Button, gtk::Label) {
    let btn = gtk::Button::builder()
        .css_classes(["side-row"])
        .halign(gtk::Align::Fill)
        .build();
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(10)
        .build();
    row.append(&crate::icons::icon_sized(icon, 17));
    let lbl = gtk::Label::builder().label(label).halign(gtk::Align::Start).build();
    row.append(&lbl);
    btn.set_child(Some(&row));
    (btn, lbl)
}

/// A sidebar row with a quiet dot suffix.
fn side_button_dot(icon: &str, label: &str) -> (gtk::Button, gtk::Image, gtk::Label) {
    let (btn, lbl) = side_button(icon, label);
    let dot = gtk::Image::builder()
        .icon_name("media-record-symbolic")
        .css_classes(["bell-dot"])
        .halign(gtk::Align::End)
        .valign(gtk::Align::Center)
        .hexpand(true)
        .visible(false)
        .build();
    if let Some(row) = btn.child().and_downcast::<gtk::Box>() {
        row.append(&dot);
    }
    (btn, dot, lbl)
}

pub fn build_main(ctx: &Ctx, feed: Rc<FeedView>) -> (gtk::Widget, ShellHandles) {
    // --- sections -------------------------------------------------------------
    let feed_nav = adw::NavigationView::new();
    feed_nav.add(&feed.page);

    let notif_nav = adw::NavigationView::new();
    notif_nav.add(&crate::notifications::notifications_page(ctx));

    let search_nav = adw::NavigationView::new();
    search_nav.add(&crate::search::search_page(ctx));

    let sections = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .hhomogeneous(false)
        .vhomogeneous(false)
        .build();
    sections.add_named(&feed_nav, Some("feed"));
    sections.add_named(&notif_nav, Some("notifications"));
    sections.add_named(&search_nav, Some("search"));

    // --- sidebar ------------------------------------------------------------------
    let sidebar = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .width_request(220)
        .css_classes(["sidebar"])
        .build();
    // Explicit false: stops hexpand propagation from inner rows (labels/
    // dots expand WITHIN the sidebar; the sidebar itself stays 220px).
    sidebar.set_hexpand(false);
    sidebar.set_halign(gtk::Align::Start);

    // Identity row at the top (click → your profile): the avatar + name
    // patch themselves via the profile service once logged in.
    let avatar = adw::Avatar::builder().size(30).show_initials(true).build();
    let account_label = gtk::Label::builder()
        .label("Account")
        .halign(gtk::Align::Start)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let account_row = gtk::Button::builder()
        .css_classes(["side-row", "side-account"])
        .halign(gtk::Align::Fill)
        .build();
    {
        let inner = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(10)
            .build();
        inner.append(&avatar);
        inner.append(&account_label);
        account_row.set_child(Some(&inner));
    }
    {
        let ctx2 = ctx.clone();
        account_row.connect_clicked(move |_| {
            if let Some(me) = ctx2.core.pubkey() {
                crate::profile::open_profile(&ctx2, me);
            }
        });
    }
    sidebar.append(&account_row);

    let (home_btn, home_dot, home_lbl) = side_button_dot("home", "Home");
    let (notif_btn, notif_dot, notif_lbl) = side_button_dot("bell", "Notifications");
    let (search_btn, search_lbl) = side_button("search", "Search");
    home_btn.add_css_class("side-active");
    for b in [&home_btn, &notif_btn, &search_btn] {
        sidebar.append(b);
    }
    let mut navs = HashMap::new();
    navs.insert("feed", feed_nav.clone());
    navs.insert("notifications", notif_nav.clone());
    navs.insert("search", search_nav.clone());
    let mut rows = HashMap::new();
    rows.insert("feed", home_btn.clone());
    rows.insert("notifications", notif_btn.clone());
    rows.insert("search", search_btn.clone());

    // Section switches.
    {
        let ctx2 = ctx.clone();
        let feed2 = Rc::clone(&feed);
        home_btn.connect_clicked(move |_| {
            ctx2.show("feed");
            ctx2.nav().pop_to_page(&feed2.page);
            feed2.refresh();
        });
    }
    {
        let ctx2 = ctx.clone();
        let dot = notif_dot.clone();
        notif_btn.connect_clicked(move |_| {
            // The view IS the seen moment.
            dot.set_visible(false);
            ctx2.core.mark_notifications_seen();
            ctx2.show("notifications");
        });
    }
    {
        let ctx2 = ctx.clone();
        search_btn.connect_clicked(move |_| ctx2.show("search"));
    }

    // Bottom: settings + account.
    let (settings_btn, settings_lbl) = side_button("gear", "Settings");
    sidebar.append(&settings_btn);

    let (logout_btn, logout_lbl) = side_button("power", "Log out");
    sidebar.append(&logout_btn);

    // Collapse toggle: last row.
    let collapse_btn = crate::icons::action_button("sidebar", "Toggle sidebar");
    sidebar.append(&collapse_btn);
    {
        let logout_btn2 = logout_btn.clone();
        logout_btn.connect_clicked(move |_| {
            logout_btn2.activate_action("app.logout", None).ok();
        });
    }

    // --- collapse --------------------------------------------------------------
    let collapsed = crate::prefs::load().sidebar_collapsed.unwrap_or(false);
    let collapsible: Vec<gtk::Widget> = vec![
        home_lbl.upcast(),
        notif_lbl.upcast(),
        search_lbl.upcast(),
        settings_lbl.upcast(),
        logout_lbl.upcast(),
        account_label.clone().upcast(),
    ];
    let sidebar_for_collapse = sidebar.clone();
    let apply_collapse = move |on: bool| {
        sidebar_for_collapse.set_width_request(if on { 64 } else { 220 });
        for w in &collapsible {
            w.set_visible(!on);
        }
    };
    apply_collapse(collapsed);
    {
        let apply = apply_collapse.clone();
        collapse_btn.connect_clicked(move |_| {
            let next = !crate::prefs::load().sidebar_collapsed.unwrap_or(false);
            let mut p = crate::prefs::load();
            p.sidebar_collapsed = Some(next);
            crate::prefs::save(&p);
            apply(next);
        });
    }

    // --- main area: the sidebar+timeline unit centers in the window -------
    let overlay = ctx.overlay.clone();
    overlay.set_child(Some(&sections));

    let layout = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .build();
    layout.append(&sidebar);
    layout.append(&ctx.overlay.clone());
    // The app unit caps at 960 and CENTERS when the window is wider;
    // below that it fills (no clipping at tiled widths). The clamp's own
    // centering is the "margin: 0 auto" primitive.
    let outer = adw::Clamp::builder().maximum_size(960).child(&layout).build();

    // Wire ctx AFTER construction (handlers above captured the parts).
    ctx.attach(&sections, navs, rows);

    // Settings.
    {
        let ctx2 = ctx.clone();
        settings_btn.connect_clicked(move |_| crate::settings::open_settings(&ctx2));
    }

    (
        outer.upcast(),
        ShellHandles { home_dot, notif_dot, avatar, account_label, collapse_btn },
    )
}

/// Identity patch: sidebar avatar + name once logged in.
pub fn update_identity(handles: &ShellHandles, ctx: &Ctx, pk: PublicKey) {
    handles.account_label.set_text(&pk.to_bech32().unwrap_or_default()[..10]);
    // Reactive: the real name + avatar arrive via the profile service.
    let label = handles.account_label.clone();
    let avatar = handles.avatar.clone();
    let core = ctx.core.clone();
    drain(core.ensure_profiles(vec![pk]), move |reply| {
        if let satori_core::manager::FeedEvent::Profiles(ps) = reply
            && let Some(p) = ps.into_iter().find(|p| p.pubkey == pk.to_string()) {
                if !p.name.is_empty() {
                    label.set_text(&p.name);
                }
                if !p.picture.is_empty() {
                    let core2 = core.clone();
                    let avatar2 = avatar.clone();
                    let url = p.picture.clone();
                    drain(core2.fetch_image(url.clone()), move |img| {
                        if let Some(decoded) = img {
                            let tex = crate::media_cache::from_decoded(&url, &decoded);
                            // AdwAvatar clips circular — GtkImage can't.
                            avatar2.set_custom_image(Some(&tex));
                        }
                    });
                }
            }
    });
}

/// The reading column: an AdwClamp wrapping the scroll — its natural width
/// is min(content, max), so the sidebar+timeline unit centers in the window
/// AND shrinks on narrow/tiled windows. (A ScrolledWindow's natural size
/// collapses to its minimum, so the clamp must sit OUTSIDE it.)
pub fn reading_column(child: &impl IsA<gtk::Widget>, max: i32) -> adw::Clamp {
    adw::Clamp::builder().maximum_size(max).child(child).build()
}

/// Dot polls (the web app's quiet-dot pollers — layout.ts).
pub fn poll_notif(ctx: &Ctx, dot: &gtk::Image) {
    let ctx = ctx.clone();
    let dot = dot.clone();
    glib::timeout_add_seconds_local(90, move || {
        let ctx2 = ctx.clone();
        let dot2 = dot.clone();
        crate::bridge::drain(ctx2.core.notifications(), move |ev| {
            if let satori_core::manager::FeedEvent::NotifDot(unread) = ev {
                dot2.set_visible(unread);
            }
        });
        glib::ControlFlow::Continue
    });
}

pub fn poll_feed_dot(ctx: &Ctx, dot: &gtk::Image) {
    let ctx = ctx.clone();
    let dot = dot.clone();
    glib::timeout_add_seconds_local(60, move || {
        let ctx2 = ctx.clone();
        let dot2 = dot.clone();
        crate::bridge::drain(ctx2.core.feed_dot(), move |ev| {
            if let satori_core::manager::FeedEvent::NotifDot(unread) = ev {
                dot2.set_visible(unread);
            }
        });
        glib::ControlFlow::Continue
    });
}

/// Time-ago tick: refresh visible labels every 60s.
pub fn poll_time_ticks() {
    glib::timeout_add_seconds_local(60, || {
        crate::note_card::tick_time_labels();
        glib::ControlFlow::Continue
    });
}
