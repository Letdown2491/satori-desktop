//! Note-body rendering: paragraphs, links, nostr mentions, media, quote embeds.
//!
//! TODO(port): crates/app/src/content.rs + note_card.rs embed_card — P2. GPUI's
//! div API can't style spans inside a wrapping text block, so rich segments
//! render as their own blocks only when a paragraph *is* the segment (a bare
//! link line, a mention line). Mid-paragraph links and refs stay plain text
//! until P3.

use std::{collections::HashMap, hash::{Hash, Hasher}, sync::Arc};

use gpui::{div, img, prelude::*, px, rgb, AnyElement, Div, Entity, EventEmitter, FontWeight, Image, SharedString};
use nostr::prelude::*;
use satori_core::nip::imeta::{self, ImageMeta};

use crate::theme;
use super::{feed::rel_time, nav::Nav};

/// A resolved note reference, for quote embeds (note_card.rs embed_card).
#[derive(Clone)]
pub struct Embed {
    pub event: Event,
    pub name: String,
}

/// bech32 ref → resolved embed. `None` = fetch in flight. Views fill this
/// from `core.event_by_id_dech32`; unresolved keys render as a quiet
/// placeholder.
pub type Quotes = HashMap<String, Option<Embed>>;

/// One parsed piece of note content.
enum Seg {
    /// A bare https:// link.
    Link(String),
    /// An npub/nprofile mention.
    User(PublicKey),
}

fn seg_id(prefix: &str, token: &str) -> SharedString {
    let mut h = std::hash::BuildHasher::build_hasher(
        &std::collections::hash_map::RandomState::new(),
    );
    token.hash(&mut h);
    SharedString::from(format!("{prefix}-{}", h.finish()))
}

/// If `token` is exactly one link / mention / note reference, decode it.
fn standalone(token: &str) -> Option<Seg> {
    let t = token.trim();
    if let Some(rest) = t.strip_prefix("https://").or(t.strip_prefix("http://")) {
        // Single whitespace-free token = a link.
        return if rest.chars().any(char::is_whitespace) {
            None
        } else {
            Some(Seg::Link(t.to_string()))
        };
    }
    let bech = t.strip_prefix("nostr:").unwrap_or(t);
    if (bech.starts_with("npub1") || bech.starts_with("nprofile1"))
        && let Ok(pk) = PublicKey::from_bech32(bech)
    {
        return Some(Seg::User(pk));
    }
    None
}

/// If `token` is exactly one note/nevent reference, its bech32 (content.ts
/// quote refs — note1/nevent1, `nostr:` scheme or bare).
fn standalone_ref(token: &str) -> Option<String> {
    let t = token.trim();
    if t.chars().any(char::is_whitespace) {
        return None;
    }
    let bech = t.strip_prefix("nostr:").unwrap_or(t);
    (bech.starts_with("note1") || bech.starts_with("nevent1")).then(|| bech.to_string())
}

/// Image URL by extension (content.rs is_image_url) — no content-type
/// sniffing, same as the GTK app.
fn is_image_url(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or("");
    path.ends_with(".png")
        || path.ends_with(".jpg")
        || path.ends_with(".jpeg")
        || path.ends_with(".webp")
        || path.ends_with(".gif")
}

/// Every media URL a note carries: imeta-tagged pictures plus standalone
/// image links in the text (NIP-68 posts from clients that skip imeta).
pub fn media_urls(ev: &Event) -> Vec<String> {
    let mut out: Vec<String> = imeta::parse(ev)
        .into_iter()
        .map(|m| m.url)
        .filter(|u| !u.is_empty())
        .collect();
    for para in ev.content.split('\n') {
        if let Some(Seg::Link(url)) = standalone(para)
            && is_image_url(&url)
            && !out.contains(&url)
        {
            out.push(url);
        }
    }
    out
}

/// Standalone note/nevent references in reading order (quote embed
/// candidates; note_card.rs takes the first 3).
pub fn note_refs(ev: &Event) -> Vec<String> {
    ev.content
        .split('\n')
        .filter_map(standalone_ref)
        .collect::<Vec<_>>()
        .into_iter()
        .take(3)
        .collect()
}

/// Short display for a bech32 mention — the GTK app resolves names inline;
/// the GPUI card shows a truncated id and opens the profile/thread on click.
fn short_bech32(bech: &str) -> String {
    let bech = bech.strip_prefix("nostr:").unwrap_or(bech);
    format!("{}…", &bech[..bech.len().min(14)])
}

