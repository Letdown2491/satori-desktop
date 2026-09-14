// Notifications - relationship signals addressed to you, mirroring Satori's
// data/notifications.ts. From your READ (inbox) relays: replies + mentions
// (kind:1 #p=you), zaps you received (kind:9735 #p=you), and votes on your polls
// (kind:1018 #e=your poll ids). No likes/reposts (deliberately omitted), no
// counts. Newest first, paged by time (?until cursor).

import type { Pool } from './pool.ts';
import type { NostrEvent, RelayList } from '../nostr/types.ts';
import { INDEXER_RELAYS, writeRelaysFor } from '../nostr/nip65.ts';
import { KIND_POLL, KIND_POLL_RESPONSE } from '../nostr/nip88.ts';
import { KIND_COMMENT } from '../nostr/nip22.ts';
import { verifyEvent } from 'nostr-tools/pure';
import { trimOldest } from './json-store.ts';

export type NotifType = 'reply' | 'mention' | 'pollvote' | 'zap' | 'reaction' | 'privateReply';
export interface Notif { type: NotifType; event: NostrEvent }
export interface NotifWindow { since?: number; until?: number; limit: number }

/** Your poll ids (kind:1068), so vote-notifications can be paged alongside the rest. */
export async function fetchMyPollIds(pool: Pool, me: string, myRelays: RelayList | null): Promise<string[]> {
    const write = writeRelaysFor(myRelays);
    const polls = await pool.query(write, { kinds: [KIND_POLL], authors: [me], limit: 50 }).catch(() => [] as NostrEvent[]);
    return polls.map((p) => p.id);
}

/** Fetch notifications addressed to `me` within a time window, newest first. */
export async function fetchNotifications(
    pool: Pool, me: string, myRelays: RelayList | null, pollIds: string[] | Promise<string[]>, win: NotifWindow, includeReactions = false,
): Promise<Notif[]> {
    const read = myRelays?.read.length ? myRelays.read : INDEXER_RELAYS;
    const bounds = {
        ...(win.since !== undefined ? { since: win.since } : {}),
        ...(win.until !== undefined ? { until: win.until } : {}),
    };
    // kind:7 reactions are only queried when the "reactions in notifications" pref is on (off by default).
    const taggedKinds = includeReactions ? [1, KIND_COMMENT, 7, 9735] : [1, KIND_COMMENT, 9735];
    // Over-fetch each stream (2x), merge, then slice to `limit`: the page's oldest item then sits
    // ABOVE each stream's own oldest, so the `until` cursor for the next page can't skip events in
    // the gap between the two streams' tails (the feed paginates the same way). One round-trip each.
    const fetchLimit = win.limit * 2;
    // The tagged stream (replies/mentions/zaps) doesn't depend on your poll ids, so it starts now; the
    // poll-vote query waits only on `pollIds` (which may still be in flight) and runs concurrently with it.
    const [tagged, votes] = await Promise.all([
        pool.query(read, { kinds: taggedKinds, '#p': [me], limit: fetchLimit, ...bounds }).catch(() => [] as NostrEvent[]),
        Promise.resolve(pollIds).then((ids) =>
            ids.length
                ? pool.query(read, { kinds: [KIND_POLL_RESPONSE], '#e': ids, limit: fetchLimit, ...bounds }).catch(() => [] as NostrEvent[])
                : [] as NostrEvent[]),
    ]);

    const items: Notif[] = [];
    const seen = new Set<string>();
    const add = (type: NotifType, ev: NostrEvent) => {
        if (ev.pubkey === me || seen.has(ev.id)) return; // skip your own actions / dupes
        seen.add(ev.id);
        items.push({ type, event: ev });
    };
    for (const ev of tagged) {
        if (ev.kind === 9735) add('zap', ev);
        // kind:7 reaction (before the e-tag check - a reaction carries an e tag). A `-` DISLIKE is omitted
        // entirely: a stranger's thumbs-down is pure agitation with no reply/zap/conversation value, so we
        // never surface it (consistent with hiding the like button + reaction counts). +/emoji still show.
        else if (ev.kind === 7) { if (ev.content !== '-') add('reaction', ev); }
        else if (ev.kind === KIND_COMMENT) add('reply', ev); // NIP-22: always a reply (article comments carry a/A, no e)
        else add(ev.tags.some((t) => t[0] === 'e') ? 'reply' : 'mention', ev);
    }
    for (const ev of votes) add('pollvote', ev);

    items.sort((a, b) => b.event.created_at - a.event.created_at);
    return items.slice(0, win.limit);
}

