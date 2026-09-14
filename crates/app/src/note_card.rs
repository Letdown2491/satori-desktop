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

/// The shared card shell — port of note.ts cardShell(): avatar / author /
/// time-thread-link head, then a content slot each kind fills. `with_text`
/// adds the pre-wrap body + media runs + CW reveal (plain notes).
pub struct Shell {
    pub root: gtk::Box,
    /// Kind-specific content slot (empty for plain notes with `with_text`).
    pub content: gtk::Box,
}

pub fn shell(
    ctx: &Ctx,
    event: &Event,
    profile: Option<&Profile>,
    focused: bool,
    with_text: bool,
) -> Shell {
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

    // --- content slot ---------------------------------------------------------
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .build();
    root.append(&content);

    if !with_text {
        return Shell { root, content };
    }

    fn mention_resolver(ctx: &Ctx) -> impl Fn(&str) -> Option<String> + '_ {
    use nostr::prelude::*;
    move |b: &str| {
        Nip19::from_bech32(b).ok().and_then(|n| match n {
            Nip19::Pubkey(pk) => ctx.core.profile_of(&pk),
            Nip19::Profile(p) => ctx.core.profile_of(&p.public_key),
            _ => None,
        })
        .map(|p| p.name)
        .filter(|n| !n.is_empty())
    }
}

    let (markup, images) = render_content(event.content.as_str(), &mention_resolver(ctx));

    // Content warning: hidden behind a reveal (content.ts cwWrap; the blur
    // node lands in M4).
    let cw_reason = event.tags.iter().find_map(|t| {
        let parts = t.as_slice();
        (parts.first().map(String::as_str) == Some("content-warning"))
            .then(|| parts.get(1).cloned().unwrap_or_default())
    });
    let body = gtk::Label::builder()
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .valign(gtk::Align::Start)
        .build();
    body.set_markup(&markup);
    {
        let ctx_link = ctx.clone();
        body.connect_activate_link(move |_, uri| {
            handle_link(&ctx_link, uri);
            glib::Propagation::Stop
        });
    }
    match cw_reason {
        Some(reason) => {
            let reveal = gtk::Button::builder()
                .label(format!("Content warning: {} — show", esc_markup(&reason)).as_str())
                .css_classes(["flat", "cw-btn"])
                .halign(gtk::Align::Start)
                .build();
            let revealed = gtk::Revealer::builder()
                .transition_type(gtk::RevealerTransitionType::SlideDown)
                .child(&body)
                .build();
            reveal.connect_clicked({
                let revealed = revealed.clone();
                let reveal = reveal.clone();
                move |_| {
                    let showing = revealed.reveals_child();
                    revealed.set_reveal_child(!showing);
                    reveal.set_label(if showing { "Hide" } else { "Show" });
                }
            });
            content.append(&reveal);
            content.append(&revealed);
        }
        None => content.append(&body),
    }

    // Media runs (content.ts: images render inline, aspect-capped).
    if !images.is_empty() {
        let media = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();
        for url in images {
            let pic = gtk::Picture::builder()
                .content_fit(gtk::ContentFit::Cover)
                .height_request(240)
                .hexpand(true)
                .css_classes(["note-media"])
                .build();
            media.append(&pic);
            let core = ctx.core.clone();
            let pic = pic.clone();
            drain(core.fetch_image(url), move |img| {
                if let Some(decoded) = img {
                    let bytes = glib::Bytes::from_owned(decoded.rgba);
                    let tex = gdk::MemoryTexture::new(
                        decoded.width as i32,
                        decoded.height as i32,
                        gdk::MemoryFormat::R8g8b8a8,
                        &bytes,
                        decoded.width as usize * 4,
                    );
                    pic.set_paintable(Some(&tex));
                }
            });
        }
        content.append(&media);
    }

    Shell { root, content }
}

/// The action row — reply / like / bookmark / repeat / trash (own), plus
/// kind-specific widgets spliced in before the trash (note.ts NOTE_ACTIONS).
pub fn actions_row(
    ctx: &Ctx,
    event: &Event,
    card_root: &gtk::Box,
    extra: &[gtk::Widget],
) -> gtk::Box {
    let acts = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .margin_top(2)
        .build();
    let me = ctx.core.pubkey();
    let is_own = me.is_some_and(|m| m == event.pubkey);
    let card_root = card_root.clone();

    // Reply — opens the composer (the web app's #modal).
    let reply = crate::icons::action_button("reply", "Reply");
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
    like.set_child(Some(&crate::icons::icon(if liked { "like-filled" } else { "like" })));
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
            like.set_child(Some(&crate::icons::icon("like-filled")));
            like.add_css_class("engaged");
            let ctx2 = ctx_btn.clone();
            let like2 = like.clone();
            drain(ctx2.core.react(&event_btn), move |ev| if let satori_core::manager::PublishEvent::Failed { error, .. } = ev {
                like2.set_child(Some(&crate::icons::icon("like")));
                like2.remove_css_class("engaged");
                eprintln!("[react] {error}");
            });
        });
    }

    // Bookmark.
    let bookmark = crate::icons::action_button("bookmark", "Bookmark");
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

    for w in extra {
        acts.append(w);
    }

    // Delete — own events only (NIP-09).
    if is_own {
        let delete = crate::icons::action_button("trash", "Delete");
        {
            let ctx = ctx.clone();
            let event = event.clone();
            let _root = card_root.clone();
            delete.connect_clicked(move |_| {
                // TODO(M1): two-step inline confirm (routes/delete.ts).
                let ctx = ctx.clone();
                let root = card_root.clone();
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
    acts
}

/// A plain note card (kind 1): text body + media runs + default actions.
pub fn note_card(ctx: &Ctx, event: &Event, profile: Option<&Profile>, focused: bool) -> gtk::Box {
    let s = shell(ctx, event, profile, focused, true);
    s.root.append(&actions_row(ctx, event, &s.root, &[]));
    s.root
}

fn open_thread(ctx: &Ctx) -> Rc<dyn Fn(&Event)> {
    let ctx = ctx.clone();
    Rc::new(move |event| {
        let page = crate::thread::thread_page(&ctx, event);
        ctx.nav.push(&page);
    })
}

/// `nostr:` / `https:` dispatch (thread links are buttons, not hrefs).
fn handle_link(ctx: &Ctx, uri: &str) {
    use nostr::prelude::*;
    // The NIP-21 router (content.ts): profiles, threads, entities — all
    // in-app; https goes to the browser.
    if let Some(bech) = uri.strip_prefix("nostr:") {
        match Nip19::from_bech32(bech) {
            Ok(Nip19::Pubkey(pk)) => crate::profile::open_profile(ctx, pk),
            Ok(Nip19::Profile(p)) => crate::profile::open_profile(ctx, p.public_key),
            Ok(Nip19::EventId(id)) => {
                crate::thread::thread_page_by_id(ctx, id);
            }
            Ok(Nip19::Event(e)) => {
                crate::thread::thread_page_by_id(ctx, e.event_id);
            }
            Ok(Nip19::Coordinate(c)) => {
                let (kind, author, ident) = (c.kind, c.public_key, c.identifier.clone());
                crate::cards::entity_page_by_naddr(ctx, u16::from(kind), author, ident);
            }
            _ => {}
        }
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
