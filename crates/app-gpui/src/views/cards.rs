//! Rich cards from the manifest registry — port of `crates/app/src/cards.rs`
//! (`card_for` dispatch). The row shell (header, actions) stays in feed.rs;
//! this module renders the kind-specific body between them.
//!
//! Honesty posture carried over: GPUI has no video/audio pipelines, so video
//! (21/22) renders through the body's play-cards and podcasts (54) wear a
//! "Listen" link — nothing is fetched until the user asks (media.ts facade).
//! TODO(port): the article reader page (markdown, crates/app/src/article.rs);
//! wiki body inline.

use std::collections::{HashMap, HashSet};

use gpui::{
    div, prelude::*, px, relative, rgb, ClipboardItem, Div, Entity, FontWeight, SharedString,
};
use nostr::prelude::*;
use satori_core::nip::{article, calendar, classified, highlight, imeta, poll, repo, wiki, podcast};

use crate::{markdown, theme};
use super::content::{self, Body};
use super::feed::{rel_time, Feed};
use super::nav::Nav;

/// Read-only card state, cloned out of the feed each frame (the list closure
/// outlives the &self borrow).
#[derive(Clone, Default)]
pub struct CardState {
    pub poll_tallies: HashMap<EventId, Vec<(String, u32)>>,
    pub poll_sel: HashMap<EventId, Vec<String>>,
    pub voted: HashSet<EventId>,
    pub rsvped: HashMap<EventId, String>,
}

/// Kinds the feed carries beyond text notes (cards.rs `card_for` / the
/// manifest registry). Picture notes (20) render through the normal body.
pub fn is_feed_kind(kind: Kind) -> bool {
    // Compare by NUMBER: the nostr crate parses some of these as named
    // variants (30023 = Kind::LongFormTextNote), so `Kind::Custom(30023)`
    // patterns never match a parsed event — filters (serialized by number)
    // matched, cards silently didn't. Numbers are variant-proof.
    matches!(
        u16::from(kind),
        20 | 21 | 22 | 54 | 1068 | 30023 | 30402 | 30617 | 30818 | 31922 | 31923 | 9802
    )
}

/// The row body: a rich card for registry kinds, the normal text pipeline
/// otherwise (cards.rs `card_for`'s `_ => note_card`).
pub fn row_body(ev: &Event, ctx: &Body, feed: &Entity<Feed>, st: &CardState) -> Div {
    match u16::from(ev.kind) {
        21 | 22 => video_card(ev, ctx, feed),
        54 => podcast_card(ev, ctx, feed),
        1068 => poll_card(ev, ctx, feed, st),
        30023 => article_card(ev, ctx, feed, st),
        30402 => classified_card(ev, ctx, feed, st),
        30617 => repo_card(ev, ctx, feed),
        30818 => wiki_card(ev, ctx, feed),
        31922 | 31923 => calendar_card(ev, ctx, feed, st),
        9802 => highlight_card(ev, ctx, feed, st),
        _ => content::body(ev, ctx, feed.clone()),
    }
}

// --- shared bits ---------------------------------------------------------

/// The small tagline above a card ("↗ Article" — cards.rs kicker).
fn kicker(text: &str) -> Div {
    div()
        .text_size(px(11.0))
        .text_color(rgb(theme::MUTED))
        .child(text.to_string())
}

fn serif_title(text: &str, size: f32) -> Div {
    div()
        .w_full()
        .font_family(theme::SERIF)
        .text_size(px(size))
        .text_color(rgb(theme::INK))
        .child(text.to_string())
}

fn meta_line(text: impl Into<String>) -> Div {
    div()
        .w_full()
        .text_size(px(12.0))
        .text_color(rgb(theme::MUTED))
        .child(text.into())
}

fn badge(text: &str, color: u32) -> Div {
    div()
        .px_2()
        .py_0p5()
        .rounded_full()
        .border_1()
        .border_color(rgb(color))
        .text_size(px(11.0))
        .text_color(rgb(color))
        .child(text.to_string())
}

/// A quiet action seal inside a card body (cards.rs flat pill). `engaged`
/// dresses it vermilion; every inner click must stop propagation so the
/// row's open-thread handler doesn't also fire.
fn seal(id: String, label: &str, color: u32, engaged: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(SharedString::from(id))
        .cursor_pointer()
        .px_2()
        .py_0p5()
        .rounded_full()
        .border_1()
        .border_color(rgb(if engaged {
            theme::VERMILION
        } else {
            theme::HAIRLINE
        }))
        .text_size(px(12.0))
        .text_color(rgb(if engaged { theme::VERMILION } else { color }))
        .child(label.to_string())
}

