//! Note-body rendering: paragraphs, links, nostr mentions, imeta images.
//!
//! TODO(port): crates/app/src/content.rs (Pango markup) — P2. GPUI's div API
//! can't style spans inside a wrapping text block, so rich segments render as
//! their own blocks only when a paragraph *is* the segment (a bare link line,
//! a mention line). Mid-paragraph links stay plain text until P3. Quoted
//! events (nevent embeds) also land with P3's quote cards.

use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    sync::Arc,
};

use gpui::{div, img, prelude::*, px, rgb, AnyElement, Div, Entity, EventEmitter, Image, SharedString};
use nostr::prelude::*;
use satori_core::nip::imeta::{self, ImageMeta};

use crate::theme;
use super::nav::Nav;

/// One parsed piece of note content.
enum Seg {
    /// A bare https:// link.
    Link(String),
    /// An npub/nprofile mention.
    User(PublicKey),
    /// A note/nevent reference.
    Note(EventId),
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
    if bech.starts_with("npub1") || bech.starts_with("nprofile1") {
        if let Ok(pk) = PublicKey::from_bech32(bech) {
            return Some(Seg::User(pk));
        }
    }
    if bech.starts_with("note1") || bech.starts_with("nevent1") {
        if let Ok(id) = EventId::from_bech32(bech) {
            return Some(Seg::Note(id));
        }
    }
    None
}

/// Short display for a bech32 mention — the GTK app resolves names inline;
/// the GPUI card shows a truncated id and opens the profile/thread on click.
fn short_bech32(bech: &str) -> String {
    let bech = bech.strip_prefix("nostr:").unwrap_or(bech);
    format!("{}…", &bech[..bech.len().min(14)])
}

/// Render a note body: paragraphs as serif text blocks, standalone links and
/// mentions clickable, imeta images as picture cards.
pub fn body<V: EventEmitter<Nav> + 'static>(
    ev: &Event,
    images: &HashMap<String, Arc<Image>>,
    handle: Entity<V>,
) -> Div {
    let metas = imeta::parse(ev);
    let mut body = div().w_full().flex().flex_col().gap_2();

    for para in ev.content.split('\n') {
        let trimmed = para.trim();
        if trimmed.is_empty() {
            continue;
        }
        let seg_handle = handle.clone();
        let el: AnyElement = match standalone(trimmed) {
            Some(Seg::Link(url)) => {
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
            Some(Seg::Note(id)) => div()
                .id(seg_id("ref", trimmed))
                .cursor_pointer()
                .text_size(px(13.5))
                .text_color(rgb(theme::MUTED))
                .on_click(move |_, _, cx| {
                    seg_handle.update(cx, |_, cx| cx.emit(Nav::OpenThread(id)));
                })
                .child(format!("↗ {}", short_bech32(trimmed)))
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

    // Attached pictures (NIP-92 imeta). Native gpui URL loading — these are
    // one-shot large images; avatars keep core's disk cache in media.rs.
    if !metas.is_empty() {
        let mut row = div().w_full().flex().flex_wrap().gap_2();
        for m in metas {
            if m.url.is_empty() {
                continue;
            }
            row = row.child(picture_card(&m, images));
        }
        body = body.child(row);
    }

    body
}

/// One imeta image, sized by its `dim` when present.
fn picture_card(m: &ImageMeta, images: &HashMap<String, Arc<Image>>) -> AnyElement {
    let dim = m.dim.unwrap_or((600, 400));
    let w = 220.0_f32;
    let h = (dim.1 as f32 / (dim.0 as f32).max(1.)) * w;
    let frame = div()
        .overflow_hidden()
        .rounded(px(4.))
        .border_1()
        .border_color(rgb(theme::HAIRLINE))
        .bg(rgb(theme::PANEL))
        .w(px(w))
        .h(px(h.min(240.)));
    match images.get(&m.url) {
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
