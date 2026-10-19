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

use gpui::{div, img, prelude::*, px, relative, rgb, AnyElement, Div, Entity, EventEmitter, FontWeight, ObjectFit, SharedString};
use nostr::prelude::*;
use satori_core::manager::Profile;
use satori_core::nip::article;
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

/// One entry of the media section, in reading order: a picture (lightbox
/// tile) or a video (poster tile that opens the browser — GPUI has no video
/// pipeline, so nothing is fetched but the poster).
#[derive(Clone)]
pub enum MediaAsset {
    Picture { url: String },
    Video { url: String, poster: Option<String> },
}
impl MediaAsset {
    fn url(&self) -> &str {
        match self {
            MediaAsset::Picture { url } => url,
            MediaAsset::Video { url, .. } => url,
        }
    }
}

/// The note's media in reading order: imeta-tagged items (a `video/*` mime
/// or extension makes it a video; its NIP-92 `image` attr is the poster),
/// then standalone text links — blossom video URLs, YouTube links (thumbnail
/// derived from the id), pictures, and extension-less blossom candidates.
/// The article's cover, wherever the author put it: the NIP-23 `image` tag
/// first, then the first imeta/content picture (some clients skip the tag —
/// article.ts header image).
pub fn article_cover(ev: &Event) -> Option<String> {
    if let Some(d) = article::parse(ev)
        && let Some(img) = d.image
        && !img.is_empty()
    {
        return Some(img);
    }
    media_assets(ev).iter().find_map(|a| match a {
        MediaAsset::Picture { url } => Some(url.clone()),
        _ => None,
    })
}

pub fn media_assets(ev: &Event) -> Vec<MediaAsset> {
    let mut out: Vec<MediaAsset> = Vec::new();
    // NIP-23: an article's cover rides the `image` tag, not the content and
    // not imeta — without it no fetch is ever queued and the card's media
    // frame stays an empty panel (article.ts header image). First, so the
    // cover leads the reading order.
    if u16::from(ev.kind) == 30023
        && let Some(cover) = article::parse(ev).and_then(|d| d.image)
        && !cover.is_empty()
    {
        out.push(MediaAsset::Picture { url: cover });
    }
    for m in imeta::parse(ev) {
        if m.url.is_empty() {
            continue;
        }
        let video = m.mime.as_deref().is_some_and(|m| m.starts_with("video/"))
            || is_video_url(&m.url);
        out.push(if video {
            MediaAsset::Video { url: m.url, poster: m.poster }
        } else {
            MediaAsset::Picture { url: m.url }
        });
    }
    for para in ev.content.split('\n') {
        if let Some(Seg::Link(url)) = standalone(para) {
            let asset = if let Some(thumb) = youtube_thumb(&url) {
                MediaAsset::Video { url: url.clone(), poster: Some(thumb) }
            } else if is_video_url(&url) {
                MediaAsset::Video { url: url.clone(), poster: None }
            } else if is_image_url(&url) || path_ext(&url).is_none() {
                MediaAsset::Picture { url: url.clone() }
            } else {
                continue;
            };
            if !out.iter().any(|a| a.url() == &url) {
                out.push(asset);
            }
        }
    }
    out
}

/// Every url worth fetching for this event: the pictures plus video posters
/// (NIP-92 `image` attrs, YouTube thumbnails). Video files themselves are
/// never fetched — the tile opens the browser.
pub fn media_urls(ev: &Event) -> Vec<String> {
    media_assets(ev)
        .into_iter()
        .filter_map(|a| match a {
            MediaAsset::Picture { url } => Some(url),
            MediaAsset::Video { poster, .. } => poster,
        })
        .collect()
}

