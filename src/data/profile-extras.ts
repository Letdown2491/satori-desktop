// Profile extras: an author's pinned items (NIP-51 kind:10001) and their long-form
// articles (kind:30023) - the two strips Satori shows above the notes feed. Both
// route through the author's NIP-65 write relays (where their events actually live,
// not the indexers). 1:1 port of Satori's loadPinnedItems / fetchAuthorArticles.

import type { Pool } from './pool.ts';
import { fetchRelayLists } from './relays.ts';
import { fetchEventsByIds } from './feeds.ts';
import { INDEXER_RELAYS } from '../nostr/nip65.ts';
import { KIND_ARTICLE } from '../nostr/nip23.ts';
import type { NostrEvent, RelayList } from '../nostr/types.ts';

const KIND_PIN = 10001;

/** The author's write relays (where their notes/articles live), or the indexers. */
async function authorWriteRelays(pool: Pool, pubkey: string): Promise<string[]> {
    const lists = await fetchRelayLists(pool, INDEXER_RELAYS, [pubkey]).catch(() => new Map<string, RelayList>());
    const write = lists.get(pubkey)?.write ?? [];
    return write.length ? write : INDEXER_RELAYS;
}

/** An author's long-form articles (kind:30023), newest per `d` identifier, newest first. */
export async function fetchAuthorArticles(pool: Pool, pubkey: string, limit = 20): Promise<NostrEvent[]> {
    const relays = await authorWriteRelays(pool, pubkey);
    const raw = await pool.query(relays, { kinds: [KIND_ARTICLE], authors: [pubkey], limit }, { fast: true }).catch(() => []);
    const byD = new Map<string, NostrEvent>(); // replaceable: keep newest per identifier
    for (const ev of raw) {
        const d = ev.tags.find((t) => t[0] === 'd')?.[1] ?? '';
        const cur = byD.get(d);
        if (!cur || ev.created_at > cur.created_at) byD.set(d, ev);
    }
    return [...byD.values()].sort((a, b) => b.created_at - a.created_at);
}

/** An author's pinned items (NIP-51 kind:10001), resolved to events IN PIN ORDER. */
export async function fetchPinnedItems(pool: Pool, pubkey: string): Promise<{ notes: NostrEvent[]; articles: NostrEvent[] }> {
    const relays = await authorWriteRelays(pool, pubkey);
    const pinList = await pool.get(relays, { kinds: [KIND_PIN], authors: [pubkey] }).catch(() => null);
    return resolveListItems(pool, pinList?.tags ?? [], relays);
}

/** Resolve the `e` (note ids) and `a` (article addresses) tags of a NIP-51 list
 * (pins, bookmarks, …) into events, preserving list order for notes. Shared by the
 * pinned strip and the bookmarks view. */
export async function resolveListItems(pool: Pool, tags: string[][], relays: string[]): Promise<{ notes: NostrEvent[]; articles: NostrEvent[] }> {
    const noteTags = tags.filter((t) => t[0] === 'e' && t[1]);
    const noteIds = noteTags.map((t) => t[1]!);
    const addresses = tags.filter((t) => t[0] === 'a' && t[1]).map((t) => t[1]!);
    if (noteIds.length === 0 && addresses.length === 0) return { notes: [], articles: [] };

    // Outbox: bookmarked notes/articles are authored by OTHERS and live on THEIR write relays. Resolve
    // those (batched) - note authors from the NIP-51 e-tag author field (t[3]), article authors from the
    // `a` coordinate - and prefer them, with the e-tag relay hints (t[2]) and the caller's relays as
    // fallback. The relay-list cache makes this cheap for anyone already routed.
    const noteAuthors = noteTags.filter((t) => t[3]).map((t) => t[3]!);
    const articleAuthors = addresses.map((a) => a.split(':')[1]).filter(Boolean) as string[];
    const lists = (noteAuthors.length || articleAuthors.length)
        ? await fetchRelayLists(pool, INDEXER_RELAYS, [...new Set([...noteAuthors, ...articleAuthors])]).catch(() => new Map<string, RelayList>())
        : new Map<string, RelayList>();
    const writesOf = (pk: string): string[] => lists.get(pk)?.write ?? [];

    const noteRelays = [...new Set([...noteAuthors.flatMap(writesOf), ...noteTags.filter((t) => t[2]).map((t) => t[2]!), ...relays])];
    const fetched = noteIds.length ? await fetchEventsByIds(pool, noteIds, noteRelays).catch(() => []) : [];
    const byId = new Map(fetched.map((n) => [n.id, n]));
    const notes = noteIds.map((id) => byId.get(id)).filter((n): n is NostrEvent => !!n); // keep list order

    // Resolve every `a`-tag article concurrently (each from its author's outbox first); a
    // sequential loop here serialized K relay round-trips, painful over Tor. Order preserved.
    const resolved = await Promise.all(addresses.map((a) => {
        const [kind, pk, ident] = a.split(':');
        if (kind !== String(KIND_ARTICLE) || !pk || !ident) return Promise.resolve(null);
        const aRelays = [...new Set([...writesOf(pk), ...relays])];
        return pool.get(aRelays, { kinds: [KIND_ARTICLE], authors: [pk], '#d': [ident] }).catch(() => null);
    }));
    const articles = resolved.filter((a): a is NostrEvent => !!a);
    return { notes, articles };
}
