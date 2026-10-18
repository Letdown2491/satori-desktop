//! Image plumbing between core's guarded fetch and gpui's `img` element.
//!
//! TODO(port): crates/app renders avatars/media straight from the GTK
//! texture pipeline; here core's `fetch_image` (SSRF-guarded, sha256-keyed
//! disk cache) hands back RGBA which we re-encode to PNG so gpui's image
//! pipeline can hold it. The `image` crate is already in the tree (core).

use std::{collections::HashMap, io::Cursor, sync::Arc};

use gpui::{div, img, prelude::*, px, rgb, AnyElement, Image, ImageFormat};
use satori_core::manager::Core;
use satori_core::media::DecodedImage;

use crate::theme;

pub type Png = Arc<Image>;

/// Encode core's decoded RGBA as PNG bytes for gpui's image pipeline.
pub fn png_from_rgba(d: &DecodedImage) -> Option<Png> {
    let img = image::RgbaImage::from_raw(d.width, d.height, d.rgba.clone())?;
    let mut out = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
        .ok()?;
    Some(Arc::new(Image::from_bytes(ImageFormat::Png, out)))
}

/// Fetch one image through core and hand the PNG back to the view.
/// `apply(url, png)` merges the result into the view's cache and notifies.
pub fn fetch_png<V: 'static>(
    core: &Arc<Core>,
    url: String,
    cx: &mut gpui::Context<V>,
    apply: impl Fn(&mut V, String, Option<Png>, &mut gpui::Context<V>) + 'static,
) {
    let rx = core.fetch_image(url.clone());
    cx.spawn(async move |this, cx| {
        let decoded = { let mut rx = rx; rx.recv().await }.flatten();
        let png = decoded.as_ref().and_then(png_from_rgba);
        let _ = this.update(cx, |view, cx| apply(view, url.clone(), png, cx));
    })
    .detach();
}

/// A profile disc: the picture when cached, otherwise a quiet ink initial.
pub fn avatar(
    images: &HashMap<String, Arc<Image>>,
    url: &str,
    name: &str,
    size_px: f32,
) -> AnyElement {
    let disc = div()
        .rounded_full()
        .overflow_hidden()
        .flex_none()
        .size(px(size_px))
        .bg(rgb(theme::PANEL))
        .border_1()
        .border_color(rgb(theme::HAIRLINE));
    match images.get(url).filter(|_| !url.is_empty()) {
        Some(png) => disc.child(img(png.clone()).size_full()).into_any_element(),
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
