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

use gpui::{div, img, prelude::*, px, relative, rgb, AnimationExt, AnyElement, Div, Entity, EventEmitter, FontWeight, ObjectFit, SharedString};
use nostr::prelude::*;
use satori_core::manager::Profile;
use satori_core::nip::article;
use satori_core::nip::imeta;

use crate::{media, theme};
use super::nav::Nav;

/// Timeline column width. Deliberately 640 — the webapp's was 720
/// (styles.css:100 `#app { max-width: 720px }`); at 12–14px desktop type,
/// 640 is right. Recorded as a DIVERGENCE, not drift (0.2-design-spec
/// §1.2 / deficiency 13). Media_picture is sized against 720px there.
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

/// Video by extension — never fetched until asked; rendered as a play-card
/// (inline player since Phase 3 — video.rs).
pub(crate) fn is_video_url(url: &str) -> bool {
    matches!(path_ext(url).as_deref(), Some("mp4" | "webm" | "mov" | "mkv" | "m4v"))
}

/// One entry of the media section, in reading order: a picture (lightbox
/// tile) or a video (poster tile; Phase 3 — inline click-to-play, the
/// imeta mime tells an extension-less blossom URL from a picture).
#[derive(Clone)]
pub enum MediaAsset {
    Picture { url: String },
    Video { url: String, poster: Option<String>, blurhash: Option<String>, mime: Option<String> },
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
/// article.ts header image). Memoized with the body parse (article::parse
/// clones the whole tag vec — it ran per article card per frame).
pub fn article_cover(ev: &Event) -> Option<String> {
    body_parse_cached(ev).cover.clone()
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
            MediaAsset::Video {
                url: m.url,
                poster: m.poster,
                blurhash: m.blurhash,
                mime: m.mime.clone(),
            }
        } else {
            MediaAsset::Picture { url: m.url }
        });
    }
    for para in ev.content.split('\n') {
        if let Some(Seg::Link(url)) = standalone(para) {
            let asset = if let Some(thumb) = youtube_thumb(&url) {
                MediaAsset::Video {
                    url: url.clone(),
                    poster: Some(thumb),
                    blurhash: None,
                    mime: None,
                }
            } else if is_video_url(&url) {
                MediaAsset::Video { url: url.clone(), poster: None, blurhash: None, mime: None }
            } else if is_image_url(&url) || path_ext(&url).is_none() {
                MediaAsset::Picture { url: url.clone() }
            } else {
                continue;
            };
            if !out.iter().any(|a| a.url() == url) {
                out.push(asset);
            }
        }
    }
    out
}

/// Every url worth fetching for this event: the pictures plus, per video,
/// the first preview source that applies — the publisher's poster, else a
/// blurhash decode (virtual key, no network), else a first-frame sniff
/// (virtual key; the video FILE is never fetched, only a capped head).
/// Strict Tor keeps the sniff off entirely (the facade rule): the tile
/// stays bare. A cache may still hold a sniff from before the switch —
/// painting already-fetched bytes breaks no new ground.
pub fn media_urls(ev: &Event) -> Vec<String> {
    media_assets(ev)
        .into_iter()
        .flat_map(|a| match a {
            MediaAsset::Picture { url } => vec![url],
            MediaAsset::Video { url, poster, blurhash, mime } => match poster {
                Some(p) => vec![p],
                None => match blurhash {
                    Some(h) => vec![crate::poster::blurhash_key(&h)],
                    None => {
                        // inline_able already refuses the facade (strict
                        // Tor) and YouTube pages (the sniffer would fetch
                        // a web page for nothing) — no sniff for either.
                        if !crate::video::inline_able(&url, mime.as_deref()) {
                            vec![]
                        } else {
                            vec![crate::poster::poster_key(&url)]
                        }
                    }
                },
            },
        })
        .collect()
}

