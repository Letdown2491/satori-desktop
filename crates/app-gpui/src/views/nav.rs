//! Navigation events emitted by content views, handled by the shell.
//!
//! TODO(port): crates/app/src/main.rs `open_thread`/`open_profile` nav calls —
//! P1. The GTK app navigates through a Ctx nav stack; here content views emit
//! `Nav` and the shell owns the stack.

use nostr::prelude::*;

pub enum Nav {
    /// Settings asked for a sign-out; the shell clears the session and
    /// hands the window back to the login doors.
    SignOut,
    /// Settings picked a palette (true = dark). The shell persists the
    /// pref, selects the palette and signals the watchers (theme.rs).
    SetTheme(bool),
    OpenThread(EventId),
    OpenProfile(PublicKey),
    /// The composer, optionally replying to a note.
    OpenComposer(Option<Event>),
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
    /// The feed crossed the deep-scroll threshold (or came back): the shell
    /// shows/hides the floating back-to-top whisper.
    FeedDeep(bool),
    /// The active pane closes itself (composer after commit/undo).
    Pop,
}
