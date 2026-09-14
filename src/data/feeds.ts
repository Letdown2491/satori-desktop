// Feed builders. Following = the outbox model over people you follow. Followers =
// the same machinery over people who follow *you* (discovered via kind:3 #p). All paginate with an
// `until` cursor.

import type { Pool } from './pool.ts';
import type { NostrEvent, RelayList } from '../nostr/types.ts';
import { INDEXER_RELAYS, MAX_AUTHORS_PER_FILTER, chunk, routeAuthorsToRelays } from '../nostr/nip65.ts';
import { relayQuality, isReadDead } from './relay-latency.ts';
import { HEX64, isAddressable, tag1 } from '../nostr/tags.ts';
import { notFakePodcast } from '../nostr/nipf4.ts';
import { isExpired } from '../nostr/nip40.ts';
import { isDeletedEvent } from './deletions.ts';
import { coalesceOne } from './coalesce.ts';
import { fetchRelayLists } from './relays.ts';
import { seenRelaysFor } from './seen-relays.ts';
import { localReadMode, localRelayUrl } from '../local-relay.ts';

export interface FeedRoute {
    authors: string[];
    route: Map<string, Set<string>>;
}

const MAX_FOLLOWERS = 200; // cap follower discovery for performance

/** Newest-first, deduped, capped. Addressable kinds (30000-39999) collapse by their (kind,pubkey,d)
 * COORDINATE keeping the newest edit - the outbox fan-out merges across relays, so a stale version and the
 * edited one arrive with DIFFERENT ids and would otherwise BOTH show (NIP-01: only the latest per coordinate
 * is retained). Plain events dedup by id. Sort first so the first kept per key is the newest. */
function dedupeNewest(events: NostrEvent[], limit: number): NostrEvent[] {
    const key = (e: NostrEvent): string => isAddressable(e.kind) ? `${e.kind}:${e.pubkey}:${tag1(e, 'd')}` : e.id;
    const seen = new Set<string>();
    return events
        .sort((a, b) => b.created_at - a.created_at)
        .filter((e) => { const k = key(e); return seen.has(k) ? false : seen.add(k); })
        .slice(0, limit);
}

function mergeNewest(lists: NostrEvent[][], limit: number): NostrEvent[] {
    return dedupeNewest(lists.flat(), limit);
}

/** People you follow → their write relays, grouped per relay (computed once). */
export async function buildFollowsRoute(pool: Pool, me: string, myRelays: RelayList): Promise<FeedRoute> {
    // Your OWN follow list (kind:3): read the COMPLETE set (private relay unioned in) so Only mode can't
    // serve a partial/stale copy that would mis-route the feed - and a future edit can't clobber it.
    let contacts = await pool.get(myRelays.write, { kinds: [3], authors: [me] }, undefined, { complete: true }).catch(() => null);
    if (!contacts) contacts = await pool.get(INDEXER_RELAYS, { kinds: [3], authors: [me] }, undefined, { complete: true }).catch(() => null);
    if (!contacts) return { authors: [], route: new Map() };
    const authors = contacts.tags
        .filter((t) => t[0] === 'p' && HEX64.test(t[1] || ''))
        .map((t) => (t[1] as string).toLowerCase()) // NIP-01: filters match lowercase hex; an uppercase p-tag would silently drop the follow
        .filter((pk) => pk !== me);
    return routeFor(pool, authors);
}

/** People who follow you → their write relays. Followers are discovered by the
 * contact lists (kind:3) that tag you; approximate (only those on queried relays)
 * and capped for performance. */
export async function buildFollowersRoute(pool: Pool, me: string, myRelays: RelayList): Promise<FeedRoute> {
    const relays = [...new Set([...INDEXER_RELAYS, ...myRelays.read])];
    const events = await pool.query(relays, { kinds: [3], '#p': [me], limit: 300 }).catch(() => []);
    const seen = new Set<string>();
    const followers: string[] = [];
    for (const ev of events) {
        if (ev.pubkey === me || seen.has(ev.pubkey)) continue;
        seen.add(ev.pubkey);
        followers.push(ev.pubkey);
        if (followers.length >= MAX_FOLLOWERS) break;
    }
    return routeFor(pool, followers);
}