/// `media_urls`, memoized per event. The parse (imeta + the content's
/// line scan) is a pure function of the event, but every pane's render
/// swept it per visible row per frame. Bounded, oldest-dropped at 4096
/// events; an entry is a handful of url strings.
pub fn media_urls_cached(ev: &Event) -> std::sync::Arc<[String]> {
    const CAP: usize = 4096;
    struct UrlsMemo {
        map: HashMap<String, std::sync::Arc<[String]>>,
        order: std::collections::VecDeque<String>,
    }
    static MEMO: std::sync::OnceLock<std::sync::Mutex<UrlsMemo>> = std::sync::OnceLock::new();
    let memo = MEMO.get_or_init(|| {
        std::sync::Mutex::new(UrlsMemo {
            map: HashMap::new(),
            order: std::collections::VecDeque::new(),
        })
    });
    let strict = crate::prefs::privacy_mode() == satori_core::privacy::PrivacyMode::Strict;
    // The mode rides the key: a strict-Tor switch must not keep emitting
    // the pre-switch sniffs for memoized events (and back).
    let key = format!("{strict}#{}", ev.id);
    let mut memo = memo.lock().unwrap();
    if let Some(hit) = memo.map.get(&key) {
        return hit.clone();
    }
    let urls: std::sync::Arc<[String]> = media_urls(ev).into();
    if memo.map.len() >= CAP
        && let Some(oldest) = memo.order.pop_front()
    {
        memo.map.remove(&oldest);
    }
    memo.order.push_back(key.clone());
    memo.map.insert(key, urls.clone());
    urls
}

