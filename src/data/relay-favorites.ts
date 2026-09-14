// Per-user relay-feed FAVORITES: a small shortlist of relays the user has starred while browsing, shown in
// the relay picker for quick re-entry. Local store (mirrors content-prefs / filters via jsonStore), so it
// never leaves the daemon. Browsing a relay needs NOTHING here - any relay works via /relay?r=<url>; the
// favorites are just saved shortcuts (the "star", à la dark-wisp). FUTURE: NIP-51 relay sets (kind:30002)
// for cross-device sync - only this module's backend would change.

import { join } from 'node:path';
import { jsonStore } from './json-store.ts';
import { normalizeRelayUrl as canonicalRelayUrl } from '../nostr/nip65.ts';
import { isPublicWsUrl } from '../ssrf.ts';

export interface SavedRelay { url: string; name?: string }

// Tolerate the bare-array shape written by earlier builds this session, alongside the current {relays} shape.
type Stored = SavedRelay[] | { relays?: SavedRelay[] };
type Store = Record<string, Stored>;

const FILE = process.env.SATORI_RELAY_FEEDS_FILE || join(process.cwd(), '.data', 'relay-feeds.json');
const { readAll, writeAll } = jsonStore<Store>(FILE, 'relay-favorites');
const MAX = 50;

const listOf = (raw: Stored | undefined): SavedRelay[] => (Array.isArray(raw) ? raw : (raw?.relays ?? []));

/** Canonicalize a user-entered relay URL with the SHARED nip65 normalizer (assume wss://, lowercase, strip
 * trailing slash), then apply the SSRF screen (public ws/wss only) - null if invalid / disallowed. Using the
 * same normalizer as the rest of the app keeps favorites directly comparable with the user's NIP-65 relays
 * in the picker (the "Your relays" dedup + star state rely on identical canonical forms). */
export function normalizeRelayUrl(input: string): string | null {
    const s = canonicalRelayUrl(input, { assumeWss: true });
    return s && isPublicWsUrl(s) ? s : null;
}

/** A relay url's display label: the saved name if any, else its host. */
export function relayLabel(url: string, name?: string): string {
    if (name && name.trim()) return name.trim();
    try { return new URL(url).host; } catch { return url; }
}

export function getFavoriteRelays(me: string): SavedRelay[] { return listOf(readAll()[me]); }

/** Toggle a relay's favorite state. Returns the NEW state (true = now favorited). Returns false unchanged on
 * an invalid url, or when already at the per-user cap while adding (so the UI never shows a star that wasn't
 * saved). */
export function toggleFavoriteRelay(me: string, url: string, name?: string): boolean {
    const norm = normalizeRelayUrl(url);
    if (!norm) return false;
    const all = readAll();
    const list = listOf(all[me]);
    const idx = list.findIndex((r) => r.url === norm);
    let now: boolean;
    if (idx >= 0) { list.splice(idx, 1); now = false; }
    else { if (list.length >= MAX) return false; list.push({ url: norm, name: (name ?? '').trim().slice(0, 60) || undefined }); now = true; }
    all[me] = list;
    writeAll(all);
    return now;
}