fn fallback(ev: &Event, ctx: &Body, feed: &Entity<Feed>) -> Div {
    content::body(ev, ctx, feed.clone())
}

// --- video (NIP-71 kinds 21/22) ------------------------------------------

fn video_card(ev: &Event, ctx: &Body, feed: &Entity<Feed>) -> Div {
    // The body already carries the pipeline for video URLs (play-card +
    // poster fetch); the card adds the kicker and the serif title.
    let title = ev
        .content
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    let mut card = div().w_full().flex().flex_col().gap_2();
    card = card.child(kicker("▶ Video"));
    if !title.is_empty() {
        card = card.child(serif_title(&title, 15.0));
    }
    card.child(content::body(ev, ctx, feed.clone()))
}

// --- podcast (kind 54) ----------------------------------------------------

fn podcast_card(ev: &Event, ctx: &Body, feed: &Entity<Feed>) -> Div {
    let data = podcast::parse(ev);
    let mut card = div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .child(kicker("♪ Podcast"));
    let Some(data) = data else {
        return card.child(fallback(ev, ctx, feed));
    };
    if !data.title.is_empty() {
        card = card.child(serif_title(&data.title, 15.0));
    }
    // The enclosure wears a listen row — opens in the browser (no audio
    // pipeline; the same nothing-fetched-until-asked facade as video).
    if let Some(url) = &data.url
        && !url.is_empty()
        && !ev.content.contains(url.as_str())
    {
        card = card.child(content::link_row(url));
    }
    card.child(fallback(ev, ctx, feed))
}

// --- poll (NIP-68 kind 1068) ----------------------------------------------

fn poll_card(ev: &Event, ctx: &Body, feed: &Entity<Feed>, st: &CardState) -> Div {
    let Some(data) = poll::parse(ev) else {
        return fallback(ev, ctx, feed);
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let closed = data.ends_at.is_some_and(|end| end < now);
    let voted = st.voted.contains(&ev.id);
    let ballot = !closed && !voted;
    let tallies = st.poll_tallies.get(&ev.id).cloned();
    let selected = st.poll_sel.get(&ev.id).cloned().unwrap_or_default();
    let multiple = data.multiple;

    let mut card = div().w_full().flex().flex_col().gap_2();
    // The question rides the content (render_content in cards.rs).
    card = card.child(content::body(ev, ctx, feed.clone()));

    let total: u32 = tallies
        .as_ref()
        .map(|t| t.iter().map(|(_, n)| n).sum())
        .unwrap_or(0);
    for (id, label) in &data.options {
        let count = tallies
            .as_ref()
            .and_then(|t| t.iter().find(|(o, _)| o == id))
            .map(|(_, n)| *n)
            .unwrap_or(0);
        let pct = if total > 0 {
            count as f32 / total as f32
        } else {
            0.0
        };
        let picked = selected.contains(id);
        let opt = id.clone();
        let ev_for_click = ev.clone();
        let feed_for_click = feed.clone();
        let mut row = div()
            .id(SharedString::from(format!("poll-{}-{id}", ev.id)))
            .w_full()
            .py_0p5();
        if ballot {
            row = row.cursor_pointer().on_click(move |_, _, cx| {
                cx.stop_propagation();
                let ev = ev_for_click.clone();
                let opt = opt.clone();
                feed_for_click.update(cx, |f, cx| f.toggle_poll_sel(&ev, opt, multiple, cx));
            });
        }
        // Bar behind, label in front (poll.ts poll-bar-fill).
        card = card.child(row.child(
            div()
                .relative()
                .h(px(24.))
                .w_full()
                .rounded(px(3.))
                .overflow_hidden()
                .bg(rgb(theme::WASHI))
                .when(!ballot && total > 0 && pct > 0.0, |d| {
                    d.child(
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left_0()
                            .w(relative(pct))
                            .bg(rgb(theme::HAIRLINE)),
                    )
                })
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .child(
                            div()
                                .size(px(8.))
                                .rounded_full()
                                .border_1()
                                .border_color(rgb(if picked {
                                    theme::VERMILION
                                } else {
                                    theme::MUTED
                                }))
                                .bg(rgb(if picked {
                                    theme::VERMILION
                                } else {
                                    theme::WASHI
                                })),
                        )
                        .child(
                            div()
                                .text_size(px(13.0))
                                .text_color(rgb(theme::INK))
                                .child(label.clone()),
                        )
                        .when(!ballot && total > 0, |d| {
                            d.child(
                                div()
                                    .flex_1()
                                    .flex()
                                    .justify_end()
                                    .text_size(px(11.0))
                                    .text_color(rgb(theme::MUTED))
                                    .child(format!("{count}")),
                            )
                        }),
                ),
        ));
    }

    if ballot && !selected.is_empty() {
        let ev_for_vote = ev.clone();
        let feed_for_vote = feed.clone();
        card = card.child(
            div()
                .w_full()
                .flex()
                .justify_end()
                .child(seal(format!("vote-{}", ev.id), "Vote", theme::LIVE, false).on_click(
                    move |_, _, cx| {
                        cx.stop_propagation();
                        let ev = ev_for_vote.clone();
                        feed_for_vote.update(cx, |f, cx| f.cast_vote(&ev, cx));
                    },
                )),
        );
    }
    if voted {
        card = card.child(meta_line("Your ballot is in — thanks for the ink."));
    } else if closed {
        card = card.child(meta_line("Poll closed."));
    }
    card
}

