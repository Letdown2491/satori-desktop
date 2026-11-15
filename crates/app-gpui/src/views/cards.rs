//! Rich cards from the manifest registry — port of `crates/app/src/cards.rs`
//! (`card_for` dispatch). The row shell (header, actions) stays in feed.rs;
//! this module renders the kind-specific body between them.
//!
//! Honesty posture carried over: video (21/22) renders through the body's
//! media section — a poster until tapped, then the Phase 3 inline player
//! (video.rs; strict Tor and YouTube degrade to browser links) — and
//! podcasts (54) wear a "Listen" link — nothing is fetched until the user
//! asks (media.ts facade). The article reader lives at views/reader.rs.
//! wiki body inline.

use std::collections::{HashMap, HashSet};

use gpui::{
    ClipboardItem, Div, Entity, FontWeight, SharedString, div, prelude::*, px, relative, rgb,
};
use nostr::prelude::*;
use satori_core::nip::{calendar, classified, fields, goal, highlight, podcast, poll, repo, wiki};

use super::content::{self, Body};
use super::feed::Feed;
use super::nav::Nav;
use crate::{markdown, theme};

/// Read-only card state, cloned out of the feed each frame (the list closure
/// outlives the &self borrow).
#[derive(Clone, Default)]
pub struct CardState {
    pub poll_tallies: HashMap<EventId, Vec<(String, u32)>>,
    pub poll_sel: HashMap<EventId, Vec<String>>,
    pub voted: HashSet<EventId>,
    pub rsvped: HashMap<EventId, String>,
    /// NIP-36: warnings the reader has chosen to reveal (the veil
    /// collapses to this bar; the reveal is a per-feed choice).
    pub revealed: HashSet<EventId>,
    /// NIP-75: zap goals' raised totals in msat (0 while the pump is out).
    pub goal_msat: HashMap<EventId, u64>,
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
        20 | 21
            | 22
            | 54
            | 1222
            | 1244
            | 1337
            | 1068
            | 30023
            | 30402
            | 30617
            | 30818
            | 31922
            | 31923
            | 9802
            | 9041
    )
}

/// The row body: a rich card for registry kinds, the normal text pipeline
/// otherwise (cards.rs `card_for`'s `_ => note_card`).
pub fn row_body(ev: &Event, ctx: &Body, feed: &Entity<Feed>, st: &CardState) -> Div {
    // NIP-36: a content warning never renders raw — the row collapses to
    // a quiet bar (the reason when the author gave one) until revealed.
    // The veil wraps EVERY kind: a warned picture leaks nothing either.
    if let Some(reason) = fields::content_warning(ev)
        && !st.revealed.contains(&ev.id)
    {
        return veil_card(ev, &reason, feed);
    }
    match u16::from(ev.kind) {
        21 | 22 => video_card(ev, ctx, feed),
        54 => podcast_card(ev, ctx, feed),
        1222 | 1244 => voice_card(ev, ctx, feed),
        1337 => code_card(ev),
        1068 => poll_card(ev, ctx, feed, st),
        30023 => article_card(ev, ctx, feed, st),
        30402 => classified_card(ev, ctx, feed, st),
        30617 => repo_card(ev, ctx, feed),
        30818 => wiki_card(ev, ctx, feed),
        31922 | 31923 => calendar_card(ev, ctx, feed, st),
        9802 => highlight_card(ev, ctx, feed, st),
        9041 => goal_card(ev, ctx, feed, st),
        _ => {
            // Unknown-but-advertised (NIP-31): an `alt` tag describes the
            // event in its author's words — render that, muted, no
            // masquerading as a supported card.
            if u16::from(ev.kind) != 1 {
                if let Some(alt) = fields::alt_description(ev) {
                    return alt_card(ev, &alt);
                }
                // The unknown-kind policy (#52, the generic card): nothing
                // arrives invisible. A kind Satori never heard of still
                // announces itself, muted, under the actor header (the
                // actor and the time ride it). The fetch lists stay
                // curated — this catches the kinds that ride unfiltered
                // paths (the bell's #p filter, store accumulation).
                if !is_feed_kind(ev.kind) {
                    return generic_card(ev);
                }
            }
            // NIP-14/NIP-24: a kind-1 subject line rides above the body,
            // semibold ink — the row's headline when the author gave one.
            let mut d = div().w_full().flex().flex_col().gap_1();
            if let Some(sub) = fields::subject_title(ev) {
                d = d.child(serif_title(&sub, 15.0));
            }
            d.child(content::body(ev, ctx, feed.clone()))
        }
    }
}

