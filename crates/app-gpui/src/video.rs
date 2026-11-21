//! Inline video playback — Phase 3 (satori-plan.md §5).
//!
//! TODO(port): feather AGENTS.md — Phase 3 §5. One player per process,
//! click-to-play: this module owns at most one `gpui_video_player::Video`
//! (GStreamer-backed; the element paints frames, the handle drives the
//! pipeline). Nothing plays until a video tile is tapped — no autoplay, no
//! feed-wide players (Feather's autoplay/multi-player system is out of
//! scope). Strict Tor degrades every tap to the browser link — the media
//! facade rule (DESIGN.md §4): nothing media-heavy may fetch direct, and a
//! GStreamer pipeline can't ride the Tor transport.
//!
//! Threading (the crate's contract): `Video::new` BLOCKS up to ~5s waiting
//! for the pipeline's caps, and `Drop` (the last Arc ref) joins the worker
//! thread — so creation runs on the background executor and a replaced
//! player's drop is deferred to a spare OS thread. Registry reads are
//! cx-less (item closures have no App); a unit gpui-global ([`Signal`])
//! wakes the watching views when the player opens, toggles or closes.

use std::sync::RwLock;

use gpui::px;
use gpui_video_player::Video;

use crate::media_guard::Gate;
use crate::views::content;

/// The at-most-one player: the URL it plays + the pipeline handle + the
/// guarded door its bytes walk through (M5 WP3 — playbin never sees the
/// origin URL; the RangeProxy dies with the Player).
struct Player {
    url: String,
    video: Video,
    /// The GIF picker's hover preview (D104): a preview player is
    /// retired by the picker's own lifecycle (pick, close, un-hover),
    /// never by url sniffing — a legit overlay video must be
    /// distinguishable from a preview even if the service ever serves
    /// look-alike urls.
    preview: bool,
    /// Held for its Drop (aborts the proxy listener when the player dies);
    /// never read, hence the underscore.
    _proxy: Option<satori_core::video_proxy::RangeProxy>,
}

static ACTIVE: RwLock<Option<Player>> = RwLock::new(None);

/// The hover-preview intent (D104): which tile's preview the cursor
/// WANTS, if any. Set on hover-in (before the dwell), cleared on
/// hover-out / pick / picker close. The landing path consults it: a
/// build that finishes after the cursor moved on — or the picker
/// closed — is dropped, not parked looping with nobody painting it.
static PREVIEW_WANT: RwLock<Option<String>> = RwLock::new(None);

/// Record the cursor's preview intent for `url` (cx-less; hover-in).
pub fn want_preview(url: &str) {
    if let Ok(mut g) = PREVIEW_WANT.write() {
        *g = Some(url.to_string());
    }
}

/// The wanted preview's url, if any (cx-less).
pub fn preview_want() -> Option<String> {
    PREVIEW_WANT.read().ok().and_then(|g| g.clone())
}

/// Is a player for `url` still being built? (The tile's breathing state.)
/// The slot itself lives in the shared door (media_guard::OPENING — the
/// audit's duplication lens); this is the video-side read.
pub fn opening(url: &str) -> bool {
    crate::media_guard::opening(url)
}

/// The wake-up global (theme.rs ThemeSignal pattern): set when the player
/// opens/toggles/closes — every video-bearing view observes it and
/// repaints poster ↔ player.
pub struct Signal;

impl gpui::Global for Signal {}

/// Watch for player changes (beside timefmt::watch, in any view that can
/// render a video tile).
pub fn watch<V: 'static>(cx: &mut gpui::Context<V>) {
    cx.observe_global::<Signal>(|_, cx| cx.notify()).detach();
}

/// The active player's handle, when it is playing `url` — cx-less. The
/// handle is a cheap Arc clone; the element and the controls share it.
pub fn active(url: &str) -> Option<Video> {
    let guard = ACTIVE.read().ok()?;
    let p = guard.as_ref()?;
    (p.url == url).then(|| p.video.clone())
}

