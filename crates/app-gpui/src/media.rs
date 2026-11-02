//! Image plumbing between core's guarded fetch and gpui's `img` element.
//!
//! TODO(port): crates/app/src/media_cache.rs + cards.rs media_picture — P2/P3.
//! core's `fetch_image` (SSRF-guarded, sha256-keyed disk cache) hands back
//! the original bytes (plus a ≤128px disc thumb) which we hand to gpui's
//! image pipeline. A global cache shares results across views (cards,
//! lightbox, avatars all key on the same URL):
//!   - Ready — painted.
//!   - NotMedia — the bytes arrived whole and are not an image (a blossom
//!     page URL); degrades to a link, never refetched.
//!   - Failed — a transient failure (weather, rate limit): cools off with a
//!     doubling backoff and tries again. Nothing media-shaped is ever
//!     written off because a CDN hiccuped.
//!
//! Verdicts ("the bytes decoded to nothing") are remembered so a page URL
//! degrades to a link instead of refetching every render.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use gpui::{div, img, prelude::*, px, rgb, AnyElement, EntityId, Image, ImageFormat, ImageSource};
use satori_core::manager::Core;
use satori_core::media::{DecodedImage, MediaError};

use crate::theme;

pub type Png = Arc<Image>;

/// Transient-failure cadence: the first failure cools off 2s (core's own
/// fetch already spread three backed-off attempts), doubling to a minute —
/// a dead CDN costs about one request a minute per URL, not a spin.
const BACKOFF_START: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// A cached image and its decoded pixel size — the size drives the frame's
/// aspect when imeta never said (media_tile used to guess one height for
/// everything, so portrait and landscape wore the same box). `thumb` is the
/// ≤128px re-encode for disc-sized renders: gpui decodes `png` at full
/// resolution on its asset thread, and an avatar never needed 48MB of
/// 4000×3000 pixels.
#[derive(Clone)]
pub struct CachedImage {
    pub png: Png,
    pub thumb: Option<Png>,
    pub size: (u32, u32),
}

/// Render-time view of the shared cache (D13 rule: snapshot, don't read
/// entities during render — globals are snapshotted the same way).
#[derive(Clone, Default)]
pub struct Snapshot {
    ready: HashMap<String, CachedImage>,
    not_media: HashSet<String>,
}

impl Snapshot {
    pub fn new(cx: &gpui::App) -> Self {
        let cache = cx.global::<Cache>().map.borrow();
        Self {
            ready: cache
                .iter()
                .filter_map(|(url, e)| Some((url.clone(), e.ready()?)))
                .collect(),
            not_media: cache
                .iter()
                .filter(|(_, e)| matches!(e, Entry::NotMedia))
                .map(|(url, _)| url.clone())
                .collect(),
        }
    }

    pub fn png(&self, url: &str) -> Option<Png> {
        self.ready.get(url).map(|c| c.png.clone())
    }

    /// Disc-sized render source: the ≤128px thumb when the original grew
    /// one, else the full image (small sources never needed a thumb — and
    /// nothing else was decoded).
    pub fn thumb(&self, url: &str) -> Option<Png> {
        self.ready
            .get(url)
            .map(|c| c.thumb.clone().unwrap_or_else(|| c.png.clone()))
    }

    /// Decoded (w, h) — None until the fetch lands.
    pub fn size(&self, url: &str) -> Option<(u32, u32)> {
        self.ready.get(url).map(|c| c.size)
    }

    pub fn not_media(&self, url: &str) -> bool {
        self.not_media.contains(url)
    }
}

/// The cache's render-visible epoch — views compare it to decide whether
/// their cached media snapshot is stale.
pub fn generation(cx: &gpui::App) -> u64 {
    cx.global::<Cache>().epoch.get()
}

type Cb = EntityId;