/// The thumbnail for a YouTube link (watch?v=, youtu.be/, shorts/) —
/// video.rs reuses the shape as the "is a YouTube page" test: GStreamer
/// can't stream watch pages, so they stay browser links.
/// mqdefault: always present, 16:9, no letterbox bars.
pub(crate) fn youtube_thumb(url: &str) -> Option<String> {
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
pub(crate) fn short_bech32(bech: &str) -> String {
    let bech = bech.strip_prefix("nostr:").unwrap_or(bech);
    format!("{}…", &bech[..bech.len().min(14)])
}

/// Link display text (content.rs display_url): schemeless host + 2 path
/// segments, truncated at 48 chars.
fn display_url(url: &str) -> String {
    let s = url.strip_prefix("https://").unwrap_or(url);
    let s = s.strip_prefix("http://").unwrap_or(s);
    let s = s.split('/').take(3).collect::<Vec<_>>().join("/");
    // Char-boundary-safe truncation: byte slicing panicked on multi-byte
    // URLs (String::truncate at a char edge), and a relay-controlled token
    // reaching 48 bytes is trivial to publish — the same regression class
    // as cards.rs short().
    if s.chars().count() > 48 {
        let t: String = s.chars().take(45).collect();
        format!("{t}…")
    } else {
        s
    }
}

/// The open-in-browser allow-list (audit security M2): feed-derived URLs
/// may only be http(s) — xdg-open dispatches any other scheme to whatever
/// desktop handler is registered. NIP-21/nostrconnect targets route
/// through uris.rs or the login flow, never through this.
pub(crate) fn web_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
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
    /// A #hashtag — opens the hashtag timeline (Phase 2 §4.2).
    Hashtag(String),
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
    // #tag — the note's t tags mirror these (Phase 2 §4.2). Alphanumerics
    // and underscore, at least one; `##` or a bare `#` stays a word.
    if let Some(tag) = core.strip_prefix('#').filter(|t| {
        !t.is_empty() && t.chars().all(|c| c.is_alphanumeric() || c == '_')
    }) {
        return Tok::Hashtag(tag.to_string());
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
        .text_size(px(theme::T_READER))
        // P052's own line box is huge (URW vertical metrics); pin the pitch
        // or every wrapped line reads double-spaced.
        .line_height(relative(1.2))
        .text_color(rgb(theme::ink()))
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
            .text_size(px(theme::T_READER))
            .line_height(relative(1.2))
            .text_color(rgb(theme::muted()))
            .underline()
            // One link system for every token: quiet muted ink at rest,
            // the seal color on hover — the same lift everywhere a click
            // would leave the note.
            .hover(|s| s.text_color(rgb(theme::vermilion())))
    };
    match tok {
        Tok::Word(w) => word(if punct.is_empty() { w } else { format!("{w}{punct}") }).into_any_element(),
        Tok::Url(url) => {
            let label = display_url(&url);
            let openable = web_url(&url);
            let el = base(seg_id("url", &format!("{n}{url}")))
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    if openable {
                        cx.open_url(&url);
                    }
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
        Tok::Hashtag(tag) => {
            let handle = handle.clone();
            let label = format!("#{tag}");
            let el = base(seg_id("tag", &format!("{n}{tag}")))
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    let tag = tag.clone();
                    handle.update(cx, |_, cx| cx.emit(Nav::OpenHashtag(tag)));
                })
                .child(label);
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
/// Also the profile page's about block (profile.rs `about_flow`).
pub(crate) fn word_flow<V: EventEmitter<Nav> + 'static>(
    para: &str,
    skip: &[String],
    ctx: &Body,
    handle: &Entity<V>,
) -> Div {
    let mut flow = div().w_full().flex().flex_wrap().gap_1();
    for (n, token) in para.split_whitespace().enumerate() {
        let (core, punct) = split_trailing(token);
        if core.is_empty() {
            continue;
        }
        let tok = classify_token(core);
        // NIP-92 (content.ts applyImetaMime parity): a media asset's URL is
        // painted by the media section — the text flow drops the duplicate
        // link.
        if let Tok::Url(u) = &tok
            && skip.contains(u)
        {
            continue;
        }
        flow = flow.child(token_el(tok, punct, n, ctx, handle));
    }
    flow
}

/// The RENDER path's per-event parse (audit perf H3): body() re-ran
/// imeta::parse (a full tag-vec clone), media_urls and media_assets per
/// visible card per frame — the fetch path's media_urls_cached existed,
/// but render never used it. One memo here, keyed by event id: the parse
/// is a pure function of the event (unlike the fetch memo, whose privacy
/// mode rides its key). Bounded, oldest-dropped at 4096 events; an entry
/// is a handful of url strings + the asset vec.
struct BodyParse {
    cover: Option<String>,
    dims: HashMap<String, Option<(u32, u32)>>,
    /// NIP-68 `alt` per url (WS4): the reader-side badge reads it here,
    /// the composer-side editor wrote it into the same tag.
    alts: HashMap<String, String>,
    murls: std::sync::Arc<[String]>,
    assets: Vec<MediaAsset>,
    media_links: Vec<String>,
}

fn body_parse_cached(ev: &Event) -> std::sync::Arc<BodyParse> {
    const CAP: usize = 4096;
    struct Memo {
        map: HashMap<String, std::sync::Arc<BodyParse>>,
        order: std::collections::VecDeque<String>,
    }
    static MEMO: std::sync::OnceLock<std::sync::Mutex<Memo>> = std::sync::OnceLock::new();
    let memo = MEMO.get_or_init(|| {
        std::sync::Mutex::new(Memo {
            map: HashMap::new(),
            order: std::collections::VecDeque::new(),
        })
    });
    let key = ev.id.to_string();
    let mut memo = memo.lock().unwrap();
    if let Some(hit) = memo.map.get(&key) {
        return hit.clone();
    }
    let assets = media_assets(ev);
    let media_links: Vec<String> = assets.iter().map(|a| a.url().to_string()).collect();
    let cover = match article::parse(ev).map(|d| d.image) {
        Some(Some(img)) if !img.is_empty() => Some(img),
        _ => assets.iter().find_map(|a| match a {
            MediaAsset::Picture { url } => Some(url.clone()),
            _ => None,
        }),
    };
    let dims: HashMap<String, Option<(u32, u32)>> = imeta::parse(ev)
        .iter()
        .map(|m| (m.url.clone(), m.dim))
        .collect();
    let alts: HashMap<String, String> = imeta::parse(ev)
        .iter()
        .filter_map(|m| m.alt.clone().map(|a| (m.url.clone(), a)))
        .collect();
    let parsed = std::sync::Arc::new(BodyParse {
        cover,
        dims,
        alts,
        murls: media_urls(ev).into(),
        assets,
        media_links,
    });
    if memo.map.len() >= CAP
        && let Some(oldest) = memo.order.pop_front()
    {
        memo.map.remove(&oldest);
    }
    memo.order.push_back(key.clone());
    memo.map.insert(key, parsed.clone());
    parsed
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

    let parsed = body_parse_cached(ev);
    let dims = &parsed.dims;
    let murls = &parsed.murls;
    // Computed once: the media section below renders these, and the text
    // flow skips their URLs (a media URL never renders twice).
    let assets = &parsed.assets;
    let media_links = &parsed.media_links;

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
                    .text_size(px(theme::T_BODY))
                    .text_color(rgb(theme::muted()))
                    .hover(|s| s.text_color(rgb(theme::vermilion())))
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        seg_handle.update(cx, |_, cx| cx.emit(Nav::OpenProfile(pk)));
                    })
                    .child(format!("@{label}"))
                    .into_any_element();
                run.push(el);
            }
            None => run.push(word_flow(para, media_links, ctx, &handle).into_any_element()),
        };
    }
    body = push_block(body, &mut run);

    // Media section (content.ts): pictures and videos in reading order — one
    // item full-width by its aspect, several as a horizontal strip; picture
    // tiles open the lightbox, video tiles open the browser.
    if !murls.is_empty()
        && let Some(section) = media_section(assets, dims, &parsed.alts, ctx, &handle)
    {
        body = body.child(section);
    }

    // Quote embeds — compact previews, click opens the thread (embed_card).
    for bech in note_refs(ev) {
        match ctx.quotes.get(&bech) {
            Some(Some(e)) => body = body.child(embed_card(&bech, e, handle.clone())),
            _ => body = body.child(
                div()
                    .text_size(px(theme::T_SMALL))
                    .text_color(rgb(theme::muted()))
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
    let openable = web_url(&target);
    div()
        .id(seg_id("link", url))
        .cursor_pointer()
        .text_size(px(theme::T_BODY))
        .text_color(rgb(theme::muted()))
        .underline()
        .hover(|s| s.text_color(rgb(theme::vermilion())))
        .on_click(move |_, _, cx| {
            if openable {
                cx.open_url(&target);
            }
        })
        .child(display_url(url))
        .into_any_element()
}

/// Video tile: a poster frame wearing a play mark when the cache has one
/// (NIP-92 `image`, YouTube thumbnail), a quiet play-row while it loads or
/// when there's nothing to show. Phase 3: when the inline player is live
/// for this URL the tile renders the player element and its control row
/// (⏸/▶, ✕, ↗ browser) instead; the tap opens the player when the URL is
/// inline-able (video.rs gates — strict Tor and YouTube stay browser
/// links), else the browser.
fn video_tile(
    url: &str,
    poster: Option<&str>,
    blurhash: Option<&str>,
    mime: Option<&str>,
    w: Option<f32>,
    h: f32,
    ctx: &Body,
) -> AnyElement {
    let url_for_open = url.to_string();
    let mime_for_open = mime.map(str::to_string);

    // The inline player is live for this URL: the element paints the frame
    // (the crate aspect-fits inside the box it's given), the row below is
    // its controls — all sealed against the tile's own click.
    if let Some(v) = crate::video::active(url) {
        let max_w = w.unwrap_or(COL_W - 32.0);
        let ar = v.aspect_ratio();
        let box_h = if ar > 0.0 { (max_w / ar).clamp(80.0, 560.0) } else { h };
        let paused_now = v.paused();
        let browser_target = url.to_string();
        return div()
            .id(seg_id("video-player", url))
            .flex_none()
            .overflow_hidden()
            .rounded(px(theme::R_EMBED))
            .border_1()
            .border_color(rgb(theme::hairline()))
            .bg(rgb(0x000000))
            .map(|d| match w {
                Some(w) => d.w(px(w)),
                None => d.w_full(),
            })
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .h(px(box_h))
                    .child(crate::video::element(&v, max_w, box_h)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_2()
                    .py_1()
                    .bg(rgb(theme::panel()))
                    .child(
                        div()
                            .id(seg_id("vp-toggle", url))
                            .cursor_pointer()
                            .text_size(px(theme::T_UI))
                            .text_color(rgb(theme::ink()))
                            .on_click(|_, _, cx| {
                                cx.stop_propagation();
                                crate::video::toggle(cx);
                            })
                            .child(if paused_now { "▶" } else { "⏸" }),
                    )
                    .child(
                        div()
                            .id(seg_id("vp-close", url))
                            .cursor_pointer()
                            .text_size(px(theme::T_UI))
                            .text_color(rgb(theme::muted()))
                            .hover(|s| s.text_color(rgb(theme::ink())))
                            .on_click(|_, _, cx| {
                                cx.stop_propagation();
                                crate::video::close(cx);
                            })
                            .child("✕"),
                    )
                    .child(
                        div()
                            .id(seg_id("vp-browser", url))
                            .cursor_pointer()
                            .text_size(px(theme::T_UI))
                            .text_color(rgb(theme::muted()))
                            .hover(|s| s.text_color(rgb(theme::ink())))
                            .on_click(move |_, _, cx| {
                                cx.stop_propagation();
                                if crate::views::content::web_url(&browser_target) {
                                    cx.open_url(&browser_target);
                                }
                            })
                            .child("↗"),
                    ),
            )
            .into_any_element();
    }

    // Opening: the pipeline is building (Video::new can take seconds). The
    // tile stays exactly what it was, but the play mark breathes — the tap
    // acknowledged without a spinner widget, honoring the motion pref like
    // the zap bolt does (sumi-e: one living mark, no chrome).
    let opening_now = crate::video::opening(url);
    let reduced = crate::desktop::reduced_motion();
    let play_mark = |size: f32| -> gpui::AnyElement {
        let mark = div()
            .text_size(px(size))
            .text_color(rgb(theme::vermilion()))
            .child("▶");
        if !opening_now {
            mark.into_any_element()
        } else if reduced {
            div().opacity(0.6).child(mark).into_any_element()
        } else {
            mark.with_animation(
                seg_id("video-opening", url),
                gpui::Animation::new(std::time::Duration::from_millis(1400)).repeat(),
                |el, delta| el.opacity(0.65 - 0.3 * (2.0 * delta - 1.0).abs()),
            )
            .into_any_element()
        }
    };

    // D38: the preview panel is a fixed 16:9 — posters mostly are, and a
    // uniform shape reads cleaner than every video its own. The player
    // above keeps the video's true aspect; this is just the poster box.
    let panel_w = w.unwrap_or(COL_W - 32.0);
    let panel_h = panel_w * 9.0 / 16.0;

    let frame = div()
        .id(seg_id("video-tile", url))
        .cursor_pointer()
        .flex_none()
        .overflow_hidden()
        .rounded(px(theme::R_EMBED))
        .border_1()
        .border_color(rgb(theme::hairline()))
        .bg(rgb(theme::panel()))
        .map(|d| match w {
            Some(w) => d.w(px(w)),
            None => d.w_full(),
        })
        .h(px(panel_h))
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            crate::video::open(url_for_open.clone(), mime_for_open.clone(), cx);
        });
    // The preview ladder (D32): the publisher's poster (now OUR uploads
    // embed one — D38's grab at attach time), a blurhash gradient, a
    // locally sniffed first frame — any hit paints under the centered play
    // mark; none means the quiet 16:9 panel, mark only (the filename row
    // is gone — nobody read it).
    let shown = poster
        .and_then(|p| ctx.media.png(p))
        .or_else(|| blurhash.and_then(|b| ctx.media.png(&crate::poster::blurhash_key(b))))
        .or_else(|| ctx.media.png(&crate::poster::poster_key(url)));
    match shown {
        Some(png) => frame
            .child(
                div()
                    .relative()
                    .size_full()
                    // Cover: the panel is the shape now — a non-16:9 poster
                    // crops (Contain's letterbox bars read as breakage).
                    .child(img(png).w_full().h_full().object_fit(ObjectFit::Cover))
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(30.0))
                            .child(play_mark(30.0)),
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
                    .child(play_mark(24.0)),
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
    alts: &HashMap<String, String>,
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
            // The imeta dim (describes the video) first, then the poster's
            // landed size, then the sniffed frame's (same key the tile
            // paints) — the provisional box wears otherwise.
            let aspect = dims
                .get(url)
                .copied()
                .flatten()
                .filter(|(w, _)| *w > 0)
                .or_else(|| poster.and_then(|p| ctx.media.size(p)))
                .or_else(|| ctx.media.size(&crate::poster::poster_key(url)));
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
                alts.get(url).map(|s| s.as_str()),
                ctx,
                handle,
            )
            .into_any_element(),
            MediaAsset::Video { url, poster, blurhash, mime } => video_tile(
                url,
                poster.as_deref(),
                blurhash.as_deref(),
                mime.as_deref(),
                None,
                height(url, poster.as_deref(), 360.0),
                ctx,
            )
            .into_any_element(),
        });
    }

    // Multi-image (NIP-68 multi-imeta): a carousel, not a strip — one
    // image at a time with ‹ › arrows and position dots (dark-wisp
    // MediaCarousel; the strip hid every picture after the first). The
    // selection is a STATE CHANGE, not an animation (§3) — reduced motion
    // is honored by construction, there is nothing to reduce. Keyboard:
    // a click/Enter on the image opens the lightbox, whose arrow keys
    // walk the same set.
    let pictures_only = visible.iter().all(|a| matches!(a, MediaAsset::Picture { .. }));
    if pictures_only && visible.len() > 1 {
        let urls: Vec<String> = visible
            .iter()
            .filter_map(|a| match a {
                MediaAsset::Picture { url } => Some(url.clone()),
                _ => None,
            })
            .collect();
        return Some(carousel(&urls, alts, pictures, ctx, handle).into_any_element());
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
                    &url, Some(560.0), height, pictures.clone(), pic_idx, alts.get(&url).map(|s| s.as_str()), ctx, handle,
                ));
                pic_idx += 1;
            }
            MediaAsset::Video { url, poster, blurhash, mime } => {
                // The imeta dim describes the video itself; the poster
                // (when there is one) shares its aspect, as does the
                // sniffed frame.
                let aspect = dims
                    .get(&url)
                    .copied()
                    .flatten()
                    .filter(|d| d.0 > 0)
                    .or_else(|| poster.as_deref().and_then(|p| ctx.media.size(p)))
                    .or_else(|| ctx.media.size(&crate::poster::poster_key(&url)));
                let height = aspect.map_or(480.0, |(w, h)| {
                    if w > 0 {
                        (560.0 * h as f32 / w as f32).clamp(240.0, 480.0)
                    } else {
                        480.0
                    }
                });
                strip = strip.child(video_tile(
                    &url,
                    poster.as_deref(),
                    blurhash.as_deref(),
                    mime.as_deref(),
                    Some(560.0),
                    height,
                    ctx,
                ));
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
/// `None` fills the column, `Some(px)` fixes a strip-tile width. `alt`:
/// the NIP-68 imeta alt — an "ALT" badge rides the frame's corner
/// (dark-wisp AltTextBadge; quiet, on hairline, never a vermilion seal).
#[allow(clippy::too_many_arguments)]
fn media_tile<V: EventEmitter<Nav> + 'static>(
    url: &str,
    w: Option<f32>,
    h: f32,
    lightbox: Vec<String>,
    idx: usize,
    alt: Option<&str>,
    ctx: &Body,
    handle: &Entity<V>,
) -> AnyElement {
    let handle = handle.clone();
    let frame = div()
        .id(seg_id("media", url))
        .cursor_pointer()
        .flex_none()
        .overflow_hidden()
        .rounded(px(theme::R_EMBED))
        .border_1()
        .border_color(rgb(theme::hairline()))
        .bg(rgb(theme::panel()))
        .map(|d| match w {
            Some(w) => d.w(px(w)),
            None => d.w_full(),
        })
        .h(px(h))
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            handle.update(cx, |_, cx| cx.emit(Nav::OpenLightbox(lightbox.clone(), idx)));
        });
    let frame = match ctx.media.png(url) {
        // Contain, not Cover: the whole picture shows (the quiet panel frame
        // letterboxes); full size is the lightbox's job.
        Some(png) => frame
            .child(img(png).w_full().h_full().object_fit(ObjectFit::Contain)),
        None => frame.child(
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(theme::T_SMALL))
                .text_color(rgb(theme::muted()))
                .child("…"),
        ),
    };
    match alt {
        Some(_) => frame
            .child(
                div()
                    .absolute()
                    .left_2()
                    .bottom_2()
                    .px_1()
                    .rounded(px(theme::R_EMBED))
                    .bg(rgb(theme::washi()))
                    .border_1()
                    .border_color(rgb(theme::hairline()))
                    .text_size(px(theme::T_SMALL))
                    .text_color(rgb(theme::muted()))
                    .child("ALT"),
            )
            .into_any_element(),
        None => frame.into_any_element(),
    }
}