/// Can this video play inline (Phase 3 §5)? Video by extension or imeta
/// mime, never a YouTube watch/shorts page (GStreamer can't stream those —
/// they stay browser links), never under strict Tor (the facade rule),
/// never cleartext http (PR #7): the tap hands the URL to a GStreamer
/// playbin, which dials it directly with its own redirect chain — none of
/// the media.rs discipline (SafeResolver, redirect re-validation) exists
/// inside gst. http(s) videos keep working end to end: `web_url()` still
/// admits them, so the tile degrades to the browser link, the exact
/// facade behavior YouTube already gets.
pub fn inline_able(url: &str, mime: Option<&str>) -> bool {
    if crate::prefs::privacy_mode() == satori_core::privacy::PrivacyMode::Strict {
        return false;
    }
    if !url.starts_with("https://") {
        return false;
    }
    if content::youtube_thumb(url).is_some() {
        return false;
    }
    mime.is_some_and(|m| m.starts_with("video/")) || content::is_video_url(url)
}

/// Tap on a video tile: open the inline player when the URL is inline-able,
/// else the browser (the honest link the tile showed before Phase 3). The
/// guards are the shared media door (media_guard.rs) — audio is this
/// function's sibling, not its copy.
pub fn open(url: String, mime: Option<String>, cx: &mut gpui::App) {
    open_inner(url, mime, false, cx);
}

/// The GIF picker's hover preview (D104): the tap's machinery WITHOUT
/// the browser. A refused preview (strict Tor, proxy failure, build
/// failure) is a NO-OP — the tile stays a still; hover is not intent,
/// and a sweep must never open browser tabs. The caller dwells first
/// (composer.rs): a sweep never even starts a pipeline.
pub fn open_preview(url: String, cx: &mut gpui::App) {
    open_inner(url, Some("video/mp4".into()), true, cx);
}

fn open_inner(url: String, mime: Option<String>, preview: bool, cx: &mut gpui::App) {
    eprintln!(
        "[video] open{}: {}",
        if preview { " preview" } else { "" },
        satori_core::pool::clean_reason(&url)
    );
    let openable = crate::views::content::web_url(&url);
    // Already open or building — a second tap on the same tile is a no-op,
    // not a second pipeline.
    let already = active(&url).is_some() || crate::media_guard::opening(&url);
    let degrade = if preview {
        crate::media_guard::Degrade::Nothing
    } else {
        crate::media_guard::Degrade::Browser
    };
    let Gate::Door(door) = crate::media_guard::gate(
        &url,
        openable && inline_able(&url, mime.as_deref()),
        already,
        "[video]",
        cx,
        degrade,
    ) else {
        return;
    };
    let crate::media_guard::Door {
        parsed,
        proxy,
        proxied,
    } = *door;
    cx.spawn(async move |cx| {
        if !crate::media_guard::screened(parsed.host_str().map(str::to_string), "[video]").await {
            cx.update(|cx| {
                crate::media_guard::clear(&url);
                // A preview's degrade is a no-op — never the browser.
                if !preview {
                    cx.open_url(&url);
                }
                cx.set_global(Signal);
            });
            return;
        }
        // Video::new blocks up to ~5s (playbin state + caps) — the
        // background executor, never the UI thread. A GIF-service mp4
        // (the /mp4/orig/ namespace, D104) LOOPS — it is a GIF's
        // animation, and a GIF that stops dead is a broken GIF.
        let looping = url.starts_with("https://gifs.nostr.build/mp4/orig/");
        let res = cx
            .background_executor()
            .spawn(async move {
                if looping {
                    Video::new_with_options(
                        &proxied,
                        gpui_video_player::VideoOptions {
                            looping: Some(true),
                            ..Default::default()
                        },
                    )
                } else {
                    Video::new(&proxied)
                }
            })
            .await;
        cx.update(|cx| match res {
            Ok(video) => {
                // The build outlived its welcome: the cursor moved on
                // (or the picker closed) while it ran — landing it here
                // would loop a pipeline nobody paints. Drop it instead.
                if preview && preview_want().as_deref() != Some(url.as_str()) {
                    std::thread::spawn(move || drop(video));
                    crate::media_guard::clear(&url);
                    cx.set_global(Signal);
                    return;
                }
                // One player per process: a replaced player's Drop joins
                // its worker — defer that off the UI thread.
                crate::media_guard::land(
                    &ACTIVE,
                    &url,
                    Player {
                        url: url.clone(),
                        video,
                        preview,
                        _proxy: Some(proxy),
                    },
                );
            }
            // The pipeline failed (missing plugins, dead URL): the honest
            // fallback is the browser — the pre-Phase-3 behavior. The
            // url is event-borne: sanitized before the log (a \n could
            // forge lines, an ESC inject terminal escapes). A PREVIEW
            // never degrades to the browser (hover is not intent): the
            // tile just stays a still.
            Err(e) => {
                eprintln!(
                    "[video] pipeline failed for {}: {e:?}; degrading to browser",
                    satori_core::pool::clean_reason(&url)
                );
                crate::media_guard::clear(&url);
                if !preview && crate::views::content::web_url(&url) {
                    cx.open_url(&url);
                }
            }
        });
        // Whatever the outcome, the wait is over — the breathing mark
        // retires.
        cx.update(|cx| cx.set_global(Signal));
    })
    .detach();
}