// --- article (NIP-23 kind 30023) ------------------------------------------

fn article_card(ev: &Event, ctx: &Body, feed: &Entity<Feed>, _st: &CardState) -> Div {
    let Some(data) = article::parse(ev) else {
        return fallback(ev, ctx, feed);
    };
    let mut card = div().w_full().flex().flex_col().gap_2();
    if let Some(img) = content::article_cover(ev) {
        let mut dims: HashMap<String, Option<(u32, u32)>> = HashMap::new();
        dims.insert(img.clone(), None);
        let asset = content::MediaAsset::Picture { url: img.clone() };
        if let Some(cover) = content::media_section(std::slice::from_ref(&asset), &dims, ctx, feed) {
            card = card.child(cover);
        }
    }
    card = card.child(kicker("↗ Article"));
    card = card.child(serif_title(&data.title, 17.0));
    // Summary, or the first ~140 chars of the body as the stand-in (article.ts).
    let summary = if data.summary.is_empty() {
        ev.content.chars().take(140).collect::<String>()
    } else {
        data.summary.clone()
    };
    card = card.child(
        div()
            .w_full()
            .text_size(px(13.0))
            .text_color(rgb(theme::MUTED))
            .child(summary),
    );
    if let Some(ts) = data.published_at {
        card = card.child(meta_line(rel_time(ts)));
    }
    // The Read seal opens the reader page (markdown). Quiet on purpose —
    // a serif word, underlined, the only door out of the summary.
    let ev_for_read = ev.clone();
    let read = feed.downgrade();
    card = card.child(
        div()
            .id(SharedString::from(format!("read-{}", ev.id.to_string())))
            .cursor_pointer()
            .font_family(theme::SERIF)
            .text_size(px(14.0))
            .text_color(rgb(theme::MUTED))
            .underline()
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                let _ = read.update(cx, |_, cx| cx.emit(Nav::OpenArticle(ev_for_read.clone())));
            })
            .child("Read"),
    );
    card
}

// --- classified (NIP-99 kind 30402) ----------------------------------------

