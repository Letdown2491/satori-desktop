// NIP-54 wiki articles (kind:30818). Addressable by naddr (kind:pubkey:d), where `d` is a NORMALIZED
// topic slug (lowercase, spaces→dashes) so many authors can write the same article. The body is AsciiDoc
// (unlike NIP-23's Markdown), rendered by render/content.ts renderAsciiDoc. Pure metadata helpers - no DOM.
// (Forks/`fork`+`a` defer redirects are out of scope for this read-only handler.)

import type { NostrEvent } from './types.ts';
import { tag1 } from './tags.ts';

export const KIND_WIKI = 30818;

export interface WikiArticle {
    title: string;
    identifier: string;   // the `d` topic slug
    summary: string;      // optional one-line summary
    publishedAt: number;  // first-publish time, falling back to created_at
    content: string;      // AsciiDoc body
}

export function parseWiki(ev: NostrEvent): WikiArticle {
    const published = Number(tag1(ev, 'published_at'));
    // Prefer the display `title`, else the `d` slug (the topic name), else a placeholder.
    return {
        title: tag1(ev, 'title') || tag1(ev, 'd') || '(untitled)',
        identifier: tag1(ev, 'd'),
        summary: tag1(ev, 'summary'),
        publishedAt: Number.isFinite(published) && published > 0 ? published : ev.created_at,
        content: ev.content,
    };
}
