// DM-relay-list (NIP-17 kind:10050) fetching + publishing. The kind-10050 list is a
// flat set of relay urls (`["relay", "wss://…"]` tags, no read/write split) telling other
// clients where to deliver your gift-wrapped DMs. Without it, senders fall back to guessing
// (your NIP-65 relays / indexers), so some messages never reach you - hence the editor.

import type { Pool } from './pool.ts';
import type { Signer } from './signer.ts';
import type { UnsignedEvent, NostrEvent } from '../nostr/types.ts';
import { INDEXER_RELAYS } from '../nostr/nip65.ts';
import { KIND_DM_RELAYS, parseDmRelays } from '../nostr/nip17.ts';
import { anyAccepted } from './pool.ts';

/** Fetch your own published kind:10050 list ([] if none - the editor wants the real
 * published set, NOT the indexer fallback that the DM read-path uses). */
export async function fetchMyDmRelays(pool: Pool, me: string, readRelays: string[]): Promise<string[]> {
    const ev = await pool.get([...readRelays, ...INDEXER_RELAYS], { kinds: [KIND_DM_RELAYS], authors: [me] }).catch(() => null);
    return ev ? parseDmRelays(ev) : [];
}

/** Build the unsigned kind:10050 from a flat url list (no signing/publishing). */
export function dmRelayListTemplate(me: string, urls: string[]): UnsignedEvent {
    return {
        kind: KIND_DM_RELAYS,
        created_at: Math.floor(Date.now() / 1000),
        tags: urls.map((u) => ['relay', u]),
        content: '',
        pubkey: me,
    };
}

/** Where a kind:10050 should land so others (and you) can discover it: your write
 * relays + the DM relays themselves + indexers. */
function publishTargets(urls: string[], writeRelays: string[]): string[] {
    return [...new Set([...writeRelays, ...urls, ...INDEXER_RELAYS])];
}

/** Publish an extension-signed kind:10050 (nip07: the signer already signed). The new
 * list is derived from the signed event's `relay` tags. */
export async function publishDmRelayListSigned(pool: Pool, signed: NostrEvent, writeRelays: string[]): Promise<string[]> {
    const next = parseDmRelays(signed);
    const results = await pool.publish(publishTargets(next, writeRelays), signed);
    if (!anyAccepted(results)) throw new Error('Failed to publish DM relay list to any relay');
    return next;
}

/** Sign + publish a new kind:10050 from a flat url list (bunker path). Returns the
 * list as published. */
export async function publishDmRelayList(pool: Pool, signer: Signer, me: string, urls: string[], writeRelays: string[]): Promise<string[]> {
    const signed = await signer.signEvent(dmRelayListTemplate(me, urls)) as NostrEvent;
    const results = await pool.publish(publishTargets(urls, writeRelays), signed);
    if (!anyAccepted(results)) throw new Error('Failed to publish DM relay list to any relay');
    return urls;
}
