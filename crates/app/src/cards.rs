//! Kind cards + registry — port of `src/manifest/` (satori.ts registrations,
//! the hand-coded handlers). `card_for` is the renderEvent dispatch; the
//! fallback is the honest plain-note (NIP-31 alt surfaces in M4 proper).

use std::collections::HashMap;

use adw::prelude::*;
use gtk::gdk;
use gtk::glib;
use gtk::pango::WrapMode;
use nostr::prelude::*;
use satori_core::manager::Profile;

use crate::bridge::drain;
use crate::content::{esc, render_content, time_ago};
use crate::ctx::Ctx;
use crate::note_card::{actions_row, note_card, shell};

/// renderEvent: kind → handler (registry.ts:92).
pub fn card_for(ctx: &Ctx, ev: &Event, profile: Option<&Profile>, focused: bool) -> gtk::Box {
    match ev.kind {
        Kind::Custom(20) => picture_card(ctx, ev, profile, focused),
        Kind::Custom(21) | Kind::Custom(22) => video_card(ctx, ev, profile, focused),
        Kind::Custom(1068) => poll_card(ctx, ev, profile, focused),
        Kind::Custom(31922) | Kind::Custom(31923) => calendar_card(ctx, ev, profile, focused),
        Kind::Custom(30023) => article_card(ctx, ev, profile, focused),
        _ => note_card(ctx, ev, profile, focused),
    }
}

// --- picture (NIP-68) ---------------------------------------------------------

fn picture_card(ctx: &Ctx, ev: &Event, profile: Option<&Profile>, focused: bool) -> gtk::Box {
    let s = shell(ctx, ev, profile, focused, false);
    let metas = satori_core::nip::imeta::parse(ev);

    // Caption: the content, minus media URLs already in imeta (picture.ts
    // dedupes caption-embedded URLs).
    if !ev.content.is_empty() {
        let body = gtk::Label::builder()
            .wrap(true)
            .wrap_mode(WrapMode::WordChar)
            .xalign(0.0)
            .valign(gtk::Align::Start)
            .build();
        body.set_markup(&render_content(ev.content.as_str()).0);
        s.content.append(&body);
    }

    for meta in &metas {
        s.content.append(&media_picture(ctx, meta, 680));
    }
    if metas.is_empty() {
        // No imeta: honest fallback to the text pipeline (which extracts URLs).
        let (markup, images) = render_content(ev.content.as_str());
        let body = gtk::Label::builder()
            .wrap(true)
            .wrap_mode(WrapMode::WordChar)
            .xalign(0.0)
            .build();
        body.set_markup(&markup);
        s.content.append(&body);
        for url in images {
            let meta = satori_core::nip::imeta::ImageMeta {
                url,
                dim: None,
                alt: None,
                mime: None,
            };
            s.content.append(&media_picture(ctx, &meta, 680));
        }
    }

    // Location dateline (picture.ts).
    if let Some(loc) = tag_value(ev, "location") {
        s.content.append(&meta_line("map-pin", &loc));
    }

    s.root.append(&actions_row(ctx, ev, &s.root, &[]));
    s.root
}

// --- video (NIP-71) -------------------------------------------------------------

fn video_card(ctx: &Ctx, ev: &Event, profile: Option<&Profile>, focused: bool) -> gtk::Box {
    let s = shell(ctx, ev, profile, focused, false);
    let metas = satori_core::nip::imeta::parse(ev);
    let short = ev.kind == Kind::Custom(22);

    // First video imeta = primary player; first image imeta = poster.
    let video = metas
        .iter()
        .find(|m| m.mime.as_deref().map(|m| m.starts_with("video")).unwrap_or(false));
    let poster = metas
        .iter()
        .find(|m| m.mime.as_deref().map(|m| m.starts_with("image")).unwrap_or(false));

    // Privacy facade: no network until play (content.ts video facade,
    // preload=none). Poster + play glyph; click swaps to a GtkVideo.
    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .build();
    let facade = gtk::Overlay::builder()
        .width_request(if short { 240 } else { 640 })
        .height_request(if short { 460 } else { 360 })
        .css_classes(["note-media", "video-facade"])
        .build();
    if let Some(p) = poster {
        // Poster behind the play glyph.
        facade.set_child(Some(&media_picture(ctx, p, if short { 460 } else { 360 })));
    }
    let facade_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .spacing(6)
        .build();
    facade_box.append(&crate::icons::icon_sized("play", 34));
    facade.add_overlay(&facade_box);
    stack.add_named(&facade, Some("facade"));
    if let Some(v) = video {
        let url = v.url.clone();
        let stack = stack.clone();
        facade.set_cursor(gtk::gdk::Cursor::from_name("pointer", None).as_ref());
        let click = gtk::GestureClick::new();
        click.set_button(1);
        {
            let stack = stack.clone();
            let url = url.clone();
            click.connect_released(move |g, _n, _x, _y| {
                let _ = g;
                let video = gtk::Video::builder()
                    .autoplay(true)
                    .loop_(false)
                    .hexpand(true)
                    .build();
                let file = gtk::gio::File::for_uri(&url);
                let media = gtk::MediaFile::for_file(&file);
                video.set_property("media-file", &media);
                stack.add_named(&video, Some("player"));
                stack.set_visible_child_name("player");
            });
        }
        facade.add_controller(click);
    }
    stack.set_visible_child_name("facade");
    s.content.append(&stack);

    // Description below (video.ts clamps it — clamp lands in M4 polish).
    if !ev.content.is_empty() {
        let body = gtk::Label::builder()
            .wrap(true)
            .wrap_mode(WrapMode::WordChar)
            .xalign(0.0)
            .build();
        body.set_markup(&render_content(ev.content.as_str()).0);
        s.content.append(&body);
    }

    s.root.append(&actions_row(ctx, ev, &s.root, &[]));
    s.root
}

