// Process-wide profile cache with stale-while-revalidate. Profiles (kind:0: name,
// avatar url, nip05, lud16) change rarely but are needed on every render, so we serve
// them INSTANTLY from a disk-backed LRU and refresh stale ones in the background -
// no relay round-trip on the hot path. The server-side analog of Satori's IndexedDB
// profile cache. Single-user daemon, so one shared cache (not per-session).

import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { debouncedFlush, lruEvictByLastUsed } from './json-store.ts';
import { verifyNip05 } from '../nostr/nip05.ts';
import type { Profile } from './profiles.ts';

interface Entry { profile: Profile; fetchedAt: number; lastUsed: number; nip05At: number }

const FILE = process.env.SATORI_PROFILE_CACHE || join(process.cwd(), '.data', 'profiles.json');
const CAP = 10_000;                    // max entries (profiles are tiny; rarely evicts)
const STALE_MS = 6 * 60 * 60 * 1000;   // 6h → serve cached, refresh in the background
const VERIFY_TTL = 12 * 60 * 60 * 1000; // re-check NIP-05 at most every 12h

const cache = new Map<string, Entry>();
let hits = 0, misses = 0, staleRefreshes = 0; // lightweight instrumentation

(function load(): void {
    try {
        const raw = JSON.parse(readFileSync(FILE, 'utf8')) as Record<string, Entry>;
        for (const [pk, e] of Object.entries(raw)) if (e?.profile) cache.set(pk, { ...e, nip05At: e.nip05At || 0 });
    } catch { /* no cache yet */ }
})();

const flusher = debouncedFlush(() => {
    try {
        mkdirSync(dirname(FILE), { recursive: true });
        writeFileSync(FILE, JSON.stringify(Object.fromEntries(cache)), { mode: 0o600 });
    } catch (e) { console.warn('[profile-cache] flush failed:', (e as Error)?.message ?? e); }
}, 8000);

const verifying = new Set<string>();

/** Background NIP-05 check: fetch the claimed domain's .well-known and confirm it maps
 * back to this pubkey, caching the result on the profile - so the ✓ means *verified*,
 * not merely *claimed*. Never blocks a render; re-checks at most every VERIFY_TTL. */
function maybeVerifyNip05(pubkey: string, e: Entry): void {
    const nip05 = e.profile.nip05;
    if (!nip05 || verifying.has(pubkey)) return;
    if (e.nip05At && Date.now() - e.nip05At < VERIFY_TTL) return;
    verifying.add(pubkey);
    void verifyNip05(nip05, pubkey)
        .then((ok) => { e.profile.nip05Verified = ok; })
        .catch(() => { e.profile.nip05Verified = false; })
        .finally(() => { e.nip05At = Date.now(); verifying.delete(pubkey); flusher.schedule(); });
}

/** Cached profile (marks it recently used), or undefined. Reads don't trigger a flush. */
export function getCachedProfile(pubkey: string): Profile | undefined {
    const e = cache.get(pubkey);
    if (!e) { misses++; return undefined; }
    hits++;
    e.lastUsed = Date.now();
    maybeVerifyNip05(pubkey, e); // verify in the background if never checked / stale
    return e.profile;
}

// In-flight coalescing: pubkeys currently being fetched (by any session), mapped to the
// promise that will populate the cache. A concurrent render awaits the same promise instead
// of firing a duplicate relay query for the same author during the cold window.
const inflight = new Map<string, Promise<void>>();

/** The in-flight fetch for this pubkey, if one is running, else undefined. */
export function inflightProfile(pubkey: string): Promise<void> | undefined {
    return inflight.get(pubkey);
}

/** Register one shared fetch promise as in-flight for a batch of pubkeys; auto-clears each
 * entry when the promise settles (only if it's still the one we registered). */
export function registerInflight(pubkeys: string[], p: Promise<void>): void {
    for (const pk of pubkeys) inflight.set(pk, p);
    void p.finally(() => { for (const pk of pubkeys) if (inflight.get(pk) === p) inflight.delete(pk); });
}

/** Cached but past its freshness window → caller should background-refresh it. */
export function isProfileStale(pubkey: string): boolean {
    const e = cache.get(pubkey);
    const s = !!e && Date.now() - e.fetchedAt > STALE_MS;
    if (s) staleRefreshes++;
    return s;
}

/** Snapshot of cache effectiveness (served-from-cache vs relay round-trips). */
export function profileCacheStats(): { size: number; hits: number; misses: number; hitRate: number; staleRefreshes: number } {
    const total = hits + misses;
    return { size: cache.size, hits, misses, hitRate: total ? +(hits / total).toFixed(3) : 0, staleRefreshes };
}

/** Store/refresh a profile (resets freshness). Carries a prior NIP-05 verification when
 * the claimed nip05 is unchanged (no need to re-verify on every metadata refresh);
 * resets it when the nip05 changes. */
export function putProfile(pubkey: string, profile: Profile): void {
    const old = cache.get(pubkey);
    let nip05At = 0;
    if (old && old.profile.nip05 === profile.nip05) { profile.nip05Verified = old.profile.nip05Verified; nip05At = old.nip05At; }
    const e: Entry = { profile, fetchedAt: Date.now(), lastUsed: Date.now(), nip05At };
    cache.set(pubkey, e);
    maybeVerifyNip05(pubkey, e);
    lruEvictByLastUsed(cache, CAP);
    flusher.schedule();
}
