// The Custom NIP kind (kind:30817, a NUD). An addressable, markdown-bodied event - structurally an
// article with a `title` and zero+ `k` tags naming the kinds it defines. We display it like an article:
// the card (timeline/feed row), the full reader page (/a/) with the defined-kind chips, the note-shaped
// thread anchor when reached via /t/, or a clean preview when embedded. Render lives in render/note.ts
// (reusing the article DOM); this is the handler. Mirrors articleHandler.

import type { KindHandler } from './registry.ts';
import { type SatoriDeps, notWired } from './deps.ts';
import { customNipRow, customNipReader, focusedNote, customNipEmbedPreview, ARTICLE_ACTIONS } from '../render/note.ts';
import { articleLikeReplies } from './article.ts';
import { KIND_CUSTOM_NIP } from '../nostr/customnip.ts';

export const customNipHandler: KindHandler<SatoriDeps> = {
    kinds: [KIND_CUSTOM_NIP],
    reader: true, // the full-page /a/ markdown reader (with defined-kind chips)
    actions: ARTICLE_ACTIONS, // comment(reply) · quote · like · zap · bookmark · pin, keyed by naddr
    ref: { as: 'article', label: '↗ custom NIP', path: (bech) => `/a/${bech}` }, // inline naddr → the reader

    prepare: articleLikeReplies, // NIP-22 reply-presence + replier avatars, shared with articles

    render(ev, surface, d) {
        if (surface === 'timeline') return customNipRow(ev, d.profiles, d.s);
        if (surface === 'reader') return customNipReader(ev, d.profiles, d.s);
        // Reached via /t/ (by nevent): the note-shaped thread anchor, matching the article handler - the
        // /a/ route owns the reader.
        if (surface === 'focused') return focusedNote(ev, d.profiles, d.s, d.inThread);
        if (surface === 'embed') return customNipEmbedPreview(ev, d.naddr ?? d.bech ?? '', d.profiles);
        return notWired(surface);
    },
};
