//! The note card — port of `src/render/note.ts` anatomy (slice 1):
//! avatar / author / time-as-thread-link, pre-wrap body with links, quiet
//! action glyphs (no counts). Media cards, reply-faces, CW and clamps land
//! with the manifest registry (M4).

use std::cell::RefCell;
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
    pub name_label: gtk::Label,
    pub avatar_stack: gtk::Stack,
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

    // --- head row (styles.css .note: gap 13px) ---------------------------------
    let head = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(13)
        .build();

    let avatar = avatar_widget(ctx, event.pubkey, profile);
    head.append(&avatar);

    let name = profile
        .map(|p| p.display())
        .unwrap_or_else(|| short_key(&event.pubkey));
    // The name is a profile link (content.ts: @mentions → /u/).
    let author = gtk::Label::builder()
        .use_markup(true)
        .halign(gtk::Align::Start)
        .css_classes(["note-author", "serif"])
        .build();
    author.set_markup(&name_markup(&name, &event.pubkey));
    {
        let ctx2 = ctx.clone();
        let who = event.pubkey;
        author.connect_activate_link(move |_, _| {
            crate::profile::open_profile(&ctx2, who);
            glib::Propagation::Stop
        });
    }
    head.append(&author);

    let spacer = gtk::Label::new(None);
    spacer.set_hexpand(true);
    head.append(&spacer);

    // Time doubles as the "open thread" affordance (note.ts time-thread).
    let time = gtk::Button::builder()
        .label(time_ago(event.created_at.as_secs()))
        .css_classes(["flat", "time-link"])
        .build();
    register_time_label(&time, event.created_at.as_secs());
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

    // Reply-faces: a quiet button holding 3 replier circles (note.ts
    // reply-faces — people, not counts); click opens the thread.
    let faces_btn = gtk::Button::builder()
        .css_classes(["flat", "feed-faces"])
        .visible(false)
        .build();
    let faces_inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .build();
    faces_btn.set_child(Some(&faces_inner));
    {
        let ctx2 = ctx.clone();
        let ev_id = event.id;
        faces_btn.connect_clicked(move |_| {
            crate::thread::thread_page_by_id(&ctx2, ev_id);
        });
    }
    root.append(&faces_btn);

    if !with_text {
        bind_profile(ctx, &Shell { root: root.clone(), content: content.clone(), name_label: author.clone(), avatar_stack: avatar.clone() }, event.pubkey, profile);
        return Shell { root, content, name_label: author, avatar_stack: avatar };
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

    // Reply chip: "↩ Replying to <name>" opens the parent's thread
    // (note.ts replyContext). The name resolves from the p tag via the
    // profile cache when available.
    if let Some(parent_id) = event.tags.clone().event_ids().next() {
        let parent_name = event
            .tags
            .clone()
            .public_keys()
            .next()
            .and_then(|pk| ctx.core.profile_of(&pk))
            .map(|p| p.display());
        let label = match &parent_name {
            Some(n) => format!("\u{21a9} Replying to {}", esc_markup(n)),
            None => "\u{21a9} Replying to an earlier note".to_string(),
        };
        let chip = gtk::Button::builder()
            .css_classes(["flat", "reply-chip"])
            .halign(gtk::Align::Start)
            .build();
        let chip_label = gtk::Label::builder().build();
        chip_label.set_markup(&label);
        chip.set_child(Some(&chip_label));
        {
            let ctx2 = ctx.clone();
            chip.connect_clicked(move |_| {
                crate::thread::thread_page_by_id(&ctx2, parent_id);
            });
        }
        content.append(&chip);
    }

    let rendered = render_content(event.content.as_str(), &mention_resolver(ctx));
    let (markup, images) = (rendered.markup, rendered.images);

    // Rich link previews (YouTube/Spotify) + play facades for direct
    // audio/video links (content.ts / youtube.ts / blossom).
    for url in rendered.previews.iter().take(2) {
        content.append(&crate::cards::link_preview_card(ctx, url));
    }
    for url in rendered.facades.iter().take(3) {
        content.append(&crate::cards::av_facade(ctx, url));
    }

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

    // Mention prefetch: mentioned npubs aren't page authors — resolve them
    // reactively and re-render the body so names replace raw bech32.
    {
        let missing: Vec<PublicKey> = mention_pubkeys(event)
            .into_iter()
            .filter(|pk| ctx.core.profile_of(pk).is_none())
            .collect();
        if !missing.is_empty() {
            let ctx2 = ctx.clone();
            let body2 = body.clone();
            let ev2 = event.clone();
            drain(ctx2.core.ensure_profiles(missing), move |reply| {
                if let satori_core::manager::FeedEvent::Profiles(_) = reply {
                    body2.set_markup(
                        &render_content(ev2.content.as_str(), &mention_resolver(&ctx2)).markup,
                    );
                }
            });
        }
    }

    // Quote embeds: lazy compact previews of referenced notes (content.ts
    // embedCard — no actions, click opens the thread).
    for bech in rendered.refs.iter().take(3) {
        let slot = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["embed-slot"])
            .build();
        let waiting = gtk::Label::builder()
            .label("Loading quote…")
            .css_classes(["tagline"])
            .halign(gtk::Align::Start)
            .build();
        slot.append(&waiting);
        content.append(&slot);
        let ctx2 = ctx.clone();
        let bech2 = bech.clone();
        drain(ctx2.core.event_by_id_dech32(&bech2), move |reply| {
            use satori_core::manager::FeedEvent;
            match reply {
                FeedEvent::One(Some(target), prof) => {
                    waiting.set_visible(false);
                    slot.append(&embed_card(&ctx2, &target, prof.as_ref()));
                }
                _ => waiting.set_text("Quote unavailable."),
            }
        });
    }

    bind_profile(ctx, &Shell { root: root.clone(), content: content.clone(), name_label: author.clone(), avatar_stack: avatar.clone() }, event.pubkey, profile);
    Shell { root, content, name_label: author, avatar_stack: avatar }
}