/// NIP-36's veil: the warning stands in for the content — a bordered,
/// quiet bar naming the reason (or the generic line when the author
/// sent none) and a Show seal. The reveal is per-feed state
/// (`CardState.revealed`); the inner click stops propagation so the
/// row's open-thread handler doesn't fire underneath it.
fn veil_card(ev: &Event, reason: &str, feed: &Entity<Feed>) -> Div {
    let label = if reason.is_empty() {
        "Sensitive content".to_string()
    } else {
        format!("Sensitive: {reason}")
    };
    let id = ev.id;
    let feed = feed.clone();
    div().w_full().flex().flex_col().gap_1().child(
        div()
            .w_full()
            .border_1()
            .border_color(rgb(theme::hairline()))
            .rounded(px(theme::R_EMBED))
            .px_3()
            .py_2()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_2()
            .child(
                div()
                    .text_size(px(theme::T_SMALL))
                    .text_color(rgb(theme::muted()))
                    .child(label),
            )
            .child(
                div()
                    .id(SharedString::from(format!("veil-{}", id)))
                    .cursor_pointer()
                    .px_2()
                    .py_0p5()
                    .rounded_full()
                    .border_1()
                    .border_color(rgb(theme::hairline()))
                    .text_size(px(theme::T_SMALL))
                    .text_color(rgb(theme::muted()))
                    .hover(|s| s.text_color(rgb(theme::ink())))
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        feed.update(cx, |f, cx| {
                            f.reveal(id);
                            cx.notify();
                        });
                    })
                    .child("Show"),
            ),
    )
}

/// NIP-31's fallback: the `alt` tag's one-line description, muted —
/// "what this is" from the author's own mouth, never a fake card.
fn alt_card(ev: &Event, alt: &str) -> Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(kicker(&format!("Kind {}", u16::from(ev.kind))))
        .child(
            div()
                .w_full()
                .text_size(px(theme::T_BODY))
                .text_color(rgb(theme::muted()))
                .child(alt.to_string()),
        )
}

/// NIP-75's zap goal (kind 9041): the author's content states the ask, a
/// hairline track fills toward the `amount` target (vermilion — the
/// seal-moment accent), and a closed goal says so (receipts still sum:
/// late zaps count, the ask just stops being open).
fn goal_card(ev: &Event, ctx: &Body, feed: &Entity<Feed>, st: &CardState) -> Div {
    let raised_msat = st.goal_msat.get(&ev.id).copied().unwrap_or(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (target_msat, closed) = match goal::parse(ev) {
        Some(d) => (d.amount_msat, d.closed_at.is_some_and(|end| end < now)),
        None => (0, false),
    };
    let pct = if target_msat > 0 {
        raised_msat.min(target_msat) as f32 / target_msat as f32
    } else {
        0.0
    };
    let meta = if closed {
        format!(
            "raised {} of {} sats · closed",
            raised_msat / 1000,
            target_msat / 1000
        )
    } else {
        format!(
            "raised {} of {} sats",
            raised_msat / 1000,
            target_msat / 1000
        )
    };
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .child(kicker("Zap goal"))
        .child(content::body(ev, ctx, feed.clone()))
        .child(
            div()
                .relative()
                .w_full()
                .h(px(6.))
                .rounded_full()
                .bg(rgb(theme::washi()))
                .overflow_hidden()
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left_0()
                        .w(relative(pct))
                        .bg(rgb(theme::vermilion())),
                ),
        )
        .child(meta_line(meta))
}

// --- shared bits ---------------------------------------------------------

/// The small tagline above a card ("↗ Article" — cards.rs kicker).
fn kicker(text: &str) -> Div {
    div()
        .text_size(px(theme::T_SMALL))
        .text_color(rgb(theme::muted()))
        .child(text.to_string())
}

