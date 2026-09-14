// Phase 1 of adaptive per-relay timeouts: a rolling latency profile per relay, built from the queries we
// already run many times a day (no extra network - pure observation of the fetches we do anyway). NOTHING
// reads it yet; it exists so a day of normal use yields a profile that a later phase turns into per-relay
// deadline budgets - a relay we've LEARNED is slow gets more rope without slowing the fast majority. It's
// self-calibrating to the deployment: on a slow-egress host every relay profiles slow (so feeds come back
// complete); on a fast host they profile fast (so feeds stay snappy). Recorded from pool.ts on each
// SINGLE-relay query (the feed fan-out path) - multi-relay queries can't attribute timing to one relay.
//
// PERSISTED (debounced + flush-on-exit, exactly like seen-relays) so the profile survives daemon restarts.

import { join } from 'node:path';
import { jsonStore, debouncedFlush } from './json-store.ts';
import { isLocalRelayUrl } from '../local-relay.ts';

const MAX_RELAYS = 500; // LRU cap so the file can't grow without bound
const ALPHA = 0.3;      // EWMA weight for a new sample (moderate smoothing over the day's queries)
// Adaptive-budget knobs (tuned against real outbox-only data, 2026-07-02). The real signal turned out to be
// `ev` (events delivered), not truncation: the slow relays deliver ~NOTHING for the feed (they take ~3.4s to
// EOSE empty), so the win is CUTTING them, not extending them. DEFAULT is the caller's `base` (today's cap =
// also cold-start/low-confidence).
const N_MIN = 5;        // min samples before the profile is trusted enough to adapt off the default
const EV_EMPTY = 1;     // avg events/query below this (i.e. ~0) = empty FOR YOUR FEED → bail fast to `floor`
const EMPTY_Q = 0.15;   // selection weight for a known-empty relay (see relayQuality): deprioritized, not banned
const TRUNC_HI = 0.3;   // a DELIVERING relay truncating above this rate looks budget-starved
const RICH_EV = 20;     // ...and only if it delivers this many events/query (a real backlog) is more time worth it
const FILE = process.env.SATORI_RELAY_LATENCY_FILE || join(process.cwd(), '.data', 'relay-latency.json');

const norm = (u: string): string => u.replace(/\/+$/, '').toLowerCase();

/** Rolling per-relay stats. `ms` = EWMA of time-to-last-event (pure relay responsiveness, excluding the
 * quiet-collapse tail); `ev` = EWMA of events delivered per query; `n` = samples; `trunc` = how often the
 * query was cut off at the hard cap while the relay was still delivering; `at` = last-updated (ms epoch).
 * `ev` disambiguates the two truncation modes that `ms` alone conflates (both pin near the cap): a truncation
 * with ~0 `ev` is a CONNECT/DELIVERY problem (waiting longer is HIGH value - the relay barely responded), a
 * truncation with high `ev` is a PROLIFIC relay/aggregator (waiting longer just fetches more older tail that
 * scroll-pagination recovers anyway - so ceiling it). The adaptive budget keys on both.
 * `deniedAt` = the relay explicitly REFUSED a read (CLOSED our REQ with a denial - a write-only blaster,
 * an auth wall we won't sign for, a paywall) - the read-dead signal (see isReadDead). */
interface RelayStat { ms: number; ev: number; n: number; trunc: number; at: number; deniedAt?: number }

const { readAll, writeAll } = jsonStore<Record<string, RelayStat>>(FILE, 'relay-latency');

const stats = new Map<string, RelayStat>(Object.entries(readAll()));
while (stats.size > MAX_RELAYS) stats.delete(stats.keys().next().value as string);

const flusher = debouncedFlush(() => {
    const out: Record<string, RelayStat> = {};
    for (const [url, s] of stats) out[url] = s;
    writeAll(out);
}, 15000);

/** Record one relay's observed time-to-last-event (ms), events delivered, and whether it was truncated at the
 * hard cap. Cheap (a map update + a debounced write); side-effect only - it never changes what the query
 * returns. */
export function recordLatency(relay: string, ms: number, truncated: boolean, events: number): void {
    const url = norm(relay);
    if (!url) return;
    const prev = stats.get(url);
    const ms2 = prev ? prev.ms * (1 - ALPHA) + ms * ALPHA : ms;
    // Seed ev from the current sample when absent (a pre-ev record, or a first sample) so the EWMA can't NaN.
    const ev2 = prev?.ev != null ? prev.ev * (1 - ALPHA) + events * ALPHA : events;
    stats.delete(url); // reinsert below → most-recently-used (LRU order)
    stats.set(url, { ms: Math.round(ms2), ev: Math.round(ev2), n: (prev?.n ?? 0) + 1, trunc: (prev?.trunc ?? 0) + (truncated ? 1 : 0), at: Date.now() });
    while (stats.size > MAX_RELAYS) stats.delete(stats.keys().next().value as string);
    flusher.schedule();
}