async function routeFor(pool: Pool, authors: string[]): Promise<FeedRoute> {
    if (authors.length === 0) return { authors, route: new Map() };
    const local = localRelayUrl();
    // 'only': the local relay IS the whole feed - one full-coverage route, no outbox fan-out and no
    // kind:10002 discovery. A follow whose notes the aggregator doesn't carry is simply absent, which
    // is exactly the signal you want when testing a custom relay in isolation.
    if (local && localReadMode() === 'only') {
        console.log(`[feeds] routing ${authors.length} authors EXCLUSIVELY via local relay ${local}`);
        return { authors, route: new Map([[local, new Set(authors)]]) };
    }
    const relayLists = await fetchRelayLists(pool, INDEXER_RELAYS, authors);
    // Score 0 = never pick: a read-dead relay (refuses REQs - a write-only blaster in follows' lists,
    // an auth/pay wall) can't be rescued by coverage weighting; its authors route to their OTHER relays.
    const route = routeAuthorsToRelays(relayLists, authors, { fallbackRelays: INDEXER_RELAYS, relayScore: (r) => isReadDead(r) ? 0 : relayQuality(r) });
    // 'add': the local relay covers everyone alongside the outbox - queried ONCE for all authors (not
    // per shard), so an aggregator can serve the whole feed from one socket while the outbox fills gaps.
    if (local && localReadMode() === 'add') route.set(local, new Set(authors));
    console.log(`[feeds] routing ${authors.length} authors across ${route.size} relays`);
    return { authors, route };
}

/** One page of a routed feed (Following / Followers). `until` bounds it to older
 * events; `kinds` defaults to notes + polls (pass [30023] for the long-form feed). */
export async function fetchRoutedPage(pool: Pool, route: Map<string, Set<string>>, limit: number, until?: number, kinds: number[] = [1, 1068], since?: number, budget?: 'page' | 'adaptive'): Promise<NostrEvent[]> {
    const queries: Promise<NostrEvent[]>[] = [];
    for (const [relay, authorSet] of route) {
        // Routes are session-cached, so a relay learned read-dead AFTER the route was built keeps its
        // shard until the next rebuild - skip it here so it stops costing a socket the moment it's
        // known. Its results were empty anyway; the authors' second leg / dot poll covers them.
        if (isReadDead(relay)) continue;
        for (const authorChunk of chunk([...authorSet], MAX_AUTHORS_PER_FILTER)) {
            const filter = { kinds, authors: authorChunk, limit: limit * 2, ...(until ? { until } : {}), ...(since ? { since } : {}) };
            queries.push(pool.query([relay], filter, { fast: true, profile: true, budget }).catch((err) => { console.warn(`[feeds] query failed for ${relay}:`, err?.message ?? err); return []; }));
        }
    }
    // Drop kind:54 events with no audio - not podcasts (kind 54 is contested; see nipf4.notFakePodcast).
    return mergeNewest((await Promise.all(queries)).map((list) => list.filter(notFakePodcast)), limit);
}

/** One page of a SINGLE relay's timeline (the "browse a relay" feed): newest-first events of the user's
 * feed kinds, paginated by `until`. No outbox routing - it's deliberately "what THIS relay carries". */
export async function fetchRelayPage(pool: Pool, url: string, limit: number, until: number | undefined, kinds: number[]): Promise<NostrEvent[]> {
    const filter = { kinds, limit, ...(until ? { until } : {}) };
    const raw = await pool.query([url], filter, { fast: true }).catch((err) => { console.warn(`[relay-feed] query failed for ${url}:`, err?.message ?? err); return [] as NostrEvent[]; });
    return mergeNewest([raw.filter(notFakePodcast)], limit);
}

