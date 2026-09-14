// Shared NIP-17 DM relay routing + the two-target publish, used by BOTH signing paths
// (data/dms.ts bunker, data/dms-nip07.ts nip07). One cache here, so the DM-relay editor's
// clearDmRelaysCache invalidates the lookup whichever path is active (each path used to keep
// its OWN cache, so clearing one left the other stale). kind:10050 almost never changes, but
// this lookup runs on every DM op + the 90s dot poll, so it's memoized per pubkey (TTL +
// in-flight coalescing) - and a never-published 10050 stops re-paying GET_MAX_WAIT every cycle.
// The 10-min TTL picks up out-of-band changes; the in-app editor invalidates your entry on save.

import { INDEXER_RELAYS } from '../nostr/nip65.ts';
import { coalesceOne } from './coalesce.ts';
import { fetchRelayLists } from './relays.ts';
import { parseDmRelays, KIND_DM_RELAYS } from '../nostr/nip17.ts';
import type { NostrEvent } from '../nostr/types.ts';
import type { Session } from '../session.ts';

const DM_RELAYS_TTL_MS = 10 * 60 * 1000;
const dmRelaysCache = new Map<string, { relays: string[]; at: number }>();
const dmRelaysInflight = new Map<string, Promise<string[]>>();

/** Drop a cached DM-relay lookup (all, or one pubkey) so the next read re-fetches.
 * Called after the DM-relay editor publishes a new kind:10050. */
export function clearDmRelaysCache(pubkey?: string): void {
    if (pubkey) dmRelaysCache.delete(pubkey); else dmRelaysCache.clear();
}

/** A pubkey's RAW kind:10050 DM-inbox relays (empty if none published), memoized. Callers layer their
 * own fallback: READ paths widen to indexers (dmRelaysOf); the PUBLISH path stays strict (NIP-17). */
async function rawDmRelays(s: Session, pubkey: string): Promise<string[]> {
    const hit = dmRelaysCache.get(pubkey);
    if (hit && Date.now() - hit.at < DM_RELAYS_TTL_MS) return hit.relays;
    return coalesceOne(dmRelaysInflight, pubkey, async (): Promise<string[]> => {
        const ev = await s.pool.get([...(s.myRelays?.read ?? []), ...INDEXER_RELAYS], { kinds: [KIND_DM_RELAYS], authors: [pubkey] }).catch(() => null);
        const relays = ev ? parseDmRelays(ev) : [];
        dmRelaysCache.set(pubkey, { relays, at: Date.now() }); // cache raw (incl. empty = negative cache)
        return relays;
    });
}

/** A pubkey's kind:10050 DM-inbox relays (or the indexers as a fallback), memoized. For READ/discovery. */
export async function dmRelaysOf(s: Session, pubkey: string): Promise<string[]> {
    const raw = await rawDmRelays(s, pubkey);
    return raw.length ? raw : INDEXER_RELAYS;
}

/** Where to PUBLISH a gift wrap for a pubkey (NIP-17: "clients MUST only publish to the relays listed
 * in the recipient's kind:10050"). We honor that strictly: the recipient's 10050 relays, and NEVER
 * public indexers. Soft fallback (the user's choice) when they published no 10050: their NIP-65 read
 * relays (where they receive regular events) - still their own declared relays, not a broadcast. Self
 * uses our own configured read set. Empty means undeliverable (the caller surfaces it). */
async function dmPublishRelays(s: Session, pubkey: string): Promise<string[]> {
    const raw = await rawDmRelays(s, pubkey);
    if (raw.length) return raw;
    if (pubkey === s.me) return s.myRelays?.read ?? [];
    // Discover the peer's relay list from indexers (reading it there is fine - we don't publish the wrap
    // there), then deliver only to their read relays.
    const lists = await fetchRelayLists(s.pool, [...(s.myRelays?.read ?? []), ...INDEXER_RELAYS], [pubkey]).catch(() => null);
    return lists?.get(pubkey)?.read ?? [];
}

/** Where to READ your own incoming wraps: your DM relays ∪ your read relays ∪ indexers.
 * Spec says senders publish to your kind-10050 DM relays, but in practice wraps also land on
 * your read relays / indexers (and some clients ignore 10050), so we query all three to find
 * everything. The 30s list cache absorbs the cost on repeat visits. */
export async function myDmReadRelays(s: Session): Promise<string[]> {
    return [...new Set([...(await dmRelaysOf(s, s.me!)), ...(s.myRelays?.read ?? []), ...INDEXER_RELAYS])];
}

/** Publish a sealed DM to both inboxes: the wrap-to-peer to the peer's DM/read relays, the wrap-to-self
 * to your own. Shared by both signing paths so the routing stays identical. NIP-17: publishes ONLY to the
 * recipients' own relays, never public indexers. Throws if the peer has no deliverable relays, so the
 * caller's existing send-failure path surfaces "couldn't deliver" rather than silently leaking nowhere. */
export async function publishWrapPair(s: Session, peer: string, toPeer: NostrEvent, toSelf: NostrEvent): Promise<void> {
    const [peerRelays, selfRelays] = await Promise.all([dmPublishRelays(s, peer), dmPublishRelays(s, s.me!)]);
    if (!peerRelays.length) throw new Error('recipient has no DM inbox (kind:10050) or relay list to deliver to');
    await Promise.all([
        s.pool.publish(peerRelays, toPeer),
        selfRelays.length ? s.pool.publish(selfRelays, toSelf) : Promise.resolve([]), // no self-copy if we have no relays; not fatal
    ]);
}