enum Entry {
    /// Fetch in flight; every waiting view id is notified on completion.
    Loading(Vec<Cb>),
    Ready(CachedImage),
    /// Verified not-an-image — the bytes arrived whole and did not decode.
    /// A verdict, not an error: never retried, never persisted.
    NotMedia,
    /// A transient failure (timeout, 429, 5xx — core retried already). The
    /// retry goes out after `backoff`, doubling per consecutive failure.
    Failed { at: Instant, backoff: Duration },
}

impl Entry {
    fn ready(&self) -> Option<CachedImage> {
        match self {
            Entry::Ready(png) => Some(png.clone()),
            _ => None,
        }
    }
}

/// Shared decoded-media cache (media_cache.rs's mem layer).
/// The decoded-image cache, bounded: every decoded RGBA stays in RAM until
/// evicted, and decoded pixels are enormous (a 4000x3000 photo is ~48MB), so
/// the cache evicts least-RECENTLY-used past an entry and byte budget (the
/// disk cache underneath is the real reservoir — eviction only costs a
/// re-decode if the picture scrolls back). `epoch` bumps on every
/// render-visible change so views can cache their snapshots instead of
/// re-reading the whole cache per frame.
pub struct Cache {
    map: std::cell::RefCell<HashMap<String, Entry>>,
    order: std::cell::RefCell<std::collections::VecDeque<String>>,
    bytes: std::cell::Cell<u64>,
    epoch: std::cell::Cell<u64>,
}
impl Default for Cache {
    fn default() -> Self {
        Self {
            map: std::cell::RefCell::new(HashMap::new()),
            order: std::cell::RefCell::new(std::collections::VecDeque::new()),
            bytes: std::cell::Cell::new(0),
            epoch: std::cell::Cell::new(0),
        }
    }
}
impl gpui::Global for Cache {}

/// Decoded-entry caps: 128 images / 96MB of decoded RGBA (the disk cache
/// is the reservoir; eviction only costs a re-decode on scroll-back).
/// 96MB decoded lands the session plateau ≈ baseline + cap + encoded
/// bytes — measured against the ~240MB gpui/tokio/LMDB floor, that keeps
/// a hard-scrolled evening under ~400MB total. (Avatars stop competing
/// for this budget at full-photo scale once their thumbs exist — a disc
/// thumb decodes at 128px, ~64KB.)
const CACHE_MAX_ENTRIES: usize = 128;
const CACHE_MAX_BYTES: u64 = 96 * 1024 * 1024;

impl Cache {
    /// Least-recently-used-first eviction past the caps. Loading entries
    /// rotate to the back (they're transient); Ready/NotMedia/Failed drop.
    /// Bounded passes. Takes cx: gpui's own asset cache NEVER evicts —
    /// fetch_asset inserts the decoded RenderImage and nothing removes it —
    /// so ours must evict gpui's decoded copy too, or every photo ever seen
    /// stays in RAM as RGBA for the whole session (measured: 400+MB anon
    /// after an evening of scrolling; the timeline leak was gpui's cache,
    /// not ours). The evicted handles come back to the caller — remove_asset
    /// needs the App, and the cache borrow must be dead first.
    fn evict(&self) -> Vec<Png> {
        // The round count borrows `order`; the temporary must be dropped
        // BEFORE the loop — a `for _ in 0..self.order.borrow().len()`
        // keeps the borrow alive across the whole loop and the
        // `borrow_mut()` inside panics (RefCell already borrowed). It
        // fired the first time the cache crossed its caps.
        let mut dropped: Vec<Png> = Vec::new();
        let rounds = self.order.borrow().len() * 2;
        for _ in 0..rounds {
            let over =
                self.map.borrow().len() > CACHE_MAX_ENTRIES || self.bytes.get() > CACHE_MAX_BYTES;
            if !over {
                break;
            }
            let Some(url) = self.order.borrow_mut().pop_front() else {
                break;
            };
            let loading = matches!(self.map.borrow().get(&url), Some(Entry::Loading(_)));
            if loading {
                self.order.borrow_mut().push_back(url);
                continue;
            }
            if let Some(Entry::Ready(c)) = self.map.borrow_mut().remove(&url) {
                let (w, h) = c.size;
                self.bytes
                    .set(self.bytes.get().saturating_sub(w as u64 * h as u64 * 4));
                dropped.push(c.png);
                if let Some(t) = c.thumb {
                    dropped.push(t);
                }
            }
            self.epoch.set(self.epoch.get() + 1);
        }
        dropped
    }
}

