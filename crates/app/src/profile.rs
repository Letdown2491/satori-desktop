//! Profile view — port of `src/routes/read.rs` getProfile + `render/note.ts`
//! profileHeader (slice 1): header (avatar, name, npub, about) + their notes.
//! Pinned/articles strips, follow/mute buttons, banner land in M2/M3.

use std::collections::HashMap;

use adw::prelude::*;
use gtk::pango::WrapMode;
use nostr::prelude::*;
use satori_core::manager::{FeedEvent, Profile};

use crate::bridge::drain;
use crate::ctx::Ctx;
use crate::note_card::note_card;

pub fn profile_page(ctx: &Ctx, who: PublicKey) -> adw::NavigationPage {
    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(12)
        .build();

    // Header card.
    let head = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .css_classes(["note-card"])
        .build();
    let avatar = crate::note_card::avatar_widget(ctx, who, None);
    avatar.set_width_request(72);
    avatar.set_height_request(72);
    head.append(&avatar);

    let name = gtk::Label::builder()
        .css_classes(["wordmark", "serif"])
        .halign(gtk::Align::Start)
        .build();
    head.append(&name);

    let npub = who.to_bech32().unwrap_or_default();
    let npub_label = gtk::Label::builder()
        .css_classes(["tagline"])
        .halign(gtk::Align::Start)
        .build();
    npub_label.set_markup(&format!("<tt>{}</tt>", &npub[..npub.len().min(24)]));
    head.append(&npub_label);

    let about_label = gtk::Label::builder()
        .wrap(true)
        .wrap_mode(WrapMode::WordChar)
        .xalign(0.0)
        .halign(gtk::Align::Start)
        .build();
    head.append(&about_label);
    column.append(&head);

    // Notes.
    let list = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    column.append(&list);
    let status = gtk::Label::builder()
        .label("Fetching notes…")
        .css_classes(["tagline"])
        .halign(gtk::Align::Center)
        .build();
    column.append(&status);

    let clamp = adw::Clamp::builder().maximum_size(720).child(&column).build();
    let scroll = gtk::ScrolledWindow::builder()
        .child(&clamp)
        .hexpand(true)
        .vexpand(true)
        .build();

    let page = adw::NavigationPage::builder()
        .title("Profile")
        .child(&scroll)
        .build();

    {
        let ctx = ctx.clone();
        let list = list.clone();
        let status = status.clone();
        let name = name.clone();
        let about = about_label.clone();
        let who_hex = who.to_string();
        let avatar_stack = avatar.clone();
        drain(ctx.core.profile(who), move |ev| match ev {
            FeedEvent::Loaded(page) => {
                status.set_visible(false);
                // Header metadata: latest profile for this author.
                if let Some(p) = page
                    .profiles
                    .iter()
                    .find(|p| p.pubkey == who_hex)
                    .cloned()
                    .or_else(|| Some(Profile::default()))
                {
                    name.set_text(if p.name.is_empty() { "—" } else { &p.name });
                    if !p.about.is_empty() {
                        about.set_text(&p.about);
                        about.set_visible(true);
                    }
                    if !p.picture.is_empty() {
                        let core = ctx.core.clone();
                        let stack = avatar_stack.clone();
                        drain(core.fetch_image(p.picture.clone()), move |img| {
                            if let Some(decoded) = img {
                                let bytes = gtk::glib::Bytes::from_owned(decoded.rgba);
                                let tex = gtk::gdk::MemoryTexture::new(
                                    decoded.width as i32,
                                    decoded.height as i32,
                                    gtk::gdk::MemoryFormat::R8g8b8a8,
                                    &bytes,
                                    decoded.width as usize * 4,
                                );
                                let pic = gtk::Picture::builder()
                                    .content_fit(gtk::ContentFit::Cover)
                                    .width_request(72)
                                    .height_request(72)
                                    .build();
                                pic.set_paintable(Some(&tex));
                                pic.add_css_class("avatar-img");
                                stack.add_child(&pic);
                                stack.set_visible_child(&pic);
                            }
                        });
                    }
                }
                let map: HashMap<String, Profile> =
                    page.profiles.iter().cloned().map(|p| (p.pubkey.clone(), p)).collect();
                for n in &page.notes {
                    let profile = map.get(&n.pubkey.to_string()).cloned();
                    let card = note_card(&ctx, n, profile.as_ref(), false);
                    list.append(&card);
                }
                if page.notes.is_empty() {
                    status.set_text("No notes yet.");
                    status.set_visible(true);
                }
            }
            FeedEvent::Failed(msg) => {
                status.set_text(&format!("Could not load profile: {msg}"));
            }
            _ => {}
        });
    }

    page
}

/// Entry from anywhere a pubkey is known.
pub fn open_profile(ctx: &Ctx, who: PublicKey) {
    let page = profile_page(ctx, who);
    ctx.nav.push(&page);
}