// --- poll (NIP-88) ---------------------------------------------------------------

fn poll_card(ctx: &Ctx, ev: &Event, profile: Option<&Profile>, focused: bool) -> gtk::Box {
    let s = shell(ctx, ev, profile, focused, true);
    let Some(data) = satori_core::nip::poll::parse(ev) else {
        return note_card(ctx, ev, profile, focused);
    };

    // Ballot: radios (single) or checks (multiple) + vote button.
    let ballot = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_top(4)
        .build();
    let group = gtk::CheckButton::new();
    let mut checks: Vec<(String, gtk::CheckButton)> = Vec::new();
    for (id, label) in &data.options {
        let cb = if data.multiple {
            gtk::CheckButton::builder().label(label).build()
        } else {
            gtk::CheckButton::builder()
                .label(label)
                .group(&group)
                .build()
        };
        checks.push((id.clone(), cb.clone()));
        ballot.append(&cb);
    }
    s.content.append(&ballot);

    let vote_btn = gtk::Button::builder()
        .label("Vote")
        .css_classes(["seal-btn", "pill"])
        .halign(gtk::Align::Start)
        .build();
    s.content.append(&vote_btn);

    // Results rows, revealed after voting (poll.ts results view).
    let results = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .visible(false)
        .build();
    s.content.append(&results);

    {
        let ctx = ctx.clone();
        let ev = ev.clone();
        let checks = checks.clone();
        let results = results.clone();
        let ballot = ballot.clone();
        let vote_recv = vote_btn.clone();
        vote_recv.connect_clicked(move |_| {
            let chosen: Vec<String> = checks
                .iter()
                .filter(|(_, cb)| cb.is_active())
                .map(|(id, _)| id.clone())
                .collect();
            if chosen.is_empty() {
                return;
            }
            // Optimistic: ballot goes, results arrive (poll.ts .voting).
            ballot.set_sensitive(false);
            vote_btn.set_sensitive(false);
            let ctx2 = ctx.clone();
            let ev2 = ev.clone();
            let results = results.clone();
            let vote_btn2 = vote_btn.clone();
            drain(ctx2.core.poll_vote(&ev2, chosen), move |reply| {
                if matches!(reply, satori_core::manager::PublishEvent::Failed { .. }) {
                    ctx2.toast("Vote failed");
                    vote_btn2.set_sensitive(true);
                    return;
                }
                show_tallies(&ctx2, &ev2, &results);
            });
        });
    }

    s.root.append(&actions_row(ctx, ev, &s.root, &[]));
    s.root
}

fn show_tallies(ctx: &Ctx, ev: &Event, results: &gtk::Box) {
    // Clear + render bars from the tally (poll.ts poll-bar-fill).
    while let Some(c) = results.first_child() {
        results.remove(&c);
    }
    let data = satori_core::nip::poll::parse(ev);
    let ctx3 = ctx.clone();
    let results = results.clone();
    drain(ctx3.core.poll_votes(ev.id), move |reply| {
        if let satori_core::manager::FeedEvent::PollVotes(tally) = reply {
            let counts: HashMap<String, u32> = tally.iter().cloned().collect();
            let total: u32 = counts.values().sum();
            let _ = &ctx3;
            if let Some(data) = &data {
                for (id, label) in &data.options {
                    let n = counts.get(id).copied().unwrap_or(0);
                    let frac = if total > 0 { n as f64 / total as f64 } else { 0.0 };
                    let row = gtk::Box::builder()
                        .orientation(gtk::Orientation::Vertical)
                        .spacing(2)
                        .build();
                    let text = gtk::Label::builder()
                        .halign(gtk::Align::Start)
                        .build();
                    text.set_markup(&format!(
                        "{} <span foreground=\"#8b8275\">· {}</span>",
                        esc(label),
                        n
                    ));
                    let bar = gtk::ProgressBar::builder().fraction(frac).build();
                    row.append(&text);
                    row.append(&bar);
                    results.append(&row);
                }
            }
            results.set_visible(true);
        }
    });
}