/// Render a note body: paragraphs as serif text blocks, standalone links and
/// mentions clickable, standalone image links and imeta tags as picture
/// cards, note refs as quote embeds.
pub fn body<V: EventEmitter<Nav> + 'static>(
    ev: &Event,
    images: &HashMap<String, Arc<Image>>,
    quotes: &Quotes,
    handle: Entity<V>,
) -> Div {
    let mut body = div().w_full().flex().flex_col().gap_2();

    for para in ev.content.split('\n') {
        let trimmed = para.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Quote refs fold out of the text into embed cards below (content.ts
        // collects refs and drops them from the markup).
        if standalone_ref(trimmed).is_some() {
            continue;
        }
        let seg_handle = handle.clone();
        let el: AnyElement = match standalone(trimmed) {
            Some(Seg::Link(url)) => {
                if is_image_url(&url) {
                    picture_card(&url, None, images)
                } else {
                    let shown = url.clone();
                    div()
                        .id(seg_id("link", &url))
                        .cursor_pointer()
                        .text_size(px(13.5))
                        .text_color(rgb(theme::MUTED))
                        .underline()
                        .on_click(move |_, _, cx| cx.open_url(&url))
                        .child(shown)
                        .into_any_element()
                }
            }
            Some(Seg::User(pk)) => div()
                .id(seg_id("mention", trimmed))
                .cursor_pointer()
                .text_size(px(13.5))
                .text_color(rgb(theme::MUTED))
                .on_click(move |_, _, cx| {
                    seg_handle.update(cx, |_, cx| cx.emit(Nav::OpenProfile(pk)));
                })
                .child(short_bech32(trimmed))
                .into_any_element(),
            None => div()
                .font_family(theme::SERIF)
                .text_size(px(15.0))
                .text_color(rgb(theme::INK))
                .child(para.to_string())
                .into_any_element(),
        };
        body = body.child(el);
    }

    // Attached pictures (NIP-92 imeta). One-shot large images; avatars keep
    // core's disk cache in media.rs.
    let metas: Vec<ImageMeta> = imeta::parse(ev);
    if !metas.is_empty() {
        let mut row = div().w_full().flex().flex_wrap().gap_2();
        for m in metas {
            if m.url.is_empty() {
                continue;
            }
            row = row.child(picture_card(&m.url, m.dim, images));
        }
        body = body.child(row);
    }

    // Quote embeds — compact previews, click opens the thread (embed_card).
    for bech in note_refs(ev) {
        match quotes.get(&bech) {
            Some(Some(e)) => body = body.child(embed_card(&bech, e, handle.clone())),
            _ => body = body.child(
                div()
                    .text_size(px(11.5))
                    .text_color(rgb(theme::MUTED))
                    .child("…"),
            ),
        }
    }

    body
}

/// Compact embed card for quotes: left rule, name + time + clamped text
/// (embed_card; the GTK thumbnail is skipped — the text carries it in P2).
fn embed_card<V: EventEmitter<Nav> + 'static>(
    bech: &str,
    e: &Embed,
    handle: Entity<V>,
) -> AnyElement {
    let id = e.event.id;
    let mut text = e.event.content.clone();
    if text.len() > 160 {
        let mut cut = 160;
        while !e.event.content.is_char_boundary(cut) {
            cut -= 1;
        }
        text = format!("{}…", &e.event.content[..cut]);
    }
    div()
        .id(seg_id("embed", bech))
        .cursor_pointer()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .pl_3()
        .py_1()
        .border_l_2()
        .border_color(rgb(theme::HAIRLINE))
        .on_click(move |_, _, cx| handle.update(cx, |_, cx| cx.emit(Nav::OpenThread(id))))
        .child(
            div()
                .flex()
                .items_baseline()
                .gap_2()
                .child(
                    div()
                        .text_size(px(12.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::INK))
                        .child(e.name.clone()),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(theme::MUTED))
                        .child(rel_time(e.event.created_at.as_secs())),
                ),
        )
        .child(
            div()
                .font_family(theme::SERIF)
                .text_size(px(13.5))
                .text_color(rgb(theme::MUTED))
                .child(text),
        )
        .into_any_element()
}

/// One picture, sized by `dim` when present (imeta or a plain image link).
fn picture_card(
    url: &str,
    dim: Option<(u32, u32)>,
    images: &HashMap<String, Arc<Image>>,
) -> AnyElement {
    let (w, h) = match dim {
        Some((dw, dh)) => (220.0_f32, ((dh as f32 / (dw as f32).max(1.)) * 220.0).min(240.)),
        None => (220.0, 160.0),
    };
    let frame = div()
        .overflow_hidden()
        .rounded(px(4.))
        .border_1()
        .border_color(rgb(theme::HAIRLINE))
        .bg(rgb(theme::PANEL))
        .w(px(w))
        .h(px(h));
    match images.get(url) {
        Some(png) => frame.child(img(png.clone()).size_full()).into_any_element(),
        None => frame
            .child(
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(11.))
                    .text_color(rgb(theme::MUTED))
                    .child("…"),
            )
            .into_any_element(),
    }
}
