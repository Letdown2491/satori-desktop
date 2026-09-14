//! NIP-22 comments — port of `src/render/comments.ts` (slice): count +
//! top compose row, threaded list (children indented with the left border),
//! per-comment Reply via a compact dialog. Comments render as shallow rows
//! (avatar/name/body) — no thread-link, like + reply only.

use std::collections::HashMap;

use adw::prelude::*;
use gtk::glib;
use gtk::pango::WrapMode;
use nostr::prelude::*;
use satori_core::manager::{FeedEvent, Profile, PublishEvent};
use satori_core::nip::comment::CommentTarget;

use crate::bridge::drain;
use crate::content::{esc, render_content};
use crate::ctx::Ctx;

pub fn comments_section(ctx: &Ctx, target: &CommentTarget) -> gtk::Box {
    let section = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(24)
        .build();

    let count = gtk::Label::builder()
        .label("Comments")
        .css_classes(["serif", "note-author"])
        .halign(gtk::Align::Start)
        .build();
    section.append(&count);

    // Top compose row (comments.ts: count + top form).
    let compose_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .css_classes(["note-card"])
        .build();
    let entry = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .height_request(56)
        .css_classes(["compose-text"])
        .build();
    compose_row.append(&entry);
    let submit = gtk::Button::builder()
        .label("Comment")
        .css_classes(["seal-btn", "pill"])
        .halign(gtk::Align::End)
        .build();
    compose_row.append(&submit);
    section.append(&compose_row);

    // The threaded tree.
    let tree = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(0)
        .build();
    section.append(&tree);

    let status = gtk::Label::builder()
        .css_classes(["tagline"])
        .halign(gtk::Align::Center)
        .visible(false)
        .build();
    section.append(&status);

    {
        let ctx = ctx.clone();
        let tree = tree.clone();
        let entry = entry.clone();
        let target = target.clone();
        submit.connect_clicked(move |_| {
            let text = entry
                .buffer()
                .text(&entry.buffer().start_iter(), &entry.buffer().end_iter(), true)
                .to_string();
            publish_top(&ctx, text, &tree, &entry, &target);
        });
    }

    // Fetch + render the tree.
    {
        let ctx = ctx.clone();
        let tree = tree.clone();
        let count = count.clone();
        let status = status.clone();
        let target = target.clone();
        drain(ctx.core.comments(target), move |reply| match reply {
            FeedEvent::Comments(comments, profiles) => {
                status.set_visible(false);
                count.set_markup(&format!(
                    "Comments <span foreground=\"#8b8275\">· {}</span>",
                    comments.len()
                ));
                let map: HashMap<String, Profile> =
                    profiles.iter().cloned().map(|p| (p.pubkey.clone(), p)).collect();
                render_tree(&ctx, &comments, &map, &tree);
                if comments.is_empty() {
                    status.set_text("No comments yet.");
                    status.set_visible(true);
                }
            }
            FeedEvent::Failed(msg) => {
                status.set_text(&format!("Comments failed to load: {msg}"));
                status.set_visible(true);
            }
            _ => {}
        });
    }

    section
}

fn publish_top(ctx: &Ctx, text: String, _tree: &gtk::Box, entry: &gtk::TextView, target: &CommentTarget) {
    if text.trim().is_empty() {
        return;
    }
    let ctx2 = ctx.clone();
    let entry2 = entry.clone();
    drain(
        ctx.core.comment(text, target.clone(), None),
        move |reply| if let PublishEvent::Failed { error, .. } = reply {
            eprintln!("[comment] {error}");
            ctx2.toast("Comment failed");
        },
    );
    entry2.buffer().set_text("");
}

/// Group by parent-comment id; depth-first render with indentation
/// (comments.ts threaded list + comment-children border).
fn render_tree(ctx: &Ctx, comments: &[Event], map: &HashMap<String, Profile>, tree: &gtk::Box) {
    // Parent key = the lowercase parent ref value (event id when replying
    // to a comment); everything else is top-level.
    let mut children: HashMap<String, Vec<&Event>> = HashMap::new();
    let mut roots: Vec<&Event> = Vec::new();
    for c in comments {
        let refs = satori_core::nip::comment::parse_refs(c);
        match refs.parent {
            Some(satori_core::nip::comment::Ref::Event(id)) => {
                // A reply to another comment only if that comment exists here.
                if comments.iter().any(|c2| c2.id == id) {
                    children.entry(id.to_string()).or_default().push(c);
                } else {
                    roots.push(c);
                }
            }
            _ => roots.push(c),
        }
    }

    fn render_row(
        ctx: &Ctx,
        ev: &Event,
        map: &HashMap<String, Profile>,
        children: &HashMap<String, Vec<&Event>>,
        depth: usize,
        parent_box: &gtk::Box,
    ) {
        let row = comment_row(ctx, ev, map.get(&ev.pubkey.to_string()), depth);
        parent_box.append(&row);

        // Children slot (indent + left border).
        if depth < 4
            && let Some(kids) = children.get(&ev.id.to_string()) {
                let slot = gtk::Box::builder()
                    .orientation(gtk::Orientation::Vertical)
                    .margin_start(if depth == 0 { 20 } else { 14 })
                    .css_classes(["comment-children"])
                    .build();
                parent_box.append(&slot);
                for kid in kids {
                    render_row(ctx, kid, map, children, depth + 1, &slot);
                }
            }
    }

    for c in &roots {
        render_row(ctx, c, map, &children, 0, tree);
    }
}

