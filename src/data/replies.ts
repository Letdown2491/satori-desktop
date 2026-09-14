// Reply-PRESENCE as FACES (not counts): "who is in this note/article's conversation?" - up to 3
// replier avatars, follows-first, with a numberless "+" when there are more. People, not a
// scoreboard ([[product-direction]]). This is about OTHER people's replies, so it's a targeted,
// best-effort batched query, TTL-cached. Notes: kind:1 `#e`. Articles: NIP-22 kind:1111 `#A`
// (comment authors). Both feed the same cache, keyed by note id OR canonical naddr.

import { decode } from 'nostr-tools/nip19';
import type { Pool } from './pool.ts';
import { INDEXER_RELAYS, readRelaysFor } from '../nostr/nip65.ts';
import { coalesceBatch } from './coalesce.ts';
import { trimOldest } from './json-store.ts';
import type { RelayList, NostrEvent } from '../nostr/types.ts';

export interface ReplyFaces { repliers: string[]; more: boolean; hasFollow: boolean } // up to 3 pubkeys (follows-first); more = others beyond them; hasFollow = a follow is in the convo (for the glyph dot)

const cache = new Map<string, ReplyFaces & { at: number }>();
const TTL = 5 * 60 * 1000;     // re-check at most this often (new replies arrive over time)
const LIMIT = 400;             // best-effort batch cap (a busy window can skew to a popular note; fine for a hint)
const CAP = 5000;              // keys are RELAY-sourced ids/naddrs (externally influenceable), so bound the cache
const SHOW = 3;                // faces shown before the "+"

const relaySet = (r: RelayList | null): string[] =>
    (r ? readRelaysFor(r) : INDEXER_RELAYS).slice(0, 8);

/** Cached reply-faces for a note id / article naddr (best-effort; null if unknown/expired). */
export function replyFaces(key: string): ReplyFaces | null {
    const c = cache.get(key);
    return c && Date.now() - c.at <= TTL ? { repliers: c.repliers, more: c.more, hasFollow: c.hasFollow } : null;
}

/** The repliers we're SHOWING across these keys (to hydrate their profiles → real avatars). */
export function replierPubkeys(keys: string[]): string[] {
    const out = new Set<string>();
    const now = Date.now();
    for (const k of keys) { const c = cache.get(k); if (c && now - c.at <= TTL) for (const p of c.repliers) out.add(p); }
    return [...out];
}

/** Store per-key replier sets: follows first, then others, capped to SHOW; `more` if there are extras. */
function store(keys: string[], byKey: Map<string, Set<string>>, follows: Set<string>): void {
    const at = Date.now();
    for (const k of keys) {
        const set = byKey.get(k);
        if (!set || set.size === 0) { cache.set(k, { repliers: [], more: false, hasFollow: false, at }); continue; }
        const all = [...set];
        const ordered = [...all.filter((p) => follows.has(p)), ...all.filter((p) => !follows.has(p))]; // follows first
        cache.set(k, { repliers: ordered.slice(0, SHOW), more: set.size > SHOW, hasFollow: all.some((p) => follows.has(p)), at });
    }
    trimOldest(cache, CAP);
}

const stale = (now: number, k: string): boolean => { const c = cache.get(k); return !c || now - c.at > TTL; };

// In-flight coalescing: two concurrent renders touching the same notes (e.g. the feed poller + a thread
// open, or two tabs) would each fire the LIMIT-400 presence query. Share one fetch per key instead - pure
// concurrency dedup, zero staleness (see coalesceBatch; the `run` receives only the not-in-flight keys).
const inflight = new Map<string, Promise<void>>();

/** Notes: collect repliers (kind:1 `#e`) per note id. Self is excluded (your own reply isn't "others"). */
export async function fetchReplyPresence(pool: Pool, myRelays: RelayList | null, noteIds: string[], follows: Set<string>, me: string): Promise<void> {
    const now = Date.now();
    const todo = [...new Set(noteIds)].filter((id) => stale(now, id));
    if (todo.length === 0) return;
    await coalesceBatch(inflight, todo, async (ids) => {
        const want = new Set(ids);
        const events = await pool.query(relaySet(myRelays), { kinds: [1], '#e': ids, limit: LIMIT }).catch(() => [] as NostrEvent[]);
        const byKey = new Map<string, Set<string>>();
        for (const ev of events) {
            if (ev.pubkey === me) continue;
            for (const t of ev.tags) {
                if (t[0] !== 'e' || !t[1] || !want.has(t[1])) continue;
                let set = byKey.get(t[1]); if (!set) { set = new Set(); byKey.set(t[1], set); }
                set.add(ev.pubkey);
            }
        }
        store(ids, byKey, follows);
    });
}

/** Articles: collect comment authors (kind:1111 `#A`) per article. Keyed by the canonical naddr the
 * caller passed (the `#A` tag carries the address, which we map back to the naddr). */
export async function fetchArticleReplyPresence(pool: Pool, myRelays: RelayList | null, naddrs: string[], follows: Set<string>, me: string): Promise<void> {
    const now = Date.now();
    const todo = [...new Set(naddrs)].filter((n) => n && stale(now, n));
    if (todo.length === 0) return;
    await coalesceBatch(inflight, todo, async (ids) => {
        const byAddress = new Map<string, string>(); // address → naddr
        for (const n of ids) { try { const d = decode(n); if (d.type === 'naddr') byAddress.set(`${d.data.kind}:${d.data.pubkey}:${d.data.identifier}`, n); } catch { /* skip */ } }
        if (byAddress.size === 0) return;
        const events = await pool.query(relaySet(myRelays), { kinds: [1111], '#A': [...byAddress.keys()], limit: LIMIT }).catch(() => [] as NostrEvent[]);
        const byKey = new Map<string, Set<string>>();
        for (const ev of events) {
            if (ev.pubkey === me) continue;
            for (const t of ev.tags) {
                if (t[0] !== 'A' || !t[1]) continue;
                const naddr = byAddress.get(t[1]); if (!naddr) continue;
                let set = byKey.get(naddr); if (!set) { set = new Set(); byKey.set(naddr, set); }
                set.add(ev.pubkey);
            }
        }
        store(ids, byKey, follows);
    });
}
