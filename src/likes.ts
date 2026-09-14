// Liked-state, now served from the persistent engagement cache (data/engagement-
// cache.ts) instead of a per-page relay query: your whole like history is synced once
// and read as an instant set lookup. `ensureLikes` just makes sure that sync has been
// kicked off (idempotent, background) - no per-note round-trip.

import { ensureEngagementSynced } from './data/engagement-cache.ts';
import { ensureUserEmoji } from './data/emoji-sets.ts';
import type { Session } from './session.ts';

/** Ensure the engagement cache is syncing (background, idempotent). No per-note query. When reactions
 * are enabled, also warm the user's NIP-30 custom emoji - FIRE-AND-FORGET (not awaited): the picker
 * degrades to the curated unicode palette while it warms, so never block first paint on a relay fetch
 * (which can be ~24s on a cold Tor circuit). TTL-cached + single-flight, so it populates within a render. */
export async function ensureLikes(s: Session & { me: string }, _noteIds: string[]): Promise<void> {
    ensureEngagementSynced(s);
    if (s.reactions) void ensureUserEmoji(s);
}