fn serif_title(text: &str, size: f32) -> Div {
    div()
        .w_full()
        .font_family(theme::SERIF)
        .text_size(px(size))
        .text_color(rgb(theme::ink()))
        .child(text.to_string())
}

fn meta_line(text: impl Into<String>) -> Div {
    div()
        .w_full()
        .text_size(px(theme::T_SMALL))
        .text_color(rgb(theme::muted()))
        .child(text.into())
}

fn badge(text: &str, color: u32) -> Div {
    div()
        .px_2()
        .py_0p5()
        .rounded_full()
        .border_1()
        .border_color(rgb(color))
        .text_size(px(theme::T_SMALL))
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
            theme::vermilion()
        } else {
            theme::hairline()
        }))
        .text_size(px(theme::T_SMALL))
        .text_color(rgb(if engaged { theme::vermilion() } else { color }))
        .child(label.to_string())
}

fn fallback(ev: &Event, ctx: &Body, feed: &Entity<Feed>) -> Div {
    content::body(ev, ctx, feed.clone())
}

// --- video (NIP-71 kinds 21/22) ------------------------------------------

fn video_card(ev: &Event, ctx: &Body, feed: &Entity<Feed>) -> Div {
    // The body already carries the media section for video URLs (poster →
    // the Phase 3 inline player on tap); the card adds the kicker and the
    // serif title.
    let title = ev.content.lines().next().unwrap_or("").trim().to_string();
    let mut card = div().w_full().flex().flex_col().gap_2();
    card = card.child(kicker("▶ Video"));
    if !title.is_empty() {
        card = card.child(serif_title(&title, 15.0));
    }
    card.child(content::body_with(ev, ctx, feed.clone(), !title.is_empty()))
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
        card = card.child(
            row.child(
                div()
                    .relative()
                    .h(px(24.))
                    .w_full()
                    .rounded(px(theme::R_EMBED))
                    .overflow_hidden()
                    .bg(rgb(theme::washi()))
                    .when(!ballot && total > 0 && pct > 0.0, |d| {
                        d.child(
                            div()
                                .absolute()
                                .top_0()
                                .bottom_0()
                                .left_0()
                                .w(relative(pct))
                                .bg(rgb(theme::hairline())),
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
                                        theme::vermilion()
                                    } else {
                                        theme::muted()
                                    }))
                                    .bg(rgb(if picked {
                                        theme::vermilion()
                                    } else {
                                        theme::washi()
                                    })),
                            )
                            .child(
                                div()
                                    .text_size(px(theme::T_UI))
                                    .text_color(rgb(theme::ink()))
                                    .child(label.clone()),
                            )
                            .when(!ballot && total > 0, |d| {
                                d.child(
                                    div()
                                        .flex_1()
                                        .flex()
                                        .justify_end()
                                        .text_size(px(theme::T_SMALL))
                                        .text_color(rgb(theme::muted()))
                                        .child(format!("{count}")),
                                )
                            }),
                    ),
            ),
        );
    }

    if ballot && !selected.is_empty() {
        let ev_for_vote = ev.clone();
        let feed_for_vote = feed.clone();
        card = card.child(div().w_full().flex().justify_end().child(
            seal(format!("vote-{}", ev.id), "Vote", theme::vermilion(), false).on_click(
                move |_, _, cx| {
                    cx.stop_propagation();
                    let ev = ev_for_vote.clone();
                    feed_for_vote.update(cx, |f, cx| f.cast_vote(&ev, cx));
                },
            ),
        ));
    }
    if voted {
        card = card.child(meta_line("Your ballot is in, thanks for the ink."));
    } else if closed {
        card = card.child(meta_line("Poll closed."));
    }
    card
}

// --- article (NIP-23 kind 30023) ------------------------------------------

