// Tiny on-disk session store so a server restart (or the dev container's
// --watch reload) doesn't log you out - it resumes the session from the cookie,
// mirroring Satori's "a reload resumes your session" (which used localStorage).
//
// What's persisted: the user pubkey + their NIP-65 relays, and for a bunker
// session the NIP-46 *transport* key (client secret, remote pubkey, relays) -
// the same thing Satori kept in localStorage. The user's nostr key is NOT here;
// it never leaves the bunker. nip07 sessions persist only the pubkey (no secret).
// File is written 0600 (owner-only); it's a local single-user daemon.

import { join } from 'node:path';
import { jsonStore } from './data/json-store.ts';
import type { RelayList } from './nostr/types.ts';

export interface PersistedSession {
    mode: 'bunker' | 'nip07';
    me: string;
    myRelays: RelayList | null;
    bunker?: { secretHex: string; remotePubkey: string; relays: string[] };
    savedAt?: number; // ms epoch; stamped on save, used to prune stale entries (see prunePersisted)
    // (Read high-waters for the bell / new-notes dot moved to a client cookie -
    // see read-state.ts - so they're no longer persisted server-side here.)
}

const FILE = process.env.SATORI_SESSION_FILE || join(process.cwd(), '.data', 'sessions.json');

// The shared jsonStore replaces a hand-rolled read/write pair: its mtime cache means a request
// with a DEAD session cookie (which falls through to loadPersisted every time) no longer re-reads
// and re-parses the file per request.
const store = jsonStore<Record<string, PersistedSession>>(FILE, 'store');
const readAll = (): Record<string, PersistedSession> => store.readAll();
const writeAll = (all: Record<string, PersistedSession>): void => store.writeAll(all);

export function loadPersisted(sid: string): PersistedSession | undefined {
    return readAll()[sid];
}

/** Distinct pubkeys across all persisted sessions, most-recently-saved first. Read once at boot to
 * adopt an already-logged-in user as the access-control owner (so adding the owner lock to an
 * existing self-host deploy doesn't log them out). */
export function persistedPubkeys(): string[] {
    const all = Object.values(readAll()).sort((a, b) => (b.savedAt ?? 0) - (a.savedAt ?? 0));
    const seen = new Set<string>();
    const out: string[] = [];
    for (const p of all) if (p.me && !seen.has(p.me)) { seen.add(p.me); out.push(p.me); }
    return out;
}

export function savePersisted(sid: string, p: PersistedSession): void {
    const all = readAll();
    all[sid] = { ...p, savedAt: Date.now() };
    writeAll(all);
}

export function removePersisted(sid: string): void {
    const all = readAll();
    if (sid in all) { delete all[sid]; writeAll(all); }
}

/** Drop persisted sessions older than maxAgeMs (their cookies, maxAge 7d, are long dead, so
 * they can't be resumed anyway). Hygiene: keeps stale bunker transport secrets from lingering
 * at rest forever. Entries with no savedAt (pre-this-field) are kept until re-stamped on login.
 * Returns the number dropped. */
export function prunePersisted(maxAgeMs: number): number {
    const all = readAll();
    const now = Date.now();
    let dropped = 0;
    for (const [sid, p] of Object.entries(all)) {
        if (p.savedAt && now - p.savedAt > maxAgeMs) { delete all[sid]; dropped++; }
    }
    if (dropped) writeAll(all);
    return dropped;
}
