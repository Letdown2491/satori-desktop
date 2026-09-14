// NIP-40 expiration timestamp, READ side: clients SHOULD ignore events whose `expiration`
// (unix seconds) has passed. Enforced at the Pool - the one door every network read walks
// through - plus re-checks where a long-lived cache could serve an event that expired mid-TTL
// (the 30-min event cache, the addressable cache). The write side already emits the tag where
// the spec calls for it (NIP-37 drafts).

import { tag1 } from './tags.ts';
import type { NostrEvent } from './types.ts';

/** True when the event carries a valid `expiration` tag in the past. A malformed timestamp
 * does not expire the event (the spec ties the behavior to the timestamp, not the tag). */
export function isExpired(ev: NostrEvent): boolean {
    const raw = tag1(ev, 'expiration');
    if (!raw) return false;
    const t = Number(raw);
    return Number.isFinite(t) && t > 0 && t <= Math.floor(Date.now() / 1000);
}
