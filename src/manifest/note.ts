// The note kind (kind 1) - Satori's fundamental kind. Renders the `.note` card (timeline), the focused
// thread anchor, or a note embed. Render code lives in the shared render layer (noteRow/focusedNote/
// embedPreview are reused by polls + the unknown-kind fallback too); this module is the handler.

import type { KindHandler } from './registry.ts';
import { type SatoriDeps, notWired } from './deps.ts';
import { noteRow, focusedNote, embedPreview, NOTE_ACTIONS } from '../render/note.ts';
import { ensureReplies, replierPubkeys } from '../replies.ts';
import { ensureProfiles } from '../routes/common.ts';

export const noteHandler: KindHandler<SatoriDeps> = {
    kinds: [1],
    actions: NOTE_ACTIONS,
    // Warm note reply-presence (keyed by event id) + the replier avatars for a page of note rows.
    async prepare(events, s) {
        const ids = events.map((e) => e.id);
        await ensureReplies(s, ids, 'race');
        // Replier avatars are a secondary touch (the reply faces); don't block first paint on them -
        // they fill from cache or upgrade on the next render, like the custom-emoji warm.
        void ensureProfiles(s, replierPubkeys(ids)).catch(() => {});
    },
    render(ev, surface, d) {
        if (surface === 'timeline') return noteRow(ev, d.profiles, d.s, d.opts);
        if (surface === 'focused') return focusedNote(ev, d.profiles, d.s, d.inThread);
        if (surface === 'embed') return embedPreview(ev, d.bech ?? '', d.profiles, d.label);
        return notWired(surface); // a note has no reader page
    },
};