/// The thumbnail for a YouTube link (watch?v=, youtu.be/, shorts/) —
/// mqdefault: always present, 16:9, no letterbox bars.
fn youtube_thumb(url: &str) -> Option<String> {
    let id = if let Some(rest) = url
        .strip_prefix("https://youtu.be/")
        .or_else(|| url.strip_prefix("http://youtu.be/"))
    {
        rest.split(['?', '/', '&', '#']).next()?
    } else if let Some((_, qs)) = url.split_once("youtube.com/watch")
        && let Some((_, v)) = qs.split_once("v=")
    {
        v.split('&').next()?
    } else if let Some((_, rest)) = url.split_once("youtube.com/shorts/") {
        rest.split(['?', '/', '#']).next()?
    } else {
        return None;
    };
    let id: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(11)
        .collect();
    (id.len() == 11).then(|| format!("https://img.youtube.com/vi/{id}/mqdefault.jpg"))
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
pub(crate) enum Tok {
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
pub(crate) fn split_trailing(token: &str) -> (&str, &str) {
    let core = token.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}', '"', '”']);
    (&token[..core.len()], &token[core.len()..])
}

pub(crate) fn classify_token(core: &str) -> Tok {
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
        // P052's own line box is huge (URW vertical metrics); pin the pitch
        // or every wrapped line reads double-spaced.
        .line_height(relative(1.2))
        .text_color(rgb(theme::INK))
        .child(text)
}

/// One clickable word-flow token (link / mention / ref).
pub(crate) fn token_el<V: EventEmitter<Nav> + 'static>(
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
            .line_height(relative(1.2))
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
                .child(format!("@{label}"));
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

    // Paragraph rhythm follows the TS renderer's pre-wrap semantics
    // (TODO(port): src/render/content.ts — `.content` is pre-wrap): a single
    // newline breaks the line tight (no paragraph gap); only blank lines
    // separate paragraphs. Consecutive lines collect into one block.
    let mut run: Vec<AnyElement> = Vec::new();
    fn push_block(body: Div, run: &mut Vec<AnyElement>) -> Div {
        match run.len() {
            0 => body,
            1 => body.child(run.remove(0)),
            _ => body.child(div().w_full().flex().flex_col().children(run.drain(..))),
        }
    }
    for para in ev.content.split('\n') {
        let trimmed = para.trim();
        if trimmed.is_empty() {
            body = push_block(body, &mut run);
            continue;
        }
        // Quote refs fold out of the text into embed cards below (content.ts
        // collects refs and drops them from the markup).
        if standalone_ref(trimmed).is_some() {
            continue;
        }
        match standalone(trimmed) {
            // A paragraph that is one link: media folds into the gallery
            // section below; a link the cache couldn't decode (a blossom
            // page, say) degrades back to a plain link row here.
            Some(Seg::Link(url)) => {
                if is_video_url(&url) || youtube_thumb(&url).is_some() {
                    // The gallery section below renders the video tile
                    // (poster + play overlay, opens the browser).
                } else if ctx.media.not_media(&url) || !(is_image_url(&url) || path_ext(&url).is_none()) {
                    body = push_block(body, &mut run).child(link_row(&url));
                }
                // else: a media URL — the gallery section below renders it.
            }
            Some(Seg::User(pk)) => {
                let key = pk.to_string();
                let label = ctx
                    .profiles
                    .get(&key)
                    .map(|p| p.display())
                    .unwrap_or_else(|| short_bech32(&key));
                let seg_handle = handle.clone();
                let el = div()
                    .id(seg_id("mention", trimmed))
                    .cursor_pointer()
                    .text_size(px(13.5))
                    .text_color(rgb(theme::MUTED))
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        seg_handle.update(cx, |_, cx| cx.emit(Nav::OpenProfile(pk)));
                    })
                    .child(format!("@{label}"))
                    .into_any_element();
                run.push(el);
            }
            None => run.push(word_flow(para, ctx, &handle).into_any_element()),
        };
    }
    body = push_block(body, &mut run);

    // Media section (content.ts): pictures and videos in reading order — one
    // item full-width by its aspect, several as a horizontal strip; picture
    // tiles open the lightbox, video tiles open the browser.
    if !murls.is_empty()
        && let Some(section) = media_section(&media_assets(ev), &dims, ctx, &handle)
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

/// Pubkeys mentioned inline (nostr:npub / nprofile entities), so a pane can
/// hydrate their profiles before render — otherwise a mention degrades to
/// @npub1… (TODO(port): content.ts mentionPubkeys).
pub(crate) fn mention_pks(content: &str) -> Vec<PublicKey> {
    content
        .split_whitespace()
        .filter_map(|tok| match classify_token(split_trailing(tok).0) {
            Tok::Mention(pk) => Some(pk),
            _ => None,
        })
        .collect()
}