/** Adaptive hard-cap (ms) for a single relay from its latency profile:
 *  - EMPTY (delivers ~nothing across many queries) → `floor`: bail fast. It returns nothing for your feed, so
 *    waiting the full cap is pure wasted latency with no completeness cost. This is the main win.
 *  - a real BACKLOG (delivers a lot AND still truncates at the cap) → extend toward `ceiling`, proportional to
 *    its truncation rate. (Rare on the outbox path - most slow relays are empty, not rich.)
 *  - otherwise (delivers and finishes in time) or too little evidence → `base` (today's default).
 * Pure profile read; cheap per query. */
export function relayBudget(relay: string, floor: number, base: number, ceiling: number): number {
    const s = stats.get(norm(relay));
    if (!s || s.n < N_MIN) return base;                                    // not enough evidence yet → default
    if (s.ev < EV_EMPTY) return floor;                                     // empty for your feed → bail fast
    // NOTE: `ev`/`ms` are EWMA (recent-weighted) but `trunc`/`n` are lifetime counts, so this truncRate is a
    // lifetime average that lags recent behavior. Only gates the rare rich-but-truncating branch; if that ever
    // misbehaves, switch trunc to a windowed count too.
    const truncRate = s.trunc / s.n;
    if (truncRate >= TRUNC_HI && s.ev >= RICH_EV)                          // rich but cut off → more rope
        return Math.min(ceiling, Math.round(base + truncRate * (ceiling - base)));
    return base;                                                           // delivers, finishes in time → default
}

// Read-dead classification. Two independent signals, both empirical:
//  - an explicit DENIAL (the relay CLOSED our REQ with a refusal) is trusted immediately, for
//    DENIAL_TTL - re-checked after that in case the relay's policy (or our auth standing) changed;
//  - the blaster PROFILE SHAPE: many samples, ~zero events, and it (almost) never EOSEs - so every
//    query rides to the hard cap. A merely QUIET relay EOSEs empty fast (trunc ~0) and is NOT dead;
//    it stays on the existing EMPTY_Q deprioritization instead.
const DENIAL_TTL = 12 * 60 * 60 * 1000;
const DEAD_N = 8;        // min samples before the profile shape alone can condemn a relay
const DEAD_TRUNC = 0.9;  // ...and it hit the hard cap on at least this share of them

/** Note a relay explicitly refused to serve a read. Never recorded for the configured local relay
 * (its pre-auth 'auth-required' CLOSEDs are expected; banning it would blank an Only-mode feed). */
export function recordReadDenial(relay: string): void {
    const url = norm(relay);
    if (!url || isLocalRelayUrl(url)) return;
    const prev = stats.get(url) ?? { ms: 0, ev: 0, n: 0, trunc: 0, at: 0 };
    stats.delete(url); // reinsert → most-recently-used
    stats.set(url, { ...prev, deniedAt: Date.now(), at: Date.now() });
    while (stats.size > MAX_RELAYS) stats.delete(stats.keys().next().value as string);
    flusher.schedule();
}

/** True when reading from this relay is known-futile: it denied us recently, or its profile is the
 * write-only-blaster shape. Routing treats read-dead as score 0 (never picked); the mid-session
 * shard skip stops paying for already-routed dead relays. The local relay is never read-dead. */
export function isReadDead(relay: string): boolean {
    const url = norm(relay);
    const s = stats.get(url);
    if (!s || isLocalRelayUrl(url)) return false;
    if (s.deniedAt && Date.now() - s.deniedAt < DENIAL_TTL) return true;
    return s.n >= DEAD_N && s.ev < EV_EMPTY && s.trunc / s.n >= DEAD_TRUNC;
}

/** Selection weight in (0,1] for outbox routing (used by routeAuthorsToRelays). A relay we've LEARNED
 * delivers ~nothing for your feed is a wasted slot - its authors' notes come from their other relays, or not
 * at all - so weight it DOWN, but never to zero: it's still picked when it's an author's only relay. Unknown
 * or low-confidence relays are neutral (1): don't penalize one we haven't profiled - it may be great. Keys on
 * `ev` (the module's primary signal), the same cut relayBudget uses to bail empty relays' timeouts. */
export function relayQuality(relay: string): number {
    const s = stats.get(norm(relay));
    if (!s || s.n < N_MIN) return 1;        // not enough evidence yet → neutral
    if (s.ev < EV_EMPTY) return EMPTY_Q;    // empty for your feed → deprioritize (still pickable)
    return 1;                               // delivers → full weight
}
