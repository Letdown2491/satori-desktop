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
    io::Cursor,
    sync::Arc,
};

use gpui::{div, img, prelude::*, px, rgb, AnyElement, Image, ImageFormat};
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

type Cb = Box<dyn FnOnce(Option<CachedImage>, &mut gpui::AsyncApp)>;

enum Entry {
    /// Fetch in flight; the joiners' callbacks fire on completion.
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

/// Decoded-entry caps: 256 images / 768MB of decoded RGBA.
const CACHE_MAX_ENTRIES: usize = 256;
const CACHE_MAX_BYTES: u64 = 768 * 1024 * 1024;

impl Cache {
    /// Oldest-first eviction past the caps. Loading entries rotate to the
    /// back (they're transient); Ready/NotMedia drop. Bounded passes.
    fn evict(&self) {
        for _ in 0..self.order.borrow().len() * 2 {
            let over =
                self.map.borrow().len() > CACHE_MAX_ENTRIES || self.bytes.get() > CACHE_MAX_BYTES;
            if !over {
                return;
            }
            let Some(url) = self.order.borrow_mut().pop_front() else {
                return;
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
            }
            self.epoch.set(self.epoch.get() + 1);
        }
    }
}

/// Encode core's decoded RGBA as PNG bytes for gpui's image pipeline,
/// keeping the decoded size for aspect-correct frames.
pub fn png_from_rgba(d: &DecodedImage) -> Option<CachedImage> {
    let img = image::RgbaImage::from_raw(d.width, d.height, d.rgba.clone())?;
    let mut out = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
        .ok()?;
    Some(CachedImage {
        png: Arc::new(Image::from_bytes(ImageFormat::Png, out)),
        size: (d.width, d.height),
    })
}

/// Fetch one image through core and hand the PNG to every interested view.
/// Joins an in-flight fetch for the same URL instead of re-requesting; a URL
/// that fails to decode is remembered as not-media and reported as `None`.
/// `apply` merges the result into the view and notifies.
pub fn fetch<V: 'static>(
    core: &Arc<Core>,
    url: String,
    cx: &mut gpui::Context<V>,
    apply: impl Fn(&mut V, String, Option<CachedImage>, &mut gpui::Context<V>) + 'static,
) {
    let handle = cx.entity();
    let apply = Arc::new(apply);
    let deliver = {
        let handle = handle.clone();
        let apply = apply.clone();
        let url2 = url.clone();
        move |res: Option<CachedImage>, app: &mut gpui::AsyncApp| {
            let _ = handle.update(app, |view, cx| apply(view, url2.clone(), res, cx));
        }
    };

    enum Action {
        Start,
        Joined,
        Now(Option<CachedImage>),
    }
    let action = {
        let cache = cx.global_mut::<Cache>();
        let mut map = cache.map.borrow_mut();
        match map.entry(url.clone()) {
            std::collections::hash_map::Entry::Occupied(mut e) => match e.get_mut() {
                Entry::Ready(c) => Action::Now(Some(c.clone())),
                Entry::NotMedia => Action::Now(None),
                Entry::Loading(cbs) => {
                    cbs.push(Box::new(deliver));
                    Action::Joined
                }
            },
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(Entry::Loading(vec![Box::new(deliver)]));
                cache.order.borrow_mut().push_back(url.clone());
                Action::Start
            }
        }
    };

    match action {
        Action::Joined => {} // the completion callback delivers
        Action::Now(res) => {
            cx.spawn(async move |this, cx| {
                let _ = this.update(cx, |view, cx| apply(view, url, res, cx));
            })
            .detach();
        }
        Action::Start => {
            let rx = core.fetch_image(url.clone());
            cx.spawn(async move |_this, cx| {
                let decoded = { let mut rx = rx; rx.recv().await }.flatten();
                let png = decoded.as_ref().and_then(png_from_rgba);
                // Publish globally, then drain every joiner.
                let cbs = cx
                    .update(|app| {
                        let cache = app.global_mut::<Cache>();
                        let mut map = cache.map.borrow_mut();
                        let st = png.as_ref().map_or(Entry::NotMedia, |c| {
                            let (w, h) = c.size;
                            cache.bytes.set(cache.bytes.get() + w as u64 * h as u64 * 4);
                            Entry::Ready(c.clone())
                        });
                        cache.epoch.set(cache.epoch.get() + 1);
                        let cbs_out = match map.remove(&url) {
                            Some(Entry::Loading(cbs)) => {
                                map.insert(url.clone(), st);
                                cbs
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
                        cache.evict();
                        cbs_out
                    })
                    .unwrap_or_default();
                for cb in cbs {
                    cb(png.clone(), cx);
                }
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
        .bg(rgb(theme::PANEL))
        .border_1()
        .border_color(rgb(theme::HAIRLINE));
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
                    .text_color(rgb(theme::MUTED))
                    .child(initial),
            )
            .into_any_element()
        }
    }
}
