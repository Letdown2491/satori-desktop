//! The guarded media door both inline players share (the five-lens
//! audit's duplication lens): the browser allow-list (audit security
//! M2), the strict-Tor degrade, the build-window registry, the M5 proxy
//! spawn, the dial-time screen and the land tail were verbatim twins in
//! video.rs and audio.rs — one sequence, two players. The registries
//! themselves stay per-player (video hands out a frame element with
//! hardcoded `video/x-raw` caps; audio paints nothing), so only the
//! door is here.

use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// The build window: the URL whose pipeline is constructing right now,
/// with the TICKET its open was granted (one slot — each player is
/// one-per-process, and the two never build concurrently). A second tap
/// inside the window on the same URL is a no-op, not a second pipeline:
/// the video player's OPENING registry, now shared. The ticket is what
/// makes "the newest tap wins" true at LANDING time: a tap preempts a
/// build in flight (D106) by taking a fresh ticket, and the superseded
/// build's landing is refused (it drops itself instead of replacing the
/// newer player, whichever finishes first).
static OPENING: RwLock<Option<(String, u64)>> = RwLock::new(None);

/// Ticket serial (0 is never issued).
static TICKET: AtomicU64 = AtomicU64::new(1);

/// Is `url` the one currently building? cx-less.
pub fn opening(url: &str) -> bool {
    OPENING
        .read()
        .ok()
        .is_some_and(|g| g.as_ref().is_some_and(|(u, _)| u == url))
}

/// Take the build window for `url` (a fresh ticket supersedes whatever
/// build is in flight).
fn mark(url: &str) -> u64 {
    let ticket = TICKET.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut g) = OPENING.write() {
        *g = Some((url.to_string(), ticket));
    }
    ticket
}

/// Does `ticket` still own the build window? A build whose ticket was
/// superseded by a newer tap must not land.
pub fn current(ticket: u64) -> bool {
    OPENING
        .read()
        .ok()
        .is_some_and(|g| g.as_ref().is_some_and(|(_, t)| *t == ticket))
}

/// What `gate` decided.
pub enum Gate {
    /// The tap is fully handled (browser fallback or a no-op) — the
    /// caller just returns.
    Handled,
    /// Past every sync guard: the proxy door is open. The dial-time
    /// screen and the pipeline build remain (async, the caller's).
    Door(Box<Door>),
}

/// What a refused open degrades to. The TAP's semantics (Browser) hand
/// the url to the system browser — an explicit tap is intent. The HOVER
/// PREVIEW's semantics (Nothing, D104) are a no-op: the tile stays a
/// still. Hover is not intent — a cursor sweep across a grid must never
/// open a browser tab per tile crossed (five-lens audit Nov 2026).
#[derive(Clone, Copy)]
pub enum Degrade {
    Browser,
    Nothing,
}

fn fall_back(url: &str, degrade: Degrade, cx: &mut gpui::App) {
    if let Degrade::Browser = degrade {
        cx.open_url(url);
    }
}

/// Is ANY pipeline building right now? (cx-less.) The build window is
/// process-wide — one player per process, so the two never build
/// concurrently; a second open inside a build window is a no-op, not a
/// second concurrent pipeline+proxy+download.
pub fn opening_busy() -> bool {
    // The SLOT's occupancy, not the lock's: `read().ok().is_some()` was
    // true whenever the lock wasn't poisoned, which refused every hover
    // preview since D105 (review, Nov 27 2026).
    OPENING.read().is_ok_and(|g| g.is_some())
}

/// `Gate::Door`'s payload, boxed (clippy: the variant dwarfed Handled).
pub struct Door {
    /// The build window's ticket: `land` refuses it once a newer open
    /// superseded it.
    pub ticket: u64,
    /// The ORIGIN url, parsed (the screen's host comes from here).
    pub parsed: url::Url,
    pub proxy: satori_core::video_proxy::RangeProxy,
    /// The proxy URI playbin dials.
    pub proxied: url::Url,
}