/// Compact embed card for quotes/references — border-left, name + time +
/// clamped text, thumbnail when media, click → thread (embedCard).
pub fn embed_card(ctx: &Ctx, target: &Event, profile: Option<&Profile>) -> gtk::Widget {
    let card = gtk::Button::builder()
        .css_classes(["embed-card"])
        .build();
    let inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(3)
        .build();

    let head = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .build();
    let name = profile
        .map(|p| p.display())
        .unwrap_or_else(|| short_key(&target.pubkey));
    let name_label = gtk::Label::builder()
        .css_classes(["note-author"])
        .halign(gtk::Align::Start)
        .build();
    name_label.set_markup(&format!(
        "<a href=\"profile:{}\">{}</a>",
        target.pubkey,
        esc_markup(&name)
    ));
    {
        let ctx2 = ctx.clone();
        let who = target.pubkey;
        name_label.connect_activate_link(move |_, _| {
            crate::profile::open_profile(&ctx2, who);
            glib::Propagation::Stop
        });
    }
    head.append(&name_label);
    head.append(
        &gtk::Label::builder()
            .label(time_ago(target.created_at.as_secs()))
            .css_classes(["tagline"])
            .build(),
    );
    inner.append(&head);

    let text = gtk::Label::builder()
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .lines(3)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .xalign(0.0)
        .halign(gtk::Align::Start)
        .build();
    text.set_markup(&render_content(target.content.as_str(), &mention_resolver(ctx)).markup);
    inner.append(&text);

    // Image thumb when the quoted note carries media.
    let thumbs: Vec<String> = satori_core::nip::imeta::parse(target)
        .into_iter()
        .map(|m| m.url)
        .collect();
    if thumbs.is_empty() {
        card.set_child(Some(&inner));
    } else {
        let holder = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .build();
        holder.append(&inner);
        holder.append(&crate::cards::thumb_picture(ctx, &thumbs[0]));
        card.set_child(Some(&holder));
    }

    {
        let ctx2 = ctx.clone();
        let target = target.clone();
        card.connect_clicked(move |_| {
            if target.kind == Kind::TextNote {
                crate::thread::thread_page_by_id(&ctx2, target.id);
            }
        });
    }
    card.upcast()
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

/// Reactive profile patching for a shell built with `profile: None`: when
/// the profile arrives, the name label + avatar update in place.
pub fn bind_profile(ctx: &Ctx, shell: &Shell, pk: PublicKey, profile: Option<&Profile>) {
    if profile.is_some() {
        return;
    }
    let ctx2 = ctx.clone();
    let nl = shell.name_label.clone();
    let st = shell.avatar_stack.clone();
    drain(ctx.core.ensure_profiles(vec![pk]), move |reply| {
        if let satori_core::manager::FeedEvent::Profiles(ps) = reply
            && let Some(p) = ps.into_iter().find(|p| p.pubkey == pk.to_string()) {
                let shown = if p.name.is_empty() {
                    short_key(&pk)
                } else {
                    p.name.clone()
                };
                nl.set_markup(&name_markup(&shown, &pk));
                if !p.picture.is_empty() {
                    if let Some(tex) = crate::media_cache::mem_get(&p.picture) {
                        let pic = avatar_picture(&tex);
                        st.add_child(&pic);
                        st.set_visible_child(&pic);
                    } else {
                        let core = ctx2.core.clone();
                        let st2 = st.clone();
                        let url = p.picture.clone();
                        drain(core.fetch_image(url.clone()), move |img| {
                            if let Some(decoded) = img {
                                let tex = crate::media_cache::from_decoded(&url, &decoded);
                                let pic = avatar_picture(&tex);
                                st2.add_child(&pic);
                                st2.set_visible_child(&pic);
                            }
                        });
                    }
                }
            }
    });
}

/// The card's faces slot (the inner Box of the faces button), for
/// feed-level hydration.
pub fn faces_slot(card: &gtk::Box) -> Option<gtk::Box> {
    let mut child = card.first_child()?;
    loop {
        if child.has_css_class("feed-faces") {
            let btn: gtk::Button = child.downcast().ok()?;
            return btn.child()?.downcast().ok();
        }
        child = child.next_sibling()?;
    }
}

/// Fill a faces slot: 3 overlapping hue circles + "+" (people, not counts).
pub fn fill_faces(slot: &gtk::Box, who: &[PublicKey]) {
    while let Some(c) = slot.first_child() {
        slot.remove(&c);
    }
    if who.is_empty() {
        slot.set_visible(false);
        return;
    }
    for pk in who.iter().take(3) {
        let disc = gtk::Box::builder()
            .width_request(20)
            .height_request(20)
            .css_classes(["avatar-circle", "face-disc", hue_class(pk).as_str()])
            .build();
        let initial = pk
            .to_string()
            .chars()
            .next()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".into());
        disc.append(&gtk::Label::builder()
            .label(&initial)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .css_classes(["face-initial"])
            .build());
        slot.append(&disc);
    }
    slot.append(&gtk::Label::builder()
        .label("+")
        .css_classes(["tagline", "face-plus"])
        .build());
    slot.set_visible(true);
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

fn name_markup(name: &str, pk: &PublicKey) -> String {
    format!("<a href=\"profile:{}\">{}</a>", pk, esc_markup(name))
}

fn esc_markup(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn short_key(pk: &PublicKey) -> String {
    let hex = pk.to_string();
    format!("npub1…{}", &hex[hex.len().saturating_sub(6)..])
}

/// Avatar slot: hue-bucket initial circle (render/util.ts:74), picture
/// swaps in when fetched through the guarded media pipeline.
pub fn avatar_widget(ctx: &Ctx, pubkey: PublicKey, profile: Option<&Profile>) -> gtk::Stack {
    let stack = gtk::Stack::builder().width_request(34).height_request(34).build();
    stack.add_child(&initial_circle(&pubkey, profile.map(|p| p.name.as_str()).filter(|n| !n.is_empty())));

    if let Some(p) = profile
        && !p.picture.is_empty()
    {
        if let Some(tex) = crate::media_cache::mem_get(&p.picture) {
            let pic = avatar_picture(&tex);
            stack.add_child(&pic);
            stack.set_visible_child(&pic);
        } else {
            let core = ctx.core.clone();
            let stack = stack.clone();
            let url = p.picture.clone();
            drain(core.fetch_image(url.clone()), move |img| {
                if let Some(decoded) = img {
                    let tex = crate::media_cache::from_decoded(&url, &decoded);
                    let pic = avatar_picture(&tex);
                    stack.add_child(&pic);
                    stack.set_visible_child(&pic);
                }
            });
        }
    }
    stack
}

/// Live time labels: every card's time button registers here; a 60s poll
/// reticks the visible ones (labels never go stale).
pub fn register_time_label(btn: &gtk::Button, secs: u64) {
    TIME_LABELS.with(|reg| {
        reg.borrow_mut().push((btn.downgrade(), secs));
    });
}

pub fn tick_time_labels() {
    TIME_LABELS.with(|reg| {
        reg.borrow_mut().retain(|(weak, secs)| match weak.upgrade() {
            Some(btn) => {
                btn.set_label(&time_ago(*secs));
                true
            }
            None => false,
        });
    });
}

thread_local! {
    static TIME_LABELS: RefCell<Vec<(glib::WeakRef<gtk::Button>, u64)>> =
        const { RefCell::new(Vec::new()) };
}

/// The hue-bucket CSS class for a pubkey (render/util.ts:74 — 36 buckets).
/// The pubkeys mentioned in a note's content (npub/nprofile bech32s).
pub fn mention_pubkeys(ev: &Event) -> Vec<PublicKey> {
    use nostr::prelude::*;
    let mut rest: &str = ev.content.as_str();
    let mut out = Vec::new();
    loop {
        let at = rest.find("@npub1");
        let nostr = rest.find("nostr:npub1").or(rest.find("nostr:nprofile1"));
        let pos = [at, nostr].into_iter().flatten().min();
        match pos {
            None => return out,
            Some(i) => {
                rest = &rest[i..];
                let start = if rest.starts_with('@') { 1 } else { "nostr:".len() };
                let end = rest[start..]
                    .find(|c: char| !c.is_ascii_alphanumeric())
                    .map(|i| i + start)
                    .unwrap_or(rest.len());
                let bech = &rest[start..end];
                if let Ok(Nip19::Pubkey(pk)) = Nip19::from_bech32(bech) {
                    if !out.contains(&pk) {
                        out.push(pk);
                    }
                } else if let Ok(Nip19::Profile(p)) = Nip19::from_bech32(bech)
                    && !out.contains(&p.public_key) {
                        out.push(p.public_key);
                    }
                rest = &rest[end..];
            }
        }
    }
}

/// Display-name resolution for inline mentions (profile cache).
pub fn mention_resolver(ctx: &Ctx) -> impl Fn(&str) -> Option<String> + '_ {
    use nostr::prelude::*;
    move |b: &str| {
        Nip19::from_bech32(b)
            .ok()
            .and_then(|n| match n {
                Nip19::Pubkey(pk) => ctx.core.profile_of(&pk),
                Nip19::Profile(p) => ctx.core.profile_of(&p.public_key),
                _ => None,
            })
            .map(|p| p.name)
            .filter(|n| !n.is_empty())
    }
}

pub fn hue_class(pk: &PublicKey) -> String {
    let hex_str = pk.to_string();
    let hue: u32 = u32::from_str_radix(&hex_str[..4], 16).unwrap_or(0) % 360;
    format!("hue-{}", hue / 10)
}

/// A colored disc with the first letter (initial fallback).
fn initial_circle(pk: &PublicKey, name: Option<&str>) -> gtk::Widget {
    let circle = gtk::Box::builder()
        .width_request(34)
        .height_request(34)
        .valign(gtk::Align::Start)
        .css_classes(["avatar-circle", hue_class(pk).as_str()])
        .build();
    let letter = name
        .and_then(|n| n.chars().next())
        .unwrap_or('?')
        .to_string();
    circle.append(
        &gtk::Label::builder()
            .label(&letter)
            .css_classes(["avatar-initial", "serif"])
            .build(),
    );
    circle.upcast()
}

fn avatar_picture(tex: &gdk::MemoryTexture) -> gtk::Picture {
    let pic = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Cover)
        .width_request(34)
        .height_request(34)
        .build();
    pic.set_paintable(Some(tex));
    pic.add_css_class("avatar-img");
    pic
}
