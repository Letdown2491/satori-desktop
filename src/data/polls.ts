// NIP-88 poll responses: fetch the kind:1018 votes for a poll, and publish your
// own. Votes go to your write relays + the poll's own `relay` tags (per spec),
// so the author and other clients can find them.

import type { Pool } from './pool.ts';
import type { NostrEvent } from '../nostr/types.ts';
import { KIND_POLL_RESPONSE } from '../nostr/nip88.ts';

/** Fetch kind:1018 responses referencing a poll. The net is necessarily wide +
 * approximate - votes scatter across voters' relays (outbox-model caveat). */
export async function fetchPollResponses(pool: Pool, pollId: string, relays: string[]): Promise<NostrEvent[]> {
    if (relays.length === 0) return [];
    return pool.query(relays, { kinds: [KIND_POLL_RESPONSE], '#e': [pollId], limit: 1000 }).catch(() => []);
}
