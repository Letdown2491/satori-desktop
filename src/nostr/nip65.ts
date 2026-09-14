// NIP-65 "outbox model": relay-list parsing + author→relay routing, plus the
// .onion→Tor-proxy URL rewriting. Pure (no pool / no DOM) and unit-tested.

import type { NostrEvent, RelayList } from './types.ts';

// Bootstrap / indexer relays: a small well-known set used ONLY to discover relay
// lists (kind:10002) and profiles (kind:0), and as a fallback when a pubkey has
// no relay list. Mirrors dark-wisp-android's RelayProber.BOOTSTRAP.
export const INDEXER_RELAYS = [
    'wss://relay.damus.io',
    'wss://relay.primal.net',
    'wss://indexer.coracle.social',
    'wss://relay.nos.social',
    'wss://purplepag.es',
];

// Caps to bound connections and filter size. MAX_RELAYS was 25, which a large follow
// list (200+ people across many write relays) blows past - the greedy set-cover would
// stop at 25 and SILENTLY DROP every follow whose relays didn't make the cut, so their
// notes never appeared. Raised to 50 (the persistent daemon pools/reuses sockets, so the
// connection cost is mild) AND uncovered authors now fall back to the indexers rather than
// being dropped (see routeAuthorsToRelays). The persistent daemon makes this affordable.
const MAX_RELAYS_PER_AUTHOR = 3;
// 1x routing of a large follow list (~225) settles around 30 relays; 2x redundancy (below) needs more,
// so the cap has headroom to avoid truncating real coverage back onto the indexer fallback. The
// persistent daemon pools/reuses sockets (SimplePool), so the extra connections are mild.
const MAX_RELAYS = 70;
// Hard ceiling for the orphan tail: orphans are recovered to their OWN relays (past the soft cap above), but
// a pathological follow graph (everyone on a unique rare relay) shouldn't explode sockets - past this, the
// remaining orphans fall back to the indexers. Generous headroom over MAX_RELAYS; the daemon pools sockets.
const HARD_MAX_RELAYS = MAX_RELAYS * 2;
// Redundancy: route each author to up to this many of their write relays (not just one), so a note
// they published to only ONE of their relays still surfaces. A note has to be absent from BOTH chosen
// relays to vanish. Capped by the author's relay count and by MAX_RELAYS (the tail keeps 1x coverage).
const RELAYS_PER_AUTHOR_TARGET = 2;
export const MAX_AUTHORS_PER_FILTER = 200;

/** Split an array into MAX_AUTHORS_PER_FILTER-sized (or any-sized) slices, for author filters
 * that would otherwise exceed what relays silently truncate. */
export function chunk<T>(arr: T[], size: number): T[][] {
    const out: T[][] = [];
    for (let i = 0; i < arr.length; i += size) out.push(arr.slice(i, i + size));
    return out;
}

/** The light relay-url MAP KEY normalizer: lowercase, no trailing slash, no validation. For keying
 * caches/sets where the url is already relay-shaped (pool auth sets, seen-relays, relay-info); the
 * validating normalizeRelayUrl below stays the boundary for user-entered urls. One copy - this
 * existed inline in four files before it earned the export. */
export const relayKey = (u: string): string => u.trim().replace(/\/+$/, '').toLowerCase();

/** Normalize a relay URL for dedup/grouping: lowercase, no trailing slash. `assumeWss` prepends wss://
 * to a bare host (for user-entered relay sets), so settings + routing agree on one canonical form. */
export function normalizeRelayUrl(url: string, opts: { assumeWss?: boolean } = {}): string | null {
    try {
        let raw = url.trim();
        // Only prepend wss:// to a BARE host (no scheme at all). A wrong scheme (http://relay.onion) must NOT
        // be rewritten - prepending wss:// gave `wss://http://relay.onion`, which new URL() then parses with
        // host "http" into a dead `wss://http//relay.onion` that still got dialed. Now it falls through to the
        // protocol check below and is rejected.
        const hasScheme = /^[a-z][a-z0-9+.-]*:\/\//i.test(raw);
        if (opts.assumeWss && raw && !hasScheme) raw = 'wss://' + raw;
        const u = new URL(raw);
        if (u.protocol !== 'ws:' && u.protocol !== 'wss:') return null;
        let s = u.toString();
        if (s.endsWith('/')) s = s.slice(0, -1);
        return s.toLowerCase();
    } catch {
        return null;
    }
}