fn classified_card(ev: &Event, ctx: &Body, feed: &Entity<Feed>, _st: &CardState) -> Div {
    let Some(data) = classified::parse(ev) else {
        return fallback(ev, ctx, feed);
    };
    let mut card = div().w_full().flex().flex_col().gap_2();
    // Head: title + sold badge (classified.ts).
    card = card.child(
        div()
            .w_full()
            .flex()
            .items_start()
            .gap_2()
            .child(serif_title(&data.title, 16.0).flex_1())
            .when(data.status.as_deref() == Some("sold"), |d| {
                d.child(badge("Sold", theme::VERMILION))
            }),
    );
    if let Some(price) = &data.price {
        card = card.child(
            div()
                .font_family(theme::SERIF)
                .text_size(px(15.0))
                .text_color(rgb(theme::VERMILION))
                .child(price.clone()),
        );
    }
    let mut line: Vec<String> = Vec::new();
    if let Some(cond) = &data.condition {
        line.push(cond.clone());
    }
    if let Some(loc) = &data.location {
        line.push(loc.clone());
    }
    if !line.is_empty() {
        card = card.child(meta_line(line.join(" · ")));
    }
    if !data.images.is_empty() {
        let dims: HashMap<String, Option<(u32, u32)>> = imeta::parse(ev)
            .into_iter()
            .filter_map(|m| m.dim.map(|d| (m.url, Some(d))))
            .collect();
        let assets: Vec<content::MediaAsset> = data
            .images
            .iter()
            .map(|u| content::MediaAsset::Picture { url: u.clone() })
            .collect();
        if let Some(gallery) = content::media_section(&assets, &dims, ctx, feed) {
            card = card.child(gallery);
        }
    }
    card.child(fallback(ev, ctx, feed))
}

// --- repo (NIP-34 kind 30617) ----------------------------------------------

fn repo_card(ev: &Event, ctx: &Body, feed: &Entity<Feed>) -> Div {
    let Some(data) = repo::parse(ev) else {
        return fallback(ev, ctx, feed);
    };
    let mut card = div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .child(kicker("⌥ Repo"));
    card = card.child(serif_title(&data.name, 15.0));
    if !data.description.is_empty() {
        card = card.child(
            div()
                .w_full()
                .text_size(px(13.0))
                .text_color(rgb(theme::MUTED))
                .child(data.description.clone()),
        );
    }
    for clone in &data.clone {
        let url = clone.clone();
        card = card.child(
            div()
                .w_full()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(12.0))
                        .text_color(rgb(theme::MUTED))
                        .child(url.clone()),
                )
                .child(
                    seal(format!("copy-{}-{url}", ev.id), "Copy", theme::MUTED, false).on_click(
                        move |_, _, cx| {
                            cx.stop_propagation();
                            cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
                        },
                    ),
                ),
        );
    }
    for web in data.web.iter().take(2) {
        card = card.child(content::link_row(web));
    }
    if !data.maintainers.is_empty() {
        card = card.child(meta_line(format!("{} maintainer(s)", data.maintainers.len())));
    }
    card
}

// --- wiki (kind 30818) ------------------------------------------------------

fn wiki_card(ev: &Event, ctx: &Body, feed: &Entity<Feed>) -> Div {
    let Some(data) = wiki::parse(ev) else {
        return fallback(ev, ctx, feed);
    };
    let mut card = div().w_full().flex().flex_col().gap_2();
    if data.topic.is_empty() {
        card = card.child(kicker("⌂ Wiki"));
    } else {
        card = card.child(kicker(&format!("⌂ Wiki · {}", data.topic)));
    }
    card = card.child(serif_title(&data.title, 16.0));
    if let Some(summary) = &data.summary {
        card = card.child(
            div()
                .w_full()
                .text_size(px(13.0))
                .text_color(rgb(theme::MUTED))
                .child(summary.clone()),
        );
    }
    // The body inline — the GTK card renders its AsciiDoc subset in place
    // (== headings, **bold** pairs; asciidoc_line).
    for line in data.content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(h) = line.strip_prefix("== ") {
            card = card.child(
                div()
                    .w_full()
                    .font_family(theme::SERIF)
                    .text_size(px(15.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(theme::INK))
                    .child(h.to_string()),
            );
            continue;
        }
        let mut row = div().w_full().flex().flex_wrap().gap_1();
        for span in markdown::spans(line) {
            if span.text.is_empty() {
                continue;
            }
            row = row.child(
                div()
                    .font_family(theme::SERIF)
                    .text_size(px(13.5))
                    .line_height(relative(1.2))
                    .text_color(rgb(theme::INK))
                    .when(span.bold, |d| d.font_weight(FontWeight::SEMIBOLD))
                    .child(span.text),
            );
        }
        card = card.child(row);
    }
    card
}

// --- calendar (NIP-52 kinds 31922/31923) ------------------------------------

