// The LOCAL declarative manifest set - PURE DATA the engine (engine.ts) renders into cards. These are the
// "kinds over the wire" payloads, kept local for now (committed here instead of fetched as nostr events).
// Because they're already pure data, adding the network layer later is just delivery + trust, not a rewrite.
// Each entry is the generic render for a long-tail kind we don't hand-code; bespoke kinds stay handlers.
//
// EMPTY right now: highlights (9802) were the demo, but they GRADUATED to a hand-coded handler
// (manifest/highlight.ts) once they earned a quote-first card the generic archetype can't express - the same
// lifecycle classifieds proved. This is consistent with the engine's settled finding that every curated kind
// eventually graduates; the engine's real value is the network-delivered future, not any one local demo. The
// engine + its `ref`/`url`/etc. formats stay live and tested, ready for the first wire-delivered manifest.

import type { Manifest } from './engine.ts';

export const LOCAL_MANIFESTS: Manifest[] = [];
