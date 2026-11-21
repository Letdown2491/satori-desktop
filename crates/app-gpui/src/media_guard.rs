//! The guarded media door both inline players share (the five-lens
//! audit's duplication lens): the browser allow-list (audit security
//! M2), the strict-Tor degrade, the build-window registry, the M5 proxy
//! spawn, the dial-time screen and the land tail were verbatim twins in
//! video.rs and audio.rs — one sequence, two players. The registries
//! themselves stay per-player (video hands out a frame element with
//! hardcoded `video/x-raw` caps; audio paints nothing), so only the
//! door is here.

use std::sync::RwLock;

/// The build window: the URL whose pipeline is constructing right now
/// (one slot — each player is one-per-process, and the two never build
/// concurrently). A second tap inside the window is a no-op, not a
/// second pipeline: the video player's OPENING registry, now shared.
static OPENING: RwLock<Option<String>> = RwLock::new(None);

/// Is `url` the one currently building? cx-less.
pub fn opening(url: &str) -> bool {
    OPENING
        .read()
        .ok()
        .is_some_and(|g| g.as_deref() == Some(url))
}

/// Give the slot back (only the URL that owns it clears — a second tap
/// on another URL while one builds never clobbers the first's mark).
pub fn clear(url: &str) {
    if let Ok(mut g) = OPENING.write()
        && g.as_deref() == Some(url)
    {
        *g = None;
    }
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
    OPENING.read().ok().is_some()
}

/// `Gate::Door`'s payload, boxed (clippy: the variant dwarfed Handled).
pub struct Door {
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
    if let Ok(mut g) = OPENING.write() {
        *g = Some(url.to_string());
    }
    cx.set_global(crate::video::Signal);
    // M5 WP3: the guarded door. Playbin never sees the origin URL — its
    // souphttpsrc resolves and redirects outside every guard we have.
    let proxy = match satori_core::video_proxy::spawn(url.to_string()) {
        Ok(proxy) => proxy,
        Err(e) => {
            eprintln!("[{tag}] proxy spawn failed ({e}); degrading to browser");
            clear(url);
            fall_back(url, degrade, cx);
            cx.set_global(crate::video::Signal);
            return Gate::Handled;
        }
    };
    let proxied = match url::Url::parse(&proxy.uri()) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("[{tag}] proxy uri unparseable ({e}); degrading to browser");
            clear(url);
            fall_back(url, degrade, cx);
            cx.set_global(crate::video::Signal);
            return Gate::Handled;
        }
    };
    Gate::Door(Box::new(Door {
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
/// deferred thread) — both players' handles are.
pub fn land<R: Send + 'static>(registry: &RwLock<Option<R>>, url: &str, entry: R) {
    let old = {
        let mut guard = match registry.write() {
            Ok(g) => g,
            Err(_) => return,
        };
        guard.replace(entry)
    };
    if let Some(old) = old {
        std::thread::spawn(move || drop(old));
    }
    clear(url);
}
