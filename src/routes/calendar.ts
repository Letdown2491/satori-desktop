// NIP-52 calendar action routes: download an event as .ics (local, no signing), and RSVP (kind 31925).
// RSVP mirrors the poll-vote flow - both signing families go through one builder: bunker signs server-
// side; nip07 gets an H-Nostr-Sign request and a continuation publishes the extension-signed event.
// The RSVP's `d` = the event coordinate, so re-RSVPing REPLACES (addressable) instead of stacking.

import { decodeNaddr } from '../nostr/nip19.ts';
import { nowSec } from '../nostr/tags.ts';
import { trimOldest } from '../data/json-store.ts';
import { requireLogin } from './common.ts';
import { readForm, sendFragment, sendDownload, sendSignRequest, redirect, safeReferer, notFound, type Ctx } from '../http.ts';
import { html } from '../html.ts';
import { signsOnClient } from '../session.ts';
import type { Session } from '../session.ts';
import { readSignedEvent } from '../nip07.ts';
import { published, writeRelays } from '../actions.ts';
import { fetchRelayLists } from '../data/relays.ts';
import { INDEXER_RELAYS, readRelaysFor } from '../nostr/nip65.ts';
import {
    KIND_CALENDAR_DATE, KIND_CALENDAR_TIME, KIND_CALENDAR_RSVP,
    buildRsvp, buildIcs, isRsvpStatus, type RsvpStatus,
} from '../nostr/nip52.ts';
import { rsvpButtons, rsvpId } from '../render/calendar.ts';
import type { NostrEvent } from '../nostr/types.ts';

interface CalTarget { kind: number; pubkey: string; identifier: string; relays: string[]; coord: string }

/** Decode an naddr and accept it only if it addresses a calendar EVENT (31922/31923). */
function decodeCalTarget(naddr: string): CalTarget | null {
    const d = decodeNaddr(naddr);
    if (!d) return null;
    if (d.kind !== KIND_CALENDAR_DATE && d.kind !== KIND_CALENDAR_TIME) return null;
    return { kind: d.kind, pubkey: d.pubkey, identifier: d.identifier, relays: d.relays, coord: d.coord };
}

function readRelays(s: Session, extra: string[] = []): string[] {
    return [...new Set([...extra, ...readRelaysFor(s.myRelays)])];
}

/** Fetch the addressable event for a target (its latest version). */
async function fetchCalEvent(s: Session & { me: string }, t: CalTarget): Promise<NostrEvent | null> {
    return s.pool.get(readRelays(s, t.relays), { kinds: [t.kind], authors: [t.pubkey], '#d': [t.identifier] }).catch(() => null);
}

const RSVP_SCAN_CAP = 500; // bound the per-event RSVP scan; the count saturates at this for huge events

type RsvpScan = Map<string, { status: RsvpStatus; at: number }>; // pubkey -> latest status
const RSVP_TTL_MS = 90_000; // memoize the who's-going scan: re-scrolling a calendar card shouldn't re-query
const rsvpScanCache = new Map<string, { latest: RsvpScan; cachedAt: number }>();

/** The RSVP state for an event: the user's own status + how many are GOING. One query (all 31925s for
 * the event), deduped to each person's LATEST status, then memoized by coordinate (90s) so repeated
 * lazy-hydrations of the same card don't re-scan. `knownMine` (the status you just published, which
 * relays may not echo back yet) forces a fresh query, overrides your own entry so the count + highlight
 * reflect it instantly, AND writes that choice into the cache so the next hydrate stays in sync. */
async function fetchRsvpState(s: Session & { me: string }, t: CalTarget, knownMine?: RsvpStatus): Promise<{ mine: RsvpStatus | null; going: number }> {
    const cached = rsvpScanCache.get(t.coord);
    let latest: RsvpScan;
    if (!knownMine && cached && Date.now() - cached.cachedAt < RSVP_TTL_MS) {
        latest = cached.latest;
    } else {
        const evs = await s.pool.query(readRelays(s, t.relays), { kinds: [KIND_CALENDAR_RSVP], '#a': [t.coord], limit: RSVP_SCAN_CAP }).catch(() => [] as NostrEvent[]);
        latest = new Map();
        for (const ev of evs) {
            const st = ev.tags.find((tag) => tag[0] === 'status')?.[1];
            if (!isRsvpStatus(st)) continue;
            const prev = latest.get(ev.pubkey);
            if (!prev || ev.created_at > prev.at) latest.set(ev.pubkey, { status: st, at: ev.created_at });
        }
        if (knownMine) latest.set(s.me, { status: knownMine, at: nowSec() });
        rsvpScanCache.set(t.coord, { latest, cachedAt: Date.now() });
        trimOldest(rsvpScanCache, 500);
    }
    let going = 0;
    for (const v of latest.values()) if (v.status === 'accepted') going++;
    return { mine: latest.get(s.me)?.status ?? null, going };
}

