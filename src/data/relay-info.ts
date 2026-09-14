// NIP-11 relay information documents. A relay's identity (name, description, icon, contact)
// and capabilities (supported NIPs, auth/payment requirements) live in a JSON document served
// over HTTP on the relay's own host with `Accept: application/nostr+json`. Satori uses it two
// ways: identity on the relay surfaces (settings rows, the /relay timeline header, the browse
// picker), and one capability gate - search queries skip relays that advertise a NIP list
// without NIP-50 in it (they'd never answer). Everything is best-effort: a relay without a
// document just stays a bare URL.
//
// Fetches ride the house rails: SSRF-guarded (the url derives from user/relay-list input),
// Privacy-Mode-aware via torRequest (.onion hosts resolve through the SOCKS proxy), size- and
// time-capped, and PERSISTED with a TTL - these documents rarely change, so a restart must not
// re-fetch the world, and a relay that's down shouldn't be hammered (failures cache shorter).

import { join } from 'node:path';
import { jsonStore, debouncedFlush } from './json-store.ts';
import { isPublicHttpUrl } from '../ssrf.ts';
import { relayKey } from '../nostr/nip65.ts';
import { torRequest } from './torfetch.ts';

const OK_TTL = 24 * 3600_000;   // a fresh document is good for a day
const FAIL_TTL = 2 * 3600_000;  // a miss (no doc, bad JSON, network error) retries after 2h
const MAX_ENTRIES = 500;        // far above any real relay list; bounds the file
const FETCH_MS = 6000;
const MAX_BYTES = 128 * 1024;
const FILE = process.env.SATORI_RELAY_INFO_FILE || join(process.cwd(), '.data', 'relay-info.json');

// Only the fields a surface actually renders (or a gate reads) are kept - icon/contact/
// software/version exist in the wild but storing them unrendered would be dead data.
export interface RelayInfo {
    ok: boolean;          // false = negative-cached miss
    at: number;           // fetch time (ms)
    name?: string;
    description?: string;
    nips?: number[];      // supported_nips, when advertised
    auth?: boolean;       // limitation.auth_required
    payment?: boolean;    // limitation.payment_required
}

interface Stored extends Record<string, unknown> { [url: string]: unknown }
const { readAll, writeAll } = jsonStore<Stored>(FILE, 'relay-info');

const norm = relayKey; // the shared light key-normalizer (nip65)
const infos = new Map<string, RelayInfo>(Object.entries(readAll()).slice(-MAX_ENTRIES) as [string, RelayInfo][]);
const flusher = debouncedFlush(() => writeAll(Object.fromEntries(infos)), 10000);

const fresh = (i: RelayInfo): boolean => Date.now() - i.at < (i.ok ? OK_TTL : FAIL_TTL);

/** The cached NIP-11 document for a relay url, or null when unknown/expired/negative. Sync - render-safe. */
export function relayInfoCached(url: string): RelayInfo | null {
    const i = infos.get(norm(url));
    return i && i.ok && fresh(i) ? i : null;
}

/** A bounded string field from untrusted JSON: trimmed, capped, and stripped of control and
 * format characters (\p{Cc}\p{Cf} - covers bidi overrides and zero-width tricks). A relay's
 * self-declared name replaces a host label in the UI, so it must not be able to visually
 * impersonate another host via RTL/invisible characters. */
function str(v: unknown, cap: number): string | undefined {
    if (typeof v !== 'string') return undefined;
    const clean = v.replace(/[\p{Cc}\p{Cf}]/gu, '').trim();
    return clean ? clean.slice(0, cap) : undefined;
}

/** Parse + sanitize a NIP-11 document. Every field is untrusted remote JSON: strings are capped
 * (they land in tooltips/labels), supported_nips keeps integers only. */
function parseInfo(json: unknown): RelayInfo {
    const j = (json ?? {}) as Record<string, unknown>;
    const lim = (j.limitation ?? {}) as Record<string, unknown>;
    const nips = Array.isArray(j.supported_nips) ? j.supported_nips.filter((n): n is number => Number.isInteger(n)).slice(0, 256) : undefined;
    return {
        ok: true,
        at: Date.now(),
        name: str(j.name, 64),
        description: str(j.description, 280),
        ...(nips ? { nips } : {}),
        ...(typeof lim.auth_required === 'boolean' ? { auth: lim.auth_required } : {}),
        ...(typeof lim.payment_required === 'boolean' ? { payment: lim.payment_required } : {}),
    };
}

function put(url: string, info: RelayInfo): void {
    infos.delete(url);
    infos.set(url, info);
    while (infos.size > MAX_ENTRIES) infos.delete(infos.keys().next().value as string);
    flusher.schedule();
}

// One in-flight fetch per relay, shared by concurrent callers (the settings page and a feed
// route asking about the same relay must not double-fetch).
const inflight = new Map<string, Promise<void>>();

async function fetchInfo(url: string): Promise<void> {
    const httpUrl = url.replace(/^ws:/, 'http:').replace(/^wss:/, 'https:');
    if (!isPublicHttpUrl(httpUrl)) { put(url, { ok: false, at: Date.now() }); return; }
    try {
        const res = await torRequest(httpUrl, { method: 'GET', headers: { accept: 'application/nostr+json' }, timeoutMs: FETCH_MS, maxBytes: MAX_BYTES });
        if (res.status !== 200) { put(url, { ok: false, at: Date.now() }); return; }
        put(url, parseInfo(JSON.parse(res.body.toString('utf8'))));
    } catch {
        put(url, { ok: false, at: Date.now() });
    }
}

/** Fetch missing/stale documents for `urls`. Resolves when all are settled, or after `waitMs` if
 * given - stragglers keep fetching in the background and land in the cache for the next render.
 * Call it just before rendering a relay surface; repeat calls are TTL-cheap. */
export function ensureRelayInfo(urls: string[], waitMs?: number): Promise<void> {
    const jobs: Promise<void>[] = [];
    for (const raw of new Set(urls.map(norm))) {
        if (!raw) continue;
        const cur = infos.get(raw);
        if (cur && fresh(cur)) continue;
        let job = inflight.get(raw);
        if (!job) {
            job = fetchInfo(raw).finally(() => inflight.delete(raw));
            inflight.set(raw, job);
        }
        jobs.push(job);
    }
    if (jobs.length === 0) return Promise.resolve();
    const all = Promise.all(jobs).then(() => undefined);
    if (waitMs === undefined) return all;
    return Promise.race([all, new Promise<void>((r) => setTimeout(r, waitMs))]);
}

/** True when a relay's NIP-11 document advertises a NIP list WITHOUT NIP-50 - the one shared
 * predicate behind the search gate and the settings "no search" chip, so they can't drift. */
export function lacksNip50(url: string): boolean {
    const i = relayInfoCached(url);
    return !!(i?.nips?.length && !i.nips.includes(50));
}

/** Search-capability gate: drop relays known (via NIP-11) to lack NIP-50 - they will never
 * answer a search filter. Unknown relays (no document, no nips field) are kept: absence of
 * evidence isn't refusal. Never returns empty - if every configured relay is known-incapable
 * the original list is used, so search degrades instead of breaking. */
export function nip50Capable(relays: string[]): string[] {
    const kept = relays.filter((u) => !lacksNip50(u));
    return kept.length ? kept : relays;
}
