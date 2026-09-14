// NIP-50 search - notes (kind:1) and people (kind:0) over dedicated search relays.
// nostr has no decentralized full-text index, so search centralizes on a handful of
// indexer relays; these defaults were liveness-tested 2026-06-20 (borrowed from
// ants.sh's vetted lists). Relays rot - see [[search-plan]]; making these a Settings
// field is the immediate follow-up (kept as constants for v1). pool.query already
// caps each query with maxWait, so a dead/slow source just contributes nothing.

import type { Pool } from './pool.ts';
import type { Filter } from 'nostr-tools';
import type { NostrEvent, RelayList } from '../nostr/types.ts';
import { parseProfile, type Profile } from './profiles.ts';
import { fetchRelayLists } from './relays.ts';
import { INDEXER_RELAYS } from '../nostr/nip65.ts';
import { ensureRelayInfo, nip50Capable } from './relay-info.ts';

// --- search operators (ants-style) -----------------------------------------
// A query mixes free text (→ NIP-50 `search`) with operators. Relay-native ones become Filter
// fields (#t / authors / #p / since / until); `has:`/`site:` can't be expressed in a filter, so
// they post-filter the fetched notes. `by:`/`p:` identifiers are resolved to pubkeys by the caller
// (it has the profile cache). Unknown `word:` tokens fall back to free text. is:/OR are not (yet).

export interface SearchQuery {
    text: string;                            // free text → NIP-50 search
    tags: string[];                          // #foo or t:foo → #t
    by: string[];                            // by:<npub|hex|name> raw, caller resolves → authors
    p: string[];                             // p:<npub|hex|name> raw, caller resolves → #p mentions
    since?: number;                          // since:<date|Nd>
    until?: number;                          // until:<date|Nd>
    has: ('image' | 'video' | 'link')[];     // has:image|video|link → post-filter
    sites: string[];                         // site:<domain> → post-filter
}

/** Parse "since:2024-01-01" (UTC midnight) or a relative "7d"/"24h"/"2w"/"30m" to unix seconds. */
function parseWhen(v: string): number | undefined {
    const rel = v.match(/^(\d+)([dwhm])$/i);
    if (rel) {
        const n = Number(rel[1]);
        const per = rel[2]!.toLowerCase() === 'd' ? 86400 : rel[2]!.toLowerCase() === 'w' ? 604800 : rel[2]!.toLowerCase() === 'h' ? 3600 : 60;
        return Math.floor(Date.now() / 1000) - n * per;
    }
    if (/^\d{4}-\d{2}-\d{2}$/.test(v)) { const ms = Date.parse(`${v}T00:00:00Z`); return Number.isFinite(ms) ? Math.floor(ms / 1000) : undefined; }
    return undefined;
}

/** Split a raw query into free text + operators. Pure + sync (no resolution). */
export function parseSearchQuery(q: string): SearchQuery {
    const out: SearchQuery = { text: '', tags: [], by: [], p: [], has: [], sites: [] };
    const free: string[] = [];
    for (const tok of q.split(/\s+/).filter(Boolean)) {
        if (tok.length > 1 && tok.startsWith('#')) { out.tags.push(tok.slice(1).toLowerCase()); continue; }
        const m = tok.match(/^([a-z]+):(.+)$/i);
        if (m) {
            const op = m[1]!.toLowerCase(), val = m[2]!;
            if (op === 't') { out.tags.push(val.toLowerCase()); continue; }
            if (op === 'by') { out.by.push(val); continue; }
            if (op === 'p') { out.p.push(val); continue; }
            if (op === 'has' && /^(image|video|link)$/i.test(val)) { out.has.push(val.toLowerCase() as 'image' | 'video' | 'link'); continue; }
            if (op === 'site') { out.sites.push(val.toLowerCase().replace(/^www\./, '')); continue; }
            if (op === 'since' || op === 'until') { const t = parseWhen(val); if (t !== undefined) { out[op] = t; continue; } }
            // unknown operator → free text
        }
        free.push(tok);
    }
    out.text = free.join(' ');
    return out;
}

// `(?![a-z0-9])` (not `(?=\s|$)`): the extension can be followed by punctuation - a URL wrapped in
// parens "(…a.png)" or trailed by a comma "a.gif," is still an image - but NOT by another letter
// (so "file.jpglkj" doesn't false-match). The optional ?query is consumed first.
const IMG_RE = /\.(?:jpe?g|png|gif|webp|avif|bmp|svg)(?:\?\S*)?(?![a-z0-9])/i;
const VID_RE = /\.(?:mp4|webm|mov|m4v|ogv|mkv)(?:\?\S*)?(?![a-z0-9])/i;
const URL_RE = /https?:\/\/\S+/i;
const hasImeta = (ev: NostrEvent, kind: 'image' | 'video'): boolean =>
    ev.tags.some((t) => t[0] === 'imeta' && t.slice(1).some((p) => p.startsWith(`m ${kind}`)));

/** Apply the non-relay-native operators (has:/site:) to a fetched note. */
function passesPostFilters(ev: NostrEvent, sq: SearchQuery): boolean {
    const c = ev.content;
    for (const h of sq.has) {
        if (h === 'image' && !IMG_RE.test(c) && !hasImeta(ev, 'image')) return false;
        if (h === 'video' && !VID_RE.test(c) && !hasImeta(ev, 'video')) return false;
        if (h === 'link' && !URL_RE.test(c)) return false;
    }
    if (sq.sites.length) {
        const hosts = (c.match(/https?:\/\/\S+/gi) ?? []).map((u) => { try { return new URL(u).hostname.toLowerCase().replace(/^www\./, ''); } catch { return ''; } });
        if (!sq.sites.some((site) => hosts.some((h) => h === site || h.endsWith(`.${site}`)))) return false;
    }
    return true;
}

