// Scheduled posts: a note or article SIGNED at compose, held on disk, and broadcast by a
// periodic sweep at its time. (Polls aren't schedulable - their endsAt is baked at sign time.) The key is only needed at compose (to sign); broadcasting a fully-signed
// event needs no key, so a scheduled post goes out even with the browser closed (nip07) - the always-on
// daemon does it. `signed.created_at` == the scheduled time, broadcast AT that moment so it reads
// as ≈now and relays don't reject future-dating. Mirrors undo-store.ts. File 0600 under .data/.

import { join } from 'node:path';
import { jsonStore } from './json-store.ts';
import { anyAccepted } from './pool.ts';
import { nowSec } from '../nostr/tags.ts';
import { newDraftId } from '../drafts.ts';
import type { Pool } from './pool.ts';
import type { NostrEvent } from '../nostr/types.ts';
import { INDEXER_RELAYS } from '../nostr/nip65.ts';

export interface ScheduledPost {
    token: string;
    pubkey: string;          // author (== signed.pubkey)
    signed: NostrEvent;      // fully-signed note or article; created_at == scheduledAt
    scheduledAt: number;     // unix SECONDS to broadcast at
    writeTargets: string[];  // relays to broadcast to (your outbox)
}

const FILE = process.env.SATORI_SCHEDULED_FILE || join(process.cwd(), '.data', 'scheduled.json');
const GIVE_UP_SECS = 3600; // stop retrying a post that keeps failing after 1h past its time

// mtime-cached read (the 30s sweep calls readAll() every tick, but the file only changes when we
// add/cancel/drop - jsonStore re-parses only when mtime moved) + 0o600 write.
const { readAll, writeAll } = jsonStore<Record<string, ScheduledPost>>(FILE, 'scheduled');

// Per-author cap: bounds the SHARED store so one account can't bloat the file (and the sweep's in-memory
// copy) for everyone on a multi-user instance. A normal user is nowhere near it - it only stops a runaway.
const MAX_PER_USER = 100;
/** The cap-reached error, shown by every compose path that schedules. One source so the number can't
 * drift out of sync with MAX_PER_USER across the note/article x bunker/nip07 call sites. */
export const SCHEDULE_FULL_MSG = `You have reached the maximum of ${MAX_PER_USER} scheduled posts. Cancel some to schedule more.`;
/** Hold a signed post for the sweep. Returns false (the caller surfaces an error) if the author is at cap. */
export function addScheduled(p: ScheduledPost): boolean {
    const all = readAll();
    if (!(p.token in all) && Object.values(all).filter((x) => x.pubkey === p.pubkey).length >= MAX_PER_USER) return false;
    all[p.token] = p; writeAll(all);
    return true;
}
/** Hold a freshly-signed post for the sweep with a generated token - the one seam the compose paths share.
 * Returns false at cap (caller surfaces SCHEDULE_FULL_MSG). */
export function holdScheduled(me: string, signed: NostrEvent, scheduledAt: number, writeTargets: string[]): boolean {
    return addScheduled({ token: newDraftId(), pubkey: me, signed, scheduledAt, writeTargets });
}
export function listScheduled(me: string): ScheduledPost[] {
    return Object.values(readAll()).filter((p) => p.pubkey === me).sort((a, b) => a.scheduledAt - b.scheduledAt);
}
export function getScheduledPost(me: string, token: string): ScheduledPost | null {
    const p = readAll()[token];
    return p && p.pubkey === me ? p : null;
}
export function cancelScheduled(me: string, token: string): boolean {
    const all = readAll();
    if (all[token]?.pubkey === me) { delete all[token]; writeAll(all); return true; }
    return false;
}

let sweeping = false;
/** Broadcast due posts (scheduledAt <= now); drop on success, or after GIVE_UP_SECS of failing.
 * Re-reads the store each tick, so it's restart-safe and needs no per-post timers. */
export async function sweepScheduled(pool: Pool): Promise<void> {
    if (sweeping) return; // never overlap a slow sweep
    sweeping = true;
    try {
        const now = nowSec();
        // Skip any entry whose stored pubkey doesn't match its signed event (corrupt store): the
        // signed event is self-authenticating, but don't waste a broadcast on a mismatch.
        const due = Object.values(readAll()).filter((p) => p.scheduledAt <= now && p.signed?.pubkey === p.pubkey);
        const remove: string[] = [];
        for (const p of due) {
            const targets = p.writeTargets.length ? p.writeTargets : INDEXER_RELAYS;
            let ok = false;
            try {
                const results = await pool.publish(targets, p.signed);
                ok = anyAccepted(results);
            } catch (e) { console.warn('[scheduled] publish failed', p.token, (e as Error)?.message ?? e); }
            if (ok) remove.push(p.token);
            else if (now - p.scheduledAt > GIVE_UP_SECS) { console.warn('[scheduled] giving up on', p.token); remove.push(p.token); }
            // else: keep it and retry on the next sweep (relay may be transiently down)
        }
        if (remove.length) dropMany(remove); // one read+write for the whole sweep, not per post
    } finally { sweeping = false; }
}

/** Remove several tokens in a single read+write (the sweep can clear many due posts at once). */
function dropMany(tokens: string[]): void {
    const all = readAll();
    let changed = false;
    for (const t of tokens) if (t in all) { delete all[t]; changed = true; }
    if (changed) writeAll(all);
}

/** Start the periodic sweep (call once at boot, after the pool is ready). */
export function startScheduledSweep(pool: Pool, intervalMs = 30_000): void {
    void sweepScheduled(pool); // catch anything already due at boot
    setInterval(() => void sweepScheduled(pool), intervalMs);
}
