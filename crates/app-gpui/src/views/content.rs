//! Note-body rendering: paragraphs, links, nostr mentions, media, quote embeds.
//!
//! TODO(port): crates/app/src/content.rs + note_card.rs embed_card — P2/P3.
//! Unlike the GTK markup pipeline (which styles runs inside one text block),
//! GPUI's div tree gets one element per whitespace token: plain words are
//! serif spans, links/mentions/refs become clickable elements and the
//! paragraph is a flex-wrap flow. Media (imeta + standalone links, blossom
//! URLs included — core sniffs bytes, not extensions) folds out into a
//! gallery section under the text; videos render as play-card links.

use std::{collections::HashMap, hash::{Hash, Hasher}};

use gpui::{div, img, prelude::*, px, rgb, AnyElement, Div, Entity, EventEmitter, FontWeight, ObjectFit, SharedString};
use nostr::prelude::*;
use satori_core::manager::Profile;
use satori_core::nip::imeta::{self, ImageMeta};

use crate::{media, theme};
use super::{feed::rel_time, nav::Nav};

/// Timeline column width (web app's centered column; media_picture sized
/// against 720px there — the desktop column is a touch narrower).
pub const COL_W: f32 = 640.0;

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

/// Render-time context threaded from each view: the shared media snapshot,
/// resolved quote embeds and known profiles (mention names).
pub struct Body<'a> {
    pub media: &'a media::Snapshot,
    pub quotes: &'a Quotes,
    pub profiles: &'a HashMap<String, Profile>,
}

/// Deterministic element id — RandomState would re-hash every render, and a
/// changing id makes gpui drop the element's click state between the mouse
/// down and up (the down handler refreshes the window), killing every click.
fn seg_id(prefix: &str, token: &str) -> SharedString {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    token.hash(&mut h);
    SharedString::from(format!("{prefix}-{}", h.finish()))
}

/// If `token` is exactly one https link / mention, decode it.
enum Seg {
    Link(String),
    User(PublicKey),
}

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

/// The URL's lowercase path extension (before ?/#), if the last path segment
/// has one. Blossom blobs (`…/b87…cf4` — no extension) return None.
fn path_ext(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next().unwrap_or("");
    let last = path.rsplit('/').next().unwrap_or("");
    let (stem, ext) = last.rsplit_once('.')?;
    (!stem.is_empty()).then(|| ext.to_ascii_lowercase())
}

/// Image URL by extension (content.rs is_image_url) — plus the byte-sniffing
/// fallback in core.fetch_image for extension-less blossom URLs.
fn is_image_url(url: &str) -> bool {
    matches!(path_ext(url).as_deref(), Some("png" | "jpg" | "jpeg" | "webp" | "gif"))
}

/// Video by extension — never fetched; rendered as a play-card link (GPUI
/// has no video pipeline; the GTK app shows these as plain links too).
fn is_video_url(url: &str) -> bool {
    matches!(path_ext(url).as_deref(), Some("mp4" | "webm" | "mov" | "mkv" | "m4v"))
}

