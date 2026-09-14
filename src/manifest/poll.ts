// The poll kind (NIP-88 kind 1068). A poll renders as a note that ALSO shows the poll box - so it
// reuses the note render and supplies the poll box via the render layer's `extra` slot. This is why
// noteRow/focusedNote no longer branch on `ev.kind === KIND_POLL`: the poll handler owns that, here.

import type { KindHandler } from './registry.ts';
import { type SatoriDeps, notWired } from './deps.ts';
import { noteRow, focusedNote, embedPreview, NOTE_ACTIONS } from '../render/note.ts';
import { pollBox } from '../render/poll.ts';
import { KIND_POLL } from '../nostr/nip88.ts';

export const pollHandler: KindHandler<SatoriDeps> = {
    kinds: [KIND_POLL],
    actions: NOTE_ACTIONS, // polls afford the same actions as notes
    render(ev, surface, d) {
        // The poll box is the `extra` element on the card + focused anchor (noteRow gates it on
        // not-pending, as before). An embed shows the plain note preview - no poll box - as today.
        if (surface === 'timeline') return noteRow(ev, d.profiles, d.s, d.opts, pollBox(ev));
        if (surface === 'focused') return focusedNote(ev, d.profiles, d.s, d.inThread, pollBox(ev));
        if (surface === 'embed') return embedPreview(ev, d.bech ?? '', d.profiles, d.label);
        return notWired(surface);
    },
};