/// Schemeless, truncated link row for URLs that aren't media.
pub(crate) fn link_row(url: &str) -> AnyElement {
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

/// Video tile: the poster frame wearing a play mark when the cache has one
/// (NIP-92 `image`, YouTube thumbnail), a quiet play-row while it loads or
/// when there's nothing to show. Click opens the video in the browser —
/// GPUI has no video pipeline, so nothing auto-plays.
fn video_tile(url: &str, poster: Option<&str>, w: Option<f32>, h: f32, ctx: &Body) -> AnyElement {
    let target = url.to_string();
    let frame = div()
        .id(seg_id("video-tile", url))
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
            cx.open_url(&target);
        });
    match poster.and_then(|p| ctx.media.png(p)) {
        Some(png) => frame
            .child(
                div()
                    .relative()
                    .size_full()
                    .child(img(png).w_full().h_full().object_fit(ObjectFit::Contain))
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(30.0))
                            .text_color(rgb(theme::VERMILION))
                            .child("▶"),
                    ),
            )
            .into_any_element(),
        None => frame
            .child(
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(18.0))
                            .text_color(rgb(theme::VERMILION))
                            .child("▶"),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(rgb(theme::MUTED))
                            .underline()
                            .child(display_url(url)),
                    ),
            )
            .into_any_element(),
    }
}

/// The gallery section: single image sized by its dim aspect (media_picture:
/// height = width·h/w clamped to 560), several images as a horizontal strip
/// (cards.rs gallery), every tile opening the lightbox.
pub(crate) fn media_section<V: EventEmitter<Nav> + 'static>(
    assets: &[MediaAsset],
    dims: &HashMap<String, Option<(u32, u32)>>,
    ctx: &Body,
    handle: &Entity<V>,
) -> Option<AnyElement> {
    // Pictures vanish when their fetch failed (media.rs remembers); videos
    // never do — a failed poster degrades to the quiet play-row.
    let visible: Vec<MediaAsset> = assets
        .iter()
        .filter(|a| match a {
            MediaAsset::Picture { url } => !ctx.media.not_media(url),
            MediaAsset::Video { .. } => true,
        })
        .cloned()
        .collect();
    if visible.is_empty() {
        return None;
    }

    // The lightbox walks pictures only.
    let pictures: Vec<String> = visible
        .iter()
        .filter_map(|a| match a {
            MediaAsset::Picture { url } => Some(url.clone()),
            MediaAsset::Video { .. } => None,
        })
        .collect();

    if visible.len() == 1 {
        // Height from the item's true aspect at full card width (~COL_W
        // minus card padding): imeta dim when the author sent one, otherwise
        // the decoded size once the fetch lands. Only an item we've never
        // seen wears the provisional box (ObjectFit::Contain letterboxes it
        // until the aspect arrives with the bytes).
        let width = COL_W - 32.0;
        let height = |url: &str, poster: Option<&str>, provisional: f32| {
            let aspect =
                dim_or(ctx.media, poster.unwrap_or(url), dims.get(url).copied().flatten());
            aspect.map_or(provisional, |(w, h)| {
                if w > 0 {
                    (width * h as f32 / w as f32).clamp(80.0, 560.0)
                } else {
                    provisional
                }
            })
        };
        return Some(match &visible[0] {
            MediaAsset::Picture { url } => media_tile(
                url,
                None,
                height(url, None, 360.0),
                pictures,
                0,
                ctx,
                handle,
            )
            .into_any_element(),
            MediaAsset::Video { url, poster } => video_tile(
                url,
                poster.as_deref(),
                None,
                height(url, poster.as_deref(), 360.0),
                ctx,
            )
            .into_any_element(),
        });
    }

    let id = seg_id(
        "strip",
        &visible.iter().map(|a| a.url()).collect::<Vec<_>>().join("|"),
    );
    let mut strip = div().id(id).w_full().overflow_x_scroll().flex().gap_2();
    let mut pic_idx = 0;
    for asset in visible {
        match asset {
            MediaAsset::Picture { url } => {
                let aspect = dim_or(ctx.media, &url, dims.get(&url).copied().flatten());
                let height = aspect.map_or(480.0, |(w, h)| {
                    if w > 0 {
                        (560.0 * h as f32 / w as f32).clamp(240.0, 480.0)
                    } else {
                        480.0
                    }
                });
                strip = strip.child(media_tile(
                    &url, Some(560.0), height, pictures.clone(), pic_idx, ctx, handle,
                ));
                pic_idx += 1;
            }
            MediaAsset::Video { url, poster } => {
                // The imeta dim describes the video itself; the poster (when
                // there is one) shares its aspect.
                let aspect = dims
                    .get(&url)
                    .copied()
                    .flatten()
                    .filter(|d| d.0 > 0)
                    .or_else(|| poster.as_deref().and_then(|p| ctx.media.size(p)));
                let height = aspect.map_or(480.0, |(w, h)| {
                    if w > 0 {
                        (560.0 * h as f32 / w as f32).clamp(240.0, 480.0)
                    } else {
                        480.0
                    }
                });
                strip = strip.child(video_tile(&url, poster.as_deref(), Some(560.0), height, ctx));
            }
        }
    }
    Some(strip.into_any_element())
}

