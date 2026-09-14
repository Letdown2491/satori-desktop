// Empirical seen-on relay memory - the READ side of the gossip/outbox model. We remember which relays we
// have actually RECEIVED each author's events from, not just their DECLARED NIP-65, and fold those into
// future single-event fetches for that author (fetchEvent, the /a/ reader, naddr embeds). This is how an
// event on an UNDECLARED relay becomes findable: a publisher who posts to a relay they don't advertise,
// with no relay hint in the reference, is invisible to a pure declared-outbox client - but once you have
// seen that author there (e.g. by browsing that relay's timeline), Satori remembers the relay and asks it
// again. Recorded centrally in pool.ts from SimplePool.seenOn.
//
// PERSISTED to disk (debounced + flush-on-exit): a daemon restarts on deploys, and without persistence a
// relay you'd taught it (e.g. a wiki-only relay you browsed) would be forgotten on every restart, so events
// there would stop resolving until you visited again. Bounded so the file can't grow without limit.

import { join } from 'node:path';
import { INDEXER_RELAYS, relayKey } from '../nostr/nip65.ts';
import { isPublicWsUrl } from '../ssrf.ts';
import { jsonStore, debouncedFlush } from './json-store.ts';

const MAX_PER_AUTHOR = 6;   // keep the most-recently-seen relays per author (a niche relay + a few others)
const MAX_AUTHORS = 8000;   // LRU cap so the map (and its file) can't grow without bound on a busy daemon
const FILE = process.env.SATORI_SEEN_RELAYS_FILE || join(process.cwd(), '.data', 'seen-relays.json');

const norm = relayKey; // the shared light key-normalizer (nip65)
const INDEXERS = new Set(INDEXER_RELAYS.map(norm));

const { readAll, writeAll } = jsonStore<Record<string, string[]>>(FILE, 'seen-relays');

// pubkey → relay urls. Within a Set, insertion order = recency (oldest dropped first); the Map itself is
// LRU by author (re-inserting on write moves the author to most-recently-used). Hydrated from disk on boot.
const seen = new Map<string, Set<string>>();
for (const [pk, relays] of Object.entries(readAll())) seen.set(pk, new Set(relays.slice(-MAX_PER_AUTHOR)));
while (seen.size > MAX_AUTHORS) seen.delete(seen.keys().next().value as string);

const flusher = debouncedFlush(() => {
    const out: Record<string, string[]> = {};
    for (const [pk, set] of seen) out[pk] = [...set];
    writeAll(out);
}, 10000);

/** Record that `pubkey`'s events arrived from `relayUrls`. Indexer relays are skipped: they're queried on
 * every fetch anyway, so recording them would only crowd out the interesting niche relays under the cap. */
export function recordSeen(pubkey: string, relayUrls: string[]): void {
    // Skip indexers (queried anyway) AND non-public hosts: since we PERSIST + REPLAY these urls into future
    // fetches, an attacker's nevent relay-hint must never teach the daemon a loopback/LAN relay, even once.
    const fresh = relayUrls.map(norm).filter((u) => u && !INDEXERS.has(u) && isPublicWsUrl(u));
    if (!fresh.length) return;
    const set = seen.get(pubkey) ?? new Set<string>();
    seen.delete(pubkey); // reinsert below → author becomes most-recently-used
    for (const u of fresh) { set.delete(u); set.add(u); } // move each to most-recent
    while (set.size > MAX_PER_AUTHOR) set.delete(set.values().next().value as string); // drop oldest relay
    seen.set(pubkey, set);
    while (seen.size > MAX_AUTHORS) seen.delete(seen.keys().next().value as string); // drop oldest author
    flusher.schedule();
}

/** The relays we've empirically seen this author on, to fold into an outbox query for their events. */
export function seenRelaysFor(pubkey: string): string[] {
    return [...(seen.get(pubkey) ?? [])];
}
