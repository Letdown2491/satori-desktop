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

use gpui::{AnyElement, EntityId, Image, ImageFormat, ImageSource, div, img, prelude::*, px, rgb};
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
    /// Render-time view of the shared cache, memoized on the cache itself
    /// keyed by the render-visible epoch: Snapshot::new is called by every
    /// pane's render — with the memo, a same-epoch call (every frame in
    /// between landings) is a refcount bump, not a full-map clone.
    pub fn new(cx: &gpui::App) -> std::sync::Arc<Self> {
        cx.global::<Cache>().snapshot()
    }

    /// The App-less core — tests build snapshots straight from a Cache.
    fn from_cache(cache: &Cache) -> Self {
        let map = cache.map.borrow();
        Self {
            ready: map
                .iter()
                .filter_map(|(url, e)| Some((url.clone(), e.ready()?)))
                .collect(),
            not_media: map
                .iter()
                .filter(|(_, e)| matches!(e, Entry::NotMedia))
                .map(|(url, _)| url.clone())
                .collect(),
        }
    }

    pub fn png(&self, url: &str) -> Option<Png> {
        self.ready.get(url).map(|c| c.png.clone())
    }

    /// Disc-sized render source: the thumb entry when one landed (the
    /// Arc avatars paint is STABLE across re-lands of the big original —
    /// its texture is never dropped by photo eviction), then the entry's
    /// own thumb field, then the full png (small originals ship no thumb —
    /// nothing else was decoded).
    pub fn thumb(&self, url: &str) -> Option<Png> {
        if let Some(c) = self.ready.get(&thumb_key(url)) {
            return Some(c.png.clone());
        }
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
    Failed {
        at: Instant,
        backoff: Duration,
    },
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
    /// Last fetch-or-landing per URL — the eviction grace window's clock
    /// (the deque is the recency ORDER; this is the recency TIME).
    touched: std::cell::RefCell<HashMap<String, Instant>>,
    /// URLs (and their disc-thumb keys) pinned against eviction: the
    /// always-on-screen images. The rail avatar's LRU touch only refreshes
    /// when the shell re-renders — never while the feed idles or scrolls —
    /// so its grace expires and photo pressure flashed it initial →
    /// photo → initial for the whole scroll. Pinned entries survive every
    /// sweep, desperate included; they are thumb-class by policy.
    pinned: std::cell::RefCell<std::collections::HashSet<String>>,
    bytes: std::cell::Cell<u64>,
    epoch: std::cell::Cell<u64>,
    /// The last snapshot handed out, keyed by the epoch it was built at:
    /// every pane's every frame wants a Snapshot, and a same-epoch build
    /// (the overwhelmingly common case) is this memo's refcount bump
    /// instead of a full-map clone.
    snap_memo: std::cell::RefCell<Option<(u64, std::sync::Arc<Snapshot>)>>,
}
impl Default for Cache {
    fn default() -> Self {
        Self {
            map: std::cell::RefCell::new(HashMap::new()),
            order: std::cell::RefCell::new(std::collections::VecDeque::new()),
            touched: std::cell::RefCell::new(HashMap::new()),
            pinned: std::cell::RefCell::new(std::collections::HashSet::new()),
            bytes: std::cell::Cell::new(0),
            epoch: std::cell::Cell::new(0),
            snap_memo: std::cell::RefCell::new(None),
        }
    }
}
impl gpui::Global for Cache {}