export interface ZapInfo { sender: string | null; sats: number }

/** Msats encoded in a bolt11 invoice's HRP (`lnbc<amount><multiplier>`). The amount itself
 * contains digits, so we anchor on the `[munp]?` multiplier before the bech32 `1` separator
 * rather than splitting on `1`. Returns 0 for an amountless ("any amount") invoice. */
function msatsFromBolt11(invoice: string): number {
    const m = invoice.toLowerCase().match(/^lnbc(\d+)([munp]?)1/);
    if (!m) return 0;
    const amount = Number(m[1]);
    if (!Number.isFinite(amount) || amount <= 0) return 0;
    switch (m[2]) {
        case 'm': return amount * 1e8;   // milli-BTC
        case 'u': return amount * 1e5;   // micro-BTC
        case 'n': return amount * 1e2;   // nano-BTC
        case 'p': return amount / 10;    // pico-BTC (must be a multiple of 10 msat)
        default: return amount * 1e11;   // bare = whole BTC
    }
}

/** Pull the sender + sats out of a kind:9735 zap receipt. The zapper identity AND the claimed amount both
 * live in the embedded kind:9734 request (the `description` tag), which ANYONE can forge in a receipt tagged
 * to you - SimplePool verifies only the receipt's OWN signature, not the embedded request's. So we verify the
 * request's signature and confirm its p/e/a match the receipt (else the named zapper is spoofable), and take
 * the amount from the bolt11 invoice (what was actually paid), authoritative over the request's self-declared
 * `amount`. An unverifiable request yields an ANONYMOUS zap - real sats from the invoice, no name, the
 * "someone zapped you" path - never a spoofed identity.
 *
 * Memoized by event id (receipts are immutable): the verify is a SYNCHRONOUS schnorr check (~ms of
 * blocked event loop), and one render calls this per zap from the mute filter, the author resolve,
 * AND the row renderer - plus the bell poller re-parses the same receipts every tick. */
const zapInfoMemo = new Map<string, ZapInfo>();
const ZAP_MEMO_CAP = 1000;
export function parseZapReceipt(ev: NostrEvent): ZapInfo {
    const hit = zapInfoMemo.get(ev.id);
    if (hit) return hit;
    const info = parseZapReceiptUncached(ev);
    zapInfoMemo.set(ev.id, info);
    trimOldest(zapInfoMemo, ZAP_MEMO_CAP);
    return info;
}

function parseZapReceiptUncached(ev: NostrEvent): ZapInfo {
    const bolt11 = ev.tags.find((t) => t[0] === 'bolt11')?.[1];
    const paid = bolt11 ? msatsFromBolt11(bolt11) : 0;
    const desc = ev.tags.find((t) => t[0] === 'description')?.[1];
    let sender: string | null = null;
    let claimed = 0;
    if (desc) {
        try {
            const req = JSON.parse(desc) as NostrEvent;
            // Only trust the zapper/amount from a validly signed 9734 whose target tags match the receipt's.
            const matches = (name: string): boolean => {
                const on = ev.tags.find((t) => t[0] === name)?.[1];
                return !on || req.tags?.find((t) => t[0] === name)?.[1] === on;
            };
            if (req.kind === 9734 && verifyEvent(req as never) && matches('p') && matches('e') && matches('a')) {
                sender = req.pubkey ?? null;
                claimed = Number(req.tags?.find((t) => t[0] === 'amount')?.[1]) || 0;
            }
        } catch { /* malformed description → anonymous */ }
    }
    const msats = paid > 0 ? paid : claimed; // bolt11 (actually paid) is authoritative; request amount only as fallback
    return { sender, sats: Math.round(msats / 1000) };
}
