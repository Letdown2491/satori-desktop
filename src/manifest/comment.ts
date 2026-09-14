// NIP-22 comment (kind 1111) - a hand-coded handler so a comment renders as a proper note-shaped card. NIP-22
// comments comment on ANY event (notes, pictures, articles, …); Satori previously only wove them under the
// article reader, so a standalone / embedded / threaded comment had no card. Body reuses the exact note
// content pipeline (noteContent: mentions, media, emoji, content-warning, clamp); the parent context reuses
// the SAME `replyContext` lazy-embed card as a kind:1 reply (taught to read the NIP-22 parent), so a comment
// shows "in reply to" identically to a reply - and the parent link actually resolves (relay/author hints).

import type { KindHandler } from './registry.ts';
import { type SatoriDeps, notWired } from './deps.ts';
import { html } from '../html.ts';
import { cardShell, noteContent, replyContext } from '../render/note.ts';
import { mediaLightboxes } from '../render/content.ts';
import { parseImeta } from '../nostr/imeta.ts';
import { KIND_COMMENT } from '../nostr/nip22.ts';

export const commentHandler: KindHandler<SatoriDeps> = {
    kinds: [KIND_COMMENT],
    actions: ['reply', 'quote', 'like', 'zap', 'bookmark'],
    render(ev, surface, d) {
        if (surface === 'reader') return notWired(surface); // the card carries the comment; no separate reader
        const im = parseImeta(ev);
        // In a thread the parent sits right above (hideParent), exactly as a kind:1 reply row does.
        const parent = d.opts?.hideParent ? null : replyContext(ev);
        const body = html`${parent}${noteContent(ev, d.profiles, surface !== 'focused', d.s?.media, im)}`;
        return cardShell(ev, d.profiles, d.s, body, { compact: surface === 'embed', depth: d.opts?.depth, lightboxes: mediaLightboxes(ev.content, d.s?.media?.autoLoad ?? true, im) });
    },
};
