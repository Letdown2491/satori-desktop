//! Profile view — port of `src/routes/read.rs` getProfile + `render/note.ts`
//! profileHeader (slice 1): header (avatar, name, npub, about) + their notes,
//! follow/unfollow and mute/unmute (encrypted list writes in core).
//! Pinned/articles strips and banner land in M2/M3.

use std::collections::HashMap;

use adw::prelude::*;
use gtk::pango::WrapMode;
use nostr::prelude::*;
use satori_core::manager::{FeedEvent, Profile, PublishEvent};

use crate::bridge::drain;
use crate::ctx::Ctx;
use crate::cards::card_for;

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
        .visible(false)
        .build();
    head.append(&about_label);

    // Follow + mute: dual-label buttons flipped by state (actions.ts
    // label-on/label-off, expressed as state).
    let actions = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();

    let following = ctx.core.is_following(&who);
    let follow_btn = gtk::Button::new();
    style_follow(&follow_btn, following);
    actions.append(&follow_btn);

    let muted = ctx.core.is_muted(&who);
    let mute_btn = gtk::Button::builder()
        .label(if muted { "Unmute" } else { "Mute" })
        .css_classes(["flat", "pill"])
        .build();
    actions.append(&mute_btn);
    head.append(&actions);
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

    // Follow toggle: optimistic, revert on failure.
    {
        let ctx = ctx.clone();
        let btn = follow_btn.clone();
        follow_btn.connect_clicked(move |_| {
            let follow = !ctx.core.is_following(&who);
            style_follow(&btn, follow);
            let ctx2 = ctx.clone();
            let btn2 = btn.clone();
            drain(ctx2.core.set_following(who, follow), move |ev| if let PublishEvent::Failed { error, .. } = ev {
                eprintln!("[follow] {error}");
                style_follow(&btn2, !btn2.css_classes().iter().any(|c| c == "flat"));
            });
        });
    }

    // Mute toggle: optimistic; muting also hides their notes from the feed
    // (gated at the store chokepoint) and from this page.
    {
        let ctx = ctx.clone();
        let btn = mute_btn.clone();
        let list = list.clone();
        mute_btn.connect_clicked(move |_| {
            let mute = !ctx.core.is_muted(&who);
            btn.set_label(if mute { "Unmute" } else { "Mute" });
            let ctx2 = ctx.clone();
            let btn2 = btn.clone();
            let list = list.clone();
            drain(ctx2.core.set_muted(who, mute), move |ev| match ev {
                PublishEvent::Failed { error, .. } => {
                    eprintln!("[mute] {error}");
                    btn2.set_label(if mute { "Mute" } else { "Unmute" });
                }
                PublishEvent::Committed { .. } if mute => {
                    // Their cards leave this page immediately.
                    let mut child = list.first_child();
                    while let Some(widget) = child {
                        child = widget.next_sibling();
                        list.remove(&widget);
                    }
                }
                _ => {}
            });
        });
    }

    // Load notes + metadata.
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
                        if let Some(tex) = crate::media_cache::mem_get(&p.picture) {
                            let pic = gtk::Picture::builder()
                                .content_fit(gtk::ContentFit::Cover)
                                .width_request(72)
                                .height_request(72)
                                .build();
                            pic.set_paintable(Some(&tex));
                            pic.add_css_class("avatar-img");
                            avatar_stack.add_child(&pic);
                            avatar_stack.set_visible_child(&pic);
                        } else {
                            let core = ctx.core.clone();
                            let stack = avatar_stack.clone();
                            let url = p.picture.clone();
                            drain(core.fetch_image(url.clone()), move |img| {
                                if let Some(decoded) = img {
                                    let tex = crate::media_cache::from_decoded(&url, &decoded);
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
                }
                let map: HashMap<String, Profile> =
                    page.profiles.iter().cloned().map(|p| (p.pubkey.clone(), p)).collect();
                for n in &page.notes {
                    let profile = map.get(&n.pubkey.to_string()).cloned();
                    let card = card_for(&ctx, n, profile.as_ref(), false);
                    list.append(&card);
                }
                if page.notes.is_empty() {
                    let empty = gtk::Box::builder()
                        .orientation(gtk::Orientation::Vertical)
                        .spacing(8)
                        .halign(gtk::Align::Center)
                        .margin_top(32)
                        .build();
                    let quote = gtk::Label::builder()
                        .css_classes(["serif", "empty-quote"])
                        .build();
                    quote.set_text("“This person has not yet spoken.”");
                    empty.append(&quote);
                    empty.append(&crate::icons::enso(90));
                    status.set_visible(false);
                    list.append(&empty);
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

fn style_follow(btn: &gtk::Button, following: bool) {
    btn.set_label(if following { "Following" } else { "Follow" });
    btn.set_css_classes(if following { &["flat", "pill"] } else { &["seal-btn", "pill"] });
}

/// Entry from anywhere a pubkey is known.
pub fn open_profile(ctx: &Ctx, who: PublicKey) {
    let page = profile_page(ctx, who);
    ctx.nav.push(&page);
}