fn article_card(ev: &Event, ctx: &Body, feed: &Entity<Feed>, _st: &CardState) -> Div {
    // The memoized parse (content::article_data_cached): article::parse
    // clones the whole tag vec, and this ran per visible card per frame.
    let Some(data) = content::article_data_cached(ev) else {
        return fallback(ev, ctx, feed);
    };
    let mut card = div().w_full().flex().flex_col().gap_2();
    if let Some(img) = content::article_cover(ev) {
        let mut dims: HashMap<String, Option<(u32, u32)>> = HashMap::new();
        dims.insert(img.clone(), None);
        let asset = content::MediaAsset::Picture { url: img.clone() };
        if let Some(cover) = content::media_section(
            std::slice::from_ref(&asset),
            &dims,
            &HashMap::new(),
            ctx,
            feed,
        ) {
            card = card.child(cover);
        }
    }
    // No type kicker and no second timestamp: the byline already carries
    // the stamp, the serif title + the Read seal say "article" — "↗
    // Article" between them was noise (owner: "remove the arrow + article
    // thing").
    card = card.child(serif_title(&data.title, 17.0));
    // Summary, or the first ~140 chars of the body as the stand-in
    // (article.ts) — the shared rule (content::article_summary).
    let summary = content::article_summary(&data, &ev.content);
    card = card.child(
        div()
            .w_full()
            .text_size(px(theme::T_UI))
            .text_color(rgb(theme::muted()))
            .child(summary),
    );
    // The Read seal opens the reader page (markdown). Quiet on purpose —
    // a serif word, underlined, the only door out of the summary.
    let ev_for_read = ev.clone();
    let read = feed.downgrade();
    card = card.child(
        div()
            .id(SharedString::from(format!("read-{}", ev.id)))
            .cursor_pointer()
            .font_family(theme::SERIF)
            .text_size(px(theme::T_BODY))
            .text_color(rgb(theme::muted()))
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
                d.child(badge("Sold", theme::vermilion()))
            }),
    );
    if let Some(price) = &data.price {
        card = card.child(
            div()
                .font_family(theme::SERIF)
                .text_size(px(theme::T_READER))
                .text_color(rgb(theme::vermilion()))
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
        // The body memo's dims/alts (content::body_parse): this ran
        // imeta::parse twice per card per frame (once per map).
        let body = content::body_parse(ev);
        let assets: Vec<content::MediaAsset> = data
            .images
            .iter()
            .map(|u| content::MediaAsset::Picture { url: u.clone() })
            .collect();
        if let Some(gallery) = content::media_section(&assets, &body.dims, &body.alts, ctx, feed) {
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
                .text_size(px(theme::T_UI))
                .text_color(rgb(theme::muted()))
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
                        .text_size(px(theme::T_SMALL))
                        .text_color(rgb(theme::muted()))
                        .child(url.clone()),
                )
                .child(
                    seal(
                        format!("copy-{}-{url}", ev.id),
                        "Copy",
                        theme::muted(),
                        false,
                    )
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
                    }),
                ),
        );
    }
    for web in data.web.iter().take(2) {
        card = card.child(content::link_row(web));
    }
    if !data.maintainers.is_empty() {
        card = card.child(meta_line(format!(
            "{} maintainer(s)",
            data.maintainers.len()
        )));
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
                .text_size(px(theme::T_UI))
                .text_color(rgb(theme::muted()))
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
                    .text_size(px(theme::T_READER))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(theme::ink()))
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
                    .text_size(px(theme::T_BODY))
                    .line_height(relative(1.2))
                    .text_color(rgb(theme::ink()))
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
                .text_size(px(theme::T_UI))
                .text_color(rgb(theme::ink()))
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
                theme::muted(),
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

