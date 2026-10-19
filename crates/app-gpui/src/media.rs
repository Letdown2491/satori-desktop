//! Image plumbing between core's guarded fetch and gpui's `img` element.
//!
//! TODO(port): crates/app/src/media_cache.rs + cards.rs media_picture — P2/P3.
//! core's `fetch_image` (SSRF-guarded, sha256-keyed disk cache) hands back
//! RGBA which we re-encode to PNG for gpui's image pipeline. A global cache
//! shares results across views (cards, lightbox, avatars all key on the same
//! URL) and remembers URLs that decoded to nothing (a blossom page URL) so
//! they degrade to links instead of refetching every render.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use gpui::{div, img, prelude::*, px, rgb, AnyElement, EntityId, Image, ImageFormat, ImageSource};
use satori_core::manager::Core;
use satori_core::media::DecodedImage;

use crate::theme;

pub type Png = Arc<Image>;

/// A cached image and its decoded pixel size — the size drives the frame's
/// aspect when imeta never said (media_tile used to guess one height for
/// everything, so portrait and landscape wore the same box).
#[derive(Clone)]
pub struct CachedImage {
    pub png: Png,
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
    NotMedia,
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
/// the cache evicts oldest-first past an entry and byte budget (the disk
/// cache underneath is the real reservoir — eviction only costs a re-decode
/// if the picture scrolls back). `epoch` bumps on every render-visible
/// change so views can cache their snapshots instead of re-reading the
/// whole cache per frame.
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
/// a hard-scrolled evening under ~400MB total.
const CACHE_MAX_ENTRIES: usize = 128;
const CACHE_MAX_BYTES: u64 = 96 * 1024 * 1024;

impl Cache {
    /// Oldest-first eviction past the caps. Loading entries rotate to the
    /// back (they're transient); Ready/NotMedia drop. Bounded passes.
    /// Takes cx: gpui's own asset cache NEVER evicts — fetch_asset inserts
    /// the decoded RenderImage and nothing removes it — so ours must evict
    /// gpui's decoded copy too, or every photo ever seen stays in RAM as
    /// RGBA for the whole session (measured: 400+MB anon after an evening
    /// of scrolling; the timeline leak was gpui's cache, not ours). The
    /// evicted handles come back to the caller — remove_asset needs the
    /// App, and the cache borrow must be dead first.
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
/// budgets decoded pixels, not file bytes.
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
        size: (d.width, d.height),
    })
}

/// Fetch one image through core into the shared cache, notifying `view`
/// when the fetch lands. Joins an in-flight fetch for the same URL (the
/// landing notifies every waiter); a URL that fails to decode is remembered
/// as not-media. Safe to call speculatively — including from render paths
/// (the lazy list's visible items): Ready/NotMedia URLs are silent no-ops
/// and only an actual landing notifies, so this can never loop a re-render.
/// An evicted URL re-enters through the disk cache — a cheap background
/// re-decode. (Eviction is the RAM budget working; THIS is what brings
/// evicted avatars and pictures back when they're seen again.)
pub fn fetch(core: &Arc<Core>, url: String, view: EntityId, cx: &mut gpui::App) {
    if url.is_empty() {
        return;
    }
    enum Action {
        Start,
        Joined,
        Ready,
    }
    let action = {
        let cache = cx.global_mut::<Cache>();
        let mut map = cache.map.borrow_mut();
        match map.entry(url.clone()) {
            std::collections::hash_map::Entry::Occupied(mut e) => match e.get_mut() {
                Entry::Ready(_) | Entry::NotMedia => Action::Ready,
                Entry::Loading(waiters) => {
                    waiters.push(view);
                    Action::Joined
                }
            },
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(Entry::Loading(vec![view]));
                cache.order.borrow_mut().push_back(url.clone());
                Action::Start
            }
        }
    };

    match action {
        Action::Joined | Action::Ready => {} // the landing notifies every waiter
        Action::Start => {
            let rx = core.fetch_image(url.clone());
            cx.spawn(async move |cx| {
                let decoded = { let mut rx = rx; rx.recv().await }.flatten();
                let cached = decoded.and_then(cached_from_decoded);
                // Publish globally, then wake every waiter.
                let _ = cx.update(|app| {
                    let cache = app.global_mut::<Cache>();
                    let mut map = cache.map.borrow_mut();
                    let st = cached.as_ref().map_or(Entry::NotMedia, |c| {
                        let (w, h) = c.size;
                        cache.bytes.set(cache.bytes.get() + w as u64 * h as u64 * 4);
                        Entry::Ready(c.clone())
                    });
                    cache.epoch.set(cache.epoch.get() + 1);
                    let waiters = match map.remove(&url) {
                        Some(Entry::Loading(waiters)) => {
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
                    for id in waiters {
                        app.notify(id);
                    }
                });
            })
            .detach();
        }
    }
}

/// A profile disc: the picture when cached, otherwise a quiet ink initial.
pub fn avatar(snap: &Snapshot, url: &str, name: &str, size_px: f32) -> AnyElement {
    let disc = div()
        .rounded_full()
        .overflow_hidden()
        .flex_none()
        .size(px(size_px))
        .bg(rgb(theme::panel()))
        .border_1()
        .border_color(rgb(theme::hairline()));
    match snap.png(url).filter(|_| !url.is_empty()) {
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