/// Every media URL a note carries: imeta-tagged pictures plus standalone
/// links in the text (NIP-68 posts from clients that skip imeta). Standalone
/// links *without* a path extension are candidates too — blossom blobs are
/// content-addressed — and degrade to plain links when the fetch doesn't
/// decode (media.rs remembers the failure). Videos are excluded (play-cards).
pub fn media_urls(ev: &Event) -> Vec<String> {
    let mut out: Vec<String> = imeta::parse(ev)
        .into_iter()
        .map(|m| m.url)
        .filter(|u| !u.is_empty())
        .collect();
    for para in ev.content.split('\n') {
        if let Some(Seg::Link(url)) = standalone(para)
            && !is_video_url(&url)
            && (is_image_url(&url) || path_ext(&url).is_none())
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

/// Short display for a bech32 mention when the profile isn't known yet.
fn short_bech32(bech: &str) -> String {
    let bech = bech.strip_prefix("nostr:").unwrap_or(bech);
    format!("{}…", &bech[..bech.len().min(14)])
}

/// Link display text (content.rs display_url): schemeless host + 2 path
/// segments, truncated at 48 chars.
fn display_url(url: &str) -> String {
    let s = url.strip_prefix("https://").unwrap_or(url);
    let s = s.strip_prefix("http://").unwrap_or(s);
    let mut s = s.split('/').take(3).collect::<Vec<_>>().join("/");
    if s.len() > 48 {
        s.truncate(45);
        s.push('…');
    }
    s
}

// --- word-flow tokens ------------------------------------------------------

/// One whitespace token inside a paragraph.
enum Tok {
    /// npub/nprofile mention.
    Mention(PublicKey),
    /// note1/nevent1 reference, resolved to its event id.
    NoteRef(EventId),
    /// naddr — addressable-event reference, kept as bech32 (njump resolves).
    AddrRef(String),
    /// Plain https link.
    Url(String),
    Word(String),
}

/// Punctuation that commonly hugs a token ("(see nostr:nevent1…).") — split
/// off so the link stays clean and the punctuation stays text.
fn split_trailing(token: &str) -> (&str, &str) {
    let core = token.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}', '"', '”']);
    (&token[..core.len()], &token[core.len()..])
}

fn classify_token(core: &str) -> Tok {
    let bech = core.strip_prefix("nostr:").unwrap_or(core);
    if let Ok(n) = Nip19::from_bech32(bech) {
        return match n {
            Nip19::Pubkey(pk) => Tok::Mention(pk),
            Nip19::Profile(p) => Tok::Mention(p.public_key),
            Nip19::EventId(id) => Tok::NoteRef(id),
            Nip19::Event(e) => Tok::NoteRef(e.event_id),
            Nip19::Coordinate(_) => Tok::AddrRef(bech.to_string()),
            _ => Tok::Word(core.to_string()),
        };
    }
    if bech.starts_with("naddr1") {
        return Tok::AddrRef(bech.to_string());
    }
    if core.starts_with("https://") || core.starts_with("http://") {
        return Tok::Url(core.to_string());
    }
    Tok::Word(core.to_string())
}

/// Display text for an naddr (content.rs display_nostr_ref, kind-aware).
fn addr_label(bech: &str) -> String {
    let label = match Nip19::from_bech32(bech) {
        Ok(Nip19::Coordinate(c)) => match u16::from(c.kind) {
            30023 => "↗ article",
            31922 | 31923 => "↗ event",
            1068 => "↗ poll",
            _ => "↗ reference",
        },
        _ => "↗ reference",
    };
    label.to_string()
}

/// Serif text span (the word-flow baseline style).
fn word(text: String) -> Div {
    div()
        .font_family(theme::SERIF)
        .text_size(px(15.0))
        .text_color(rgb(theme::INK))
        .child(text)
}

/// One clickable word-flow token (link / mention / ref).
fn token_el<V: EventEmitter<Nav> + 'static>(
    tok: Tok,
    punct: &str,
    n: usize,
    ctx: &Body,
    handle: &Entity<V>,
) -> AnyElement {
    let base = |id: SharedString| {
        div()
            .id(id)
            .cursor_pointer()
            .font_family(theme::SERIF)
            .text_size(px(15.0))
            .text_color(rgb(theme::MUTED))
            .underline()
    };
    match tok {
        Tok::Word(w) => word(if punct.is_empty() { w } else { format!("{w}{punct}") }).into_any_element(),
        Tok::Url(url) => {
            let label = display_url(&url);
            let el = base(seg_id("url", &format!("{n}{url}")))
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    cx.open_url(&url);
                })
                .child(label);
            with_punct(el, punct)
        }
        Tok::Mention(pk) => {
            let key = pk.to_string();
            let label = ctx
                .profiles
                .get(&key)
                .map(|p| p.display())
                .unwrap_or_else(|| short_bech32(&key));
            let handle = handle.clone();
            let el = base(seg_id("mention", &format!("{n}{key}")))
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    handle.update(cx, |_, cx| cx.emit(Nav::OpenProfile(pk)));
                })
                .child(label);
            with_punct(el, punct)
        }
        Tok::NoteRef(id) => {
            let handle = handle.clone();
            let el = base(seg_id("ref", &format!("{n}{id}")))
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    handle.update(cx, |_, cx| cx.emit(Nav::OpenThread(id)));
                })
                .child("↗ note");
            with_punct(el, punct)
        }
        Tok::AddrRef(bech) => {
            let label = addr_label(&bech);
            let el = base(seg_id("addr", &format!("{n}{bech}")))
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    // Addressable events have no local reader yet; njump
                    // renders any naddr (P3: resolve via event_by_address).
                    cx.open_url(&format!("https://njump.me/{bech}"));
                })
                .child(label);
            with_punct(el, punct)
        }
    }
}

