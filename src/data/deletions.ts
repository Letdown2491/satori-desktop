// NIP-09 deletion tombstones - the read side of event deletion. Kind:5 events are consumed
// at the pool chokepoint wherever they arrive and remembered here; every query/get result is
// then filtered against the store, so a deleted event stops rendering everywhere (feeds,
// threads, profiles, the embed cache) with no per-render network cost. The spec's MUST -
// only the author can delete their own event - is enforced structurally: id tombstones are
// KEYED by target-id + deleter-pubkey, so a lookup for an event only ever matches a deletion
// signed by that event's own author (someone else's kind:5 for the same id lands under a
// different key and never applies). Address (`a`) tombstones validate the coordinate's pubkey
// against the kind:5 author at record time, and apply only to versions with created_at at or
// before the deletion's - a later re-publish supersedes the delete, per NIP-09.
//
// Hostile-input posture: any relay can deliver any self-signed kind:5, so what one event may
// write is bounded (tags consumed per event, key length) and YOUR OWN published deletions are
// PINNED in separate maps that relay traffic can never evict - otherwise a stranger's tag-spam
// kind:5 could flush your tombstones and resurrect your deleted posts.
//
// PERSISTED (debounced + flush-on-exit): a tombstone must outlive the 30-min event caches and
// daemon restarts, or deleted events resurrect on reboot. Bounded like seen-relays.

import { join } from 'node:path';
import { jsonStore, debouncedFlush, lruSet } from './json-store.ts';
import { HEX64, coordParts, coordinateOf, isAddressable } from '../nostr/tags.ts';
import type { NostrEvent } from '../nostr/types.ts';

const MAX_TOMBS = 5000;        // per relay-fed map; a tombstone is ~100 bytes, so this stays a small file
const MAX_PINNED = 1000;       // per own-deletions map; only your own publishes write here
const MAX_TAGS_PER_EVENT = 100; // a real NIP-09 delete names a handful of events; tag-spam is an attack
const MAX_KEY = 256;           // coordinate keys embed the d identifier, which is unbounded on the wire
const CHECK_TTL = 30 * 60_000; // don't re-ask relays about the same event's deletion for 30 min
const FILE = process.env.SATORI_DELETIONS_FILE || join(process.cwd(), '.data', 'deletions.json');

interface Stored extends Record<string, unknown> {
    ids?: Record<string, number>; addrs?: Record<string, number>;
    pids?: Record<string, number>; paddrs?: Record<string, number>; // pinned (your own deletions)
}
const { readAll, writeAll } = jsonStore<Stored>(FILE, 'deletions');

// `${targetId}:${deleterPubkey}` → deletion created_at, and coordinate → deletion created_at.
// Map insertion order = recency; oldest entries are dropped first at the cap. The pinned pair
// holds the same shapes but is written only from your own accepted publishes.
const stored = readAll();
const load = (r: Record<string, number> | undefined, cap: number) => new Map(Object.entries(r ?? {}).slice(-cap));
const ids = load(stored.ids, MAX_TOMBS);
const addrs = load(stored.addrs, MAX_TOMBS);
const pinnedIds = load(stored.pids, MAX_PINNED);
const pinnedAddrs = load(stored.paddrs, MAX_PINNED);

const flusher = debouncedFlush(() => {
    writeAll({ ids: Object.fromEntries(ids), addrs: Object.fromEntries(addrs), pids: Object.fromEntries(pinnedIds), paddrs: Object.fromEntries(pinnedAddrs) });
}, 10000);

/** Keep the NEWEST deletion time per key, most-recently-used, oldest evicted over the cap. */
function put(map: Map<string, number>, cap: number, key: string, at: number): void {
    const cur = map.get(key);
    if (cur !== undefined && cur >= at) return;
    lruSet(map, key, at, cap);
}

/** Record a kind:5's targets as tombstones. `e` targets are keyed with the deleter's pubkey
 * (pubkey-matching happens at lookup); `a` targets are only honored when the coordinate's
 * pubkey IS the kind:5 author (the NIP-09 MUST - anyone can e-tag, but an address names its
 * owner, so a mismatch is a cross-author deletion attempt and is dropped). `pin` = this is the
 * logged-in user's own accepted deletion: stored in the pinned maps, immune to relay-fed
 * eviction. Both key halves are lowercased - hex is lowercase per NIP-01, but tag values from
 * the wire aren't guaranteed to be. */
export function recordDeletion(ev: NostrEvent, pin = false): void {
    if (ev.kind !== 5) return;
    const pk = ev.pubkey.toLowerCase();
    const [idMap, idCap] = pin ? [pinnedIds, MAX_PINNED] as const : [ids, MAX_TOMBS] as const;
    const [addrMap, addrCap] = pin ? [pinnedAddrs, MAX_PINNED] as const : [addrs, MAX_TOMBS] as const;
    let touched = false;
    let consumed = 0;
    for (const t of ev.tags) {
        if (t[0] !== 'e' && t[0] !== 'a') continue;
        if (++consumed > MAX_TAGS_PER_EVENT) break; // tag-spam bound: one event can't flush the store
        if (t[0] === 'e' && HEX64.test(t[1] ?? '')) {
            put(idMap, idCap, `${t[1]!.toLowerCase()}:${pk}`, ev.created_at);
            touched = true;
        } else if (t[0] === 'a' && t[1] && t[1].length <= MAX_KEY) {
            const c = coordParts(t[1]);
            if (c && c.pubkey.toLowerCase() === pk) { put(addrMap, addrCap, t[1].toLowerCase(), ev.created_at); touched = true; }
        }
    }
    if (touched) flusher.schedule();
}

/** Is this event deleted by its own author? Id tombstones match only when the deleter IS the
 * event's author (the key embeds both). Address tombstones suppress versions up to the
 * deletion time, so a re-published (newer) version renders again. Pinned (own) tombstones are
 * checked alongside the relay-fed maps. */
export function isDeletedEvent(ev: NostrEvent): boolean {
    const idKey = `${ev.id.toLowerCase()}:${ev.pubkey.toLowerCase()}`;
    if (ids.has(idKey) || pinnedIds.has(idKey)) return true;
    if (!isAddressable(ev.kind)) return false;
    const coord = coordinateOf(ev).toLowerCase();
    if (coord.length > MAX_KEY) return false;
    const at = addrs.get(coord) ?? pinnedAddrs.get(coord);
    return at !== undefined && ev.created_at <= at;
}

// TTL memory for the background deletion sweep: which ids/coordinates we've recently asked
// relays about. In-memory only - a restart just re-checks once, which is fine.
const checked = new Map<string, number>();

/** Should the sweep ask relays about this target (id or coordinate)? True at most once per
 * TTL per target; marks the target as checked when it says yes. Oversized keys (an unbounded
 * `d` identifier) are never checked - they can't be tombstoned either. */
export function shouldCheckDeletion(key: string): boolean {
    if (key.length > MAX_KEY) return false;
    const now = Date.now();
    const last = checked.get(key);
    if (last !== undefined && now - last < CHECK_TTL) return false;
    if (checked.size >= 20000) checked.clear(); // cheap bound; worst case is one early re-check
    checked.set(key, now);
    return true;
}
