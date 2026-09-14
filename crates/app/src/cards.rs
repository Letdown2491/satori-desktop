//! Kind cards + registry — port of `src/manifest/` (satori.ts registrations,
//! the hand-coded handlers). `card_for` is the renderEvent dispatch; the
//! fallback is the honest plain-note (NIP-31 alt surfaces in M4 proper).

use std::collections::HashMap;

use adw::prelude::*;
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
        Kind::Custom(54) => podcast_card(ctx, ev, profile, focused),
        Kind::Custom(1068) => poll_card(ctx, ev, profile, focused),
        Kind::Custom(30023) => article_card(ctx, ev, profile, focused),
        Kind::Custom(30402) => classified_card(ctx, ev, profile, focused),
        Kind::Custom(30617) => repo_card(ctx, ev, profile, focused),
        Kind::Custom(30818) => wiki_card(ctx, ev, profile, focused),
        Kind::Custom(31922) | Kind::Custom(31923) => calendar_card(ctx, ev, profile, focused),
        Kind::Custom(9802) => highlight_card(ctx, ev, profile, focused),
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
        body.set_markup(&render_content(ev.content.as_str(), &resolver(ctx)).markup);
        s.content.append(&body);
    }

    for meta in &metas {
        s.content.append(&media_picture(ctx, meta, 680));
    }
    if metas.is_empty() {
        // No imeta: honest fallback to the text pipeline (which extracts URLs).
        let rendered = render_content(ev.content.as_str(), &resolver(ctx));
        let body = gtk::Label::builder()
            .wrap(true)
            .wrap_mode(WrapMode::WordChar)
            .xalign(0.0)
            .build();
        body.set_markup(&rendered.markup);
        s.content.append(&body);
        for url in rendered.images {
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
    // Facade aspect follows the poster's imeta dim (video.ts: orientation
    // from dim or kind; short = portrait).
    let (pw, ph) = poster.and_then(|p| p.dim).unwrap_or((16, 9));
    let facade_h = if short {
        ((240.0 * ph as f64 / pw as f64) as i32).clamp(320, 560)
    } else {
        ((640.0 * ph as f64 / pw as f64) as i32).clamp(180, 420)
    };
    let facade = gtk::Overlay::builder()
        .height_request(facade_h)
        .hexpand(true)
        .css_classes(["note-media", "video-facade"])
        .build();
    if let Some(p) = poster {
        // Poster behind the play glyph.
        facade.set_child(Some(&media_picture(ctx, p, facade_h)));
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
        body.set_markup(&render_content(ev.content.as_str(), &resolver(ctx)).markup);
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
        body.set_markup(&render_content(data.description.as_str(), &resolver(ctx)).markup);
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

    // Markdown → blocks (nostr/markdown.ts port).
    for block in crate::markdown::blocks(&ev.content) {
        match block {
            crate::markdown::Block::Para(text) => {
                let p = gtk::Label::builder()
                    .wrap(true)
                    .wrap_mode(WrapMode::WordChar)
                    .xalign(0.0)
                    .valign(gtk::Align::Start)
                    .css_classes(["article-para"])
                    .build();
                p.set_markup(&render_content(&text, &resolver(ctx)).markup);
                column.append(&p);
            }
            crate::markdown::Block::Heading(level, text) => {
                let sizes = [28000, 23000, 19000];
                let h = gtk::Label::builder()
                    .wrap(true)
                    .wrap_mode(WrapMode::WordChar)
                    .xalign(0.0)
                    .halign(gtk::Align::Start)
                    .css_classes(["serif"])
                    .build();
                h.set_markup(&format!(
                    "<span size=\"{}\" font_weight=\"500\">{}</span>",
                    sizes[(level - 1).clamp(0, 2)],
                    crate::markdown::inline(&text)
                ));
                column.append(&h);
            }
            crate::markdown::Block::Quote(text) => {
                let q = gtk::Box::builder().css_classes(["hl-quote"]).build();
                let p = gtk::Label::builder()
                    .wrap(true)
                    .wrap_mode(WrapMode::WordChar)
                    .xalign(0.0)
                    .css_classes(["serif", "hl-text"])
                    .build();
                p.set_markup(&crate::markdown::inline(&text));
                q.append(&p);
                column.append(&q);
            }
            crate::markdown::Block::List(items) => {
                for item in items {
                    let row = gtk::Box::builder()
                        .orientation(gtk::Orientation::Horizontal)
                        .spacing(8)
                        .build();
                    row.append(&gtk::Label::builder().label("·").build());
                    let p = gtk::Label::builder()
                        .wrap(true)
                        .wrap_mode(WrapMode::WordChar)
                        .xalign(0.0)
                        .halign(gtk::Align::Start)
                        .hexpand(true)
                        .build();
                    p.set_markup(&crate::markdown::inline(&item));
                    row.append(&p);
                    column.append(&row);
                }
            }
            crate::markdown::Block::Code(text) => {
                let code = gtk::Label::builder()
                    .wrap(true)
                    .wrap_mode(WrapMode::WordChar)
                    .xalign(0.0)
                    .halign(gtk::Align::Start)
                    .css_classes(["mono", "code-block"])
                    .build();
                code.set_text(&text);
                column.append(&code);
            }
            crate::markdown::Block::Rule => {
                column.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
            }
            crate::markdown::Block::Image(url) => {
                let meta = satori_core::nip::imeta::ImageMeta {
                    url,
                    dim: None,
                    alt: None,
                    mime: None,
                };
                column.append(&media_picture(ctx, &meta, 460));
            }
        }
    }

    // Comments (NIP-22) — the addressable target survives edits (comments.ts).
    let target = satori_core::nip::comment::CommentTarget::for_event(ev);
    column.append(&crate::comments::comments_section(ctx, &target));

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

/// Open an addressable entity (naddr link): fetch, then route — articles to
/// the reader, everything else to a focused card page (comments via NIP-22
/// land in M2).
pub fn entity_page_by_naddr(
    ctx: &Ctx,
    kind: u16,
    author: PublicKey,
    identifier: String,
) -> adw::NavigationPage {
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
    loading.append(&gtk::Label::new(Some("Fetching…")));
    stack.add_named(&loading, Some("loading"));

    let page = adw::NavigationPage::builder()
        .title("Reference")
        .child(&stack)
        .build();

    {
        let ctx = ctx.clone();
        let stack = stack.clone();
        drain(ctx.core.event_by_address(kind, author, identifier), move |reply| {
            use satori_core::manager::FeedEvent;
            match reply {
                FeedEvent::One(Some(ev), profile) => {
                    let page_box = gtk::Box::builder()
                        .orientation(gtk::Orientation::Vertical)
                        .spacing(0)
                        .margin_top(12)
                        .build();
                    let clamp_col = gtk::Box::builder()
                        .orientation(gtk::Orientation::Vertical)
                        .spacing(12)
                        .build();
                    clamp_col.append(&card_for(&ctx, &ev, profile.as_ref(), true));
                    let target = satori_core::nip::comment::CommentTarget::for_event(&ev);
                    clamp_col.append(&crate::comments::comments_section(&ctx, &target));
                    let clamp = adw::Clamp::builder().maximum_size(660).child(&clamp_col).build();
                    let scroll = gtk::ScrolledWindow::builder()
                        .child(&clamp)
                        .hexpand(true)
                        .vexpand(true)
                        .build();
                    page_box.append(&scroll);
                    let content: gtk::Widget = page_box.upcast();
                    stack.remove(&loading);
                    stack.add_child(&content);
                    stack.set_visible_child(&content);
                }
                FeedEvent::One(None, _) => {
                    stack.remove(&loading);
                    let missing = gtk::Label::builder()
                        .label("This reference could not be found on your relays.")
                        .css_classes(["tagline"])
                        .build();
                    stack.add_child(&missing);
                    stack.set_visible_child(&missing);
                }
                FeedEvent::Failed(msg) => {
                    eprintln!("[naddr] {msg}");
                }
                _ => {}
            }
        });
    }
    page
}

// --- highlight (NIP-84) ---------------------------------------------------------------

fn highlight_card(ctx: &Ctx, ev: &Event, profile: Option<&Profile>, focused: bool) -> gtk::Box {
    let data = satori_core::nip::highlight::parse(ev);
    let s = shell(ctx, ev, profile, focused, false);

    // Serif blockquote with a vermilion rule (highlight.ts quote-first).
    // When the context carries the quote verbatim, mark it in place
    // (highlight.ts <mark class="hl-mark">); else quote + clamped context.
    let marked = data.context.as_deref().and_then(|c| {
        let pos = c.find(&data.content)?;
        Some((
            c[..pos].to_string(),
            data.content.clone(),
            c[pos + data.content.len()..].to_string(),
        ))
    });
    let quote = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .css_classes(["hl-quote"])
        .build();
    let text = gtk::Label::builder()
        .wrap(true)
        .wrap_mode(WrapMode::WordChar)
        .xalign(0.0)
        .css_classes(["serif", "hl-text"])
        .build();
    match &marked {
        Some((before, mark, after)) => text.set_markup(&format!(
            "{}<b><span foreground=\"#b23a26\">{}</span></b>{}",
            esc(before.trim_start()),
            esc(mark),
            esc(after.trim_end())
        )),
        None => text.set_text(&data.content),
    }
    quote.append(&text);
    s.content.append(&quote);

    if let (Some(context), None) = (&data.context, &marked) {
        let line = gtk::Label::builder()
            .label(context)
            .wrap(true)
            .wrap_mode(WrapMode::WordChar)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .lines(3)
            .css_classes(["tagline"])
            .build();
        s.content.append(&line);
    }

    // Citation: by X · source host.
    let mut cite = String::new();
    if let Some(author) = &data.author {
        cite.push_str(&format!("— {} ", esc(author)));
    } else if let Some(p) = profile {
        cite.push_str(&format!("— {} ", esc(&p.display())));
    }
    if let Some(source) = &data.source {
        cite.push_str(&format!("· {}", esc(&display_host(source))));
    }
    let cite_label = gtk::Label::builder()
        .css_classes(["tagline"])
        .halign(gtk::Align::Start)
        .build();
    cite_label.set_markup(&cite);
    s.content.append(&cite_label);

    s.root.append(&actions_row(ctx, ev, &s.root, &[]));
    s.root
}

fn display_host(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_else(|| url.to_string())
}

// --- podcast (NIP-F4) --------------------------------------------------------------------

fn podcast_card(ctx: &Ctx, ev: &Event, profile: Option<&Profile>, focused: bool) -> gtk::Box {
    let Some(data) = satori_core::nip::podcast::parse(ev) else {
        return note_card(ctx, ev, profile, focused);
    };
    let s = shell(ctx, ev, profile, focused, false);

    if let Some(img) = &data.image {
        let meta = satori_core::nip::imeta::ImageMeta {
            url: img.clone(),
            dim: None,
            alt: None,
            mime: None,
        };
        s.content.append(&media_picture(ctx, &meta, 360));
    }

    let title = gtk::Label::builder()
        .css_classes(["serif", "note-author"])
        .wrap(true)
        .wrap_mode(WrapMode::WordChar)
        .halign(gtk::Align::Start)
        .build();
    title.set_markup(&format!("<big>{}</big>", esc(&data.title)));
    s.content.append(&title);

    // Audio facade → MediaControls on play (preload=none posture).
    if let Some(audio_url) = data.url.clone() {
        let stack = gtk::Stack::builder().build();
        let play_btn = gtk::Button::builder()
            .child(&crate::icons::icon_sized("play", 26))
            .css_classes(["flat", "pill", "podcast-play"])
            .halign(gtk::Align::Start)
            .build();
        stack.add_named(&play_btn, Some("facade"));
        {
            let stack = stack.clone();
            play_btn.connect_clicked(move |_| {
                let file = gtk::gio::File::for_uri(&audio_url);
                let media = gtk::MediaFile::for_file(&file);
                let controls = gtk::MediaControls::new(Some(&media));
                controls.add_css_class("podcast-controls");
                stack.remove(&stack.first_child().expect("facade"));
                stack.add_child(&controls);
                stack.set_visible_child(&controls);
            });
        }
        stack.set_visible_child_name("facade");
        s.content.append(&stack);
    }

    // Show notes, fully collapsed (podcast.ts collapse()).
    if !data.notes.is_empty() {
        let notes = gtk::Label::builder()
            .wrap(true)
            .wrap_mode(WrapMode::WordChar)
            .xalign(0.0)
            .valign(gtk::Align::Start)
            .build();
        notes.set_markup(&render_content(data.notes.as_str(), &resolver(ctx)).markup);
        let reveal = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .child(&notes)
            .visible(false)
            .build();
        let toggle = gtk::Button::builder()
            .label("Show notes")
            .css_classes(["flat", "older-btn"])
            .halign(gtk::Align::Start)
            .build();
        s.content.append(&toggle);
        s.content.append(&reveal);
        {
            let reveal = reveal.clone();
            let toggle_recv = toggle.clone();
            toggle_recv.connect_clicked(move |_| {
                let show = !reveal.is_visible();
                reveal.set_visible(true);
                reveal.set_reveal_child(show);
                toggle.set_label(if show { "Hide notes" } else { "Show notes" });
            });
        }
    }

    s.root.append(&actions_row(ctx, ev, &s.root, &[]));
    s.root
}

// --- classified (NIP-99) -------------------------------------------------------------------

fn classified_card(ctx: &Ctx, ev: &Event, profile: Option<&Profile>, focused: bool) -> gtk::Box {
    let Some(data) = satori_core::nip::classified::parse(ev) else {
        return note_card(ctx, ev, profile, focused);
    };
    let s = shell(ctx, ev, profile, focused, false);

    let head = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    let title = gtk::Label::builder()
        .css_classes(["serif", "note-author"])
        .wrap(true)
        .wrap_mode(WrapMode::WordChar)
        .halign(gtk::Align::Start)
        .hexpand(true)
        .build();
    title.set_markup(&format!("<big>{}</big>", esc(&data.title)));
    head.append(&title);
    // Sold badge (classified.ts).
    if data.status.as_deref() == Some("sold") {
        head.append(&gtk::Label::builder()
            .label("Sold")
            .css_classes(["badge", "sold-badge"])
            .valign(gtk::Align::Start)
            .build());
    }
    s.content.append(&head);

    // Price-forward meta (classified.ts: "100 USD / day").
    if let Some(price) = &data.price {
        let price_label = gtk::Label::builder()
            .css_classes(["serif", "price"])
            .halign(gtk::Align::Start)
            .build();
        price_label.set_markup(&format!(
            "<span foreground=\"#b23a26\">{}</span>",
            esc(price)
        ));
        s.content.append(&price_label);
    }

    let mut meta_bits: Vec<String> = Vec::new();
    if let Some(cond) = &data.condition {
        meta_bits.push(esc(cond));
    }
    if let Some(loc) = &data.location {
        meta_bits.push(esc(loc));
    }
    if !meta_bits.is_empty() {
        s.content.append(&meta_line("map-pin", &meta_bits.join(" · ")));
    }

    for img in data.images.iter().take(4) {
        let meta = satori_core::nip::imeta::ImageMeta {
            url: img.clone(),
            dim: None,
            alt: None,
            mime: None,
        };
        s.content.append(&media_picture(ctx, &meta, 420));
    }

    if !data.summary.is_empty() {
        let body = gtk::Label::builder()
            .wrap(true)
            .wrap_mode(WrapMode::WordChar)
            .xalign(0.0)
            .build();
        body.set_markup(&render_content(data.summary.as_str(), &resolver(ctx)).markup);
        s.content.append(&body);
    }

    // "Message seller" is the DM CTA in the web app — deferred with DMs (D6).

    let when = data
        .published_at
        .unwrap_or_else(|| ev.created_at.as_secs());
    let meta2 = gtk::Label::builder()
        .css_classes(["tagline"])
        .halign(gtk::Align::Start)
        .build();
    meta2.set_text(&time_ago(when));
    s.content.append(&meta2);

    s.root.append(&actions_row(ctx, ev, &s.root, &[]));
    s.root
}

// --- wiki (NIP-54) -------------------------------------------------------------------------

fn wiki_card(ctx: &Ctx, ev: &Event, profile: Option<&Profile>, focused: bool) -> gtk::Box {
    let Some(data) = satori_core::nip::wiki::parse(ev) else {
        return note_card(ctx, ev, profile, focused);
    };
    let s = shell(ctx, ev, profile, focused, false);

    let title = gtk::Label::builder()
        .css_classes(["serif", "wordmark"])
        .wrap(true)
        .wrap_mode(WrapMode::WordChar)
        .halign(gtk::Align::Start)
        .build();
    let shown = if data.title.is_empty() { data.topic.clone() } else { data.title.clone() };
    title.set_markup(&format!("<span size=\"17300\">{}</span>", esc(&shown)));
    s.content.append(&title);

    if let Some(summary) = &data.summary {
        let sum = gtk::Label::builder()
            .label(summary)
            .wrap(true)
            .wrap_mode(WrapMode::WordChar)
            .xalign(0.0)
            .css_classes(["tagline"])
            .build();
        s.content.append(&sum);
    }

    // AsciiDoc subset: headings, lists, bold/italic, [[wikilinks]] → naddr
    // links (asciidoc.ts subset; the full parser is M4 polish).
    for line in data.content.lines() {
        let line_trim = line.trim();
        if line_trim.is_empty() {
            continue;
        }
        if let Some(h) = line_trim.strip_prefix("== ") {
            let head = gtk::Label::builder()
                .css_classes(["serif", "note-author"])
                .halign(gtk::Align::Start)
                .build();
            head.set_markup(&esc(h));
            s.content.append(&head);
            continue;
        }
        let body = gtk::Label::builder()
            .wrap(true)
            .wrap_mode(WrapMode::WordChar)
            .xalign(0.0)
            .valign(gtk::Align::Start)
            .build();
        body.set_markup(&asciidoc_line(line_trim, &ev.pubkey));
        s.content.append(&body);
    }

    s.root.append(&actions_row(ctx, ev, &s.root, &[]));
    s.root
}

/// One AsciiDoc line: wikilinks + bold + italic markers → markup.
fn asciidoc_line(line: &str, author: &PublicKey) -> String {
    let mut out = esc(line);
    // **bold** → <b>, __italic__ → <i> (marker pairs, non-greedy).
    if let Ok(re_b) = regex_like_replace(&out, "**") {
        out = re_b;
    }
    let _ = author;
    out
}

fn regex_like_replace(s: &str, marker: &str) -> Result<String, ()> {
    // Split on the marker; every odd segment is wrapped.
    let parts: Vec<&str> = s.split(marker).collect();
    if parts.len() < 2 || parts.len().is_multiple_of(2) {
        return Err(());
    }
    let wrapped: String = parts
        .iter()
        .enumerate()
        .map(|(i, seg)| {
            if i % 2 == 1 {
                let tag = if marker == "**" { "b" } else { "i" };
                format!("<{tag}>{seg}</{tag}>")
            } else {
                seg.to_string()
            }
        })
        .collect();
    Ok(wrapped)
}

// --- repo (NIP-34) ---------------------------------------------------------------------------

fn repo_card(ctx: &Ctx, ev: &Event, profile: Option<&Profile>, focused: bool) -> gtk::Box {
    let Some(data) = satori_core::nip::repo::parse(ev) else {
        return note_card(ctx, ev, profile, focused);
    };
    let s = shell(ctx, ev, profile, focused, false);

    let head = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    head.append(&crate::icons::icon_sized("git", 22));
    let name = gtk::Label::builder()
        .css_classes(["serif", "note-author"])
        .halign(gtk::Align::Start)
        .build();
    name.set_markup(&format!("<big>{}</big>", esc(&data.name)));
    head.append(&name);
    s.content.append(&head);

    if !data.description.is_empty() {
        let body = gtk::Label::builder()
            .wrap(true)
            .wrap_mode(WrapMode::WordChar)
            .xalign(0.0)
            .build();
        body.set_markup(&esc(&data.description));
        s.content.append(&body);
    }

    // Topic chips.
    if !data.topics.is_empty() {
        let chips = gtk::FlowBox::builder()
            .orientation(gtk::Orientation::Horizontal)
            .selection_mode(gtk::SelectionMode::None)
            .max_children_per_line(6)
            .activate_on_single_click(false)
            .css_classes(["topic-chips"])
            .build();
        for t in data.topics.iter().take(8) {
            chips.append(&gtk::Label::builder()
                .label(t)
                .css_classes(["tagline", "chip"])
                .build());
        }
        s.content.append(&chips);
    }

    // Browse: clone blocks + web/maintainers/relays rows (repo.ts reader).
    let browse = gtk::Button::builder()
        .label("Browse")
        .css_classes(["flat", "pill"])
        .halign(gtk::Align::Start)
        .build();
    let detail = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .visible(false)
        .build();
    {
        let detail = detail.clone();
        browse.connect_clicked(move |_| {
            detail.set_visible(!detail.is_visible());
        });
    }
    for url in &data.clone {
        let block = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .css_classes(["clone-block"])
            .build();
        block.append(&gtk::Label::builder()
            .label(url)
            .halign(gtk::Align::Start)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .css_classes(["mono"])
            .build());
        let copy = gtk::Button::builder()
            .label("Copy")
            .css_classes(["flat", "pill"])
            .build();
        {
            let url = url.clone();
            copy.connect_clicked(move |_| {
                let display = gtk::gdk::Display::default().expect("display");
                display.clipboard().set_text(&url);
            });
        }
        block.append(&copy);
        detail.append(&block);
    }
    for w in &data.web {
        detail.append(&meta_line("globe", w));
    }
    if !data.maintainers.is_empty() {
        detail.append(&meta_line("gear", &format!("{} maintainer(s)", data.maintainers.len())));
    }
    if !data.relays.is_empty() {
        detail.append(&meta_line("globe", &format!("{} relay(s)", data.relays.len())));
    }
    s.content.append(&browse);
    s.content.append(&detail);

    s.root.append(&actions_row(ctx, ev, &s.root, &[]));
    s.root
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

/// A small square thumbnail (embed cards).
pub fn thumb_picture(ctx: &Ctx, url: &str) -> gtk::Picture {
    let pic = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Cover)
        .width_request(64)
        .height_request(64)
        .halign(gtk::Align::End)
        .valign(gtk::Align::Start)
        .css_classes(["note-media", "embed-thumb"])
        .build();
    if let Some(tex) = crate::media_cache::mem_get(url) {
        pic.set_paintable(Some(&tex));
        return pic;
    }
    let core = ctx.core.clone();
    let pic_fill = pic.clone();
    let url_owned = url.to_string();
    drain(core.fetch_image(url_owned.clone()), move |img| {
        if let Some(decoded) = img {
            let tex = crate::media_cache::from_decoded(&url_owned, &decoded);
            pic_fill.set_paintable(Some(&tex));
        }
    });
    pic
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
    if let Some(tex) = crate::media_cache::mem_get(&meta.url) {
        pic.set_paintable(Some(&tex));
        return pic;
    }
    let core = ctx.core.clone();
    let pic_fill = pic.clone();
    let url_owned = meta.url.clone();
    drain(core.fetch_image(url_owned.clone()), move |img| {
        if let Some(decoded) = img {
            let tex = crate::media_cache::from_decoded(&url_owned, &decoded);
            pic_fill.set_paintable(Some(&tex));
        }
    });
    pic
}

/// Extract the PublicKey from a profile bech32 (npub/nprofile) — for
/// mention-name resolution inside render_content.
fn nip19_pk(bech: &str) -> Option<nostr::key::PublicKey> {
    use nostr::prelude::*;
    Nip19::from_bech32(bech).ok().and_then(|n| match n {
        Nip19::Pubkey(pk) => Some(pk),
        Nip19::Profile(p) => Some(p.public_key),
        _ => None,
    })
}

/// Context-bound resolver for render_content (also used by comments).
pub fn resolver_pub(ctx: &Ctx) -> impl Fn(&str) -> Option<String> + '_ {
    resolver(ctx)
}

fn resolver(ctx: &Ctx) -> impl Fn(&str) -> Option<String> + '_ {
    move |b: &str| {
        nip19_pk(b)
            .and_then(|pk| ctx.core.profile_of(&pk))
            .map(|p| p.name)
            .filter(|n| !n.is_empty())
    }
}