// --- calendar (NIP-52) -------------------------------------------------------------

fn calendar_card(ctx: &Ctx, ev: &Event, profile: Option<&Profile>, focused: bool) -> gtk::Box {
    let s = shell(ctx, ev, profile, focused, false);
    let Some(data) = satori_core::nip::calendar::parse(ev) else {
        return note_card(ctx, ev, profile, focused);
    };

    let title = gtk::Label::builder()
        .css_classes(["serif", "note-author"])
        .halign(gtk::Align::Start)
        .build();
    title.set_markup(&format!("<big>{}</big>", esc(&data.title)));
    s.content.append(&title);

    s.content.append(&meta_line("calendar", &format_when(&data)));
    if let Some(loc) = &data.location {
        s.content.append(&meta_line("map-pin", loc));
    }
    if !data.description.is_empty() {
        let body = gtk::Label::builder()
            .wrap(true)
            .wrap_mode(WrapMode::WordChar)
            .xalign(0.0)
            .build();
        body.set_markup(&render_content(data.description.as_str()).0);
        s.content.append(&body);
    }

    // RSVP pills (calendar.ts): Going / Maybe / Can't-go, self-reporting.
    let rsvp = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_top(4)
        .build();
    for (status, label) in [
        ("accepted", "Going"),
        ("tentative", "Maybe"),
        ("declined", "Can't go"),
    ] {
        let btn = gtk::Button::builder()
            .label(label)
            .css_classes(["flat", "pill", "rsvp"])
            .build();
        rsvp.append(&btn);
        {
            let ctx = ctx.clone();
            let ev = ev.clone();
            let btn_recv = btn.clone();
            let rsvp_all = rsvp.clone();
            btn_recv.connect_clicked(move |_| {
                let status = status;
                let ctx2 = ctx.clone();
                let btn2 = btn.clone();
                let rsvp2 = rsvp_all.clone();
                drain(ctx2.core.rsvp(&ev, status), move |reply| match reply {
                    satori_core::manager::PublishEvent::Failed { error, .. } => {
                        eprintln!("[rsvp] {error}");
                    }
                    _ => {
                        for b in [rsvp2.first_child()].into_iter().flatten() {
                            b.remove_css_class("engaged");
                        }
                        btn2.add_css_class("engaged");
                    }
                });
            });
        }
    }
    s.content.append(&rsvp);

    s.root.append(&actions_row(ctx, ev, &s.root, &[]));
    s.root
}

fn format_when(data: &satori_core::nip::calendar::CalendarData) -> String {
    match chrono::DateTime::from_timestamp(data.start as i64, 0) {
        Some(dt) => {
            let fmt = dt.format("%a %b %e %Y %H:%M");
            match data.end.and_then(|e| chrono::DateTime::from_timestamp(e as i64, 0)) {
                Some(end) => format!("{} – {}", fmt, end.format("%H:%M")),
                None => fmt.to_string(),
            }
        }
        None => "unreadable time".into(),
    }
}

// --- article (NIP-23) ---------------------------------------------------------------

