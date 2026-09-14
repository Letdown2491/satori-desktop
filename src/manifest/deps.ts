// Shared render deps the registry hands to every Satori kind handler, plus the unwired-surface guard.
// Lives in its own module so each kind handler (manifest/{note,poll,article,picture}.ts) imports the
// same shape - no per-kind coupling, no import cycle (types only + a tiny helper).

import type { Surface } from './registry.ts';
import type { ProfileMap } from '../render/util.ts';
import type { Session } from '../session.ts';
import type { NoteOpts } from '../render/note.ts';

export interface SatoriDeps {
    profiles?: ProfileMap;
    s?: Session;       // timeline/focused/reader: media prefs, signing mode, engagement state
    opts?: NoteOpts;   // timeline note-render options (pending, depth, mute, hideParent, inThread)
    inThread?: string; // focused: this thread's nevent, so reply buttons append back here
    bech?: string;     // embed: the embedded entity's bech32 (note/nevent/naddr) - link + lazy-load key
    naddr?: string;    // embed: an article's addressable form
    label?: string;    // embed: the note-embed label ("↗ quoted note" / "↩ in reply to")
}

/** A surface a handler doesn't implement (e.g. a note has no 'reader' page). Loud, not silent. */
export const notWired = (surface: Surface): never => {
    throw new Error(`satori handler: surface '${surface}' not wired for this kind`);
};