/** The relays to PUBLISH the user's own events to: their write relays, or the indexers if they have none. */
export const writeRelaysFor = (r: RelayList | null | undefined): string[] =>
    r && r.write.length ? r.write : INDEXER_RELAYS;

/** The user's OWN relay urls: read ∪ write, deduped. No indexer fallback (callers that want the indexers
 * add them - e.g. readRelaysFor below). The relays you'd browse as a timeline, and the reader-side of the
 * outbox model when resolving a single event by id/coordinate. */
export const myRelayUrls = (r: RelayList | null | undefined): string[] =>
    [...new Set([...(r?.read ?? []), ...(r?.write ?? [])])];

/** The relays to QUERY the user's own data from: read ∪ write ∪ indexers (deduped). */
export const readRelaysFor = (r: RelayList | null | undefined): string[] =>
    [...new Set([...myRelayUrls(r), ...INDEXER_RELAYS])];

/** Parse a kind:10002 event into read/write lists (NIP-65 r-tag markers). */
export function parseRelayList(event: NostrEvent): RelayList {
    const read = new Set<string>();
    const write = new Set<string>();
    for (const tag of event.tags || []) {
        if (tag[0] !== 'r' || !tag[1]) continue;
        const url = normalizeRelayUrl(tag[1]);
        if (!url) continue;
        const marker = tag[2];
        if (marker === 'read') read.add(url);
        else if (marker === 'write') write.add(url);
        else { read.add(url); write.add(url); }
    }
    return { read: [...read], write: [...write] };
}

/**
 * Route authors to the relays they write to. Each author gets up to 3 write
 * relays; authors with none go to `fallbackRelays`. A greedy set-cover then
 * bounds the relay set to MAX_RELAYS, routing each author to up to
 * RELAYS_PER_AUTHOR_TARGET (2) of their relays for redundancy - so a note they put
 * on only one of their relays still surfaces. Returns Map<relayUrl, Set<pubkey>>.
 *
 * QUALITY-AWARE: `relayScore` (default: neutral 1) weights each relay's coverage by
 * how well it actually delivers for your feed (fed from relay-latency). The greedy
 * picks by coverage x score, so a relay we've LEARNED is empty/useless loses to a
 * smaller relay that delivers - its authors get covered by their better relays, and
 * the empty relay is only picked when it's someone's ONLY option. Wasting fewer
 * slots on dead relays also fits more real authors under the cap.
 *
 * ORPHAN RECOVERY: an author whose relays didn't make the cut is NOT dumped on the
 * indexers (which may not mirror their notes) - they're routed to their OWN best
 * relay (by score). Only an author with no relay list at all falls to the indexers.
 */
