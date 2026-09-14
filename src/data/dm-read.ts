// Per-conversation read positions for the quiet unread dot. Kept server-side (the daemon
// already holds DM state) and persisted, keyed by your pubkey -> { peer: openedAtTs }, plus a
// per-account baseline so the dot is forward-looking: existing conversations don't all light
// up on first use, only activity that arrives after you start tracking. Not sensitive (just
// timestamps), so it survives logout. High-waters only advance forward.

import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { debouncedFlush } from './json-store.ts';

const FILE = process.env.SATORI_DM_READ || join(process.cwd(), '.data', 'dm-read.json');
const BASELINE = '*'; // reserved key: "everything at/before here is considered read"
type Acct = Record<string, number>; // peer hex -> openedAt (BASELINE key holds the account baseline)
const read = new Map<string, Acct>();

(function load(): void {
    try {
        const raw = JSON.parse(readFileSync(FILE, 'utf8')) as Record<string, Acct>;
        for (const [me, a] of Object.entries(raw)) if (a && typeof a === 'object') read.set(me, a);
    } catch { /* none yet */ }
})();

const flusher = debouncedFlush(() => {
    try { mkdirSync(dirname(FILE), { recursive: true }); writeFileSync(FILE, JSON.stringify(Object.fromEntries(read)), { mode: 0o600 }); }
    catch (e) { console.warn('[dm-read] flush failed:', (e as Error)?.message ?? e); }
}, 5000);
const flush = (): void => flusher.schedule();

/** Set the forward-looking baseline on first sight of an account (so pre-existing convos
 * don't all show unread). No-op once set. */
export function ensureDmBaseline(me: string, now: number): void {
    let a = read.get(me);
    if (!a) { a = {}; read.set(me, a); }
    if (a[BASELINE] == null) { a[BASELINE] = now; flush(); }
}

/** The read high-water for a peer = max(per-peer open, account baseline). */
export function dmReadAt(me: string, peer: string): number {
    const a = read.get(me);
    return a ? Math.max(a[peer] ?? 0, a[BASELINE] ?? 0) : 0;
}

/** True when a conversation has unseen incoming activity: the latest message is from the peer
 * (not you) and newer than your read high-water for them. */
export function dmUnread(me: string, peer: string, lastAt: number, lastFromMe: boolean): boolean {
    return !lastFromMe && lastAt > dmReadAt(me, peer);
}

/** Mark a conversation read as of `at` (monotonic). Call when you open the thread. */
export function markDmRead(me: string, peer: string, at: number): void {
    let a = read.get(me);
    if (!a) { a = {}; read.set(me, a); }
    if ((a[peer] ?? 0) >= at) return;
    a[peer] = at;
    flush();
}
