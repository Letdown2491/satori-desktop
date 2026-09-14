// NIP-34 git repositories. kind:30617 = repository announcement, addressable by naddr (d = the repo id).
// All metadata lives in tags (name, description, clone/web urls, maintainers, topics); content is unused.
// This is the READ side: Satori renders a repo as a card + a detail page + an inline reference. Patches
// (kind 1617) and issues (kind 1621) are a later phase. Pure metadata helpers - no DOM, no network.

import type { NostrEvent } from './types.ts';
import { tag1, tagValues } from './tags.ts';

export const KIND_REPO = 30617;

export interface Repo {
    identifier: string;    // the `d` repo id
    name: string;
    description: string;
    web: string[];         // human browse urls
    clone: string[];       // git clone urls (https / git / ssh)
    relays: string[];      // relays carrying the repo's patches/issues
    maintainers: string[]; // maintainer pubkeys
    topics: string[];      // `t` hashtags
    euc: string;           // earliest-unique-commit id (repo identity across maintainers), or ''
}

export function parseRepo(ev: NostrEvent): Repo {
    return {
        identifier: tag1(ev, 'd'),
        name: tag1(ev, 'name') || tag1(ev, 'd') || '(unnamed repo)',
        description: tag1(ev, 'description'),
        web: tagValues(ev, 'web'),
        clone: tagValues(ev, 'clone'),
        relays: tagValues(ev, 'relays'),
        // NIP-34 lists maintainers in a `maintainers` tag; also accept `p` tags. Deduped, author-agnostic.
        maintainers: [...new Set([...tagValues(ev, 'maintainers'), ...ev.tags.filter((t) => t[0] === 'p' && t[1]).map((t) => t[1]!)])],
        topics: tagValues(ev, 't'),
        euc: ev.tags.find((t) => t[0] === 'r' && t[2] === 'euc')?.[1] ?? '',
    };
}
