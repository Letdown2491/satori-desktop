//! Navigation events emitted by content views, handled by the shell.
//!
//! TODO(port): crates/app/src/main.rs `open_thread`/`open_profile` nav calls —
//! P1. The GTK app navigates through a Ctx nav stack; here content views emit
//! `Nav` and the shell owns the stack.

use nostr::prelude::*;

pub enum Nav {
    OpenThread(EventId),
    OpenProfile(PublicKey),
    /// The composer, optionally replying to a note.
    OpenComposer(Option<Event>),
    /// The lightbox overlay: full-screen image viewer (lightbox.rs), the
    /// URL list plus the starting index.
    OpenLightbox(Vec<String>, usize),
    /// The notifications pane.
    OpenNotifications,
    /// The active pane closes itself (composer after commit/undo).
    Pop,
}
