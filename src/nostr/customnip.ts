// Custom NIP (kind:30817) - a NUD ("Nostr Unofficial Document"): a community-authored NIP. Addressable
// by naddr (kind:pubkey:d), the body is Markdown, exactly like a NIP-23 article, plus a `title` and zero
// or more `k` tags naming the event kinds the NIP defines (number + human name). We render it like an
// article (reader + rows + embed) with those defined-kinds surfaced. Pure metadata helpers - no DOM.

import type { NostrEvent } from './types.ts';
import { tag1 } from './tags.ts';

export const KIND_CUSTOM_NIP = 30817;

/** A kind this NIP defines, from a `["k", "<number>", "<name>"]` tag (name optional). */
export interface DefinedKind { num: string; name: string }

export interface CustomNip {
    title: string;
    identifier: string;   // the `d` slug
    summary: string;      // the `alt` tag (NUDs use it as a one-line summary)
    publishedAt: number;  // first-publish time, falling back to created_at
    kinds: DefinedKind[]; // the `k` tags - the event kinds this NIP defines
    content: string;      // markdown body
}

export function parseCustomNip(ev: NostrEvent): CustomNip {
    const published = Number(tag1(ev, 'published_at'));
    return {
        title: tag1(ev, 'title') || '(untitled)',
        identifier: tag1(ev, 'd'),
        summary: tag1(ev, 'alt'),
        publishedAt: Number.isFinite(published) && published > 0 ? published : ev.created_at,
        kinds: ev.tags.filter((t) => t[0] === 'k' && t[1]).map((t) => ({ num: t[1]!, name: t[2] ?? '' })),
        content: ev.content,
    };
}
