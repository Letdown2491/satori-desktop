// NIP-10 threading: find the note a reply is answering.

import type { NostrEvent } from './types.ts';
import { quotedIds } from './tags.ts';

/** If a note is a reply, return the parent { id, relays } to embed as context -
 * the "reply"-marked e-tag, else "root", else (legacy, no markers) the last
 * e-tag. Returns null for non-replies.
 *
 * "mention"-marked e-tags and quoted events (`q` tags, or the older mention
 * convention) are references, NOT the note being replied to - excluding them
 * keeps a quote from being misread as a reply (NIP-10 / NIP-18). */
export function replyParent(note: NostrEvent): { id: string; relays: string[] } | null {
    const tags = note.tags || [];
    const quoted = quotedIds(note);
    const eTags = tags.filter((t) => t[0] === 'e' && t[1] && t[3] !== 'mention');
    const tag = eTags.find((t) => t[3] === 'reply')
        || eTags.find((t) => t[3] === 'root')
        || [...eTags].reverse().find((t) => !t[3] && !quoted.has(t[1]!)); // legacy: last unmarked, non-quote (eTags guarantees t[1])
    if (!tag || !tag[1]) return null;
    return { id: tag[1], relays: tag[2] ? [tag[2]] : [] };
}

/** The thread ROOT a reply belongs to - the "root"-marked e-tag, else the reply target. For a PRIVATE
 * reply this is the PUBLIC note the conversation hangs off (always published, so a linkable /t/), unlike
 * the immediate parent which, in a private sub-conversation, is an UNPUBLISHED private reply (a dead link). */
export function replyRoot(note: NostrEvent): { id: string; relays: string[] } | null {
    const tags = note.tags || [];
    const root = tags.find((t) => t[0] === 'e' && t[1] && t[3] === 'root');
    if (root && root[1]) return { id: root[1], relays: root[2] ? [root[2]] : [] };
    // No explicit root marker. Under the deprecated POSITIONAL scheme the root is the FIRST e-tag (the reply
    // is the last), so for a multi-level unmarked thread we must NOT collapse the root onto the immediate
    // parent - take the first non-mention, non-quote e-tag. For a direct reply (a single e-tag) first == last,
    // so the root correctly IS the parent.
    const quoted = quotedIds(note);
    const first = tags.find((t) => t[0] === 'e' && t[1] && t[3] !== 'mention' && !quoted.has(t[1]));
    if (first && first[1]) return { id: first[1], relays: first[2] ? [first[2]] : [] };
    return replyParent(note); // no usable e-tags
}