fn highlight_card(ev: &Event, ctx: &Body, feed: &Entity<Feed>, _st: &CardState) -> Div {
    let data = highlight::parse(ev);
    let mut card = div().w_full().flex().flex_col().gap_2();
    // NIP-84 `comment` (highlight.ts commentEl): the highlighter's own
    // words ARE the post; the passage beneath is the quote. An empty or
    // whitespace comment renders nothing.
    if let Some(comment) = data
        .comment
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
    {
        card = card.child(content::word_flow(comment, &[], ctx, feed));
    }
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
            .border_color(rgb(theme::vermilion()))
            .pl_3()
            .font_family(theme::SERIF)
            .text_size(px(theme::T_BODY))
            .text_color(rgb(theme::ink()))
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
                            .text_color(rgb(theme::vermilion()))
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
                        .text_size(px(theme::T_SMALL))
                        .text_color(rgb(theme::muted()))
                        .child(context.clone()),
                );
            }
        }
    }
    // The citation (highlight.ts): "by X, Y" from the role-filtered p
    // tags, then the source row — a web page opens the browser, an `e` ref
    // opens the thread in-app, anything else lands on njump.
    if !data.authors.is_empty() {
        let names: Vec<String> = data
            .authors
            .iter()
            .map(|pk| {
                ctx.profiles
                    .get(pk)
                    .map(|p| p.display())
                    .unwrap_or_else(|| short(pk))
            })
            .collect();
        card = card.child(meta_line(format!("by {}", names.join(", "))));
    }
    match &data.source {
        Some(highlight::Source::Url(url)) if url.starts_with("http") => {
            card = card.child(content::link_row(url));
        }
        Some(highlight::Source::Nostr(raw)) => {
            if let Ok(id) = EventId::from_hex(raw) {
                let feed = feed.downgrade();
                card = card.child(
                    div()
                        .id(SharedString::from(format!("hl-src-{}", ev.id)))
                        .cursor_pointer()
                        .text_size(px(theme::T_SMALL))
                        .text_color(rgb(theme::muted()))
                        .hover(|s| s.text_color(rgb(theme::vermilion())))
                        .on_click(move |_, _, cx| {
                            cx.stop_propagation();
                            let _ = feed.update(cx, |_, cx| cx.emit(Nav::OpenThread(id)));
                        })
                        .child("↗ quoted note"),
                );
            } else {
                card = card.child(content::link_row(&format!("https://njump.me/nostr:{raw}")));
            }
        }
        _ => {}
    }
    card
}

// --- voice (NIP-A0 kinds 1222/1244) ---------------------------------------

fn voice_card(ev: &Event, ctx: &Body, feed: &Entity<Feed>) -> Div {
    // The content IS the audio URL (NIP-A0 §Specification): the media
    // section renders the voice row (imeta waveform + duration, playbin
    // playback behind the M5 proxy); the body contributes no text — the
    // URL folds into the row. The kicker names the shape; a 1244 reply's
    // refs hang it in its thread.
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .child(kicker("♪ Voice"))
        .child(content::body(ev, ctx, feed.clone()))
}

// --- code snippet (NIP-C0 kind 1337) --------------------------------------

/// The NIP-C0 tags: `l` (language, lowercase), `name` (usually a
/// filename), `extension`, `description`, `runtime`, `license`. One value
/// each; a repeated tag means the author violated the NIP — first wins.
fn code_tags(ev: &Event) -> Vec<(String, String)> {
    let wanted = [
        "l",
        "name",
        "extension",
        "description",
        "runtime",
        "license",
    ];
    let mut out: Vec<(String, String)> = Vec::new();
    for key in wanted {
        for t in ev.tags.clone().to_vec() {
            let parts = t.as_slice();
            if parts.first().map(String::as_str) == Some(key)
                && let Some(v) = parts.get(1)
                && !v.is_empty()
            {
                out.push((key.to_string(), v.clone()));
                break;
            }
        }
    }
    out
}

fn code_card(ev: &Event) -> Div {
    let tags = code_tags(ev);
    let get = |k: &str| -> Option<String> {
        tags.iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.clone())
    };
    let name = get("name");
    let lang = get("l");
    // The header: the filename (or language) names the chip; the
    // description and runtime ride beneath, muted.
    let header = name
        .clone()
        .unwrap_or_else(|| lang.clone().unwrap_or_default());
    let mut card = div().w_full().flex().flex_col().gap_1();
    card = card.child(kicker("⌨ Code"));
    if !header.is_empty() {
        card = card.child(serif_title(&header, 15.0));
    }
    let meta = [get("description"), get("runtime"), get("license")]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
    if !meta.is_empty() {
        card = card.child(meta_line(meta));
    }
    // The snippet itself: mono on the panel wash, R_EMBED (the token's
    // budget names code chips). The render is CAPPED — the content is
    // attacker-controlled and the block must not eat a page.
    const CODE_RENDER_CAP: usize = 2000;
    let mut code = ev.content.clone();
    if code.len() > CODE_RENDER_CAP {
        code = code.chars().take(CODE_RENDER_CAP).collect();
        code.push_str("\n…");
    }
    let copy_text = ev.content.clone();
    let mut block = div()
        .id(SharedString::from(format!("copy-code-{}", ev.id)))
        .overflow_x_scroll()
        .w_full()
        .p_3()
        .rounded(px(theme::R_EMBED))
        .bg(rgb(theme::panel()))
        .border_1()
        .border_color(rgb(theme::hairline()))
        .font_family(theme::MONO)
        .text_size(px(theme::T_UI))
        .line_height(relative(1.35))
        .text_color(rgb(theme::ink()))
        .child(code);
    if !ev.content.is_empty() {
        // Click-to-copy — the snippet is the payload, the tap takes it
        // whole (sealed against the row's open-thread click).
        block = block.cursor_pointer().on_click(move |_, _, cx| {
            cx.stop_propagation();
            cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
        });
    }
    card = card.child(block);
    card
}