// A resolved event is immutable, so a hit caches hard (30 min); a MISS is cached briefly (3 min) - the
// event may land on a relay later, but without this the same unresolvable quoted note re-pays the full
// get() timeout on every feed/thread render and scroll-back (the embed-storm that dominated the logs).
const EVENT_TTL = 30 * 60_000;
const EVENT_MISS_TTL = 3 * 60_000;
const EVENT_CAP = 3000;
const eventCache = new Map<string, { ev: NostrEvent | null; at: number }>();
function rememberEvent(id: string, ev: NostrEvent | null): NostrEvent | null {
    if (eventCache.size >= EVENT_CAP) { const oldest = eventCache.keys().next().value; if (oldest) eventCache.delete(oldest); }
    eventCache.set(id, { ev, at: Date.now() });
    return ev;
}

// Coalesce concurrent fetches for the same id (two renders quoting the same note - the embed-storm case)
// onto one round-trip, matching the in-flight idiom of the other fetch caches (dm-routing, emoji-sets).
const inflightEvents = new Map<string, Promise<NostrEvent | null>>();

/** Fetch a single event by id (for embedded reply parents / quoted notes). `opts.maxWait` shortens the
 * clearnet get for best-effort callers (embeds); results are cached (immutable hit / brief miss) and
 * concurrent fetches for the same id are coalesced. */
export async function fetchEvent(pool: Pool, id: string, relayHints: string[] = [], author?: string, opts: { maxWait?: number } = {}): Promise<NostrEvent | null> {
    const hit = eventCache.get(id);
    // NIP-40 + NIP-09 re-check on hits: the pool never returns expired or deleted events, but a
    // cached one can cross its expiration - or have its deletion arrive - inside the 30-min TTL.
    if (hit && Date.now() - hit.at < (hit.ev ? EVENT_TTL : EVENT_MISS_TTL)) return hit.ev && (isExpired(hit.ev) || isDeletedEvent(hit.ev)) ? null : hit.ev;
    return coalesceOne(inflightEvents, id, () => resolveEvent(pool, id, relayHints, author, opts.maxWait));
}

/** Cache-only lookup of a recently-seen event (NO network fetch). Used at write time to enrich a
 * bookmark/pin tag with the note's author, so it resolves via outbox later. Null on a miss or an
 * expired entry. */
export function cachedEvent(id: string): NostrEvent | null {
    const hit = eventCache.get(id);
    if (!hit || (hit.ev && (isExpired(hit.ev) || isDeletedEvent(hit.ev)))) return null; // NIP-40 + NIP-09 re-check (see fetchEvent)
    return Date.now() - hit.at < (hit.ev ? EVENT_TTL : EVENT_MISS_TTL) ? hit.ev : null;
}

async function resolveEvent(pool: Pool, id: string, relayHints: string[], author: string | undefined, maxWait?: number): Promise<NostrEvent | null> {
    // Outbox model (NIP-65): the author's OWN write relays are the canonical home of their events, so
    // query those (+ any nevent relay hints) FIRST - the big indexers are a best-effort aggregator that
    // can miss. The relay list is cached per pubkey, so for a followed author this adds no round-trip.
    if (author) {
        const writes = (await fetchRelayLists(pool, INDEXER_RELAYS, [author]).catch(() => null))?.get(author)?.write ?? [];
        // ...plus relays we've empirically seen this author on (finds events on relays they don't advertise).
        // Read-dead relays (blasters/walls) are dropped - a get against one just burns its share of the race.
        const primary = [...new Set([...relayHints, ...writes, ...seenRelaysFor(author)])].filter((u) => u && !isReadDead(u));
        if (primary.length) {
            const ev = await pool.get(primary, { ids: [id] }, maxWait, { resolve: true }).catch(() => null);
            if (ev) return rememberEvent(id, ev);
        }
    }
    const relays = [...new Set([...relayHints, ...INDEXER_RELAYS])].filter(Boolean);
    return rememberEvent(id, await pool.get(relays, { ids: [id] }, maxWait, { resolve: true }).catch(() => null));
}

// Addressable (kind:pubkey:d) fetches, the naddr twin of the id cache above. Addressables are
// EDITABLE - the newest version wins - so a hit lives minutes, not the immutable 30: long enough to
// stop every feed revisit re-paying a relay round-trip per quoted article/wiki (the same storm the
// id cache fixed for notes), short enough that an edit surfaces promptly. Reader pages (/a/) stay
// uncached - a full read wants the freshest version.
const ADDR_TTL = 5 * 60_000;
const ADDR_CAP = 500;
const addrCache = new Map<string, { ev: NostrEvent | null; at: number }>();
const inflightAddr = new Map<string, Promise<NostrEvent | null>>();

