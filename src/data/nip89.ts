// NIP-89 handler discovery: find apps that declare support for a kind (kind:31990 with #k=kind).
// Cached per kind - handler announcements change rarely, and this only runs when you actually view
// an unknown-kind card (lazy /handlers hydration), not on the hot feed path.

import type { Pool } from './pool.ts';
import type { NostrEvent } from '../nostr/types.ts';
import { KIND_HANDLER_INFO, parseHandler, type HandlerInfo } from '../nostr/nip89.ts';

const TTL_MS = 30 * 60 * 1000;
const cache = new Map<number, { handlers: HandlerInfo[]; at: number }>();

/** Web handlers that declare support for `kind` (newest per author, parsed, web-capable only). */
export async function fetchHandlers(pool: Pool, relays: string[], kind: number): Promise<HandlerInfo[]> {
    const hit = cache.get(kind);
    if (hit && Date.now() - hit.at < TTL_MS) return hit.handlers;
    // Privacy note: this `#k` query tells the queried relay WHICH KIND you're viewing (not the event
    // id). Bounded: it's Tor-routed like every query, fires only on the cold unknown-kind path, and
    // the per-kind cache below collapses it to one lookup per kind per TTL.
    const evs = await pool.query(relays, { kinds: [KIND_HANDLER_INFO], '#k': [String(kind)], limit: 40 }).catch(() => [] as NostrEvent[]);
    const newest = new Map<string, NostrEvent>(); // kind:31990 is addressable - keep the latest per author
    for (const ev of evs) { const cur = newest.get(ev.pubkey); if (!cur || ev.created_at > cur.created_at) newest.set(ev.pubkey, ev); }
    const handlers = [...newest.values()].map(parseHandler).filter((h): h is HandlerInfo => !!h);
    cache.set(kind, { handlers, at: Date.now() });
    return handlers;
}