export const SEARCH_NOTE_RELAYS = [
    'wss://search.nos.today',
    'wss://relay.ditto.pub',
    'wss://antiprimal.net',
];
export const SEARCH_PROFILE_RELAYS = [
    'wss://relay.ditto.pub',
    'wss://relay.vertexlab.io',
    'wss://antiprimal.net',
];

/** Operator-aware note search: free text → NIP-50 `search`, operators → Filter fields (#t / authors
 * / #p / since / until), then post-filter for has:/site:. `authors`/`mentions` are pre-resolved
 * pubkeys (the caller resolves by:/p: against the profile cache). Merge + dedupe by id, newest first.
 * Returns [] when there's no relay-native constraint (a pure has:/site: query can't be enumerated). */
export async function searchNotes(pool: Pool, relays: string[], sq: SearchQuery, authors: string[], mentions: string[], limit = 30): Promise<NostrEvent[]> {
    // A by:/p: the caller couldn't resolve to ANY pubkey → return nothing, rather than silently
    // dropping the constraint and falling through to an unfiltered text search (which would show
    // results NOT by the requested author). The user asked for a specific person; honor or empty.
    if ((sq.by.length && !authors.length) || (sq.p.length && !mentions.length)) return [];

    // by:<author> → route to the author's OUTBOX (their NIP-65 write relays), not the NIP-50 search
    // relays (which don't hold full timelines); free text + has:/site: post-filter locally.
    if (authors.length) return searchByAuthors(pool, sq, authors, mentions, limit);

    // Otherwise: full-text / tag / time search over the dedicated NIP-50 search relays.
    const postOnly = sq.has.length > 0 || sq.sites.length > 0;
    const filter: Filter = { kinds: [1], limit: postOnly ? limit * 4 : limit }; // over-fetch when we'll post-filter
    if (sq.text) filter.search = sq.text;
    if (mentions.length) filter['#p'] = mentions;
    if (sq.tags.length) filter['#t'] = sq.tags;
    if (sq.since !== undefined) filter.since = sq.since;
    if (sq.until !== undefined) filter.until = sq.until;
    const native = !!(filter.search || filter['#p'] || filter['#t'] || filter.since || filter.until);
    if (!native) return []; // has:/site: alone has nothing to query - we can't ask a relay for "all notes with an image"
    void ensureRelayInfo(relays); // warm NIP-11 in the background; the gate reads whatever's cached
    const events = await pool.query(nip50Capable(relays), filter).catch(() => [] as NostrEvent[]);
    const byId = new Map<string, NostrEvent>();
    for (const ev of events) if (!byId.has(ev.id) && passesPostFilters(ev, sq)) byId.set(ev.id, ev);
    return [...byId.values()].sort((a, b) => b.created_at - a.created_at).slice(0, limit);
}

/** by:<author> notes from the author's WRITE relays (outbox). Relay-native fields (#p / #t / since /
 * until) ride the filter; the free text + has:/site: are matched LOCALLY (write relays are general
 * relays without NIP-50 full-text), so we over-fetch the author's recent notes and filter down. */
async function searchByAuthors(pool: Pool, sq: SearchQuery, authors: string[], mentions: string[], limit: number): Promise<NostrEvent[]> {
    const lists = await fetchRelayLists(pool, INDEXER_RELAYS, authors).catch(() => new Map<string, RelayList>());
    const writes = [...new Set(authors.flatMap((pk) => lists.get(pk)?.write ?? []))];
    const relays = writes.length ? writes : INDEXER_RELAYS; // outbox; indexers only if the author has no kind:10002
    const filter: Filter = { kinds: [1], authors, limit: Math.max(limit * 4, 120) }; // over-fetch for the local text filter
    if (mentions.length) filter['#p'] = mentions;
    if (sq.tags.length) filter['#t'] = sq.tags;
    if (sq.since !== undefined) filter.since = sq.since;
    if (sq.until !== undefined) filter.until = sq.until;
    const events = await pool.query(relays, filter).catch(() => [] as NostrEvent[]);
    const terms = sq.text ? sq.text.toLowerCase().split(/\s+/).filter(Boolean) : []; // free text → AND of terms
    const byId = new Map<string, NostrEvent>();
    for (const ev of events) {
        if (byId.has(ev.id)) continue;
        if (terms.length && !terms.every((t) => ev.content.toLowerCase().includes(t))) continue;
        if (!passesPostFilters(ev, sq)) continue;
        byId.set(ev.id, ev);
    }
    return [...byId.values()].sort((a, b) => b.created_at - a.created_at).slice(0, limit);
}

/** People search: kind:0 full-text → newest profile per pubkey, parsed for rendering. */
export async function searchPeople(pool: Pool, relays: string[], q: string, limit = 20): Promise<{ pubkey: string; profile: Profile }[]> {
    void ensureRelayInfo(relays); // warm NIP-11 in the background; the gate reads whatever's cached
    const events = await pool.query(nip50Capable(relays), { kinds: [0], search: q, limit: limit * 2 }).catch(() => [] as NostrEvent[]);
    const newest = new Map<string, NostrEvent>();
    for (const ev of events) {
        const cur = newest.get(ev.pubkey);
        if (!cur || ev.created_at > cur.created_at) newest.set(ev.pubkey, ev);
    }
    const out: { pubkey: string; profile: Profile }[] = [];
    for (const ev of newest.values()) {
        const profile = parseProfile(ev.content, ev.tags);
        if (profile) out.push({ pubkey: ev.pubkey, profile });
    }
    return out.slice(0, limit);
}
