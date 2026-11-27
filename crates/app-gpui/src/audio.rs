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

use crate::media_guard::Gate;
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
/// happened at classification), else the browser. The guards are the
/// shared media door (media_guard.rs): the allow-list, the build-window
/// registry, the M5 proxy and the dial-time screen — audio differs from
/// video only in the pipeline it builds.
pub fn open(url: String, cx: &mut gpui::App) {
    // Already live — a second tap on the same row toggles, not rebuilds
    // (the row drives its play glyph off `active`); while the pipeline
    // BUILDS the shared opening registry no-ops the taps (audit
    // usage/security: the unguarded window admitted two pipelines).
    if active(&url) {
        toggle(cx);
        return;
    }
    let inline = content::web_url(&url)
        && url.starts_with("https://")
        && crate::prefs::privacy_mode() != satori_core::privacy::PrivacyMode::Strict;
    let Gate::Door(door) = crate::media_guard::gate(
        &url,
        inline,
        crate::media_guard::opening(&url),
        "[audio]",
        cx,
        crate::media_guard::Degrade::Browser,
    ) else {
        return;
    };
    let crate::media_guard::Door {
        ticket,
        parsed,
        proxy,
        proxied,
    } = *door;
    cx.spawn(async move |cx| {
        if !crate::media_guard::screened(parsed.host_str().map(str::to_string), "[audio]").await {
            cx.update(|cx| {
                crate::media_guard::release(ticket);
                cx.open_url(&url);
                cx.set_global(crate::video::Signal);
            });
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
                crate::media_guard::land(
                    &ACTIVE,
                    ticket,
                    Audio {
                        url: url.clone(),
                        pipeline,
                        _proxy: proxy,
                    },
                );
                cx.set_global(crate::video::Signal);
            }
            Err(e) => {
                eprintln!("[audio] pipeline failed: {e}; degrading to browser");
                crate::media_guard::release(ticket);
                cx.open_url(&url);
                cx.set_global(crate::video::Signal);
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