export function routeAuthorsToRelays(
    relayLists: Map<string, RelayList>,
    authors: string[],
    { fallbackRelays = [] as string[], relayScore = (_r: string) => 1 }: { fallbackRelays?: string[]; relayScore?: (relay: string) => number } = {},
): Map<string, Set<string>> {
    const fallback = fallbackRelays.map((u) => normalizeRelayUrl(u)).filter((u): u is string => !!u);

    const authorRelays = new Map<string, string[]>();
    for (const author of authors) {
        const write = (relayLists.get(author)?.write ?? []).slice(0, MAX_RELAYS_PER_AUTHOR);
        authorRelays.set(author, write.length > 0 ? write : fallback);
    }

    // `want` = how many MORE relays each author still wants (capped by how many they have). An author
    // stays in the cover pool until they've been routed to RELAYS_PER_AUTHOR_TARGET relays, so the greedy
    // keeps adding a 2nd relay per author after the 1st pass - not just one-and-done.
    const want = new Map<string, number>();
    for (const author of authors) {
        const n = (authorRelays.get(author) ?? []).length;
        if (n > 0) want.set(author, Math.min(RELAYS_PER_AUTHOR_TARGET, n));
    }
    const everCovered = new Set<string>(); // authors routed to >= 1 chosen relay (so they won't vanish)
    const chosen = new Map<string, Set<string>>();

    while (want.size > 0 && chosen.size < MAX_RELAYS) {
        // Coverage counts only authors who still WANT another relay, and only over relays not yet chosen.
        const coverage = new Map<string, string[]>();
        for (const [author, remaining] of want) {
            if (remaining <= 0) continue;
            for (const relay of authorRelays.get(author) ?? []) {
                if (chosen.has(relay)) continue; // this relay is already in the set
                (coverage.get(relay) ?? coverage.set(relay, []).get(relay)!).push(author);
            }
        }
        if (coverage.size === 0) break;

        // Pick by coverage WEIGHTED by relay quality: a big empty relay (score ~0.15) loses to a smaller
        // one that delivers. With the default neutral score (1) this is exactly highest-coverage-first.
        let best: string | null = null;
        let bestScore = 0;
        let bestAuthors: string[] = [];
        for (const [relay, covered] of coverage) {
            const score = covered.length * relayScore(relay);
            if (score > bestScore) { best = relay; bestScore = score; bestAuthors = covered; }
        }
        if (!best) break;
        chosen.set(best, new Set(bestAuthors));
        for (const author of bestAuthors) {
            everCovered.add(author);
            const remaining = want.get(author)! - 1;
            if (remaining <= 0) want.delete(author); else want.set(author, remaining);
        }
    }

    // Don't silently drop the tail: any author with ZERO chosen relays (theirs didn't make the cut) is
    // recovered. Their notes live on THEIR write relays, so route them to their best one (by score) rather
    // than dumping them on the indexers, which may not mirror the note - better their real relay than an
    // approximation. A relayScore of ZERO means "never pick" (read-dead: refuses REQs) - recovering an
    // orphan to one recovers nothing, so those are excluded here too; an author whose every write relay
    // is dead falls to the indexers like an author with no list at all.
    const addAuthor = (relay: string, author: string): void => {
        const set = chosen.get(relay) ?? new Set<string>();
        set.add(author);
        chosen.set(relay, set);
    };
    for (const author of authors) {
        if (everCovered.has(author)) continue;
        const write = (relayLists.get(author)?.write ?? []).slice(0, MAX_RELAYS_PER_AUTHOR).filter((r) => relayScore(r) > 0);
        if (write.length) {
            let best = write[0]!;
            for (const r of write) if (relayScore(r) > relayScore(best)) best = r;
            // Recover to their real relay - unless it'd be a NEW socket past the hard ceiling, then indexers.
            if (chosen.has(best) || chosen.size < HARD_MAX_RELAYS) { addAuthor(best, author); continue; }
        }
        for (const relay of fallback) addAuthor(relay, author); // no relay list, or past the ceiling
    }

    return chosen;
}

// --- Relay URL mapping -----------------------------------------------------
// In the browser SPA, .onion relays are tunnelled through a same-origin Tor proxy
// (a browser can't open .onion WebSockets). This server port routes them at the
// WebSocket layer instead - `data/ws-tor.ts` adds a SOCKS5 agent for .onion hosts
// when TOR_SOCKS is set - so URLs pass through unchanged here (identity). With no
// Tor configured, onion relays simply fail to connect, which SimplePool tolerates.

export function toPoolUrl(url: string): string {
    return url;
}

export function fromPoolUrl(url: string): string {
    const i = url.indexOf('/tor?target=');
    if (i === -1) return url;
    try { return decodeURIComponent(url.slice(i + '/tor?target='.length)); } catch { return url; }
}

export const toPoolUrls = (urls: string[]): string[] => urls.map(toPoolUrl);
