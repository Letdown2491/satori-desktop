// Zapped-state, served from the persistent engagement cache (data/engagement-cache.ts) - the SAME
// source as likes/reposts/replies, so it's a zero-round-trip set lookup on the hot path. Your zaps
// are synced once (windowed query of 9735 receipts whose `P` tag = you) and read instantly; a zap
// you just made fills immediately via markZapped. One key space covers BOTH notes (event id) and
// articles (canonical naddr), so isZapped works the same for either - no separate article path.
// Anonymous/private zaps carry an ephemeral sender key (no `P`=you), so they stay local-only.

import { cachedZapped, addZapped, ensureEngagementSynced } from './data/engagement-cache.ts';
import type { Session } from './session.ts';

/** Have you zapped this? `key` = a note id OR an article's canonical naddr. */
export function isZapped(s: Session, key: string): boolean {
    return !!s.me && cachedZapped(s.me, key);
}

/** Record a confirmed zap so its button fills immediately and survives reloads (note id or naddr). */
export function markZapped(s: Session, key: string): void {
    if (s.me) addZapped(s.me, key);
}

/** Ensure the engagement cache (which includes your zaps) is syncing - background, idempotent, no
 * per-note query. Covers notes AND articles in one sync. Call it wherever zap glyphs render. */
export function ensureZaps(s: Session & { me: string }): void {
    ensureEngagementSynced(s);
}