/// The frame aspect for a media url: the author's imeta dim first, the
/// decoded size once the bytes land, else nothing yet.
fn dim_or(
    media: &media::Snapshot,
    url: &str,
    dim: Option<(u32, u32)>,
) -> Option<(u32, u32)> {
    match dim {
        Some(d) if d.0 > 0 => Some(d),
        _ => media.size(url).filter(|d| d.0 > 0),
    }
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

/// Compact note card shared by the notifications and search panes: avatar,
/// name, time, word-flow body; the author row opens the profile, the body
/// opens the thread (the GTK panes re-use the full note card — the desktop
/// keeps them lighter: no action glyphs on secondary surfaces).
pub(crate) fn compact_card<V: EventEmitter<Nav> + 'static>(
    idx: usize,
    tag: &str,
    ev: &Event,
    action: Option<String>,
    profiles: &HashMap<String, Profile>,
    media: &media::Snapshot,
    quotes: &Quotes,
    handle: Entity<V>,
) -> AnyElement {
    let key = ev.pubkey.to_string();
    let name = profiles
        .get(&key)
        .map(|p| p.display())
        .unwrap_or_else(|| format!("{}…", &key[..8]));
    let picture = profiles
        .get(&key)
        .map(|p| p.picture.clone())
        .unwrap_or_default();
    let time = rel_time(ev.created_at.as_secs());
    let ev_for_open = ev.clone();
    let ev_for_profile = ev.clone();
    let handle_open = handle.clone();
    let handle_profile = handle.clone();
    let ctx = Body { media, quotes, profiles };

    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .px_4()
        .py_3()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(media::avatar(media, &picture, &name, 28.0))
                .child(
                    div()
                        .id(SharedString::from(format!("{tag}-author-{idx}")))
                        .cursor_pointer()
                        .flex_1()
                        .text_size(px(13.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::INK))
                        .on_click(move |_, _, cx| {
                            let ev = ev_for_profile.clone();
                            handle_profile.update(cx, |_, cx| cx.emit(Nav::OpenProfile(ev.pubkey)));
                        })
                        .child(name),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(rgb(theme::MUTED))
                        .children(action.map(|a| format!("· {a}")))
                        .child(time),
                ),
        )
        .child(
            div()
                .id(SharedString::from(format!("{tag}-open-{idx}")))
                .cursor_pointer()
                .on_click(move |_, _, cx| {
                    let ev = ev_for_open.clone();
                    handle_open.update(cx, |_, cx| {
                        // A NIP-22 comment notifies about someone else's
                        // note — the interesting thread is the parent's
                        // (comment.rs parse_refs: lowercase e = parent).
                        let target = if u16::from(ev.kind) == 1111 {
                            match satori_core::nip::comment::parse_refs(&ev).parent {
                                Some(satori_core::nip::comment::Ref::Event(id)) => id,
                                _ => ev.id,
                            }
                        } else {
                            ev.id
                        };
                        cx.emit(Nav::OpenThread(target));
                    });
                })
                .child(body(ev, &ctx, handle)),
        )
        .into_any_element()
}
