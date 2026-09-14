//! Thread view — port of `src/routes/read.rs` getThread + `render/note.ts`
//! focusedNote (slice 1): focused note + flat reply list, reply composer.
//! The NIP-10 depth-4 tree and private-reply folding land in M2.

use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use nostr::prelude::*;
use satori_core::manager::{FeedEvent, Profile};

use crate::bridge::drain;
use crate::ctx::Ctx;
use crate::note_card::note_card;

/// Open a thread by event id (nostr:note/nevent links): a loading page that
/// fetches the anchor event, then fills with the thread.
pub fn thread_page_by_id(ctx: &Ctx, id: EventId) -> adw::NavigationPage {
    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .build();
    let loading = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    loading.append(&crate::icons::enso(120));
    loading.append(&gtk::Label::new(Some("Fetching the note…")));
    stack.add_named(&loading, Some("loading"));

    let page = adw::NavigationPage::builder()
        .title("Thread")
        .child(&stack)
        .build();

    {
        let ctx = ctx.clone();
        let stack = stack.clone();
        let page_for_title = page.clone();
        let _ = &page_for_title;
        drain(ctx.core.event_by_id(id), move |reply| {
            use satori_core::manager::FeedEvent;
            match reply {
                FeedEvent::One(Some(ev), profile) => {
                    let content = thread_content(&ctx, &ev, profile.as_ref());
                    stack.remove(&loading);
                    stack.add_child(&content);
                    stack.set_visible_child(&content);
                }
                FeedEvent::One(None, _) => {
                    stack.remove(&loading);
                    let missing = gtk::Label::builder()
                        .label("This note could not be found on your relays.")
                        .css_classes(["tagline"])
                        .build();
                    stack.add_child(&missing);
                    stack.set_visible_child(&missing);
                }
                FeedEvent::Failed(msg) => {
                    eprintln!("[thread] fetch by id: {msg}");
                }
                _ => {}
            }
        });
    }
    page
}

pub fn thread_page(ctx: &Ctx, event: &Event) -> adw::NavigationPage {
    let content = thread_content(ctx, event, None);
    let scroll = gtk::ScrolledWindow::builder()
        .child(&content)
        .hexpand(true)
        .vexpand(true)
        .build();
    adw::NavigationPage::builder()
        .title("Thread")
        .child(&scroll)
        .build()
}

fn thread_content(ctx: &Ctx, event: &Event, profile: Option<&Profile>) -> gtk::Widget {
    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .margin_top(6)
        .spacing(8)
        .build();

    let focused = note_card(ctx, event, profile, true);
    column.append(&focused);

    let reply_btn = gtk::Button::builder()
        .label("Reply")
        .css_classes(["seal-btn"])
        .halign(gtk::Align::Start)
        .margin_top(6)
        .build();
    column.append(&reply_btn);

    let replies_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(0)
        .build();
    let replies_label = gtk::Label::builder()
        .label("Replies")
        .css_classes(["serif", "note-author"])
        .halign(gtk::Align::Start)
        .margin_top(12)
        .visible(false)
        .build();
    column.append(&replies_label);
    column.append(&replies_box);

    let loading = gtk::Label::builder()
        .label("Gathering replies…")
        .css_classes(["tagline"])
        .build();
    column.append(&loading);

    // Reply composer bound to this thread: pending replies prepend here.
    {
        let ctx = ctx.clone();
        let event = event.clone();
        let replies_box = replies_box.clone();
        reply_btn.connect_clicked(move |_| {
            let ctx_h = ctx.clone();
            let replies_h = replies_box.clone();
            let event_h = event.clone();
            crate::composer::compose(
                &ctx,
                Some(event_h),
                Some(crate::composer::WriteHooks {
                    held: Rc::new(move |ev, _token, _secs| {
                        let card = note_card(&ctx_h, &ev, None, false);
                        card.add_css_class("pending");
                        replies_h.append(&card);
                    }),
                    undone: Rc::new(|_| {}), // TODO(M1): remove the pending reply
                    committed: Rc::new(|_| {}),
                }),
            );
        });
    }

    // Fetch replies.
    {
        let ctx = ctx.clone();
        let event = event.clone();
        let replies_box = replies_box.clone();
        let replies_label = replies_label.clone();
        let loading = loading.clone();
        drain(ctx.core.thread(event.id), move |ev| match ev {
            FeedEvent::Replies(replies, profiles) => {
                loading.set_visible(false);
                let map: HashMap<String, Profile> =
                    profiles.iter().cloned().map(|p| (p.pubkey.clone(), p)).collect();
                if replies.is_empty() {
                    replies_label.set_text("No replies yet");
                    replies_label.set_visible(true);
                } else {
                    replies_label.set_visible(true);
                    for r in &replies {
                        let profile = map.get(&r.pubkey.to_string()).cloned();
                        let card = note_card(&ctx, r, profile.as_ref(), false);
                        replies_box.append(&card);
                    }
                }
            }
            FeedEvent::Failed(msg) => {
                loading.set_text(&format!("Replies failed to load: {msg}"));
            }
            _ => {}
        });
    }

    let clamp = adw::Clamp::builder().maximum_size(720).child(&column).build();
    clamp.upcast()
}