fn comment_row(
    ctx: &Ctx,
    ev: &Event,
    profile: Option<&Profile>,
    depth: usize,
) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .css_classes(["comment-row"])
        .build();

    let avatar = crate::note_card::avatar_widget(ctx, ev.pubkey, profile);
    avatar.set_width_request(26);
    avatar.set_height_request(26);
    row.append(&avatar);

    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .build();

    let head = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .build();
    let name = profile.map(|p| p.display()).unwrap_or_else(|| {
        let hex = ev.pubkey.to_string();
        format!("npub1…{}", &hex[hex.len().saturating_sub(6)..])
    });
    let name_label = gtk::Label::builder()
        .css_classes(["note-author"])
        .halign(gtk::Align::Start)
        .build();
    name_label.set_markup(&format!("<a href=\"profile:{}\">{}</a>", ev.pubkey, esc(&name)));
    {
        let ctx = ctx.clone();
        let who = ev.pubkey;
        name_label.connect_activate_link(move |_, _| {
            crate::profile::open_profile(&ctx, who);
            glib::Propagation::Stop
        });
    }
    head.append(&name_label);
    let time = gtk::Label::builder()
        .label(crate::content::time_ago(ev.created_at.as_secs()))
        .css_classes(["tagline"])
        .build();
    head.append(&time);
    body.append(&head);

    let text = gtk::Label::builder()
        .wrap(true)
        .wrap_mode(WrapMode::WordChar)
        .xalign(0.0)
        .halign(gtk::Align::Start)
        .build();
    text.set_markup(&render_content(ev.content.as_str(), &crate::cards::resolver_pub(ctx)).markup);
    body.append(&text);

    // Reply + like — the comment action row (comments.ts per-comment Reply).
    let acts = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(2)
        .build();
    let reply_btn = gtk::Button::builder()
        .label("Reply")
        .css_classes(["flat", "act", "comment-reply"])
        .build();
    {
        let ctx = ctx.clone();
        let ev = ev.clone();
        let _body = body.clone();
        let depth2 = depth;
        let _ = depth2;
        reply_btn.connect_clicked(move |_| {
            // Compact reply dialog: text + submit (comments.ts lazy reply form).
            let dlg_content = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(8)
                .margin_top(12)
                .margin_bottom(12)
                .margin_start(12)
                .margin_end(12)
                .build();
            let view = gtk::TextView::builder()
                .wrap_mode(gtk::WrapMode::WordChar)
                .height_request(80)
                .css_classes(["compose-text"])
                .build();
            dlg_content.append(&view);
            let send = gtk::Button::builder()
                .label("Reply")
                .css_classes(["seal-btn", "pill"])
                .halign(gtk::Align::End)
                .build();
            dlg_content.append(&send);
            let dialog = adw::Dialog::builder()
                .title("Reply")
                .content_width(460)
                .content_height(240)
                .build();
            dialog.set_child(Some(&dlg_content));

            let ctx2 = ctx.clone();
            let ev2 = ev.clone();
            let view2 = view.clone();
            let dialog2 = dialog.clone();
            send.connect_clicked(move |_| {
                let text = view2
                    .buffer()
                    .text(&view2.buffer().start_iter(), &view2.buffer().end_iter(), true)
                    .to_string();
                if text.trim().is_empty() {
                    return;
                }
                let target = CommentTarget::for_event(&ev2);
                let ctx3 = ctx2.clone();
                let ev3 = ev2.clone();
                drain(ctx3.core.comment(text, target, Some(ev3)), move |reply| if let PublishEvent::Failed { error, .. } = reply {
                    eprintln!("[comment] {error}");
                    ctx3.toast("Reply failed");
                });
                dialog2.force_close();
            });
            dialog.present(ctx.nav().root().as_ref());
        });
    }
    acts.append(&reply_btn);

    let like = gtk::Button::builder()
        .css_classes(["flat", "act"])
        .tooltip_text("React")
        .build();
    let liked = ctx.core.is_liked(&ev.id);
    like.set_child(Some(&crate::icons::icon(if liked { "like-filled" } else { "like" })));
    if liked {
        like.add_css_class("engaged");
    }
    acts.append(&like);
    {
        let ctx = ctx.clone();
        let ev = ev.clone();
        let like_recv = like.clone();
        like_recv.connect_clicked(move |_| {
            if ctx.core.is_liked(&ev.id) {
                return;
            }
            like.set_child(Some(&crate::icons::icon("like-filled")));
            like.add_css_class("engaged");
            let ctx2 = ctx.clone();
            let like2 = like.clone();
            drain(ctx2.core.react(&ev), move |reply| {
                if let PublishEvent::Failed { error, .. } = reply {
                    like2.set_child(Some(&crate::icons::icon("like")));
                    like2.remove_css_class("engaged");
                    eprintln!("[react] {error}");
                }
            });
        });
    }
    body.append(&acts);

    row.append(&body);
    row
}