/// The sync guards, in video.rs's original order: the browser
/// allow-list (feed-derived URLs only ever open as http(s) — anything
/// else does NOTHING, it never reaches xdg-open's scheme handlers),
/// then the inline/strict-Tor decision (the caller folds its own
/// inline-able rule into `inline`), the busy check, the build-window
/// mark, and the M5 proxy door (no door, no dial: degrade to the
/// browser, fail closed). Every degrade arm wakes the views so a
/// breathing build mark never sticks.
pub fn gate(
    url: &str,
    inline: bool,
    already: bool,
    tag: &str,
    cx: &mut gpui::App,
    degrade: Degrade,
) -> Gate {
    // The browser allow-list (audit security M2). The AUDIO player's tap
    // used to open_url here unguarded (audit security: an attacker imeta
    // URL of any scheme rode the row straight to a desktop handler).
    let openable = crate::views::content::web_url(url);
    if !openable || !inline {
        if openable {
            fall_back(url, degrade, cx);
        }
        return Gate::Handled;
    }
    let Ok(parsed) = url::Url::parse(url) else {
        // Not a real URL at all — the browser can sort it out or not.
        fall_back(url, degrade, cx);
        return Gate::Handled;
    };
    // Already open or building — a second tap on the same row is a
    // no-op, not a second pipeline. The build WINDOW (five-lens audit
    // Nov 2026 — a hover sweep used to construct one pipeline per tile
    // crossed) refuses HOVER opens only (Degrade::Nothing — hover is
    // not intent, it can queue). A TAP (Degrade::Browser — intent)
    // PREEMPTS: it takes the slot, and the build in flight — a hover
    // preview whose want the caller retired, or a prior tap's build the
    // newer tap replaces — lands over or drops itself. The field test
    // round-tripped "the tap degrades to the browser" because a WEDGED
    // preview build (a stalled origin hangs Video::new past any grace
    // period) held the window forever; taps never queue now. At most
    // two pipelines coexist, and one of them is doomed.
    if already || (opening_busy() && matches!(degrade, Degrade::Nothing)) {
        if !already && matches!(degrade, Degrade::Nothing) {
            eprintln!("[{tag}] preview refused: a build is already in flight");
        }
        return Gate::Handled;
    }
    let ticket = mark(url);
    cx.set_global(crate::video::Signal);
    // M5 WP3: the guarded door. Playbin never sees the origin URL — its
    // souphttpsrc resolves and redirects outside every guard we have.
    let proxy = match satori_core::video_proxy::spawn(url.to_string()) {
        Ok(proxy) => proxy,
        Err(e) => {
            eprintln!("[{tag}] proxy spawn failed ({e}); degrading to browser");
            release(ticket);
            fall_back(url, degrade, cx);
            cx.set_global(crate::video::Signal);
            return Gate::Handled;
        }
    };
    let proxied = match url::Url::parse(&proxy.uri()) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("[{tag}] proxy uri unparseable ({e}); degrading to browser");
            release(ticket);
            fall_back(url, degrade, cx);
            cx.set_global(crate::video::Signal);
            return Gate::Handled;
        }
    };
    Gate::Door(Box::new(Door {
        ticket,
        parsed,
        proxy,
        proxied,
    }))
}

/// The dial-time screen (audit security, D59 cycle) — the UX layer of
/// the two-layer guard. WP3's proxy enforces the invariant at the
/// socket (every origin fetch rides media::client's pinned resolution);
/// this pre-check decides the HANDOFF before any player is built: an
/// origin name that rebinds private degrades to the browser link
/// instead of a dead inline player. A DNS miss is weather — the proxy's
/// fetch reports it honestly; only the guard speaking (NonPublic)
/// refuses.
pub async fn screened(host: Option<String>, tag: &str) -> bool {
    match host.as_deref() {
        Some(h) => match satori_core::net_guard::resolve_pinned_public(h, 443).await {
            Ok(_) => true,
            Err(satori_core::net_guard::NetGuardError::Resolve(_)) => true,
            Err(satori_core::net_guard::NetGuardError::NonPublic(_)) => {
                eprintln!("[{tag}] dial-time screen refused {host:?}; degrading to browser");
                false
            }
        },
        None => false,
    }
}

/// The land tail: register the new player, defer the replaced player's
/// drop off the UI thread (set_state(Null) can block on a live sink),
/// and give the build window back. The entry must be `Send` (the
/// deferred thread) — both players' handles are. A build whose ticket a
/// newer open superseded does NOT land: its entry drops (deferred, same
/// reason) and the newer build keeps the window. Returns whether it
/// landed.
pub fn land<R: Send + 'static>(registry: &RwLock<Option<R>>, ticket: u64, entry: R) -> bool {
    if !current(ticket) {
        std::thread::spawn(move || drop(entry));
        return false;
    }
    let old = {
        let mut guard = match registry.write() {
            Ok(g) => g,
            Err(_) => return false,
        };
        guard.replace(entry)
    };
    if let Some(old) = old {
        std::thread::spawn(move || drop(old));
    }
    release(ticket);
    true
}

/// Give the window back by ticket (a superseded build never clears the
/// newer build's mark).
pub fn release(ticket: u64) {
    if let Ok(mut g) = OPENING.write()
        && g.as_ref().is_some_and(|(_, t)| *t == ticket)
    {
        *g = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The window statics are process-wide: serialize the tests on them.
    static SERIAL: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    fn reset() {
        *OPENING.write().unwrap() = None;
    }

    /// The D105 regression: an EMPTY window is not busy (the old check
    /// tested the lock, not the slot, and refused every hover preview).
    #[test]
    fn empty_window_is_not_busy() {
        let _s = SERIAL.lock();
        reset();
        assert!(!opening_busy());
        let t = mark("https://a.example/x.mp4");
        assert!(opening_busy());
        release(t);
        assert!(!opening_busy());
    }

    /// Tap A, tap B, A finishes LAST: B must still be the one playing.
    #[test]
    fn superseded_build_never_lands() {
        let _s = SERIAL.lock();
        reset();
        let registry: RwLock<Option<&'static str>> = RwLock::new(None);
        let a = mark("https://a.example/x.mp4");
        let b = mark("https://b.example/y.mp4");
        assert!(land(&registry, b, "b"));
        assert!(!land(&registry, a, "a"));
        assert_eq!(*registry.read().unwrap(), Some("b"));
        assert!(!opening_busy());
    }

    /// Same race, A finishes FIRST: A is refused, B lands on its own.
    #[test]
    fn superseded_build_refused_before_newer_lands() {
        let _s = SERIAL.lock();
        reset();
        let registry: RwLock<Option<&'static str>> = RwLock::new(None);
        let a = mark("https://a.example/x.mp4");
        let b = mark("https://b.example/y.mp4");
        assert!(!land(&registry, a, "a"));
        assert!(
            opening("https://b.example/y.mp4"),
            "A's refusal kept B's window"
        );
        assert!(land(&registry, b, "b"));
        assert_eq!(*registry.read().unwrap(), Some("b"));
    }
}
