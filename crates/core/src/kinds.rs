//! The KindHandler registry — port of `src/manifest/registry.ts`.
//!
//! Affordances are declared by data, not by widget code: each event kind has a
//! handler that names its kinds, its surfaces, and its action vocabulary. The UI
//! renders actions from the vocabulary; unknown kinds degrade honestly through
//! the fallback handler (NIP-31 `alt`, NIP-89 "open in" chips — no masquerading).

use std::sync::Arc;

use nostr::prelude::*;

/// Where a card is being rendered; handlers may specialize per surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// Feed / timeline row.
    Timeline,
    /// Focused view (thread anchor, DM anchor): no clamp, no reply-faces.
    Focused,
    /// Long-form reader.
    Reader,
    /// Lazy embed card (reply context, quote).
    Embed,
}

/// The declared action vocabulary (note.ts:368 `NOTE_ACTIONS` and friends).
#[derive(Debug, Clone, Copy, Default)]
pub struct ActionVocabulary {
    pub reply: bool,
    pub quote: bool,
    pub like: bool,
    pub zap: bool,
    pub bookmark: bool,
    pub mute: bool,
    pub pin: bool,
    pub delete: bool,
}

/// Context handed to `prepare` for batch prefetch (profiles, reply-faces).
#[derive(Debug, Default)]
pub struct PrepareCtx {
    /// Author pubkeys needed for upcoming renders, to warm profile caches.
    pub author_hints: Vec<PublicKey>,
}

pub trait KindHandler: Send + Sync {
    fn kinds(&self) -> &'static [u32];
    /// Declares itself a reader (article-like); gets reader navigation treatment.
    fn is_reader(&self) -> bool {
        false
    }
    fn actions(&self) -> ActionVocabulary;
    /// Batch hook: warm caches before a mixed page renders.
    fn prepare(&self, _events: &[Event], _ctx: &PrepareCtx) {}
}

pub struct Registry {
    handlers: Vec<Arc<dyn KindHandler>>,
    fallback: Option<Arc<dyn KindHandler>>,
}

impl Registry {
    pub fn new() -> Self {
        Self { handlers: Vec::new(), fallback: None }
    }

    /// Last registration wins (matches registry.ts `registerKind`).
    pub fn register(&mut self, handler: Arc<dyn KindHandler>) {
        self.handlers.push(handler);
    }

    pub fn set_fallback(&mut self, handler: Arc<dyn KindHandler>) {
        self.fallback = Some(handler);
    }

    pub fn handler_for(&self, kind: u32) -> Option<&Arc<dyn KindHandler>> {
        self.handlers.iter().rev().find(|h| h.kinds().contains(&kind))
    }

    pub fn fallback_handler(&self) -> Option<&Arc<dyn KindHandler>> {
        self.fallback.as_ref()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}