// --- the multi-image carousel (dark-wisp MediaCarousel) ---------------------

/// Per-carousel selection, keyed by the carousel's seg id — a cx-less
/// static map (the theme.rs/timefmt.rs split, media_urls_cached's bound):
/// the state IS the map. The wake-up rides the host entity's notify (the
/// handle media_section already carries), so no view needs to observe a
/// global and feed.rs stays untouched. Bounded, oldest-dropped.
fn sel_map() -> &'static std::sync::Mutex<HashMap<String, usize>> {
    static SEL: std::sync::OnceLock<std::sync::Mutex<HashMap<String, usize>>> =
        std::sync::OnceLock::new();
    SEL.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

fn carousel_sel(id: &str, n: usize) -> usize {
    let map = sel_map().lock().unwrap();
    map.get(id).copied().unwrap_or(0).min(n.saturating_sub(1))
}

fn carousel_step<V: 'static>(
    id: &str,
    n: usize,
    delta: i64,
    handle: &Entity<V>,
    cx: &mut gpui::App,
) {
    {
        let mut map = sel_map().lock().unwrap();
        if map.len() >= 4096 && !map.contains_key(id) {
            let oldest = map.keys().next().cloned();
            if let Some(k) = oldest {
                map.remove(&k);
            }
        }
        let cur = map.get(id).copied().unwrap_or(0).min(n.saturating_sub(1)) as i64;
        // Wrap-around: past the last image, the first — a carousel, not a list.
        let next = ((cur + delta).rem_euclid(n as i64)) as usize;
        map.insert(id.to_string(), next);
    }
    handle.update(cx, |_, cx| cx.notify());
}

