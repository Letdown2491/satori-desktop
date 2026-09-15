//! Notifications — port of `src/routes/notifications.ts` (slice 1): the new
//! set as cards, a quiet mark-seen on open, the caught-up line. No counts,
//! no badges — a dot (layout.ts notifBell).

use std::collections::HashMap;

use adw::prelude::*;
use satori_core::manager::{FeedEvent, Profile};

use crate::bridge::drain;
use crate::ctx::Ctx;
use crate::cards::card_for;

pub fn notifications_page(ctx: &Ctx) -> adw::NavigationPage {
    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .margin_top(6)
        .build();

    let list = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    column.append(&list);

    let status = gtk::Label::builder()
        .label("Gathering…")
        .css_classes(["tagline"])
        .halign(gtk::Align::Center)
        .margin_top(24)
        .build();
    column.append(&status);

    let clamp = crate::shell::reading_column(&column, 720);
    let scroll = gtk::ScrolledWindow::builder()
        .child(&clamp)
        .hexpand(true)
        .vexpand(true)
        .build();

    let page = adw::NavigationPage::builder()
        .title("Notifications")
        .child(&scroll)
        .build();

    // Opening the page marks seen (routes/notifications.ts:115) — the dot
    // goes quiet even if the fetch is slow. The pre-visit high-water splits
    // the page into New / Earlier.
    let prev_high_water = satori_core::read_state::load().notif;
    ctx.core.mark_notifications_seen();

    {
        let ctx = ctx.clone();
        let list = list.clone();
        let status = status.clone();
        let page2 = page.clone();
        let _ = &page2;
        drain(ctx.core.notifications(), move |ev| match ev {
            FeedEvent::Notifications(notifs, profiles) => {
                let map: HashMap<String, Profile> =
                    profiles.iter().cloned().map(|p| (p.pubkey.clone(), p)).collect();
                status.set_visible(false);
                if notifs.is_empty() {
                    status.set_text("Nothing new. Sit quietly.");
                    status.set_visible(true);
                }
                let new_count = notifs
                    .iter()
                    .filter(|n| n.created_at.as_secs() > prev_high_water)
                    .count();
                if new_count > 0 && new_count < notifs.len() {
                    let head = gtk::Label::builder()
                        .label("New")
                        .css_classes(["serif", "note-author", "section-title"])
                        .halign(gtk::Align::Start)
                        .margin_top(8)
                        .build();
                    list.append(&head);
                }
                for (i, n) in notifs.iter().enumerate() {
                    if new_count > 0
                        && new_count < notifs.len()
                        && i == new_count
                    {
                        let head = gtk::Label::builder()
                            .label("Earlier")
                            .css_classes(["serif", "section-title", "tagline"])
                            .halign(gtk::Align::Start)
                            .margin_top(16)
                            .build();
                        list.append(&head);
                    }
                    let profile = map.get(&n.pubkey.to_string()).cloned();
                    let card = card_for(&ctx, n, profile.as_ref(), false);
                    list.append(&card);
                }
            }
            FeedEvent::Failed(msg) => {
                status.set_text(&format!("Could not load notifications: {msg}"));
            }
            _ => {}
        });
    }

    page
}
