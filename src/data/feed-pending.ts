// Per-(pubkey, tab) buffer of feed events the BACKGROUND backfill fetched, so the new-notes dot and the
// click-to-load path read the SAME events. The following landing paints fast on the tight PAGE_MAX_WAIT
// budget and so can miss events on slow relays; the off-feed dot poll re-fetches those on the GENEROUS
// adaptive budget (latency is invisible there) and folds the late arrivals here. Because the dot COUNTS this
// buffer and the load RENDERS this buffer, the dot can never promise a note the load then misses - the
// "indicator fires but loads empty" bug is structurally impossible.
//
// Like the feed cache: RAW events (mutes/filters apply at count/render time, so a stale buffer stays
// correct), keyed by pubkey (no cross-account bleed), in-memory + per-process. NOT persisted - it's
// ephemeral backfill state. Bounded by a per-key event cap + TTL + an LRU cap on keys.
//
// `shown` marks events already rendered (on the landing, or by a prior dot-load) so they stop counting
// toward the dot - otherwise on-screen notes would show as "new". It's sticky-true (a re-fold can't un-show).

import type { NostrEvent } from '../nostr/types.ts';

const TTL_MS = 5 * 60_000; // an event lingers this long after it was last folded, then prunes out
const MAX_EVENTS = 300;    // per (pubkey, tab): keep the newest this many (a slow relay can't bloat it)
const MAX_KEYS = 200;      // LRU cap on (pubkey, tab) entries (multi-tenant safety)

interface Entry { ev: NostrEvent; shown: boolean; at: number }
const buf = new Map<string, Map<string, Entry>>(); // "me:tab" -> (eventId -> entry)
const key = (me: string, tab: string): string => `${me}:${tab}`;

/** Drop TTL-expired events, then trim to the newest MAX_EVENTS by created_at. */
function prune(m: Map<string, Entry>): void {
    const cutoff = Date.now() - TTL_MS;
    for (const [id, e] of m) if (e.at < cutoff) m.delete(id);
    if (m.size > MAX_EVENTS) {
        const oldestFirst = [...m.entries()].sort((a, b) => a[1].ev.created_at - b[1].ev.created_at);
        for (const [id] of oldestFirst.slice(0, m.size - MAX_EVENTS)) m.delete(id);
    }
}

/** Fold fetched events into the buffer. Pass `shown: true` for events already rendered (landing / a prior
 * load) so the dot won't re-surface them; background backfill folds with `shown: false`. shown is sticky. */
export function foldPending(me: string, tab: string, events: NostrEvent[], shown: boolean): void {
    if (!events.length) return;
    const k = key(me, tab);
    const m = buf.get(k) ?? new Map<string, Entry>();
    buf.delete(k); buf.set(k, m); // reinsert → most-recently-used (LRU order)
    const now = Date.now();
    for (const ev of events) {
        const prev = m.get(ev.id);
        m.set(ev.id, { ev, shown: shown || (prev?.shown ?? false), at: now });
    }
    prune(m);
    while (buf.size > MAX_KEYS) buf.delete(buf.keys().next().value as string);
}

/** Unshown buffered events newer than the high-water `seen`, newest-first. The dot counts these and the
 * load renders them (identical set → they can't disagree). Raw: the caller applies mutes/content filters. */
export function pendingNew(me: string, tab: string, seen: number): NostrEvent[] {
    const m = buf.get(key(me, tab));
    if (!m) return [];
    const cutoff = Date.now() - TTL_MS; // a reader (the landing) may hit a key with no recent fold to prune it
    const out: NostrEvent[] = [];
    for (const e of m.values()) if (!e.shown && e.at >= cutoff && e.ev.created_at > seen) out.push(e.ev);
    return out.sort((a, b) => b.created_at - a.created_at);
}
