// The LOCAL MANIFEST: Satori's bundled kind registrations. Each card-rendered kind is its own module
// (manifest/{note,poll,article,picture}.ts); this file wires them into the registry at boot (server.ts
// calls registerSatoriKinds, mirroring setPageRenderer) and provides the unknown-kind fallback. Render
// handlers stay code (relocated, not rewritten) so behavior is byte-identical; the registry makes
// DISPATCH manifest-driven, and adding a kind is a new module + one register call - no core edit.

import { registerKind, registerFallback, registeredKinds } from './registry.ts';
import { CONTENT_TYPES } from '../data/content-prefs.ts';
import { kindLabel } from '../nostr/nip89.ts';
import { noteHandler } from './note.ts';
import { pollHandler } from './poll.ts';
import { articleHandler } from './article.ts';
import { pictureHandler } from './picture.ts';
import { podcastEpisodeHandler } from './podcast.ts';
import { calendarEventHandler } from './calendar.ts';
import { classifiedHandler } from './classified.ts';
import { videoHandler } from './video.ts';
import { highlightHandler } from './highlight.ts';
import { commentHandler } from './comment.ts';
import { customNipHandler } from './customnip.ts';
import { wikiHandler } from './wiki.ts';
import { repoHandler } from './repo.ts';
import { fromManifest } from './engine.ts';
import { LOCAL_MANIFESTS } from './manifests.ts';
import { fallbackHandler } from './fallback.ts';

export function registerSatoriKinds(): void {
    registerKind(noteHandler);
    registerKind(pollHandler);
    registerKind(articleHandler);
    registerKind(pictureHandler);
    registerKind(podcastEpisodeHandler);
    registerKind(calendarEventHandler);
    registerKind(classifiedHandler);
    registerKind(videoHandler);
    registerKind(highlightHandler);
    registerKind(commentHandler);
    registerKind(customNipHandler);
    registerKind(wikiHandler);
    registerKind(repoHandler);
    // DECLARATIVE kinds: pure-data manifests synthesized into handlers by the engine. The registry can't
    // tell these from the hand-coded handlers above - a kind is data OR code, dispatched identically.
    for (const m of LOCAL_MANIFESTS) registerKind(fromManifest(m));
    registerFallback(fallbackHandler);
    auditContentCatalog();
}

/** Boot audit: the content catalog (CONTENT_TYPES) is hand-curated (it carries labels, grouping, and
 * per-surface defaults the registry can't), and it's load-bearing - feed/profile queries draw their
 * kinds from it, so a kind missing from the catalog silently never appears in feeds/profiles. Warn loudly
 * if any registered kind has no catalog entry, so "forgot to add it" can't pass unnoticed. */
function auditContentCatalog(): void {
    const cataloged = new Set(CONTENT_TYPES.flatMap((c) => c.kinds));
    for (const k of registeredKinds()) {
        if (!cataloged.has(k)) console.warn(`[content-catalog] registered kind ${k} (${kindLabel(k)}) has no CONTENT_TYPES entry - it won't appear in feeds/profiles. Add one in data/content-prefs.ts.`);
    }
}