/** Publish an RSVP to the user's write relays, the event's relay hints, and the ORGANIZER's read relays
 * (NIP-65 inbox) so the organizer actually receives it. True iff a relay accepted it. */
async function publishRsvp(s: Session & { me: string }, t: CalTarget, signed: NostrEvent): Promise<boolean> {
    const lists = await fetchRelayLists(s.pool, INDEXER_RELAYS, [t.pubkey]).catch(() => null);
    const organizerInbox = lists?.get(t.pubkey)?.read ?? [];
    const targets = [...new Set([...writeRelays(s), ...t.relays, ...organizerInbox])];
    return published(s, signed, targets);
}

// --- GET /cal/ics/<naddr> : download .ics ----------------------------------

export async function getCalIcs(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const t = decodeCalTarget(ctx.params.naddr ?? '');
    if (!t) { notFound(ctx, 'Not a calendar event'); return; }
    const ev = await fetchCalEvent(s, t);
    if (!ev) { notFound(ctx, 'Event not found'); return; }
    const name = (ev.tags.find((x) => x[0] === 'title')?.[1] ?? 'event');
    sendDownload(ctx, buildIcs(ev), `${name}.ics`, 'text/calendar; charset=utf-8');
}

// --- RSVP (kind 31925) -----------------------------------------------------

/** GET /cal/rsvp/<naddr> : lazy-hydrate the RSVP box to the user's current status. */
export async function getCalRsvp(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const naddr = ctx.params.naddr ?? '';
    const t = decodeCalTarget(naddr);
    if (!t) { notFound(ctx, 'Not a calendar event'); return; }
    const { mine, going } = await fetchRsvpState(s, t);
    sendFragment(ctx, rsvpButtons(naddr, mine, going));
}

/** POST /cal/rsvp/<naddr> : record an RSVP. Bunker signs here; nip07 gets a sign request. */
export async function postCalRsvp(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const naddr = ctx.params.naddr ?? '';
    const t = decodeCalTarget(naddr);
    if (!t) { notFound(ctx, 'Not a calendar event'); return; }
    const form = await readForm(ctx.req);
    const status = form.get('status') ?? '';
    if (!isRsvpStatus(status)) { sendFragment(ctx, html`<div class="notice error">Pick a response.</div>`, {}, 400); return; }

    const template = buildRsvp(s.me, t.coord, t.pubkey, status, t.relays[0]);
    if (signsOnClient(s)) { sendSignRequest(ctx, template, `/cal/rsvp/${naddr}/publish`); return; }

    const signed = await s.signer!.signEvent(template);
    if (!await publishRsvp(s, t, signed)) { sendFragment(ctx, html`<div class="notice error">Couldn't record your RSVP - no relay accepted it.</div>`, {}, 502); return; }
    if (!ctx.isPartial) { redirect(ctx, safeReferer(ctx)); return; }
    const { going } = await fetchRsvpState(s, t, status);
    sendFragment(ctx, rsvpButtons(naddr, status, going));
}

/** nip07 continuation: publish the extension-signed RSVP. */
export async function postCalRsvpPublish(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const naddr = ctx.params.naddr ?? '';
    const t = decodeCalTarget(naddr);
    if (!t) { notFound(ctx, 'Not a calendar event'); return; }
    const signed = await readSignedEvent(ctx.req);
    if (!signed || signed.pubkey !== s.me || signed.kind !== KIND_CALENDAR_RSVP
        || !signed.tags.some((tag) => tag[0] === 'a' && tag[1] === t.coord)) {
        sendFragment(ctx, html`<div class="notice error">Couldn't verify the RSVP.</div>`, {}, 400);
        return;
    }
    if (!await publishRsvp(s, t, signed as NostrEvent)) { sendFragment(ctx, html`<div class="notice error">Couldn't record your RSVP - no relay accepted it.</div>`, {}, 502); return; }
    const status = (signed.tags.find((tag) => tag[0] === 'status')?.[1] ?? 'accepted') as RsvpStatus;
    const { going } = await fetchRsvpState(s, t, status);
    // The sign-request poisoned the swap (H-Reswap:none); re-declare placement to swap the updated
    // buttons back into the box (same gotcha the poll/like continuations handle).
    sendFragment(ctx, rsvpButtons(naddr, status, going), { 'H-Reswap': 'inner', 'H-Retarget': `#${rsvpId(naddr)}` });
}
