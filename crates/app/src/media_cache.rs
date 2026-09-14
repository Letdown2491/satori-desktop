//! App-side media cache: an in-memory LRU of decoded textures in front of
//! core's sha256 disk cache. Refeeds, rescrolls and profile revisits stop
//! touching disk entirely (avatar-cache.ts / feed-cache.ts semantics).

use std::collections::HashMap;

use gtk::gdk;
use std::sync::Mutex;

/// (url → texture), LRU-capped at 400 entries (~a long feed scroll).
struct TexCache {
    map: HashMap<String, gdk::MemoryTexture>,
    order: Vec<String>,
    cap: usize,
}

impl TexCache {
    fn new() -> Self {
        Self { map: HashMap::new(), order: Vec::with_capacity(256), cap: 400 }
    }

    fn get(&mut self, url: &str) -> Option<gdk::MemoryTexture> {
        if let Some(tex) = self.map.get(url) {
            // LRU touch: move to the back.
            self.order.retain(|u| u != url);
            self.order.push(url.to_string());
            Some(tex.clone())
        } else {
            None
        }
    }

    fn insert(&mut self, url: &str, tex: gdk::MemoryTexture) {
        if self.map.len() >= self.cap && !self.map.contains_key(url) {
            if let Some(oldest) = self.order.first().cloned() {
                self.map.remove(&oldest);
                self.order.remove(0);
            }
        }
        self.map.insert(url.to_string(), tex);
        self.order.push(url.to_string());
    }
}

thread_local! {
    static TEXTURES: Mutex<TexCache> = Mutex::new(TexCache::new());
}

pub fn mem_get(url: &str) -> Option<gdk::MemoryTexture> {
    TEXTURES.with(|c| c.lock().unwrap().get(url))
}

pub fn mem_insert(url: &str, tex: gdk::MemoryTexture) {
    TEXTURES.with(|c| c.lock().unwrap().insert(url, tex));
}

/// Build a texture from decoded RGBA and register it.
pub fn from_decoded(
    url: &str,
    decoded: &satori_core::media::DecodedImage,
) -> gdk::MemoryTexture {
    let bytes = gtk::glib::Bytes::from_owned(decoded.rgba.clone());
    let tex = gdk::MemoryTexture::new(
        decoded.width as i32,
        decoded.height as i32,
        gdk::MemoryFormat::R8g8b8a8,
        &bytes,
        decoded.width as usize * 4,
    );
    mem_insert(url, tex.clone());
    tex
}