/// A link token plus its trailing punctuation, as one flow fragment.
fn with_punct(el: impl IntoElement, punct: &str) -> AnyElement {
    if punct.is_empty() {
        el.into_any_element()
    } else {
        div()
            .flex()
            .items_baseline()
            .child(el)
            .child(word(punct.to_string()))
            .into_any_element()
    }
}

/// A whole paragraph as a wrapping flow of word elements — links, mentions
/// and refs become clickable anywhere in the text, which the GTK markup
/// pipeline got from rich runs and GPUI's div tree gets from flex-wrap.
fn word_flow<V: EventEmitter<Nav> + 'static>(
    para: &str,
    ctx: &Body,
    handle: &Entity<V>,
) -> Div {
    let mut flow = div().w_full().flex().flex_wrap().gap_1();
    for (n, token) in para.split_whitespace().enumerate() {
        let (core, punct) = split_trailing(token);
        if core.is_empty() {
            continue;
        }
        flow = flow.child(token_el(classify_token(core), punct, n, ctx, handle));
    }
    flow
}

/// Render a note body: word-flow paragraphs, media gallery section, quote
/// embeds. Media fetches ride the shared cache (`media::Snapshot`); the view
/// queues them via `content::media_urls` + `media::fetch`.
pub fn body<V: EventEmitter<Nav> + 'static>(
    ev: &Event,
    ctx: &Body,
    handle: Entity<V>,
) -> Div {
    let mut body = div().w_full().flex().flex_col().gap_2();

    let metas: Vec<ImageMeta> = imeta::parse(ev);
    let dims: HashMap<String, Option<(u32, u32)>> = metas
        .iter()
        .map(|m| (m.url.clone(), m.dim))
        .collect();
    let murls = media_urls(ev);

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
        let el: AnyElement = match standalone(trimmed) {
            // A paragraph that is one link: media folds into the gallery
            // section below; a link the cache couldn't decode (a blossom
            // page, say) degrades back to a plain link row here.
            Some(Seg::Link(url)) => {
                if is_video_url(&url) {
                    video_card(&url)
                } else if ctx.media.not_media(&url) || !(is_image_url(&url) || path_ext(&url).is_none()) {
                    link_row(&url)
                } else {
                    continue;
                }
            }
            Some(Seg::User(pk)) => {
                let key = pk.to_string();
                let label = ctx
                    .profiles
                    .get(&key)
                    .map(|p| p.display())
                    .unwrap_or_else(|| short_bech32(&key));
                let seg_handle = handle.clone();
                div()
                    .id(seg_id("mention", trimmed))
                    .cursor_pointer()
                    .text_size(px(13.5))
                    .text_color(rgb(theme::MUTED))
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        seg_handle.update(cx, |_, cx| cx.emit(Nav::OpenProfile(pk)));
                    })
                    .child(label)
                    .into_any_element()
            }
            None => word_flow(para, ctx, &handle).into_any_element(),
        };
        body = body.child(el);
    }

    // Media section (content.ts): one image full-width by its aspect; several
    // as a horizontal strip; tiles open the lightbox.
    if !murls.is_empty()
        && let Some(section) = media_section(&murls, &dims, ctx, &handle)
    {
        body = body.child(section);
    }

    // Quote embeds — compact previews, click opens the thread (embed_card).
    for bech in note_refs(ev) {
        match ctx.quotes.get(&bech) {
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

/// Schemeless, truncated link row for URLs that aren't media.
fn link_row(url: &str) -> AnyElement {
    let target = url.to_string();
    div()
        .id(seg_id("link", url))
        .cursor_pointer()
        .text_size(px(13.5))
        .text_color(rgb(theme::MUTED))
        .underline()
        .on_click(move |_, _, cx| cx.open_url(&target))
        .child(display_url(url))
        .into_any_element()
}

/// Video play-card: GPUI has no video pipeline, so the link wears one —
/// glyph, display text, opens in the browser.
fn video_card(url: &str) -> AnyElement {
    let target = url.to_string();
    div()
        .id(seg_id("video", url))
        .cursor_pointer()
        .w_full()
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        .py_2()
        .rounded(px(4.))
        .border_1()
        .border_color(rgb(theme::HAIRLINE))
        .bg(rgb(theme::PANEL))
        .on_click(move |_, _, cx| cx.open_url(&target))
        .child(
            div()
                .text_size(px(14.0))
                .text_color(rgb(theme::VERMILION))
                .child("▶"),
        )
        .child(
            div()
                .text_size(px(13.5))
                .text_color(rgb(theme::MUTED))
                .underline()
                .child(display_url(url)),
        )
        .into_any_element()
}

/// The gallery section: single image sized by its dim aspect (media_picture:
/// height = width·h/w clamped to 560), several images as a horizontal strip
/// (cards.rs gallery), every tile opening the lightbox.
fn media_section<V: EventEmitter<Nav> + 'static>(
    urls: &[String],
    dims: &HashMap<String, Option<(u32, u32)>>,
    ctx: &Body,
    handle: &Entity<V>,
) -> Option<AnyElement> {
    let visible: Vec<(usize, &String)> = urls
        .iter()
        .enumerate()
        .filter(|(_, u)| !ctx.media.not_media(u))
        .collect();
    if visible.is_empty() {
        return None;
    }

    if visible.len() == 1 {
        let (i, url) = visible[0];
        let dim = dims.get(url).copied().flatten();
        // Height from the dim aspect at full card width (~COL_W minus card
        // padding); ObjectFit::Cover absorbs the difference on other widths.
        let height = match dim {
            Some((w, h)) if w > 0 => ((COL_W - 32.0) * h as f32 / w as f32).clamp(80.0, 560.0),
            _ => 360.0,
        };
        return Some(
            media_tile(url, None, height, urls.to_vec(), i, ctx, handle)
                .into_any_element(),
        );
    }

    let id = seg_id("strip", &urls.join("|"));
    let mut strip = div().id(id).w_full().overflow_x_scroll().flex().gap_2();
    for (i, url) in visible {
        strip = strip.child(media_tile(url, Some(560.0), 480.0, urls.to_vec(), i, ctx, handle));
    }
    Some(strip.into_any_element())
}

/// One gallery tile: the picture when the shared cache has it, a quiet
/// loading frame otherwise; click opens the lightbox at this index. `w`:
/// `None` fills the column, `Some(px)` fixes a strip-tile width.
fn media_tile<V: EventEmitter<Nav> + 'static>(
    url: &str,
    w: Option<f32>,
    h: f32,
    lightbox: Vec<String>,
    idx: usize,
    ctx: &Body,
    handle: &Entity<V>,
) -> AnyElement {
    let handle = handle.clone();
    let frame = div()
        .id(seg_id("media", url))
        .cursor_pointer()
        .flex_none()
        .overflow_hidden()
        .rounded(px(4.))
        .border_1()
        .border_color(rgb(theme::HAIRLINE))
        .bg(rgb(theme::PANEL))
        .map(|d| match w {
            Some(w) => d.w(px(w)),
            None => d.w_full(),
        })
        .h(px(h))
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            handle.update(cx, |_, cx| cx.emit(Nav::OpenLightbox(lightbox.clone(), idx)));
        });
    match ctx.media.png(url) {
        // Contain, not Cover: the whole picture shows (the quiet panel frame
        // letterboxes); full size is the lightbox's job.
        Some(png) => frame
            .child(img(png).w_full().h_full().object_fit(ObjectFit::Contain))
            .into_any_element(),
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
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            handle.update(cx, |_, cx| cx.emit(Nav::OpenThread(id)))
        })
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