/// Keyboard path for the carousel (the feed's ←/→, via the shell's key
/// handler → feed.rs carousel_nav): the cursor row's multi-image
/// selection steps exactly as its ‹ › arrows do. The seg id is
/// re-derived the way media_section paints the carousel (media_assets →
/// the not_media screen → the joined picture urls), so the keys hit the
/// live selection; a row without a carousel answers false and the shell
/// ignores the key. No wake-up here — the caller owns the entity and
/// notifies it (it IS the view being updated).
pub fn carousel_key_step(ev: &Event, media: &media::Snapshot, delta: i64) -> bool {
    let urls: Vec<String> = media_assets(ev)
        .iter()
        .filter_map(|a| match a {
            MediaAsset::Picture { url } if !media.not_media(url) => Some(url.clone()),
            _ => None,
        })
        .collect();
    if urls.len() < 2 {
        return false;
    }
    let n = urls.len();
    let key = seg_id("car", &urls.join("|"));
    let mut map = sel_map().lock().unwrap();
    let cur = map.get(key.as_ref()).copied().unwrap_or(0).min(n - 1) as i64;
    // Same wrap-around as carousel_step: past the last image, the first.
    let next = ((cur + delta).rem_euclid(n as i64)) as usize;
    map.insert(key.to_string(), next);
    true
}

