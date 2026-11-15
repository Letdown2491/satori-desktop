//! Inline audio playback — the NIP-A0 voice row's engine (#52). The video
//! player's audio twin: one player per process, tap-to-play, the same
//! guarded door (M5 WP3 — playbin never sees the origin URL) and the same
//! wake signal ([`crate::video::Signal`] — every media-bearing view
//! already observes it, so no extra wiring).
//!
//! No element: audio paints nothing — the row is theme chrome (waveform
//! bars + controls) driven by the pipeline's state, so unlike video.rs
//! there is no frame element to hand out, only state reads.
//!
//! Threading (the crate's contract, video.rs's rules): pipeline building
//! runs on the background executor; a replaced player's drop (set_state
//! to Null can block a live sink) is deferred to a spare OS thread.

use std::sync::RwLock;

use gstreamer as gst;
use gstreamer::prelude::*;

use crate::views::content;

struct Audio {
    url: String,
    pipeline: gst::Pipeline,
    /// Held for its Drop (aborts the proxy listener when the player dies);
    /// never read, hence the underscore — the video.rs Player shape.
    _proxy: satori_core::video_proxy::RangeProxy,
}

static ACTIVE: RwLock<Option<Audio>> = RwLock::new(None);

/// Is `url` the live audio (playing or paused)? cx-less.
pub fn active(url: &str) -> bool {
    ACTIVE
        .read()
        .ok()
        .is_some_and(|g| g.as_ref().is_some_and(|a| a.url == url))
}

/// Is the live audio actually playing (not paused)? cx-less; the row's
/// glyph reads this.
pub fn playing(url: &str) -> bool {
    let guard = ACTIVE.read().ok();
    let Some(guard) = guard else { return false };
    let Some(a) = guard.as_ref() else {
        return false;
    };
    if a.url != url {
        return false;
    }
    // The momentary state (no blocking wait): buffers still filling or
    // the sink still ramping read as Playing too — the row's pause glyph
    // is the tap's acknowledgment, not a status light.
    a.pipeline.current_state() == gst::State::Playing
}

/// Tap on a voice row: open the inline player (strict Tor and non-https
/// degrade to the browser — the facade rule; the mime/ext gate already
/// happened at classification), else the browser.
pub fn open(url: String, cx: &mut gpui::App) {
    if !content::web_url(&url) || !url.starts_with("https://") {
        cx.open_url(&url);
        return;
    }
    if crate::prefs::privacy_mode() == satori_core::privacy::PrivacyMode::Strict {
        cx.open_url(&url);
        return;
    }
    // Already live — a second tap on the same row toggles, not rebuilds
    // (the row drives its play glyph off `active`).
    if active(&url) {
        toggle(cx);
        return;
    }
    // M5 WP3: the guarded door. Playbin never sees the origin URL — its
    // souphttpsrc resolves and redirects outside every guard we have.
    // No door, no dial: degrade to the browser link, fail closed.
    let proxy = match satori_core::video_proxy::spawn(url.clone()) {
        Ok(proxy) => proxy,
        Err(e) => {
            eprintln!("[audio] proxy spawn failed ({e}); degrading to browser");
            cx.open_url(&url);
            return;
        }
    };
    let proxied = match url::Url::parse(&proxy.uri()) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("[audio] proxy uri unparseable ({e}); degrading to browser");
            cx.open_url(&url);
            return;
        }
    };
    cx.spawn(async move |cx| {
        // Dial-time screen (the video player's pre-check): an origin name
        // that rebinds private degrades to the browser instead of a dead
        // inline player. A DNS miss is weather — the proxy's fetch reports
        // it honestly; only the guard speaking (NonPublic) refuses.
        let allowed = match url::Url::parse(&url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
        {
            Some(h) => match satori_core::net_guard::resolve_pinned_public(&h, 443).await {
                Ok(_) => true,
                Err(satori_core::net_guard::NetGuardError::Resolve(_)) => true,
                Err(satori_core::net_guard::NetGuardError::NonPublic(_)) => false,
            },
            None => false,
        };
        if !allowed {
            eprintln!("[audio] dial-time screen refused; degrading to browser");
            cx.update(|cx| cx.open_url(&url));
            return;
        }
        let res = cx
            .background_executor()
            .spawn(async move {
                (|| -> Result<gst::Pipeline, String> {
                    gst::init().map_err(|e| e.to_string())?;
                    let el = gst::parse::launch(&format!("playbin uri=\"{proxied}\""))
                        .map_err(|e| e.to_string())?;
                    el.downcast::<gst::Pipeline>()
                        .map_err(|_| "not a pipeline".to_string())
                })()
            })
            .await;
        cx.update(|cx| match res {
            Ok(pipeline) => {
                let _ = pipeline.set_state(gst::State::Playing);
                let old = {
                    let mut guard = match ACTIVE.write() {
                        Ok(g) => g,
                        Err(_) => return,
                    };
                    guard.replace(Audio {
                        url: url.clone(),
                        pipeline,
                        _proxy: proxy,
                    })
                };
                if let Some(old) = old {
                    std::thread::spawn(move || drop(old));
                }
                cx.set_global(crate::video::Signal);
            }
            Err(e) => {
                eprintln!("[audio] pipeline failed: {e}; degrading to browser");
                cx.open_url(&url);
            }
        });
    })
    .detach();
}

/// The row's play/pause tap: toggle the live player (a no-op when nothing
/// is live — the row only renders a toggle while active).
pub fn toggle(cx: &mut gpui::App) {
    if let Ok(g) = ACTIVE.read()
        && let Some(a) = g.as_ref()
    {
        let next = if a.pipeline.current_state() == gst::State::Playing {
            gst::State::Paused
        } else {
            gst::State::Playing
        };
        let _ = a.pipeline.set_state(next);
    }
    cx.set_global(crate::video::Signal);
}

/// The ✕ tap: stop and drop the player (the row returns to its quiet
/// waveform).
pub fn close(cx: &mut gpui::App) {
    let old = ACTIVE.write().ok().and_then(|mut g| g.take());
    if let Some(old) = old {
        // set_state(Null) can block on a live sink — off the UI thread.
        std::thread::spawn(move || drop(old));
    }
    cx.set_global(crate::video::Signal);
}
