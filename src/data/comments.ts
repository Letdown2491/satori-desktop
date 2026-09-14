// Fetch the discussion under an article (Satori's data/comments.ts): NIP-22
// comments (kind:1111 by the article's root `A` tag) + legacy kind:1 replies
// (`a` tag). Resolves the author's relays so comments hosted there are found.

import type { Pool } from './pool.ts';
import type { NostrEvent, RelayList } from '../nostr/types.ts';
import { INDEXER_RELAYS } from '../nostr/nip65.ts';
import { fetchRelayLists } from './relays.ts';
import { KIND_COMMENT } from '../nostr/nip22.ts';

export async function fetchArticleComments(pool: Pool, address: string, authorPubkey: string, relayHints: string[] = []): Promise<NostrEvent[]> {
    const lists = await fetchRelayLists(pool, INDEXER_RELAYS, [authorPubkey]).catch(() => new Map<string, RelayList>());
    const write = lists.get(authorPubkey)?.write ?? [];
    const relays = [...new Set([...relayHints, ...write, ...INDEXER_RELAYS])].filter(Boolean);
    const [comments, legacy] = await Promise.all([
        pool.query(relays, { kinds: [KIND_COMMENT], '#A': [address], limit: 200 }).catch(() => [] as NostrEvent[]),
        pool.query(relays, { kinds: [1], '#a': [address], limit: 200 }).catch(() => [] as NostrEvent[]),
    ]);
    const seen = new Set<string>();
    const out: NostrEvent[] = [];
    for (const e of [...comments, ...legacy]) if (!seen.has(e.id)) { seen.add(e.id); out.push(e); }
    return out;
}