/// Decoded-entry caps, by size class. BIG (photos, articles' covers —
/// anything decoding over SMALL_DECODED_MAX) lives under the main budget:
/// 128 images / 96MB of decoded RGBA. SMALL (avatars, disc thumbs — 128px
/// is ~64KB) has its own budget (1024 / 64MB) so photo pressure can NEVER
/// evict them: an avatar is painted on many surfaces at once (the rail,
/// every visible row) and its re-decode flashes visibly; 64MB of thumbs
/// does not compete with the 96MB photo budget. (The disk cache is the
/// real reservoir; eviction only costs a re-decode on scroll-back.)
const BIG_MAX_ENTRIES: usize = 128;
const BIG_MAX_BYTES: u64 = 96 * 1024 * 1024;
/// A Ready entry decoding over this many bytes (w·h·4) is big class.
const SMALL_DECODED_MAX: u64 = 256 * 1024;
const SMALL_MAX_ENTRIES: usize = 1024;
const SMALL_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Eviction grace: entries fetched/touched within this window are
/// immune, even over budget. Eviction runs INSIDE a landing's update —
/// before the deferred notify can re-touch anything — so "oldest-touched"
/// during a landing burst (articles scroll, a page of avatars arriving)
/// is frequently an image CURRENTLY ON SCREEN. Evicting one calls
/// remove_asset on it: gpui drops the decoded texture and re-decodes
/// asynchronously — a guaranteed multi-frame blank. The flash-then-
/// refetch flicker. Grace makes visible images un-evictable; the
/// overshoot is bounded by the in-flight window and collected at the
/// next landing once the burst goes stale.
///
/// 120s, not 10s: an idle pane stops rendering, and only renders touch —
/// its on-screen images age past a short grace and become the permanent
/// victims of every landing (the search pane's People rows blinked on
/// each feed landing while the pane sat unrendered). Two quiet minutes
/// covers any idle a user sits in; the caps and the desperate valve
/// below still bound memory.
const EVICT_GRACE: Duration = Duration::from_secs(120);
/// Safety valve: past double budget the grace is ignored (a pathological
/// burst can hold everything fresh for a while; memory must stay bounded).
const EVICT_DESPERATE_BYTES: u64 = 2 * BIG_MAX_BYTES;

impl Cache {
    /// The memoized render snapshot: same epoch → refcount bump; a bumped
    /// epoch (a landing or eviction) rebuilds once, and every subsequent
    /// frame in that epoch reuses the build. (All render-visible map
    /// transitions bump the epoch — the landing for Ready/NotMedia/Failed,
    /// eviction per removal; Loading inserts and LRU touches don't change
    /// what a snapshot would return.)
    fn snapshot(self: &Cache) -> std::sync::Arc<Snapshot> {
        let hit = self
            .snap_memo
            .borrow()
            .as_ref()
            .filter(|(e, _)| *e == self.epoch.get())
            .map(|(_, s)| s.clone());
        if let Some(snap) = hit {
            return snap;
        }
        let snap = std::sync::Arc::new(Snapshot::from_cache(self));
        *self.snap_memo.borrow_mut() = Some((self.epoch.get(), snap.clone()));
        snap
    }

    /// Unpin urls (and their disc-thumb keys) — `unpin`'s App-less core.
    fn unpin_urls(&self, urls: &[String]) {
        let mut pinned = self.pinned.borrow_mut();
        for url in urls {
            pinned.remove(url.as_str());
            pinned.remove(&thumb_key(url));
        }
    }