/// Build the cached entry from core's handoff. The ORIGINAL encoded bytes
/// go to gpui's pipeline as-is: gpui decodes on its own asset thread and
/// caches the texture by content hash. (The old path re-encoded everything
/// to PNG on the main thread — a 12MP photo spent hundreds of ms in deflate
/// there, ×40 per top-up page: the endless-scroll hang.) `size` stays the
/// original dimensions — the frame's aspect is the picture's, and eviction
/// budgets decoded pixels, not file bytes. The disc thumb rides beside it.
pub fn cached_from_decoded(d: DecodedImage) -> Option<CachedImage> {
    let format = match d.format {
        image::ImageFormat::Png => ImageFormat::Png,
        image::ImageFormat::Jpeg => ImageFormat::Jpeg,
        image::ImageFormat::WebP => ImageFormat::Webp,
        image::ImageFormat::Gif => ImageFormat::Gif,
        _ => return None,
    };
    Some(CachedImage {
        png: Arc::new(Image::from_bytes(format, d.bytes)),
        thumb: d
            .thumb
            .map(|t| Arc::new(Image::from_bytes(ImageFormat::Png, t))),
        size: (d.width, d.height),
    })
}

/// Fetch one image through core into the shared cache, notifying `view`
/// when the fetch lands. Joins an in-flight fetch for the same URL (the
/// landing notifies every waiter). Verdicts (NotMedia) and ready entries
/// are silent no-ops; a transient failure cools off — re-entered after the
/// backoff, once, by timer, and again by any later look. Safe to call
/// speculatively — including from render paths (the lazy list's visible
/// items): only an actual landing notifies, so this can never loop a
/// re-render.
/// An evicted URL re-enters through the disk cache — a cheap background
/// re-decode. (Eviction is the RAM budget working; THIS is what brings
/// evicted avatars and pictures back when they're seen again.)
pub fn fetch(core: &Arc<Core>, url: String, view: EntityId, cx: &mut gpui::App) {
    if url.is_empty() {
        return;
    }
    enum Action {
        /// Begin a fetch; `Duration` is the cool-off to record on failure.
        Start(Duration),
        Joined,
        Ready,
    }
    let action = {
        let cache = cx.global_mut::<Cache>();
        let mut map = cache.map.borrow_mut();
        match map.entry(url.clone()) {
            std::collections::hash_map::Entry::Occupied(mut e) => {
                // LRU touch: a fetch call for an entry we HAVE is a use —
                // visible cards re-request their media every frame, so an
                // on-screen image never sits at the evict-front. (`order`
                // was insertion-order: visible pictures could be evicted
                // while on screen — the flash-then-refetch flicker.)
                {
                    let mut order = cache.order.borrow_mut();
                    if order.back().map(|b| b != &url).unwrap_or(true) {
                        order.retain(|u| u != &url);
                        order.push_back(url.clone());
                    }
                }
                match e.get_mut() {
                    Entry::Ready(_) | Entry::NotMedia => Action::Ready,
                    Entry::Loading(waiters) => {
                        waiters.push(view);
                        Action::Joined
                    }
                    Entry::Failed { at, backoff } => {
                        if at.elapsed() >= *backoff {
                            let next = (*backoff * 2).min(BACKOFF_MAX);
                            e.insert(Entry::Loading(vec![view]));
                            Action::Start(next)
                        } else {
                            // Still cooling off: a no-op. The retry is
                            // scheduled ONCE — where the Failed entry was
                            // born (the landing, below) — never per peek:
                            // a frame storm over a cooling URL must not
                            // pile up timer tasks.
                            Action::Ready
                        }
                    }
                }
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(Entry::Loading(vec![view]));
                cache.order.borrow_mut().push_back(url.clone());
                Action::Start(BACKOFF_START)
            }
        }
    };

    match action {
        Action::Joined | Action::Ready => {} // the landing notifies every waiter
        Action::Start(next_backoff) => {
            let rx = core.fetch_image(url.clone());
            let core2 = core.clone();
            let view2 = view;
            cx.spawn(async move |cx| {
                let outcome =
                    { let mut rx = rx; rx.recv().await }.unwrap_or(Err(MediaError::Transient));
                let st = match outcome {
                    Ok(d) => match cached_from_decoded(d) {
                        Some(c) => Entry::Ready(c),
                        None => Entry::NotMedia,
                    },
                    Err(MediaError::NotMedia) => Entry::NotMedia,
                    Err(MediaError::Transient) => Entry::Failed {
                        at: Instant::now(),
                        backoff: next_backoff,
                    },
                };
                // Publish globally, then wake every waiter.
                let _ = cx.update(|app| {
                    let cache = app.global_mut::<Cache>();
                    let mut map = cache.map.borrow_mut();
                    cache.epoch.set(cache.epoch.get() + 1);
                    if let Entry::Ready(c) = &st {
                        let (w, h) = c.size;
                        cache.bytes.set(cache.bytes.get() + w as u64 * h as u64 * 4);
                    }
                    let mut retry = None;
                    let waiters = match map.remove(&url) {
                        Some(Entry::Loading(waiters)) => {
                            // A transient failure books exactly one retry —
                            // the cool-off timer (below, after the cache
                            // borrow dies) re-enters fetch when the backoff
                            // is spent. Booked ONCE, where the Failed entry
                            // is born: render-path peeks during the cool-off
                            // are no-ops, so a frame storm over a cooling
                            // URL never piles up timer tasks.
                            if let Entry::Failed { backoff, .. } = &st {
                                retry = Some(*backoff);
                            }
                            map.insert(url.clone(), st);
                            waiters
                        }
                        other => {
                            // Already replaced (shouldn't happen); restore it.
                            if let Some(e) = other {
                                map.insert(url.clone(), e);
                            }
                            Vec::new()
                        }
                    };
                    drop(map);
                    // Evict OUTSIDE the cache borrow; gpui's decoded
                    // copies go with the entries (the asset cache never
                    // evicts on its own — that was the RAM leak).
                    for png in cache.evict() {
                        ImageSource::Image(png).remove_asset(app);
                    }
                    if let Some(backoff) = retry {
                        let core = core2.clone();
                        let url = url.clone();
                        app.spawn(async move |app| {
                            app.background_executor().timer(backoff).await;
                            let _ = app.update(|app| fetch(&core, url, view2, app));
                        })
                        .detach();
                    }
                    for id in waiters {
                        app.notify(id);
                    }
                });
            })
            .detach();
        }
    }
}

/// A profile disc: the picture when cached (the ≤128px thumb when the
/// original was huge — full-res decodes for a 28px circle were the avatar
/// path's whole cost), otherwise a quiet ink initial.
pub fn avatar(snap: &Snapshot, url: &str, name: &str, size_px: f32) -> AnyElement {
    let disc = div()
        .rounded_full()
        .overflow_hidden()
        .flex_none()
        .size(px(size_px))
        .bg(rgb(theme::panel()))
        .border_1()
        .border_color(rgb(theme::hairline()));
    match snap.thumb(url).filter(|_| !url.is_empty()) {
        // The img paints its own quad — the disc's rounding doesn't clip it,
        // so the photo wears the same circle itself.
        Some(png) => disc.child(img(png).size_full().rounded_full()).into_any_element(),
        None => {
            let initial = name
                .chars()
                .next()
                .map(String::from)
                .unwrap_or_else(|| "·".into());
            disc.child(
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(size_px * 0.45))
                    .text_color(rgb(theme::muted()))
                    .child(initial),
            )
            .into_any_element()
        }
    }
}
