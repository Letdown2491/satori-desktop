// Brief per-(pubkey, tab) cache of a feed's LANDING page, so back-nav / tab re-visits skip the
// relay query and render instantly - the biggest perceived-perf lever for the home base.
//
// We cache the RAW fetched events, not rendered HTML: filters, mutes, and like/zap glyphs all
// apply at RENDER time, so a re-render from cache stays correct even if you change a filter or
// mute between visits (it just re-derives from the same raw events). Only the `following` LANDING
// page and the promoted content-type timelines (`type:<id>`, e.g. articles) are cached; scroll
// pages aren't (you rarely scroll-then-revisit). Your own posts aren't in your following/timeline
// feed (the route excludes you), so posting needs NO invalidation - the only staleness is OTHERS'
// new posts within the TTL, which the new-notes dot/poller already surfaces (fresh, uncached).
// Keyed by pubkey, so an account switch can't read a prior account's feed. Per-process.

import type { NostrEvent } from '../nostr/types.ts';

// Per-source TTL: `following` is your live note timeline (short 25s - others' new posts matter); a promoted
// content-type timeline (`type:<id>`, keyed like the source) moves far slower (articles publish on a daily/
// weekly cadence, not by-the-minute), so 90s is plenty and spares the re-fetch on a re-visit. Sources with
// no TTL here (relay browse, followers) aren't cached (get/put simply no-op).
const TTL_MS: Record<string, number> = { following: 25_000 };
const TYPE_TTL_MS = 90_000; // a promoted content-type timeline (the old longform cadence)
const ttlFor = (src: string): number => (src.startsWith('type:') ? TYPE_TTL_MS : (TTL_MS[src] ?? 0));
const cache = new Map<string, { events: NostrEvent[]; at: number }>();
const key = (me: string, src: string): string => `${me}:${src}`;

/** The cached landing-page events for (me, src) if still fresh, else null. */
export function cachedFeed(me: string, src: string): NostrEvent[] | null {
    const ttl = ttlFor(src);
    if (!ttl) return null;
    const hit = cache.get(key(me, src));
    return hit && Date.now() - hit.at < ttl ? hit.events : null;
}

/** Store a landing-page fetch. Call only on a MISS (not on hits) so the TTL doesn't extend
 * indefinitely under constant re-visiting - staleness stays bounded at the source's TTL. */
export function putCachedFeed(me: string, src: string, events: NostrEvent[]): void {
    if (ttlFor(src)) cache.set(key(me, src), { events, at: Date.now() });
}