/// The carousel frame: one image at a time (full column width, the
/// single-tile aspect math), ‹ › arrows, position dots. Pure state —
/// no slide, no fade (§3: a state appears; it does not travel).
fn carousel<V: EventEmitter<Nav> + 'static>(
    urls: &[String],
    alts: &HashMap<String, String>,
    lightbox: Vec<String>,
    ctx: &Body,
    handle: &Entity<V>,
) -> AnyElement {
    let n = urls.len();
    let key = seg_id("car", &urls.join("|"));
    let idx = carousel_sel(&key, n);
    let url = &urls[idx];
    let width = COL_W - 32.0;
    // The visible image keeps the single-tile aspect discipline.
    let aspect = dim_or(ctx.media, url, None);
    let height = aspect.map_or(480.0, |(w, h)| {
        if w > 0 {
            (width * h as f32 / w as f32).clamp(240.0, 480.0)
        } else {
            480.0
        }
    });
    let handle_cl = handle.clone();
    let mk_arrow = |which: &str, label: &'static str, delta: i64| {
        let key = key.clone();
        let handle = handle_cl.clone();
        div()
            .id(SharedString::from(format!("{key}-{which}")))
            .cursor_pointer()
            .px_2()
            .rounded(px(theme::R_EMBED))
            .border_1()
            .border_color(rgb(theme::hairline()))
            .text_size(px(theme::T_UI))
            .text_color(rgb(theme::muted()))
            .hover(|s| s.text_color(rgb(theme::ink())))
            .on_click(move |_, _, cx| carousel_step(&key, n, delta, &handle, cx))
            .child(label)
    };
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(media_tile(
            url,
            None,
            height,
            lightbox,
            idx,
            alts.get(url).map(|s| s.as_str()),
            ctx,
            handle,
        ))
        .child(
            div()
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .gap_2()
                .child(mk_arrow("prev", "‹", -1))
                .child(
                    div()
                        .flex()
                        .gap_1()
                        .items_center()
                        .children(urls.iter().enumerate().map(|(i, _)| {
                            let key = key.clone();
                            let handle = handle_cl.clone();
                            div()
                                .id(SharedString::from(format!("{key}-dot-{i}")))
                                .cursor_pointer()
                                .size(px(6.))
                                .rounded_full()
                                .bg(rgb(if i == idx {
                                    theme::ink()
                                } else {
                                    theme::hairline()
                                }))
                                .on_click(move |_, _, cx| {
                                    carousel_step(&key, n, i as i64 - idx as i64, &handle, cx)
                                })
                        })),
                )
                .child(mk_arrow("next", "›", 1)),
        )
        .into_any_element()
}

