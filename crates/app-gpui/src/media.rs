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

/// Render-time view of the shared cache (D13 rule: snapshot, don't read
/// entities during render — globals are snapshotted the same way).
#[derive(Clone, Default)]
pub struct Snapshot {
    ready: HashMap<String, Png>,
    not_media: HashSet<String>,
}

impl Snapshot {
    pub fn new(cx: &gpui::App) -> Self {
        let cache = cx.global::<Cache>().0.borrow();
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
        self.ready.get(url).cloned()
    }

    pub fn not_media(&self, url: &str) -> bool {
        self.not_media.contains(url)
    }
}

type Cb = Box<dyn FnOnce(Option<Png>, &mut gpui::AsyncApp)>;

enum Entry {
    /// Fetch in flight; the joiners' callbacks fire on completion.
    Loading(Vec<Cb>),
    Ready(Png),
    NotMedia,
}

impl Entry {
    fn ready(&self) -> Option<Png> {
        match self {
            Entry::Ready(png) => Some(png.clone()),
            _ => None,
        }
    }
}

/// Shared decoded-media cache (media_cache.rs's mem layer).
#[derive(Default)]
pub struct Cache(std::cell::RefCell<HashMap<String, Entry>>);
impl gpui::Global for Cache {}

/// Encode core's decoded RGBA as PNG bytes for gpui's image pipeline.
pub fn png_from_rgba(d: &DecodedImage) -> Option<Png> {
    let img = image::RgbaImage::from_raw(d.width, d.height, d.rgba.clone())?;
    let mut out = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
        .ok()?;
    Some(Arc::new(Image::from_bytes(ImageFormat::Png, out)))
}

/// Fetch one image through core and hand the PNG to every interested view.
/// Joins an in-flight fetch for the same URL instead of re-requesting; a URL
/// that fails to decode is remembered as not-media and reported as `None`.
/// `apply` merges the result into the view and notifies.
pub fn fetch<V: 'static>(
    core: &Arc<Core>,
    url: String,
    cx: &mut gpui::Context<V>,
    apply: impl Fn(&mut V, String, Option<Png>, &mut gpui::Context<V>) + 'static,
) {
    let handle = cx.entity();
    let apply = Arc::new(apply);
    let deliver = {
        let handle = handle.clone();
        let apply = apply.clone();
        let url2 = url.clone();
        move |res: Option<Png>, app: &mut gpui::AsyncApp| {
            let _ = handle.update(app, |view, cx| apply(view, url2.clone(), res, cx));
        }
    };

    enum Action {
        Start,
        Joined,
        Now(Option<Png>),
    }
    let action = {
        let mut cache = cx.global_mut::<Cache>().0.borrow_mut();
        match cache.entry(url.clone()) {
            std::collections::hash_map::Entry::Occupied(mut e) => match e.get_mut() {
                Entry::Ready(png) => Action::Now(Some(png.clone())),
                Entry::NotMedia => Action::Now(None),
                Entry::Loading(cbs) => {
                    cbs.push(Box::new(deliver));
                    Action::Joined
                }
            },
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(Entry::Loading(vec![Box::new(deliver)]));
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
                        let mut cache = app.global_mut::<Cache>().0.borrow_mut();
                        let st = png.as_ref().map_or(Entry::NotMedia, |p| {
                            Entry::Ready(Png::clone(p))
                        });
                        match cache.remove(&url) {
                            Some(Entry::Loading(cbs)) => {
                                cache.insert(url.clone(), st);
                                cbs
                            }
                            other => {
                                // Already replaced (shouldn't happen); restore it.
                                if let Some(e) = other {
                                    cache.insert(url.clone(), e);
                                }
                                Vec::new()
                            }
                        }
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