    /// Least-recently-used-first eviction past the caps, per size class.
    /// Loading entries rotate to the back (they're transient);
    /// Ready/NotMedia/Failed drop. Bounded passes. Takes cx: gpui's own
    /// asset cache NEVER evicts — fetch_asset inserts the decoded
    /// RenderImage and nothing removes it — so ours must evict gpui's
    /// decoded copy too, or every photo ever seen stays in RAM as RGBA
    /// for the whole session (measured: 400+MB anon after an evening of
    /// scrolling; the timeline leak was gpui's cache, not ours). The
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
            let (big_over, small_over, total_bytes) = self.pressure();
            if !big_over && !small_over {
                break;
            }
            // Safety valve: memory stays bounded no matter how long a
            // burst keeps everything "fresh".
            let desperate = total_bytes > EVICT_DESPERATE_BYTES;
            // The victim: the LRU-front droppable entry of an over-budget
            // class. The deque is the global recency order, so the first
            // over-class entry IS that class's oldest; a fresh one means
            // the whole class is fresh (a landing burst between two
            // renders must not evict painted images — skip the class).
            // Desperation ignores the grace.
            let mut victim: Option<String> = None;
            {
                let order = self.order.borrow();
                for url in order.iter() {
                    if self.pinned.borrow().contains(url) {
                        // Pinned: on screen for the session, never a victim
                        // (the rail avatar's touch freezes while the shell
                        // idles — the grace window can't protect it).
                        continue;
                    }
                    let class = {
                        let map = self.map.borrow();
                        match map.get(url) {
                            Some(Entry::Loading(_)) => None, // transient: rotate past
                            Some(Entry::Ready(c)) => {
                                let (w, h) = c.size;
                                Some(w as u64 * h as u64 * 4 > SMALL_DECODED_MAX)
                            }
                            // Byte-free entries ride the small class.
                            Some(_) | None => Some(false),
                        }
                    };
                    let Some(big) = class else { continue };
                    let over = if big { big_over } else { small_over };
                    if !over {
                        continue; // this class has room — its entries stay
                    }
                    if !desperate
                        && self
                            .touched
                            .borrow()
                            .get(url)
                            .is_some_and(|t| t.elapsed() < EVICT_GRACE)
                    {
                        // Fresh over-class entry: skip it (landings freshen
                        // `touched` without rotating the deque, so keep
                        // scanning — another class may still have a stale
                        // candidate deeper).
                        continue;
                    }
                    victim = Some(url.clone());
                    break;
                }
            }
            let Some(url) = victim else {
                break;
            };
            // The victim may sit mid-deque (an over class's front behind
            // another class's fresher entries) — retain, not pop_front.
            self.order.borrow_mut().retain(|u| u != &url);
            if let Some(Entry::Ready(c)) = self.map.borrow_mut().remove(&url) {
                let (w, h) = c.size;
                self.bytes
                    .set(self.bytes.get().saturating_sub(w as u64 * h as u64 * 4));
                // ONLY the full png's texture goes: a thumb rides its own
                // small-class entry (thumb_key) — the avatars painting that
                // Arc must not flash when the big original evicts.
                dropped.push(c.png);
            }
            self.touched.borrow_mut().remove(&url);
            self.epoch.set(self.epoch.get() + 1);
        }
        dropped
    }

    /// Class pressures: (big over, small over, total Ready bytes). Big =
    /// decodes over SMALL_DECODED_MAX; small = avatars/thumbs and the
    /// byte-free verdict entries.
    fn pressure(&self) -> (bool, bool, u64) {
        let map = self.map.borrow();
        let (mut big_n, mut big_b, mut small_n, mut small_b) = (0usize, 0u64, 0usize, 0u64);
        for e in map.values() {
            match e {
                Entry::Ready(c) => {
                    let (w, h) = c.size;
                    let b = w as u64 * h as u64 * 4;
                    if b > SMALL_DECODED_MAX {
                        big_n += 1;
                        big_b += b;
                    } else {
                        small_n += 1;
                        small_b += b;
                    }
                }
                Entry::NotMedia | Entry::Failed { .. } => small_n += 1,
                Entry::Loading(_) => {}
            }
        }
        (
            big_n > BIG_MAX_ENTRIES || big_b > BIG_MAX_BYTES,
            small_n > SMALL_MAX_ENTRIES || small_b > SMALL_MAX_BYTES,
            big_b + small_b,
        )
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

/// The thumb entry's cache key — a NUL keeps it off real-URL space. Avatars
/// paint the ≤128px thumb (media::avatar), so the thumb lives as its OWN
/// small-class entry: photo pressure can never evict it, no matter how big
/// the original decodes (a 4000×3000 avatar original is a 48MB BIG entry;
/// the thumb the rail actually paints is 64KB and stays put).
fn thumb_key(url: &str) -> String {
    format!("{url}\u{0}thumb")
}

/// Core re-encodes thumbs at thumbnail(128, 128) — the byte math uses the
/// ceiling; nothing reads a thumb entry's aspect.
const THUMB_DIMS: (u32, u32) = (128, 128);

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
/// Pin an always-on-screen image (and its disc thumb) against eviction.
/// For the rail avatar: the shell only re-renders on events, so while you
/// idle or scroll the feed its LRU touch freezes, the 10s grace expires,
/// and timeline photo pressure evicted it — remove_asset flashing it to
/// the ink initial, the next sweep fetching it back, repeating. Pinned
/// urls skip every eviction sweep.
pub fn pin(url: &str, cx: &mut gpui::App) {
    if url.is_empty() {
        return;
    }
    let cache = cx.global_mut::<Cache>();
    let mut pinned = cache.pinned.borrow_mut();
    pinned.insert(url.to_string());
    pinned.insert(thumb_key(url));
}

/// Release pins (the inverse of `pin`): a surface handing its visible
/// rows to a new set unpins the old first, so the pinned set stays
/// bounded by what is actually on screen.
pub fn unpin(urls: &[String], cx: &mut gpui::App) {
    if urls.is_empty() {
        return;
    }
    cx.global_mut::<Cache>().unpin_urls(urls);
}

pub fn fetch(core: &Arc<Core>, url: String, view: EntityId, cx: &mut gpui::App) {
    if url.is_empty() {
        return;
    }
    // The key kinds: real media URLs, and the two virtual namespaces the
    // poster ladder synthesizes (D32) — a blurhash decode (no network)
    // and a sniffed poster frame (capped head download + local decode).
    // Everything else about a fetch — the entry bookkeeping, the backoff,
    // the landing, the waiters — is the same for all three.
    enum Kind {
        Plain,
        Blurhash(String),
        Poster(String),
    }
    let kind = if let Some(h) = url.strip_prefix("blurhash:") {
        Kind::Blurhash(h.to_string())
    } else if let Some(v) = url.strip_prefix("poster:") {
        Kind::Poster(v.to_string())
    } else {
        Kind::Plain
    };
    enum Action {
        /// Begin a fetch; `Duration` is the cool-off to record on failure.
        Start(Duration),
        Joined,
        Ready,
    }
    let action = {
        let cache = cx.global_mut::<Cache>();
        let mut map = cache.map.borrow_mut();
        let action = match map.entry(url.clone()) {
            std::collections::hash_map::Entry::Occupied(mut e) => {
                // LRU touch: a fetch call for an entry we HAVE is a use —
                // visible cards re-request their media every frame, so an
                // on-screen image never sits at the evict-front. (`order`
                // was insertion-order: visible pictures could be evicted
                // while on screen — the flash-then-refetch flicker.) The
                // timestamp feeds the eviction grace window (the deque
                // only carries the ORDER).
                {
                    let mut order = cache.order.borrow_mut();
                    if order.back().map(|b| b != &url).unwrap_or(true) {
                        order.retain(|u| u != &url);
                        order.push_back(url.clone());
                    }
                    // Touch at most once a second per url (audit perf L3):
                    // interleaved visible images made every frame pay an
                    // O(entries) retain + a map write. The eviction grace
                    // window is seconds - a 1s staleness is nothing.
                    let mut touched = cache.touched.borrow_mut();
                    let now = Instant::now();
                    if touched
                        .get(&url)
                        .is_none_or(|t| now.duration_since(*t) >= std::time::Duration::from_secs(1))
                    {
                        touched.insert(url.clone(), now);
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
                cache
                    .touched
                    .borrow_mut()
                    .insert(url.clone(), Instant::now());
                Action::Start(BACKOFF_START)
            }
        };
        // The painted thumb rides along (either match arm — the thumb entry
        // can even outlive its big entry to eviction): a surface fetching
        // this URL is (very likely) painting its avatar, so keep the thumb
        // entry fresh in the small-class LRU too.
        let tk = thumb_key(&url);
        if map.contains_key(&tk) {
            let mut order = cache.order.borrow_mut();
            if order.back().map(|b| b != &tk).unwrap_or(true) {
                order.retain(|u| u != &tk);
                order.push_back(tk.clone());
            }
            drop(order);
            cache.touched.borrow_mut().insert(tk, Instant::now());
        }
        action
    };

    match action {
        Action::Joined | Action::Ready => {} // the landing notifies every waiter
        Action::Start(next_backoff) => {
            let core2 = core.clone();
            let view2 = view;
            cx.spawn(async move |cx| {
                let st = match kind {
                    Kind::Plain => {
                        let rx = core2.fetch_image(url.clone());
                        let outcome = {
                            let mut rx = rx;
                            rx.recv().await
                        }
                        .unwrap_or(Err(MediaError::Transient));
                        landing_state(outcome, next_backoff)
                    }
                    // A blurhash is decoded LOCALLY — no network at all.
                    // A hash that won't decode is NotMedia (terminal for
                    // the session): a poisoned tag can't spin.
                    Kind::Blurhash(hash) => {
                        let frame = cx
                            .background_executor()
                            .spawn(async move { crate::poster::blurhash_png(&hash) })
                            .await;
                        ready_or_not(frame.and_then(crate::poster::decoded_dynamic))
                    }
                    // A sniffed poster: cache read-through first (a sniffs
                    // survives restarts), then the capped head download +
                    // local first-frame decode. Over-full workshop books a
                    // short backoff — no queued promises, the ordinary
                    // retry timer re-enters fetch.
                    Kind::Poster(video) => {
                        // The workshop cap stays UI-side (over-full = a
                        // short backoff), but the job itself runs on core's
                        // runtime: every tokio call inside it — the guarded
                        // head fetch, the cache slots, the sniffer's
                        // spawn_blocking — needs that context, and polled
                        // from gpui's executor it panicked thread 'main
                        // (the crash the day D32 shipped). The channel is
                        // fetch_image's exact seam.
                        let outcome = match crate::poster::InFlight::take() {
                            None => Err(MediaError::Transient),
                            Some(_slot) => {
                                let rx = core2.fetch_poster(
                                    video,
                                    crate::poster::POSTER_HEAD_BYTES,
                                    crate::poster::sniff_png,
                                );
                                {
                                    let mut rx = rx;
                                    rx.recv().await
                                }
                                .unwrap_or(Err(MediaError::Transient))
                            }
                        };
                        landing_state(outcome, next_backoff)
                    }
                };
                // Publish globally, then wake every waiter.
                cx.update(|app| {
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
                            // The thumb rides as its OWN small-class entry
                            // (thumb_key): avatars paint THIS Arc, so photo
                            // pressure can never drop its texture. Overwrite
                            // on re-land is deliberate (a changed image at
                            // the same URL must update); the old texture is
                            // not remove_asset'd — a painted element keeps
                            // its pixels until it re-renders with the new
                            // Arc, and one 64KB texture per re-land is
                            // noise.
                            let thumb = match map.get(&url) {
                                Some(Entry::Ready(c)) => c.thumb.clone(),
                                _ => None,
                            };
                            if let Some(t) = thumb {
                                let tk = thumb_key(&url);
                                // Byte math: an overwrite REPLACES the old
                                // entry — subtract its bytes before adding
                                // the new, or re-lands inflate the total.
                                let mut old_bytes = 0u64;
                                if let Some(Entry::Ready(oc)) = map.insert(
                                    tk.clone(),
                                    Entry::Ready(CachedImage {
                                        png: t,
                                        thumb: None,
                                        size: THUMB_DIMS,
                                    }),
                                ) {
                                    let (ow, oh) = oc.size;
                                    old_bytes = ow as u64 * oh as u64 * 4;
                                }
                                // Rotate to the back (a re-land must not
                                // leave a stale duplicate mid-deque).
                                let mut order = cache.order.borrow_mut();
                                order.retain(|u| u != &tk);
                                order.push_back(tk.clone());
                                drop(order);
                                cache.touched.borrow_mut().insert(tk, Instant::now());
                                cache.bytes.set(
                                    cache.bytes.get()
                                        + THUMB_DIMS.0 as u64 * THUMB_DIMS.1 as u64 * 4
                                        - old_bytes,
                                );
                            }
                            // A landing is a touch: the bytes just arrived
                            // for whatever rendered surface asked for them.
                            cache
                                .touched
                                .borrow_mut()
                                .insert(url.clone(), Instant::now());
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
                            app.update(|app| fetch(&core, url, view2, app));
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

/// A job's outcome as the entry to land: validated decodes go Ready,
/// verified not-an-image (or an unsafe/over-cap URL) is NotMedia, and a
/// transient failure cools off with the caller's backoff.
fn landing_state(
    outcome: Result<satori_core::media::DecodedImage, MediaError>,
    backoff: Duration,
) -> Entry {
    match outcome {
        Ok(d) => ready_or_not(Some(d)),
        Err(MediaError::NotMedia) => Entry::NotMedia,
        Err(MediaError::Transient) => Entry::Failed {
            at: Instant::now(),
            backoff,
        },
    }
}

/// The Ready/NotMedia fold shared by every landing path.
fn ready_or_not(d: Option<satori_core::media::DecodedImage>) -> Entry {
    match d.and_then(cached_from_decoded) {
        Some(c) => Entry::Ready(c),
        None => Entry::NotMedia,
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
        Some(png) => disc
            .child(img(png).size_full().rounded_full())
            .into_any_element(),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A Ready entry with an arbitrary decoded size — the SIZE is what the
    /// byte budget counts; the bytes are never touched by evict (Arcs move,
    /// nothing decodes).
    fn ready(w: u32, h: u32) -> Entry {
        Entry::Ready(CachedImage {
            png: Arc::new(Image::from_bytes(ImageFormat::Png, Vec::new())),
            thumb: None,
            size: (w, h),
        })
    }

    /// Insert a Ready entry as fetch's landing would: map + order + touch.
    /// `age` back-dates the touch — the grace window's whole question.
    fn insert(cache: &Cache, url: &str, w: u32, h: u32, age: Duration) {
        cache.map.borrow_mut().insert(url.into(), ready(w, h));
        cache.order.borrow_mut().push_back(url.into());
        cache
            .touched
            .borrow_mut()
            .insert(url.into(), Instant::now() - age);
        cache.bytes.set(cache.bytes.get() + w as u64 * h as u64 * 4);
    }

    /// The flicker regression: a landing burst pushes the cache over
    /// budget between two renders — but every entry was touched by the
    /// last render (ages inside the grace window), so NOTHING may evict.
    /// Evicting a painted image remove_assets it: gpui drops the decoded
    /// texture and re-decodes async — the multi-frame blank.
    #[test]
    fn evict_spares_fresh_entries_even_over_budget() {
        let cache = Cache::default();
        insert(&cache, "a", 3000, 3000, Duration::from_secs(1));
        insert(&cache, "b", 3000, 3000, Duration::from_secs(2));
        insert(&cache, "c", 2000, 2000, Duration::from_secs(3));
        insert(&cache, "d", 2000, 2000, Duration::from_secs(1)); // 104MB > 96
        let dropped = cache.evict();
        assert!(dropped.is_empty(), "fresh entries must survive the grace");
        assert_eq!(cache.map.borrow().len(), 4);
    }

    /// The snapshot memo: a same-epoch request returns the SAME Arc (the
    /// per-frame Snapshot::new is a refcount bump), a bumped epoch (a
    /// landing/eviction) builds a fresh one.
    #[test]
    fn snapshot_memoizes_on_epoch() {
        let cache = Cache::default();
        insert(&cache, "a", 100, 100, Duration::from_secs(1));
        let s1 = cache.snapshot();
        let s2 = cache.snapshot();
        assert!(
            std::sync::Arc::ptr_eq(&s1, &s2),
            "same epoch hands out the same snapshot"
        );
        cache.epoch.set(cache.epoch.get() + 1);
        let s3 = cache.snapshot();
        assert!(!std::sync::Arc::ptr_eq(&s1, &s3), "a bumped epoch rebuilds");
        assert!(s3.png("a").is_some(), "content survives the rebuild");
    }

    /// Once the oldest entry is past the grace window it goes — and only
    /// it: the fresh ones stay put (the overshoot collects as entries age).
    #[test]
    fn evict_takes_stale_before_fresh() {
        let cache = Cache::default();
        insert(&cache, "stale", 3000, 3000, Duration::from_secs(7200));
        insert(&cache, "fresh", 3000, 3000, Duration::from_secs(1));
        insert(&cache, "f2", 2000, 2000, Duration::from_secs(2));
        insert(&cache, "f3", 2000, 2000, Duration::from_secs(3)); // 104MB
        let dropped = cache.evict();
        assert_eq!(dropped.len(), 1, "only the stale entry goes");
        assert!(!cache.map.borrow().contains_key("stale"));
        assert!(cache.map.borrow().contains_key("fresh"));
    }

    /// Past double budget the grace is ignored — memory stays bounded no
    /// matter how long a burst keeps everything "fresh".
    #[test]
    fn desperate_eviction_ignores_grace() {
        let cache = Cache::default();
        for i in 0..6 {
            insert(&cache, &format!("e{i}"), 3000, 3000, Duration::from_secs(1));
        } // 216MB = over the 192MB desperation line
        let dropped = cache.evict();
        assert!(!dropped.is_empty(), "desperation must evict");
        assert!(cache.bytes.get() <= EVICT_DESPERATE_BYTES);
    }

    /// The rail-avatar glitch: a pinned entry (the always-on-screen disc,
    /// whose LRU touch freezes while the shell idles) must survive even a
    /// desperate sweep — photo pressure flashed it to the ink initial and
    /// refetched, photo → initial → photo, for the whole scroll.
    #[test]
    fn pinned_entries_survive_desperate_eviction() {
        let cache = Cache::default();
        for i in 0..6 {
            insert(&cache, &format!("e{i}"), 3000, 3000, Duration::from_secs(1));
        } // 216MB = over the 192MB desperation line
        cache.pinned.borrow_mut().insert("e0".to_string());
        let dropped = cache.evict();
        assert!(!dropped.is_empty(), "desperation must still evict");
        assert!(cache.map.borrow().contains_key("e0"), "pinned survives");
    }

    /// Unpin releases the guard: url AND its disc-thumb key leave the
    /// set, so a pane's old rows become evictable when the next query
    /// lands — pinned stays bounded by what is on screen.
    #[test]
    fn unpin_releases_url_and_thumb_key() {
        let cache = Cache::default();
        cache
            .pinned
            .borrow_mut()
            .insert("https://x/a.png".to_string());
        cache
            .pinned
            .borrow_mut()
            .insert(thumb_key("https://x/a.png").to_string());
        cache.unpin_urls(&["https://x/a.png".to_string()]);
        assert!(
            cache.pinned.borrow().is_empty(),
            "url and thumb key both go"
        );
    }

    /// The articles-feed flash: a VISIBLE cover whose pane idles — GPUI
    /// renders only on events, and only renders touch — ages past the
    /// grace while foreign landings (avatars' originals, other panes'
    /// covers) keep the big class over budget. Every landing then evicts
    /// the on-screen cover (remove_asset: a multi-frame blank), the
    /// pane's next render re-fetches it, and it flashes back. The rail
    /// avatar got `pin` for exactly this freeze (its touch freezes while
    /// the shell idles); a feed row's media has the same freeze and the
    /// same protection: the feed pins its visible window's media and
    /// releases the previous window's.
    #[test]
    fn an_idle_panes_visible_cover_survives_foreign_landings() {
        let cache = Cache::default();
        // The cover the reader is looking at, last touched by a render
        // two minutes ago; the feed pins what it paints.
        insert(
            &cache,
            "cover",
            3000,
            3000,
            EVICT_GRACE + Duration::from_secs(1),
        );
        cache.pinned.borrow_mut().insert("cover".to_string());
        // Foreign landings: a burst of fresh bigs over the budget.
        for i in 0..5 {
            insert(&cache, &format!("f{i}"), 3000, 3000, Duration::from_secs(1));
        } // 216MB > 96MB
        let dropped = cache.evict();
        assert!(!dropped.is_empty(), "pressure must still evict something");
        assert!(
            cache.map.borrow().contains_key("cover"),
            "the on-screen cover must not be the victim"
        );
    }

    /// The notifications-page flicker regression: photo pressure must
    /// never evict avatars. Thirty 128px thumbs (the rail avatar's shape,
    /// stale — an idle pane's touches are old) plus stale bigs over the
    /// 96MB budget: the bigs go, every avatar stays.
    #[test]
    fn photo_pressure_never_evicts_avatars() {
        let cache = Cache::default();
        for i in 0..30 {
            insert(
                &cache,
                &format!("av{i}"),
                128,
                128,
                Duration::from_secs(3600),
            );
        }
        for i in 0..4 {
            insert(
                &cache,
                &format!("ph{i}"),
                3000,
                3000,
                Duration::from_secs(3600),
            );
        } // 144MB of bigs — over budget, all past grace
        let dropped = cache.evict();
        assert!(!dropped.is_empty(), "stale bigs must go");
        for i in 0..30 {
            assert!(
                cache.map.borrow().contains_key(&format!("av{i}")),
                "avatar av{i} was evicted by photo pressure"
            );
        }
    }

    /// Avatars have their own ceiling: past it the STALE ones go (LRU
    /// within the class), fresh ones stay — an avatar burst can't push
    /// painted avatars out either.
    #[test]
    fn small_cap_evicts_stale_avatars_first() {
        let cache = Cache::default();
        for i in 0..SMALL_MAX_ENTRIES {
            insert(
                &cache,
                &format!("a{i}"),
                128,
                128,
                Duration::from_secs(3600),
            );
        }
        insert(&cache, "fresh", 128, 128, Duration::from_secs(1));
        let dropped = cache.evict();
        assert_eq!(dropped.len(), 1, "only the overflow avatar goes");
        assert!(
            !cache.map.borrow().contains_key("a0"),
            "LRU small goes first"
        );
        assert!(cache.map.borrow().contains_key("fresh"));
    }

    /// THE sidebar-flicker regression: a big original (the user's avatar
    /// decodes over the small line) whose THUMB the rail paints — photo
    /// pressure evicts the big entry, but the thumb entry (what avatars
    /// actually paint) stays, and the dropped texture is only the full png.
    #[test]
    fn photo_pressure_spares_the_painted_thumb() {
        let cache = Cache::default();
        // The avatar's landing: a big full entry plus its own thumb entry.
        // The helper builds thumb: None — the painted Arc is constructed
        // inline; eviction math only reads `size` anyway.
        let thumb_arc: Png = Arc::new(Image::from_bytes(ImageFormat::Png, b"thumb-bytes".to_vec()));
        insert(&cache, "me", 1000, 1000, Duration::from_secs(3600)); // 4MB — BIG
        cache.map.borrow_mut().insert(
            thumb_key("me"),
            Entry::Ready(CachedImage {
                png: thumb_arc.clone(),
                thumb: None,
                size: THUMB_DIMS,
            }),
        );
        cache.order.borrow_mut().push_back(thumb_key("me"));
        // Photo burst: bigs over budget, all stale.
        for i in 0..4 {
            insert(
                &cache,
                &format!("ph{i}"),
                3000,
                3000,
                Duration::from_secs(3600),
            );
        } // 144MB + 4MB
        let dropped = cache.evict();
        assert!(!dropped.is_empty(), "stale bigs must go");
        assert!(
            dropped.iter().all(|p| !Arc::ptr_eq(p, &thumb_arc)),
            "the painted thumb texture must never be dropped"
        );
        assert!(
            cache.map.borrow().contains_key(&thumb_key("me")),
            "the thumb entry survives photo pressure"
        );
    }

    /// Snapshot::thumb serves the thumb ENTRY when one landed — that Arc is
    /// the stable one avatars paint (photo eviction can't touch it). The
    /// full entry's own thumb FIELD is a different Arc here, so the test
    /// proves the entry takes precedence over the field.
    #[test]
    fn snapshot_thumb_prefers_the_thumb_entry() {
        let cache = Cache::default();
        let field_arc: Png = Arc::new(Image::from_bytes(ImageFormat::Png, b"field-thumb".to_vec()));
        let entry_arc: Png = Arc::new(Image::from_bytes(ImageFormat::Png, b"entry-thumb".to_vec()));
        cache.map.borrow_mut().insert(
            "me".into(),
            Entry::Ready(CachedImage {
                png: Arc::new(Image::from_bytes(ImageFormat::Png, b"full-bytes".to_vec())),
                thumb: Some(field_arc),
                size: (1000, 1000),
            }),
        );
        cache.map.borrow_mut().insert(
            thumb_key("me"),
            Entry::Ready(CachedImage {
                png: entry_arc.clone(),
                thumb: None,
                size: THUMB_DIMS,
            }),
        );
        let snap = Snapshot::from_cache(&cache);
        let served = snap.thumb("me").expect("thumb served");
        assert!(
            Arc::ptr_eq(&served, &entry_arc),
            "the thumb entry's Arc is served"
        );
    }
}
