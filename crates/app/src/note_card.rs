//! The note card — port of `src/render/note.ts` anatomy (slice 1):
//! avatar / author / time-as-thread-link, pre-wrap body with links, quiet
//! action glyphs (no counts). Media cards, reply-faces, CW and clamps land
//! with the manifest registry (M4).

use std::rc::Rc;

use adw::prelude::*;
use gtk::gdk;
use gtk::glib;
use nostr::prelude::*;
use satori_core::manager::Profile;

use crate::bridge::drain;
use crate::content::{render_content, time_ago};
use crate::ctx::Ctx;

pub fn note_card(ctx: &Ctx, event: &Event, profile: Option<&Profile>, focused: bool) -> gtk::Box {
    let me = ctx.core.pubkey();
    let is_own = me.is_some_and(|m| m == event.pubkey);

    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .css_classes(if focused {
            vec!["note-card", "focused"]
        } else {
            vec!["note-card"]
        })
        .build();

    // --- head row -------------------------------------------------------------
    let head = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();

    let avatar = avatar_widget(ctx, event.pubkey, profile);
    head.append(&avatar);

    let name = profile
        .map(|p| p.display())
        .unwrap_or_else(|| short_key(&event.pubkey));
    let author = gtk::Label::builder()
        .label(esc_markup(&name))
        .use_markup(true)
        .halign(gtk::Align::Start)
        .css_classes(["note-author", "serif"])
        .build();
    head.append(&author);

    let spacer = gtk::Label::new(None);
    spacer.set_hexpand(true);
    head.append(&spacer);

    // Time doubles as the "open thread" affordance (note.ts time-thread).
    let time = gtk::Button::builder()
        .label(time_ago(event.created_at.as_secs()))
        .css_classes(["flat", "time-link"])
        .build();
    {
        let open = open_thread(ctx);
        let event = event.clone();
        time.connect_clicked(move |_| open(&event));
    }
    head.append(&time);
    root.append(&head);

    // --- body -----------------------------------------------------------------
    let body = gtk::Label::builder()
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .valign(gtk::Align::Start)
        .build();
    body.set_markup(&render_content(event.content.as_str()));
    body.connect_activate_link(|_, uri| {
        handle_link(uri);
        glib::Propagation::Stop
    });
    root.append(&body);

    // --- actions --------------------------------------------------------------
    let acts = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .margin_top(2)
        .build();

    // Reply — opens the composer (the web app's #modal).
    let reply = gtk::Button::builder()
        .icon_name("mail-reply-sender-symbolic")
        .css_classes(["flat", "act"])
        .tooltip_text("Reply")
        .build();
    {
        let ctx = ctx.clone();
        let event = event.clone();
        reply.connect_clicked(move |_| {
            crate::composer::compose(&ctx, Some(event.clone()), None);
        });
    }
    acts.append(&reply);

    // Like — optimistic glyph, revert on failure (the h-optimistic dance, as state).
    let like = gtk::Button::builder()
        .css_classes(["flat", "act"])
        .tooltip_text("React")
        .build();
    let liked = ctx.core.is_liked(&event.id);
    like.set_icon_name(if liked { "starred-symbolic" } else { "non-starred-symbolic" });
    if liked {
        like.add_css_class("engaged");
    }
    acts.append(&like);
    {
        let like_btn = like.clone();
        let ctx_btn = ctx.clone();
        let event_btn = event.clone();
        like_btn.connect_clicked(move |_| {
            if ctx_btn.core.is_liked(&event_btn.id) {
                return; // unlike lands with the private-list work (M3)
            }
            like.set_icon_name("starred-symbolic");
            like.add_css_class("engaged");
            let ctx2 = ctx_btn.clone();
            let like2 = like.clone();
            drain(ctx2.core.react(&event_btn), move |ev| if let satori_core::manager::PublishEvent::Failed { error, .. } = ev {
                like2.set_icon_name("non-starred-symbolic");
                like2.remove_css_class("engaged");
                eprintln!("[react] {error}");
            });
        });
    }

    // Bookmark.
    let bookmark = gtk::Button::builder()
        .icon_name("bookmark-new-symbolic")
        .css_classes(["flat", "act"])
        .tooltip_text("Bookmark")
        .build();
    if ctx.core.is_bookmarked(&event.id) {
        bookmark.add_css_class("engaged");
    }
    acts.append(&bookmark);
    {
        let bookmark_btn = bookmark.clone();
        let ctx_btn = ctx.clone();
        let event_btn = event.clone();
        bookmark_btn.connect_clicked(move |_| {
            if ctx_btn.core.is_bookmarked(&event_btn.id) {
                return;
            }
            bookmark.add_css_class("engaged");
            let ctx2 = ctx_btn.clone();
            let bookmark2 = bookmark.clone();
            drain(ctx2.core.bookmark(&event_btn), move |ev| if let satori_core::manager::PublishEvent::Failed { error, .. } = ev {
                bookmark2.remove_css_class("engaged");
                eprintln!("[bookmark] {error}");
            });
        });
    }

    // Delete — own events only (NIP-09).
    if is_own {
        let delete = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .css_classes(["flat", "act"])
            .tooltip_text("Delete")
            .build();
        {
            let ctx = ctx.clone();
            let event = event.clone();
            let root = root.clone();
            delete.connect_clicked(move |_| {
                // TODO(M1): two-step inline confirm (routes/delete.ts).
                let ctx = ctx.clone();
                let root = root.clone();
                drain(ctx.core.delete(&event), move |ev| match ev {
                    satori_core::manager::PublishEvent::Committed { .. } => {
                        root.set_visible(false);
                    }
                    satori_core::manager::PublishEvent::Failed { error, .. } => {
                        ctx.toast("Delete failed");
                        eprintln!("[delete] {error}");
                    }
                    _ => {}
                });
            });
        }
        acts.append(&delete);
    }

    root.append(&acts);

    // Zap placeholder stays out until the wallet milestone (D6): a dead
    // button is a lie.
    root
}