/** Fetch an addressable event by coordinate (for naddr embeds), cached + coalesced. */
export async function fetchAddressable(pool: Pool, kind: number, pubkey: string, identifier: string, relays: string[], opts: { maxWait?: number } = {}): Promise<NostrEvent | null> {
    const key = `${kind}:${pubkey}:${identifier}`;
    const hit = addrCache.get(key);
    if (hit && Date.now() - hit.at < (hit.ev ? ADDR_TTL : EVENT_MISS_TTL)) return hit.ev && isExpired(hit.ev) ? null : hit.ev; // NIP-40 re-check
    return coalesceOne(inflightAddr, key, async () => {
        const ev = await pool.get(relays, { kinds: [kind], authors: [pubkey], '#d': [identifier] }, opts.maxWait, { resolve: true }).catch(() => null);
        if (addrCache.size >= ADDR_CAP) { const oldest = addrCache.keys().next().value; if (oldest) addrCache.delete(oldest); }
        addrCache.set(key, { ev, at: Date.now() });
        return ev;
    });
}

/** Fetch many events by id (e.g. bookmarked notes), deduped. */
export async function fetchEventsByIds(pool: Pool, ids: string[], relayHints: string[] = []): Promise<NostrEvent[]> {
    if (ids.length === 0) return [];
    const relays = [...new Set([...relayHints, ...INDEXER_RELAYS])].filter(Boolean);
    const raw = await pool.query(relays, { ids }, { resolve: true }).catch(() => []);
    const seen = new Set<string>();
    return raw.filter((e) => (seen.has(e.id) ? false : seen.add(e.id)));
}

/** Direct replies to a note (kind:1 referencing it), oldest first. The note's
 * own parent context is already shown inline by the Note component, so a thread
 * is just the focused note + this - one query, fast. */
export async function fetchReplies(pool: Pool, noteId: string, relayHints: string[] = []): Promise<NostrEvent[]> {
    const relays = [...new Set([...relayHints, ...INDEXER_RELAYS])];
    // NIP-10 kind:1 replies (lowercase `e` = this note) PLUS NIP-22 kind:1111 comments: by lowercase `e`
    // (direct children of this note/comment) and by uppercase `E` (the whole comment subtree rooted here -
    // catches nested comments whose immediate `e` parent is another comment). Merge + dedupe across both.
    const [direct, rooted] = await Promise.all([
        pool.query(relays, { kinds: [1, 1111], '#e': [noteId], limit: 100 }, { fast: true, resolve: true }).catch(() => [] as NostrEvent[]),
        pool.query(relays, { kinds: [1111], '#E': [noteId], limit: 100 }, { fast: true, resolve: true }).catch(() => [] as NostrEvent[]),
    ]);
    const seen = new Set<string>();
    return [...direct, ...rooted]
        .filter((e) => e.id !== noteId && (seen.has(e.id) ? false : seen.add(e.id)))
        .sort((a, b) => a.created_at - b.created_at);
}

/** A user's notes, from their own write relays. `until` pages into older notes. */
export async function fetchAuthorNotes(pool: Pool, pubkey: string, kinds: number[], limit = 30, until?: number): Promise<NostrEvent[]> {
    if (kinds.length === 0) return []; // the viewer disabled every kind for profiles - nothing to fetch
    const lists = await fetchRelayLists(pool, INDEXER_RELAYS, [pubkey]).catch(() => new Map<string, RelayList>());
    const write = lists.get(pubkey)?.write ?? [];
    const relays = write.length ? write : INDEXER_RELAYS;
    // `kinds` is the VIEWER's profile content-type prefs (notes/polls + whichever rich kinds they enabled).
    // The manifest renders each kind's card, so no per-kind branch here - just the viewer-chosen query.
    const raw = await pool.query(relays, { kinds, authors: [pubkey], limit, ...(until ? { until } : {}) }, { fast: true, resolve: true }).catch(() => []);
    return dedupeNewest(raw.filter(notFakePodcast), limit);
}
