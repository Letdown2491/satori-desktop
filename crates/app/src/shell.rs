//! Shell chrome — port of `src/render/layout.ts` bar structure: home + bell
//! left, feed switcher centered, compose/search/account right. The 720px
//! reading column lives inside each view.

use std::rc::Rc;

use adw::prelude::*;
use nostr::prelude::*;

use crate::ctx::Ctx;
use crate::feed::FeedView;

pub fn build_main(
    ctx: &Ctx,
    feed: Rc<FeedView>,
    content: gtk::Widget,
) -> (gtk::Widget, adw::Avatar) {
    let header = gtk::HeaderBar::new();

    // Home: back to the feed + fetch new since the newest known note.
    let home = gtk::Button::builder()
        .icon_name("home-symbolic")
        .css_classes(["flat"])
        .tooltip_text("Home")
        .build();
    {
        let feed = Rc::clone(&feed);
        home.connect_clicked(move |_| feed.refresh());
    }
    header.pack_start(&home);

    // Feed switcher (center): Following / timelines / Browse a relay — a real
    // popover menu arrives with the timeline views (M2).
    let switcher = gtk::MenuButton::builder()
        .label("Following")
        .css_classes(["flat", "switcher"])
        .build();
    header.set_title_widget(Some(&switcher));

    // Compose — the FAB also exists in the feed; the pen is the reachable twin.
    let compose_btn = gtk::Button::builder()
        .icon_name("document-edit-symbolic")
        .css_classes(["flat"])
        .tooltip_text("Compose")
        .build();
    {
        let ctx = ctx.clone();
        let feed = Rc::clone(&feed);
        compose_btn.connect_clicked(move |_| {
            crate::composer::compose(&ctx, None, Some(crate::feed::hooks(&feed)));
        });
    }
    header.pack_end(&compose_btn);

    let search_btn = gtk::Button::builder()
        .icon_name("edit-find-symbolic")
        .css_classes(["flat"])
        .tooltip_text("Search")
        .sensitive(false) // M2 (NIP-50)
        .build();
    header.pack_end(&search_btn);

    let avatar = adw::Avatar::builder().size(28).show_initials(true).build();
    let account_menu = gtk::gio::Menu::new();
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
    (toolbar.upcast(), avatar)
}

/// The account avatar shows a short npub until profile cards land (M2's
/// hue-bucket + profile cache takes over — render/util.ts:74).
pub fn update_avatar(avatar: &adw::Avatar, pubkey: PublicKey) {
    let npub = pubkey.to_bech32().unwrap_or_default();
    if npub.len() >= 7 {
        avatar.set_text(Some(&npub[..7]));
    }
}

pub(crate) fn enso_path() -> std::path::PathBuf {
    // Assets ship as GResources in packaging; dev-time path for now.
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/enso.svg")
}