fn open_thread(ctx: &Ctx) -> Rc<dyn Fn(&Event)> {
    let ctx = ctx.clone();
    Rc::new(move |event| {
        let page = crate::thread::thread_page(&ctx, event);
        ctx.nav.push(&page);
    })
}

/// `nostr:` / `https:` dispatch (thread links are buttons, not hrefs).
fn handle_link(uri: &str) {
    // npub links: profile view lands in M2 — inert for now.
    if uri.starts_with("nostr:") {
        return;
    }
    gtk::UriLauncher::new(uri).launch(
        None::<&adw::ApplicationWindow>,
        gtk::gio::Cancellable::NONE,
        |_| {},
    );
}

fn esc_markup(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn short_key(pk: &PublicKey) -> String {
    let hex = pk.to_string();
    format!("npub1…{}", &hex[hex.len().saturating_sub(6)..])
}

/// Avatar slot: initials fallback (hue-bucket color lands in M2), picture
/// swaps in when fetched through the guarded media pipeline.
pub fn avatar_widget(ctx: &Ctx, pubkey: PublicKey, profile: Option<&Profile>) -> gtk::Stack {
    let _ = pubkey;
    let stack = gtk::Stack::builder().width_request(34).height_request(34).build();
    let initials = adw::Avatar::builder()
        .size(34)
        .show_initials(true)
        .text(profile.map(|p| p.name.clone()).unwrap_or_else(|| "?".into()))
        .build();
    stack.add_child(&initials);

    if let Some(p) = profile
        && !p.picture.is_empty() {
            let core = ctx.core.clone();
            let stack = stack.clone();
            drain(core.fetch_image(p.picture.clone()), move |img| {
                if let Some(decoded) = img {
                    let bytes = glib::Bytes::from_owned(decoded.rgba);
                    let tex = gdk::MemoryTexture::new(
                        decoded.width as i32,
                        decoded.height as i32,
                        gdk::MemoryFormat::R8g8b8a8,
                        &bytes,
                        decoded.width as usize * 4,
                    );
                    let pic = gtk::Picture::builder()
                        .content_fit(gtk::ContentFit::Cover)
                        .width_request(34)
                        .height_request(34)
                        .build();
                    pic.set_paintable(Some(&tex));
                    pic.add_css_class("avatar-img");
                    stack.add_child(&pic);
                    stack.set_visible_child(&pic);
                }
            });
        }
    stack
}