fn article_card(ctx: &Ctx, ev: &Event, profile: Option<&Profile>, focused: bool) -> gtk::Box {
    let Some(data) = satori_core::nip::article::parse(ev) else {
        return note_card(ctx, ev, profile, focused);
    };

    let s = shell(ctx, ev, profile, focused, false);

    // Cover (or the ensō seal on coverless articles — article.ts).
    if let Some(img) = &data.image {
        let meta = satori_core::nip::imeta::ImageMeta {
            url: img.clone(),
            dim: None,
            alt: None,
            mime: None,
        };
        s.content.append(&media_picture(ctx, &meta, 300));
    }

    let kicker = gtk::Label::builder()
        .label("↗ Article")
        .css_classes(["tagline"])
        .halign(gtk::Align::Start)
        .build();
    s.content.append(&kicker);

    let title = gtk::Label::builder()
        .css_classes(["serif"])
        .wrap(true)
        .wrap_mode(WrapMode::WordChar)
        .xalign(0.0)
        .halign(gtk::Align::Start)
        .build();
    title.set_markup(&format!(
        "<span size=\"17300\">{}</span>",
        esc(&data.title)
    ));
    s.content.append(&title);

    let summary = if data.summary.is_empty() {
        // First ~140 chars of the body as the stand-in summary.
        ev.content.chars().take(140).collect::<String>()
    } else {
        data.summary.clone()
    };
    let sum = gtk::Label::builder()
        .label(&summary)
        .wrap(true)
        .wrap_mode(WrapMode::WordChar)
        .xalign(0.0)
        .css_classes(["tagline"])
        .build();
    s.content.append(&sum);

    let when = data.published_at.unwrap_or_else(|| ev.created_at.as_secs());
    let meta = gtk::Label::builder()
        .css_classes(["tagline"])
        .halign(gtk::Align::Start)
        .build();
    meta.set_markup(&format!(
        "{} · {} · {} min read",
        profile.map(|p| p.display()).unwrap_or_default(),
        time_ago(when),
        std::cmp::max(1, ev.content.len() / 1000)
    ));
    s.content.append(&meta);

    let read = gtk::Button::builder()
        .label("Read")
        .css_classes(["seal-btn", "pill"])
        .halign(gtk::Align::Start)
        .margin_top(4)
        .build();
    {
        let ctx = ctx.clone();
        let ev = ev.clone();
        let data = data.clone();
        read.connect_clicked(move |_| {
            let page = article_reader(&ctx, &ev, &data);
            ctx.nav.push(&page);
        });
    }
    s.content.append(&read);

    s.root.append(&actions_row(ctx, ev, &s.root, &[]));
    s.root
}

fn article_reader(ctx: &Ctx, ev: &Event, data: &satori_core::nip::article::ArticleData) -> adw::NavigationPage {
    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(24)
        .build();

    let title = gtk::Label::builder()
        .css_classes(["serif", "wordmark"])
        .wrap(true)
        .wrap_mode(WrapMode::WordChar)
        .halign(gtk::Align::Start)
        .build();
    title.set_text(&data.title);
    column.append(&title);

    if let Some(img) = &data.image {
        let meta = satori_core::nip::imeta::ImageMeta {
            url: img.clone(),
            dim: None,
            alt: None,
            mime: None,
        };
        column.append(&media_picture(ctx, &meta, 420));
    }

    // TODO(port): markdown → rich text (nostr/markdown.ts) in the M4 pass.
    for para in ev.content.split("\n\n") {
        if para.trim().is_empty() {
            continue;
        }
        let p = gtk::Label::builder()
            .wrap(true)
            .wrap_mode(WrapMode::WordChar)
            .xalign(0.0)
            .valign(gtk::Align::Start)
            .build();
        p.set_markup(&render_content(para).0);
        column.append(&p);
    }

    let clamp = adw::Clamp::builder().maximum_size(660).child(&column).build();
    let scroll = gtk::ScrolledWindow::builder()
        .child(&clamp)
        .hexpand(true)
        .vexpand(true)
        .build();
    adw::NavigationPage::builder()
        .title(&data.title)
        .child(&scroll)
        .build()
}

// --- shared bits ---------------------------------------------------------------------

fn tag_value(ev: &Event, name: &str) -> Option<String> {
    ev.tags
        .clone()
        .to_vec()
        .iter()
        .find(|t| t.as_slice().first().map(String::as_str) == Some(name))
        .and_then(|t| t.as_slice().get(1).cloned())
}

fn meta_line(icon: &str, text: &str) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    row.append(&crate::icons::icon_sized(icon, 16));
    let label = gtk::Label::builder()
        .label(text)
        .wrap(true)
        .wrap_mode(WrapMode::WordChar)
        .halign(gtk::Align::Start)
        .css_classes(["tagline"])
        .build();
    row.append(&label);
    row
}

/// A media tile: aspect-reserved (imeta dim) placeholder, filled async
/// through the guarded media pipeline.
pub fn media_picture(
    ctx: &Ctx,
    meta: &satori_core::nip::imeta::ImageMeta,
    max_height: i32,
) -> gtk::Picture {
    let (w, h) = meta.dim.unwrap_or((16, 9));
    let height = if w > 0 {
        ((720.0 * h as f64 / w as f64) as i32).clamp(80, max_height)
    } else {
        240
    };
    let pic = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Cover)
        .height_request(height)
        .hexpand(true)
        .css_classes(["note-media"])
        .build();
    let core = ctx.core.clone();
    let pic_fill = pic.clone();
    drain(core.fetch_image(meta.url.clone()), move |img| {
        if let Some(decoded) = img {
            let bytes = glib::Bytes::from_owned(decoded.rgba);
            let tex = gdk::MemoryTexture::new(
                decoded.width as i32,
                decoded.height as i32,
                gdk::MemoryFormat::R8g8b8a8,
                &bytes,
                decoded.width as usize * 4,
            );
            pic_fill.set_paintable(Some(&tex));
        }
    });
    pic
}
