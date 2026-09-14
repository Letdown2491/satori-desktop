// NIP-23 long-form articles (kind:30023). Addressable by naddr (kind:pubkey:d),
// body is Markdown. Pure metadata helpers - no DOM, no network.

import type { NostrEvent } from './types.ts';
import { tag1 } from './tags.ts';

export const KIND_ARTICLE = 30023;

export interface Article {
    title: string;
    summary: string;
    image: string;
    publishedAt: number; // first-publish time (falls back to created_at)
    identifier: string;  // the `d` slug
    topics: string[];
    content: string;     // markdown body
}

export function parseArticle(ev: NostrEvent): Article {
    const published = Number(tag1(ev, 'published_at'));
    return {
        title: tag1(ev, 'title') || '(untitled)',
        summary: tag1(ev, 'summary'),
        image: tag1(ev, 'image'),
        publishedAt: Number.isFinite(published) && published > 0 ? published : ev.created_at,
        identifier: tag1(ev, 'd'),
        topics: ev.tags.filter((t) => t[0] === 't' && t[1]).map((t) => t[1]!),
        content: ev.content,
    };
}

/** Estimated reading time in whole minutes (≥1), at ~220 wpm. Counted with a char walk: the
 * split() this replaced materialized a throwaway array of every word in the full body, per card
 * render (a 100KB article = a ~15k-element array per feed card). */
export function readingMinutes(markdown: string): number {
    let words = 0;
    let inWord = false;
    for (let i = 0; i < markdown.length; i++) {
        const ws = markdown.charCodeAt(i) <= 32; // space/tab/newline - close enough for an estimate
        if (!ws && !inWord) words++;
        inWord = !ws;
    }
    return Math.max(1, Math.round(words / 220));
}
