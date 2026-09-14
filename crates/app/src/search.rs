//! Search — port of `src/routes/search.ts` (slice 1: notes only, NIP-50
//! `search` filter). People results and the operator grammar (`by:`, `p:`,
//! `has:`, `since:`, `site:`) land in M2.

use std::collections::HashMap;

use adw::prelude::*;
use nostr::prelude::*;
use satori_core::manager::{FeedEvent, Profile};

use crate::bridge::drain;
use crate::ctx::Ctx;
use crate::note_card::note_card;

pub fn search_page(ctx: &Ctx) -> adw::NavigationPage {
    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .margin_top(12)
        .spacing(10)
        .build();

    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    let entry = gtk::SearchEntry::builder()
        .placeholder_text("Search notes…")
        .hexpand(true)
        .build();
    let go = gtk::Button::builder()
        .label("Search")
        .css_classes(["seal-btn"])
        .build();
    row.append(&entry);
    row.append(&go);
    column.append(&row);

    let status = gtk::Label::builder()
        .css_classes(["tagline"])
        .halign(gtk::Align::Center)
        .visible(false)
        .build();
    column.append(&status);

    let list = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    column.append(&list);

    let clamp = adw::Clamp::builder().maximum_size(720).child(&column).build();
    let scroll = gtk::ScrolledWindow::builder()
        .child(&clamp)
        .hexpand(true)
        .vexpand(true)
        .build();

    let page = adw::NavigationPage::builder()
        .title("Search")
        .child(&scroll)
        .build();

    let run = {
        let ctx = ctx.clone();
        let entry = entry.clone();
        let list = list.clone();
        let status = status.clone();
        move || {
            let q = entry.text().to_string();
            if q.trim().is_empty() {
                return;
            }
            // New question, clean answer: clear previous results.
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            status.set_text("Searching…");
            status.set_visible(true);
            let ctx = ctx.clone();
            let list = list.clone();
            let status = status.clone();
            drain(ctx.core.search(q), move |ev| match ev {
                FeedEvent::Search(results, profiles) => {
                    let map: HashMap<String, Profile> =
                        profiles.iter().cloned().map(|p| (p.pubkey.clone(), p)).collect();
                    if results.is_empty() {
                        status.set_text("Nothing found.");
                        status.set_visible(true);
                    } else {
                        status.set_visible(false);
                    }
                    for n in &results {
                        let profile = map.get(&n.pubkey.to_string()).cloned();
                        let card = note_card(&ctx, n, profile.as_ref(), false);
                        list.append(&card);
                    }
                }
                FeedEvent::Failed(msg) => {
                    status.set_text(&format!("Search failed: {msg}"));
                    status.set_visible(true);
                }
                _ => {}
            });
        }
    };

    go.connect_clicked({
    let run = run.clone();
    move |_| run()
});
    entry.connect_activate({
        let run = run.clone();
        move |_| run()
    });

    page
}