/// Compact embed card for quotes: left rule, name + time + clamped text
/// (embed_card; the GTK thumbnail is skipped — the text carries it in P2).
fn embed_card<V: EventEmitter<Nav> + 'static>(
    bech: &str,
    e: &Embed,
    handle: Entity<V>,
) -> AnyElement {
    let id = e.event.id;
    // The preview reads as text only — URLs never do (a raw blossom link
    // used to leak into the serif line as “https//…” noise; core's
    // plain_snippet drops them the way the whisper does).
    let text = satori_core::manager::plain_snippet(&e.event.content, 160);
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
        .border_color(rgb(theme::hairline()))
        .hover(|s| s.border_color(rgb(theme::vermilion())))
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
                        .text_size(px(theme::T_SMALL))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::ink()))
                        .child(e.name.clone()),
                )
                .child(
                    div()
                        .text_size(px(theme::T_SMALL))
                        .text_color(rgb(theme::muted()))
                        // Mode-aware without a cx (no bridge on embed
                        // paths): date mode shows dates, block keeps
                        // relative.
                        .child(crate::timefmt::stamp_text(
                            e.event.created_at.as_secs(),
                        )),
                ),
        )
        .child(
            div()
                .font_family(theme::SERIF)
                .text_size(px(theme::T_BODY))
                .text_color(rgb(theme::muted()))
                .child(text),
        )
        .into_any_element()
}

