// Server-side "undo window": a signed event is held here for a few seconds before
// publishing, so the countdown pill can cancel it. Zero app JS - the pill POLLS
// /note/tick to show the countdown and confirm the note in place when the deadline
// passes. Publishing no longer depends on that poll: a server-side backstop
// auto-commits shortly after the deadline (see holdPublish), so navigating away - or
// closing the tab - still publishes. The explicit Undo button is the only cancel.
// (This diverges from the earlier "undo ≡ close tab" rule, which silently dropped a
// post if you clicked away within the window.) The held signed event is no new trust
// concern: the server already receives + publishes it; we just delay that briefly.

import { randomBytes } from 'node:crypto';
import type { Pool } from './data/pool.ts';
import { publishSigned, type Prepared } from './data/publish.ts';
import { loadHolds, saveHold, removeHold } from './undo-store.ts';
import type { NostrEvent } from './nostr/types.ts';

export interface Held {
    prepared: Prepared;
    deadline: number;
    /** Set for an optimistic thread reply (the tick renders the reply card, not the
     * feed); `inThread` is the thread's nevent, carried onto the confirmed card. */
    reply?: { inThread: string };
}
const held = new Map<string, Held>();

// Briefly remembers tokens that COMMITTED (published) - NOT undone ones - so the countdown poll
// can still render the confirmed reply card when its tick lands after the hold was dropped. The
// deadline tick `await`s the network publish; during that await the next 1s poll would otherwise
// find the hold gone and wipe the optimistic card (and the backstop committing first - e.g. a
// backgrounded tab - has the same effect). TTL-pruned.
interface Committed { signed: NostrEvent; reply?: { inThread: string }; at: number }
const committed = new Map<string, Committed>();
const COMMITTED_TTL = 60_000;

/** A recently-committed token's confirmed event (+ reply context), or undefined if not committed
 * (or expired/undone). Lets the poll re-render the confirmed card instead of clearing it. */
export function getCommitted(token: string, me?: string): { signed: NostrEvent; reply?: { inThread: string } } | undefined {
    const c = committed.get(token);
    if (!c) return undefined;
    if (Date.now() - c.at > COMMITTED_TTL) { committed.delete(token); return undefined; }
    if (me && c.signed.pubkey !== me) return undefined; // only the author's session may read its own hold
    return c;
}

// Grace after the deadline before the server-side backstop auto-commits. Wider than
// the 1s poll interval so an on-page client reliably commits-and-renders the
// confirmed note first - the backstop only takes over once the page is gone.
const GRACE_MS = 1500;
// On resume, honor the publish intent across a restart, but drop holds whose deadline
// is more than this in the past - a long outage means the user has moved on, and a
// note surfacing hours later would be surprising. The common case (docker restart /
// --watch reload) is seconds, well inside this.
const STALE_MS = 60 * 60 * 1000;

/** Schedule the auto-commit backstop for a held token using `pool`. */
function armCommit(pool: Pool, token: string, deadline: number): void {
    const wait = deadline + GRACE_MS - Date.now();
    if (wait <= 0) { void commitIfDue(pool, token); return; }
    setTimeout(() => { void commitIfDue(pool, token); }, wait).unref?.();
}

/** Hold a prepared (signed) event for `seconds`; returns its undo token. */
export function holdPublish(pool: Pool, prepared: Prepared, seconds: number, reply?: { inThread: string }): string {
    const token = randomBytes(12).toString('base64url');
    const entry: Held = { prepared, deadline: Date.now() + seconds * 1000, reply };
    held.set(token, entry);
    saveHold(token, entry); // survive a restart inside the window (resumeHolds re-arms)
    // Auto-commit shortly AFTER the window elapses - even if the page navigated away
    // (the countdown poll stops on navigation, so this backstop is what publishes
    // then). The explicit Undo button is the only cancel. `commitIfDue` deletes-
    // before-publish, so this and a racing client tick can't double-send.
    armCommit(pool, token, entry.deadline);
    return token;
}

/** Re-arm persisted holds after a restart: a signed event held when the server went
 * down still publishes (overdue ones commit now; in-window ones keep their backstop
 * and their client poll resumes). Stale holds (deadline long past) are dropped.
 * Called once at boot with a standalone pool - a resumed commit has no session signer,
 * so NIP-42 auth-required relays in the target set may reject it (best-effort). */
export function resumeHolds(pool: Pool): void {
    const now = Date.now();
    let resumed = 0, dropped = 0;
    for (const [token, h] of Object.entries(loadHolds())) {
        if (now - h.deadline > STALE_MS) { removeHold(token); dropped++; continue; }
        held.set(token, h);
        armCommit(pool, token, h.deadline);
        resumed++;
    }
    if (resumed || dropped) console.log(`[undo] resumed ${resumed} held publish(es)${dropped ? `, dropped ${dropped} stale` : ''}`);
}

/** The held record (for the tick to re-render the pending/confirmed card). Pass `me` (the session pubkey)
 * to enforce ownership - the held event's own author is the owner, so another signed-in user holding a
 * (secret) token still can't read/act on it. */
export function getHeld(token: string, me?: string): Held | undefined {
    const h = held.get(token);
    if (!h) return undefined;
    if (me && h.prepared.signed.pubkey !== me) return undefined;
    return h;
}

/** Seconds remaining (ceil) for a held publish, or null if it's gone (committed/undone). */
export function remainingSeconds(token: string): number | null {
    const h = held.get(token);
    return h ? Math.max(0, Math.ceil((h.deadline - Date.now()) / 1000)) : null;
}

/** Cancel a held publish (undo) - the event is discarded, never sent to a relay. Also clears any
 * committed memory (defensive: an undone token must never render as a confirmed card). */
export function cancelPublish(token: string, me?: string): void {
    if (me && held.get(token)?.prepared.signed.pubkey !== me) return; // only the author's session may undo its own hold
    held.delete(token); removeHold(token); committed.delete(token);
}

/** If the window has elapsed, publish + drop the held event. Deletes BEFORE awaiting
 * the publish so a second concurrent tick can't double-publish. No-op if not due/gone. */
export async function commitIfDue(pool: Pool, token: string): Promise<void> {
    const h = held.get(token);
    if (!h || Date.now() < h.deadline) return;
    held.delete(token);
    removeHold(token);
    // Remember it as committed BEFORE the (possibly slow) publish, so a racing poll renders the
    // confirmed card rather than wiping it. Prune expired entries while we're here.
    committed.set(token, { signed: h.prepared.signed, reply: h.reply, at: Date.now() });
    for (const [t, c] of committed) if (Date.now() - c.at > COMMITTED_TTL) committed.delete(t);
    await publishSigned(pool, h.prepared).catch(() => { /* best-effort, like the immediate path */ });
}
