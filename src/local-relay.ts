// Local relay: ONE optional personal relay (a self-hosted aggregator / outbox / blaster) the
// daemon can mirror writes to and/or read from. Kept OUT of your published NIP-65 list - it's
// daemon-side operational config, exactly like Privacy Mode (privacy.ts), decoupled from what
// other clients see. A master `enabled` (the UI's "Use" on/off) gates it; when on, read and write
// are each add|only:
//   add  - UNION: the local relay alongside your normal outbox / write relays (type-agnostic
//          supplement; a thin relay costs nothing, an aggregator serves the whole feed).
//   only - EXCLUSIVE: talk ONLY to the local relay for this direction, skipping the outbox /
//          NIP-65 relays - the isolation you want to run everything through your own relay.
// Server-wide (single-user daemon), persisted to .data, live-settable. Env SATORI_LOCAL_RELAY
// seeds the url (enabled, add/add) on first run.

import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { dirname } from 'node:path';
import { normalizeRelayUrl } from './nostr/nip65.ts';
import { isSingleUser } from './access.ts';

export type RelayUse = 'off' | 'add' | 'only';
export const RELAY_USES: RelayUse[] = ['off', 'add', 'only'];
/** A per-direction choice is add|only; the master `enabled` flag is the on/off (was a third 'off' value). */
const dir = (v: unknown): RelayUse => (v === 'only' ? 'only' : 'add');

export interface LocalRelay { url: string; enabled: boolean; read: RelayUse; write: RelayUse; fetchMissing: boolean }

const FILE = process.env.SATORI_LOCAL_RELAY_FILE ?? '.data/local-relay.json';
let state: LocalRelay | null = null;

(function load(): void {
    try {
        const j = JSON.parse(readFileSync(FILE, 'utf8')) as { url?: string; enabled?: boolean; read?: string; write?: string; fetchMissing?: boolean };
        if (j && typeof j.url === 'string') {
            const url = normalizeRelayUrl(j.url, { assumeWss: true });
            if (url) {
                // Migrate the old {read,write: off|add|only} shape: a direction set to 'off' meant the relay
                // was disabled for it, which the master `enabled` flag now carries; each direction is add|only.
                const enabled = typeof j.enabled === 'boolean' ? j.enabled : (j.read !== 'off' || j.write !== 'off');
                state = { url, enabled, read: dir(j.read), write: dir(j.write), fetchMissing: j.fetchMissing === true };
                return;
            }
        }
    } catch { /* no file yet - fall through to the env seed */ }
    const envUrl = process.env.SATORI_LOCAL_RELAY?.trim();
    if (envUrl) { const url = normalizeRelayUrl(envUrl, { assumeWss: true }); if (url) state = { url, enabled: true, read: 'add', write: 'add', fetchMissing: false }; }
})();

export function localRelay(): LocalRelay | null { return state; }

/** Set (url present + valid) or clear (blank/invalid url) the local relay. Returns the new state. */
export function setLocalRelay(url: string, enabled: boolean, read: RelayUse, write: RelayUse, fetchMissing: boolean): LocalRelay | null {
    const norm = normalizeRelayUrl(url, { assumeWss: true });
    state = norm ? { url: norm, enabled, read: dir(read), write: dir(write), fetchMissing } : null;
    try { mkdirSync(dirname(FILE), { recursive: true }); writeFileSync(FILE, JSON.stringify(state ?? {}), { mode: 0o600 }); }
    catch (e) { console.warn('[local-relay] persist failed:', (e as Error)?.message ?? e); }
    return state;
}

// Private relay is SINGLE-USER only: its config is process-global, so on a multi-pubkey instance one
// account's routing would govern EVERYONE's reads and writes (including others' gift-wrapped DMs). Gate the
// whole feature on a single-pubkey access policy - the config still persists, but stays inert on a shared
// instance (isSingleUser() is true for a normal self-host: an owner lock or a one-entry allowlist).
const active = (): boolean => !!state && state.enabled && isSingleUser();
/** How reads use the local relay: its read mode when enabled (single-user only), else off. */
export const localReadMode = (): RelayUse => (active() ? state!.read : 'off');
/** How writes use the local relay: its write mode when enabled (single-user only), else off. */
export const localWriteMode = (): RelayUse => (active() ? state!.write : 'off');
/** In Only read mode, whether to fetch OTHER people's events the private relay lacks from your outbox
 * relays (the "Fetch events not on your private relay" setting). Off = stay isolated (gaps). Only
 * meaningful when read = only; in add/off, everything already resolves via the outbox. */
export const localFetchMissing = (): boolean => (localReadMode() === 'only' && (state?.fetchMissing ?? false));
/** Only mode with "fetch missing" OFF: reads are confined to the private relay, so events it lacks (a
 * non-follow's notes, a quoted note, a thread parent) simply don't show. Drives the "turn it on" nudge. */
export const localIsolated = (): boolean => (localReadMode() === 'only' && !(state?.fetchMissing ?? false));
/** The local relay url, or null - regardless of read/write mode (used for the Tor bypass). */
export const localRelayUrl = (): string | null => state?.url ?? null;

/** True if `url` is the configured local relay. Used by the Tor bypass: a CLEARNET local relay
 * (e.g. ws://localhost) must not be forced through Tor by Privacy Mode (Tor can't reach it); an
 * .onion local relay still routes via Tor by nature (that path keys off the hostname, not this). */
export function isLocalRelayUrl(url: string): boolean {
    if (!state) return false;
    const norm = normalizeRelayUrl(url, { assumeWss: true });
    return !!norm && norm === state.url;
}
