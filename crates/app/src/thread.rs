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
    let clamped = crate::shell::reading_column(&scroll, 720);
    adw::NavigationPage::builder()
        .title("Thread")
        .child(&clamped)
        .build()
}

/// NIP-10 nesting: a reply's parent is its last `e` tag that exists in the
/// reply set (else it's a root); children indent under the left border.
fn nest_replies(ctx: &Ctx, replies: &[Event], map: &HashMap<String, Profile>, box_: &gtk::Box) {
    let ids: std::collections::HashSet<String> =
        replies.iter().map(|r| r.id.to_string()).collect();

    fn render(
        ctx: &Ctx,
        ev: &Event,
        map: &HashMap<String, Profile>,
        children: &HashMap<String, Vec<Event>>,
        depth: usize,
        parent_box: &gtk::Box,
    ) {
        let card = note_card(ctx, ev, map.get(&ev.pubkey.to_string()), false);
        card.add_css_class("thread-reply");
        card.set_margin_top(if depth == 0 { 0 } else { 4 });
        parent_box.append(&card);
        if depth < 3
            && let Some(kids) = children.get(&ev.id.to_string()) {
                let slot = gtk::Box::builder()
                    .orientation(gtk::Orientation::Vertical)
                    .margin_start(if depth == 0 { 20 } else { 14 })
                    .css_classes(["comment-children"])
                    .build();
                parent_box.append(&slot);
                for kid in kids {
                    render(ctx, kid, map, children, depth + 1, &slot);
                }
            }
    }

    let mut children: HashMap<String, Vec<Event>> = HashMap::new();
    let mut roots: Vec<Event> = Vec::new();
    for r in replies {
        // Last e tag = direct parent (NIP-10 marked-e reads right-to-left).
        let parent = r
            .tags
            .clone()
            .event_ids()
            .last()
            .filter(|pid| ids.contains(&pid.to_string()) && pid != &r.id);
        match parent {
            Some(pid) => children.entry(pid.to_string()).or_default().push(r.clone()),
            None => roots.push(r.clone()),
        }
    }
    for r in &roots {
        render(ctx, r, map, &children, 0, box_);
    }
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
                    nest_replies(&ctx, &replies, &map, &replies_box);
                }
            }
            FeedEvent::Failed(msg) => {
                loading.set_text(&format!("Replies failed to load: {msg}"));
            }
            _ => {}
        });
    }

    column.upcast()
}
