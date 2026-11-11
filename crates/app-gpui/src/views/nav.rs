//! Navigation events emitted by content views, handled by the shell.
//!
//! TODO(port): crates/app/src/main.rs `open_thread`/`open_profile` nav calls —
//! P1. The GTK app navigates through a Ctx nav stack; here content views emit
//! `Nav` and the shell owns the stack.

use nostr::prelude::*;

/// Clone by design: views that host another view (the profile's embedded
/// notes feed) forward child navs upward with `cx.emit(ev.clone())`.
#[derive(Clone)]
pub enum Nav {
    /// Settings asked for a sign-out; the shell clears the session and
    /// hands the window back to the login doors.
    SignOut,
    /// Settings picked a palette (true = dark). The shell persists the
    /// pref, selects the palette and signals the watchers (theme.rs).
    SetTheme(bool),
    OpenThread(EventId),
    OpenProfile(PublicKey),
    /// The profile edit form (D38) for the given (own) pubkey.
    OpenEditProfile(PublicKey),
    /// The composer, optionally replying to a note.
    OpenComposer(Option<Event>),
    /// Reopen a synced draft (D38): text + the resolved context events
    /// (boxed — Nav must not carry a three-event-sized variant).
    OpenDraft(
        String,
        Vec<Vec<String>>,
        Option<Box<Event>>,
        Option<Box<Event>>,
    ),
    /// The composer quoting a note (NIP-18 quote repost).
    OpenQuote(Event),
    /// The article reader page (markdown).
    OpenArticle(Event),
    /// The lightbox overlay: full-screen image viewer (lightbox.rs), the
    /// URL list plus the starting index.
    OpenLightbox(Vec<String>, usize),
    /// The video overlay (Phase 3): full-screen player for a video URL —
    /// inline when video.rs allows (strict Tor/YouTube degrade to the
    /// browser); the mime gates extension-less blossom URLs.
    OpenVideo(String, Option<String>),
    /// The zap amount sheet for a note (bolt long-press / right-click).
    OpenZapSheet(Event),
    /// The NWC wallet connect pane (D20).
    OpenWallet,
    /// A #tag tapped in a note body — the hashtag timeline (Phase 2 §4.2).
    OpenHashtag(String),
    /// A scrollable list pane crossed the deep-scroll threshold (or came
    /// back — the feed, articles shelf, hashtag timeline, notifications):
    /// the shell shows/hides the floating back-to-top whisper for the
    /// ACTIVE pane. Payloadless on purpose — the shell re-reads the view's
    /// `deep` field at render (a stored copy goes stale across switches);
    /// this event is only the repaint signal.
    PaneDeep,
    /// The active pane closes itself (composer after commit/undo).
    Pop,
}
