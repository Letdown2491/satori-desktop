//! Shell chrome — port of `src/render/layout.ts` bar structure: home + bell
//! left, feed switcher centered, compose/search/account right. The 720px
//! reading column lives inside each view.

use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use nostr::prelude::*;

use crate::ctx::Ctx;
use crate::feed::FeedView;

#[derive(Clone)]
pub struct ShellHandles {
    pub avatar: adw::Avatar,
    /// The quiet dot on the bell (hidden by default).
    pub bell_dot: gtk::Image,
}

/// Bell poll — the web app's quiet-dot poller (layout.ts: every 90s while
/// visible). One cheap question: anything newer than the high-water?
pub fn poll_notif(ctx: &Ctx, dot: &gtk::Image) {
    let ctx = ctx.clone();
    let dot = dot.clone();
    glib::timeout_add_seconds_local(90, move || {
        let ctx = ctx.clone();
        let dot = dot.clone();
        crate::bridge::drain(ctx.core.notifications(), move |ev| if let satori_core::manager::FeedEvent::NotifDot(unread) = ev { dot.set_visible(unread) });
        glib::ControlFlow::Continue
    });
}

pub fn build_main(
    ctx: &Ctx,
    feed: Rc<FeedView>,
    content: gtk::Widget,
) -> (gtk::Widget, ShellHandles) {
    let header = gtk::HeaderBar::new();

    // Home: back to the feed (pop the whole nav stack — routes/pages.ts
    // home = navigate to /) + fetch new since the newest known note.
    let home = gtk::Button::builder()
        .child(&crate::icons::icon("home"))
        .css_classes(["flat"])
        .tooltip_text("Home")
        .build();
    {
        let feed = Rc::clone(&feed);
        let nav = ctx.nav.clone();
        home.connect_clicked(move |_| {
            nav.pop_to_page(&feed.page);
            feed.refresh();
        });
    }
    header.pack_start(&home);

    // Bell with quiet dot — opens the notifications page (layout.ts notifBell).
    let bell_img = gtk::Image::builder()
        
        .icon_size(gtk::IconSize::Normal)
        .build();
    let bell_dot = gtk::Image::builder()
        .icon_name("media-record-symbolic")
        .css_classes(["bell-dot"])
        .visible(false)
        .build();
    let bell_overlay = gtk::Overlay::builder().child(&bell_img).build();
    bell_overlay.add_overlay(&bell_dot);
    let bell = gtk::Button::builder()
        .css_classes(["flat"])
        .tooltip_text("Notifications")
        .build();
    bell.set_child(Some(&bell_overlay));
    {
        let ctx = ctx.clone();
        let dot_hide = bell_dot.clone();
        bell.connect_clicked(move |_| {
            // Mark seen immediately; the page re-advances the high-water.
            dot_hide.set_visible(false);
            let page = crate::notifications::notifications_page(&ctx);
            ctx.nav.push(&page);
        });
    }
    header.pack_start(&bell);

    // Feed switcher (center): Following / timelines / Browse a relay — a real
    // popover menu arrives with the timeline views (M2).
    let switcher = gtk::MenuButton::builder()
        .label("Following")
        .css_classes(["flat", "switcher"])
        .build();
    header.set_title_widget(Some(&switcher));

    // Compose — the FAB also exists in the feed; the pen is the reachable twin.
    let compose_btn = crate::icons::action_button("compose", "Compose");
    {
        let ctx = ctx.clone();
        let feed = Rc::clone(&feed);
        compose_btn.connect_clicked(move |_| {
            crate::composer::compose(&ctx, None, Some(crate::feed::hooks(&feed)));
        });
    }
    header.pack_end(&compose_btn);

    let search_btn = crate::icons::action_button("search", "Search");
    {
        let ctx = ctx.clone();
        search_btn.connect_clicked(move |_| {
            let page = crate::search::search_page(&ctx);
            ctx.nav.push(&page);
        });
    }
    header.pack_end(&search_btn);

    let avatar = adw::Avatar::builder().size(28).show_initials(true).build();
    let account_menu = gtk::gio::Menu::new();
    account_menu.append(Some("Settings"), Some("app.settings"));
    account_menu.append(Some("Log out"), Some("app.logout"));
    let avatar_btn = gtk::MenuButton::builder()
        .child(&avatar)
        .css_classes(["flat"])
        .menu_model(&account_menu)
        .build();
    header.pack_end(&avatar_btn);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&content));
    (toolbar.upcast(), ShellHandles { avatar, bell_dot })
}

/// The account avatar shows a short npub until profile cards land (M2's
/// hue-bucket + profile cache takes over — render/util.ts:74).
pub fn update_avatar(avatar: &adw::Avatar, pubkey: PublicKey) {
    let npub = pubkey.to_bech32().unwrap_or_default();
    if npub.len() >= 7 {
        avatar.set_text(Some(&npub[..7]));
    }
}