/// Compact note card shared by the notifications and search panes: avatar,
/// name, time, word-flow body; the author row opens the profile, the body
/// opens the thread (the GTK panes re-use the full note card — the desktop
/// keeps them lighter: no action glyphs on secondary surfaces).
#[allow(clippy::too_many_arguments)]
pub(crate) fn compact_card<V: EventEmitter<Nav> + 'static>(
    idx: usize,
    tag: &str,
    ev: &Event,
    profiles: &HashMap<String, Profile>,
    media: &media::Snapshot,
    quotes: &Quotes,
    time: String,
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
                        .text_size(px(theme::T_UI))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::ink()))
                        .on_click(move |_, _, cx| {
                            let ev = ev_for_profile.clone();
                            handle_profile.update(cx, |_, cx| cx.emit(Nav::OpenProfile(ev.pubkey)));
                        })
                        .child(name),
                )
                .child(
                    // The timestamp is the third mode toggle (timefmt.rs):
                    // tap cycles relative → date → block height.
                    div()
                        .id(SharedString::from(format!("{tag}-time-{idx}")))
                        .cursor_pointer()
                        .text_size(px(theme::T_SMALL))
                        .text_color(rgb(theme::muted()))
                        .on_click(|_, _, cx| crate::timefmt::cycle(cx))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// An extension-less blossom URL alone on a line classifies as a
    /// picture (the byte-sniff fallback path), so the gallery section
    /// claims it instead of the text flow.
    #[test]
    fn standalone_blossom_link_classifies_as_picture() {
        let keys = Keys::generate();
        let unsigned = UnsignedEvent::new(
            keys.public_key(),
            Timestamp::from_secs(1),
            Kind::TextNote,
            [],
            "nostr:npub1fa05cayzx9xze32d2nhxnp7vhf42mkvpf9zr8xa9keq35tlrps5q7k0z97\nhttps://logen.btcforplebs.com/584dbd0e64cf17d19e4838c10ec22482495e61b52705029a263b8f85168f9048",
        );
        let ev = keys.sign_event(unsigned).unwrap();
        let assets = media_assets(&ev);
        let urls: Vec<&str> = assets.iter().map(|a| a.url()).collect();
        assert_eq!(urls, vec!["https://logen.btcforplebs.com/584dbd0e64cf17d19e4838c10ec22482495e61b52705029a263b8f85168f9048"]);
        assert!(matches!(assets[0], MediaAsset::Picture { .. }));
    }
}
