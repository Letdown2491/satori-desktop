// The wiki article kind (kind:30818, NIP-54). An addressable, AsciiDoc-bodied article: `d` = the topic
// slug, optional `title`. We display it like an article - the card (feed/profile row), the full reader
// page (/a/), the note-shaped thread anchor when reached via /t/, and a clean preview when embedded (which
// is what makes a `nostr:naddr` reference to a wiki render as a card, not a bare njump link). Render lives
// in render/note.ts (reusing the article DOM, through renderAsciiDoc); this is the handler. Mirrors
// articleHandler / customNipHandler.

import type { KindHandler } from './registry.ts';
import { type SatoriDeps, notWired } from './deps.ts';
import { wikiRow, wikiReader, focusedNote, wikiEmbedPreview, ARTICLE_ACTIONS } from '../render/note.ts';
import { articleLikeReplies } from './article.ts';
import { KIND_WIKI } from '../nostr/nip54.ts';

export const wikiHandler: KindHandler<SatoriDeps> = {
    kinds: [KIND_WIKI],
    reader: true, // earns the full-page /a/ reader (AsciiDoc body), like articles + custom NIPs
    actions: ARTICLE_ACTIONS, // comment(reply) · quote · like · zap · bookmark · pin, keyed by naddr
    ref: { as: 'article', label: '↗ wiki article', path: (bech) => `/a/${bech}` }, // inline naddr → the reader

    prepare: articleLikeReplies, // NIP-22 reply-presence + replier avatars, shared with articles

    render(ev, surface, d) {
        if (surface === 'timeline') return wikiRow(ev, d.profiles, d.s);
        if (surface === 'reader') return wikiReader(ev, d.profiles, d.s);
        // Reached via /t/ (by nevent): the note-shaped thread anchor, matching the article handler - the
        // /a/ route owns the reader.
        if (surface === 'focused') return focusedNote(ev, d.profiles, d.s, d.inThread);
        if (surface === 'embed') return wikiEmbedPreview(ev, d.naddr ?? d.bech ?? '', d.profiles);
        return notWired(surface);
    },
};
