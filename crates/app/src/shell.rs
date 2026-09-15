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
    /// Sidebar identity widgets (patched on login/resume).
    pub avatar: gtk::Stack,
    pub account_label: gtk::Label,
}

fn side_button(icon: &str, label: &str) -> gtk::Button {
    let btn = gtk::Button::builder()
        .css_classes(["side-row"])
        .halign(gtk::Align::Fill)
        .build();
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(10)
        .build();
    row.append(&crate::icons::icon_sized(icon, 17));
    row.append(&gtk::Label::builder().label(label).halign(gtk::Align::Start).build());
    btn.set_child(Some(&row));
    btn
}

/// A sidebar row with a quiet dot suffix.
fn side_button_dot(icon: &str, label: &str) -> (gtk::Button, gtk::Image) {
    let btn = side_button(icon, label);
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
    (btn, dot)
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
        .margin_top(14)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(6)
        .css_classes(["sidebar"])
        .build();
    // Explicit false: stops hexpand propagation from inner rows (labels/
    // dots expand WITHIN the sidebar; the sidebar itself stays 220px).
    sidebar.set_hexpand(false);
    sidebar.set_halign(gtk::Align::Start);

    // Wordmark: still ensō + serif name (layout.ts wordmark).
    let wordmark = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_start(10)
        .margin_bottom(18)
        .build();
    wordmark.append(&crate::icons::enso(30));
    wordmark.append(&gtk::Label::builder()
        .label("Satori")
        .css_classes(["serif", "wordmark", "side-title"])
        .build());
    sidebar.append(&wordmark);

    let (home_btn, home_dot) = side_button_dot("home", "Home");
    let (notif_btn, notif_dot) = side_button_dot("bell", "Notifications");
    let search_btn = side_button("search", "Search");
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
    let spacer = gtk::Label::new(None);
    spacer.set_vexpand(true);
    sidebar.append(&spacer);
    sidebar.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

    let settings_btn = side_button("gear", "Settings");
    sidebar.append(&settings_btn);

    // Account row (click → your profile) + Log out row.
    let placeholder_pk = PublicKey::from_hex(&"0".repeat(64)).unwrap();
    let avatar = crate::note_card::avatar_widget(ctx, placeholder_pk, None);
    avatar.set_width_request(28);
    avatar.set_height_request(28);
    let account_label = gtk::Label::builder()
        .label("Account")
        .halign(gtk::Align::Start)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let account_inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(10)
        .build();
    account_inner.append(&avatar);
    account_inner.append(&account_label);
    let account_row = gtk::Button::builder()
        .child(&account_inner)
        .css_classes(["side-row", "side-account"])
        .halign(gtk::Align::Fill)
        .build();
    {
        let ctx2 = ctx.clone();
        account_row.connect_clicked(move |_| {
            if let Some(me) = ctx2.core.pubkey() {
                crate::profile::open_profile(&ctx2, me);
            }
        });
    }
    sidebar.append(&account_row);

    let logout_btn = side_button("power", "Log out");
    sidebar.append(&logout_btn);
    {
        let logout_btn2 = logout_btn.clone();
        logout_btn.connect_clicked(move |_| {
            logout_btn2.activate_action("app.logout", None).ok();
        });
    }

    // --- main area: expands, the sidebar stays at its 220px --------------
    let overlay = ctx.overlay.clone();
    overlay.set_child(Some(&sections));
    overlay.set_hexpand(true);

    let separator = gtk::Separator::new(gtk::Orientation::Vertical);
    let layout = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .build();
    layout.append(&sidebar);
    layout.append(&separator);
    layout.append(&ctx.overlay.clone());

    // Wire ctx AFTER construction (handlers above captured the parts).
    ctx.attach(&sections, navs, rows);

    // Settings.
    {
        let ctx2 = ctx.clone();
        settings_btn.connect_clicked(move |_| crate::settings::open_settings(&ctx2));
    }

    (
        layout.upcast(),
        ShellHandles { home_dot, notif_dot, avatar, account_label },
    )
}

/// Identity patch: sidebar avatar + name once logged in.
pub fn update_identity(handles: &ShellHandles, ctx: &Ctx, pk: PublicKey) {
    handles.account_label.set_text(&pk.to_bech32().unwrap_or_default()[..10]);
    // Reactive: the real name + avatar arrive via the profile service.
    let label = handles.account_label.clone();
    let stack = handles.avatar.clone();
    let core = ctx.core.clone();
    drain(core.ensure_profiles(vec![pk]), move |reply| {
        if let satori_core::manager::FeedEvent::Profiles(ps) = reply
            && let Some(p) = ps.into_iter().find(|p| p.pubkey == pk.to_string()) {
                if !p.name.is_empty() {
                    label.set_text(&p.name);
                }
                if !p.picture.is_empty() {
                    let mk = |tex: &gtk::gdk::MemoryTexture| {
                        let img = gtk::Image::builder()
                            .pixel_size(28)
                            .css_classes(["avatar-img"])
                            .build();
                        img.set_paintable(Some(tex));
                        img
                    };
                    if let Some(tex) = crate::media_cache::mem_get(&p.picture) {
                        let img = mk(&tex);
                        stack.add_child(&img);
                        stack.set_visible_child(&img);
                    } else {
                        let core2 = core.clone();
                        let stack2 = stack.clone();
                        let url = p.picture.clone();
                        drain(core2.fetch_image(url.clone()), move |img| {
                            if let Some(decoded) = img {
                                let tex = crate::media_cache::from_decoded(&url, &decoded);
                                let img = mk(&tex);
                                stack2.add_child(&img);
                                stack2.set_visible_child(&img);
                            }
                        });
                    }
                }
            }
    });
}

/// The reading column: max-width clamp that HUGS the sidebar (the
/// sidebar + timeline form one tight unit; empty paper goes to the right).
pub fn reading_column(child: &impl IsA<gtk::Widget>, max: i32) -> adw::Clamp {
    let clamp = adw::Clamp::builder().maximum_size(max).child(child).build();
    clamp.set_halign(gtk::Align::Start);
    clamp.set_margin_start(16);
    clamp
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