fn calendar_card(ev: &Event, ctx: &Body, feed: &Entity<Feed>, st: &CardState) -> Div {
    let Some(data) = calendar::parse(ev) else {
        return fallback(ev, ctx, feed);
    };
    let mut card = div().w_full().flex().flex_col().gap_2();
    card = card.child(serif_title(&data.title, 16.0));
    card = card.child(meta_line(format!("🗓 {}", fmt_when(&data))));
    if let Some(loc) = &data.location {
        card = card.child(meta_line(format!("📍 {loc}")));
    }
    if !data.description.is_empty() {
        card = card.child(
            div()
                .w_full()
                .text_size(px(13.0))
                .text_color(rgb(theme::INK))
                .child(data.description.clone()),
        );
    }
    // RSVP pills (calendar.ts): Going / Maybe / Can't go, self-reporting.
    let my_rsvp = st.rsvped.get(&ev.id).cloned();
    let mut pills = div().w_full().flex().gap_2();
    for (status, label) in [
        ("accepted", "Going"),
        ("tentative", "Maybe"),
        ("declined", "Can't go"),
    ] {
        let ev_for_click = ev.clone();
        let feed_for_click = feed.clone();
        pills = pills.child(
            seal(
                format!("rsvp-{}-{status}", ev.id),
                label,
                theme::MUTED,
                my_rsvp.as_deref() == Some(status),
            )
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                let ev = ev_for_click.clone();
                feed_for_click.update(cx, |f, cx| f.send_rsvp(&ev, status, cx));
            }),
        );
    }
    card.child(pills)
}

/// "%a %b %e %Y %H:%M" without chrono (cards.rs format_when; civil-from-days,
/// Hinnant — epoch day 0 was a Thursday).
fn fmt_when(data: &calendar::CalendarData) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let fmt = |secs: u64| -> String {
        let days = (secs / 86400) as i64;
        let rem = secs % 86400;
        let (h, mi) = (rem / 3600, (rem % 3600) / 60);
        let z = days + 719468;
        let era = if z >= 0 { z } else { z - 146096 } / 146097;
        let doe = z - era * 146097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
        format!(
            "{} {} {} {} {:02}:{:02}",
            DAYS[days.rem_euclid(7) as usize],
            MONTHS[(m - 1) as usize],
            d,
            y,
            h,
            mi
        )
    };
    match data.end {
        Some(end) => format!("{} – {}", fmt(data.start), fmt(end)),
        None => fmt(data.start),
    }
}

// --- highlight (kind 9802) ---------------------------------------------------

fn highlight_card(ev: &Event, ctx: &Body, _feed: &Entity<Feed>, _st: &CardState) -> Div {
    let data = highlight::parse(ev);
    let mut card = div().w_full().flex().flex_col().gap_2();
    // Serif blockquote with a vermilion rule (highlight.ts quote-first): when
    // the context carries the quote verbatim, mark it in place.
    let marked = data
        .context
        .as_deref()
        .and_then(|c| c.find(&data.content).map(|pos| (c, pos)));
    let quote = |text: Div| {
        div()
            .w_full()
            .border_l_2()
            .border_color(rgb(theme::VERMILION))
            .pl_3()
            .font_family(theme::SERIF)
            .text_size(px(14.0))
            .text_color(rgb(theme::INK))
            .child(text)
    };
    match marked {
        Some((context, pos)) => {
            let (before, after) = (&context[..pos], &context[pos + data.content.len()..]);
            card = card.child(quote(
                div()
                    .flex()
                    .flex_wrap()
                    .child(before.trim_start().to_string())
                    .child(
                        div()
                            .text_color(rgb(theme::VERMILION))
                            .font_weight(gpui::FontWeight::BOLD)
                            .child(data.content.clone()),
                    )
                    .child(after.trim_end().to_string()),
            ));
        }
        None => {
            card = card.child(quote(div().child(data.content.clone())));
            if let Some(context) = &data.context
                && !context.is_empty()
            {
                card = card.child(
                    div()
                        .w_full()
                        .text_size(px(12.0))
                        .text_color(rgb(theme::MUTED))
                        .child(context.clone()),
                );
            }
        }
    }
    if let Some(author) = &data.author {
        let name = ctx.profiles.get(author).map(|p| p.display());
        card = card.child(meta_line(format!("— {}", name.unwrap_or_else(|| short(author)))));
    }
    if let Some(source) = &data.source
        && source.starts_with("http")
    {
        card = card.child(content::link_row(source));
    }
    card
}

fn short(key: &str) -> String {
    format!("{}…", &key[..8.min(key.len())])
}