// --- unknown kinds (the #52 generic card) ---------------------------------

/// The generic card: a kind Satori has never heard of still announces
/// itself — one muted line, no masquerade. The actor header above it
/// carries the actor and the time.
fn generic_card(ev: &Event) -> Div {
    div()
        .w_full()
        .text_size(px(theme::T_SMALL))
        .text_color(rgb(theme::muted()))
        .child(format!("unrendered kind {}", u16::from(ev.kind)))
}

/// First 8 chars + ellipsis. Char cut, not byte cut: the highlight's
/// relay-controlled tag text (nip/highlight.rs p tags) once landed a byte
/// slice at 8 mid-character on multi-byte names — a panic that took the
/// whole window down on render.
fn short(key: &str) -> String {
    let cut = key.char_indices().nth(8).map_or(key.len(), |(i, _)| i);
    format!("{}…", &key[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixture gate (D59) for the code-snippet rider (#52): a NIP-C0
    /// kind-1337 event the way the NIP's example publishes it — content
    /// the code, `l`/`name`/`extension`/`description`/`runtime`/`license`
    /// tags, no d tag (not addressable). The tag grammar must surface
    /// each attribute once, first value wins on a repeated tag, and the
    /// kind is a feed kind (the store filters admit it, the card renders).
    #[test]
    fn code_snippet_fixture_parses_the_tag_grammar() {
        let keys = Keys::generate();
        let unsigned = UnsignedEvent::new(
            keys.public_key(),
            Timestamp::from_secs(1),
            Kind::Custom(1337),
            [
                Tag::custom("l", ["rust"]),
                Tag::custom("name", ["hello.rs"]),
                Tag::custom("extension", ["rs"]),
                Tag::custom("description", ["says hello"]),
                Tag::custom("runtime", ["rustc 1.90"]),
                Tag::custom("license", ["MIT"]),
                // A repeated tag: first value wins (the NIP's one-value
                // grammar; a second `l` is the author's error).
                Tag::custom("l", ["python"]),
            ],
            "fn main() {\n    println!(\"hello\");\n}",
        );
        let ev = keys.sign_event(unsigned).unwrap();

        assert!(is_feed_kind(ev.kind));
        let tags = code_tags(&ev);
        let get = |k: &str| {
            tags.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("l").as_deref(), Some("rust"), "first value wins");
        assert_eq!(get("name").as_deref(), Some("hello.rs"));
        assert_eq!(get("extension").as_deref(), Some("rs"));
        assert_eq!(get("license").as_deref(), Some("MIT"));
        assert_eq!(tags.len(), 6, "one entry per wanted tag");

        // Not addressable: no d tag, kind < 30000 — the comment target is
        // the event id (comments on snippets thread like notes).
        assert!(matches!(
            satori_core::nip::comment::CommentTarget::for_event(&ev),
            satori_core::nip::comment::CommentTarget::Event { kind: 1337, .. }
        ));
    }

    #[test]
    fn short_cuts_on_char_boundaries() {
        // The regression: byte 8 of a CJK author tag is mid-character.
        assert_eq!(
            super::short("作者某某作者某某作者某某"),
            "作者某某作者某某…"
        );
        assert_eq!(super::short("abcdefghijkl"), "abcdefgh…");
        assert_eq!(super::short("ab"), "ab…");
        assert_eq!(super::short(""), "…");
    }
}