/// Play/pause the active player (the control row's toggle). Interior
/// mutability — the handle's methods take &self.
pub fn toggle(cx: &mut gpui::App) {
    let Some(video) = ACTIVE
        .read()
        .ok()
        .and_then(|g| g.as_ref().map(|p| p.video.clone()))
    else {
        return;
    };
    video.set_paused(!video.paused());
    cx.set_global(Signal);
}

/// Stop and drop the active player (the ✕ control, Esc in the overlay).
pub fn close(cx: &mut gpui::App) {
    let old = ACTIVE.write().ok().and_then(|mut g| g.take());
    if let Some(old) = old {
        // Drop joins the GStreamer worker thread — off the UI thread.
        std::thread::spawn(move || drop(old));
        cx.set_global(Signal);
    }
}

/// Close the player only if it's a preview (the picker's hover player,
/// D104): the composer's picker closing must not murder a video the
/// overlay is legitimately playing — but a preview left running without
/// its tile is a pipeline nobody paints. The flag, not the url, decides
/// (a look-alike url from the service must not immune-ize a preview,
/// nor doom an overlay video). The pending want retires with it: a
/// build still in flight will not land.
pub fn close_gif_preview(cx: &mut gpui::App) {
    if let Ok(mut g) = PREVIEW_WANT.write() {
        *g = None;
    }
    let is_preview = ACTIVE
        .read()
        .ok()
        .and_then(|g| g.as_ref().map(|p| p.preview))
        .unwrap_or(false);
    if is_preview {
        close(cx);
    }
}

/// The cursor left the tile: retire its want, and stop this tile's
/// preview if it already landed.
pub fn unwant_preview(url: &str, cx: &mut gpui::App) {
    if let Ok(mut g) = PREVIEW_WANT.write()
        && g.as_deref() == Some(url)
    {
        *g = None;
    }
    if active(url).is_some() {
        close_gif_preview(cx);
    }
}

/// Drop ALL preview state, cx-less (the composer's Drop): the want and
/// any landed preview player. No wake fires — the only painter was the
/// composer that is dying.
pub fn abandon_previews() {
    if let Ok(mut g) = PREVIEW_WANT.write() {
        *g = None;
    }
    let preview = ACTIVE
        .read()
        .ok()
        .and_then(|g| g.as_ref().map(|p| p.preview))
        .unwrap_or(false);
    if preview && let Ok(mut a) = ACTIVE.write() {
        let old = a.take();
        std::thread::spawn(move || drop(old));
    }
}

/// The player element in an aspect box: the crate's element aspect-fits and
/// centers its frame within the given bounds, so the box IS the layout
/// contract — callers hand the area they can spare.
pub fn element(video: &Video, box_w: f32, box_h: f32) -> gpui_video_player::VideoElement {
    gpui_video_player::video(video.clone()).size(px(box_w), px(box_h))
}

#[cfg(test)]
mod tests {
    use super::inline_able;

    /// PR #7: the inline pipeline hands the URL to a GStreamer playbin,
    /// which dials it with no SSRF discipline — cleartext http never
    /// inlines; the tile degrades to the browser link instead.
    #[test]
    fn inline_video_is_https_only() {
        assert!(!inline_able(
            "http://127.0.0.1:8080/x.mp4",
            Some("video/mp4")
        ));
        assert!(!inline_able(
            "http://cdn.example.com/x.mp4",
            Some("video/mp4")
        ));
        assert!(inline_able(
            "https://cdn.example.com/x.mp4",
            Some("video/mp4")
        ));
        // An https non-video stays un-inlinable (mime + extension rule).
        assert!(!inline_able("https://example.com/page", None));
    }
}
