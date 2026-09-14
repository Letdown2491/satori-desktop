// NIP-88 poll routes: compose (kind 1068), lazy-hydrate the poll box to its real
// ballot/results state, and vote (kind 1018). Like notes, both signing modes go
// through the same builders - bunker signs server-side; nip07 returns H-Nostr-Sign
// and a continuation publishes the extension-signed event.

import { signPoll, publishSigned, captureSigner, type Prepared } from '../data/publish.ts';
import { fetchPollResponses } from '../data/polls.ts';
import {
    KIND_POLL, KIND_POLL_RESPONSE, parsePollOptions, parsePollRelays,
    buildResponseTags, tallyResponses, isPollEnded,
} from '../nostr/nip88.ts';
import { INDEXER_RELAYS, readRelaysFor } from '../nostr/nip65.ts';
import { published, writeRelays, chosenTargets, appendRelayTargets } from '../actions.ts';
import { pollSection, pollOptionRow, POLL_DURATION_DAYS } from '../render/poll.ts';
import { readSignedEvent, requireSigned } from '../nip07.ts';
import { decode } from 'nostr-tools/nip19';
import type { NostrEvent, UnsignedEvent } from '../nostr/types.ts';
import { html } from '../html.ts';
import { isHex64 } from '../nostr/tags.ts';
import { requireLogin } from './common.ts';
import { tryUndoWindow, stayPutCloseModal, landOnFeed } from './undo-window.ts';
import { readForm, redirect, safeReferer, sendFragment, sendSignRequest, notFound, type Ctx } from '../http.ts';
import type { Session } from '../session.ts';
import { signsOnClient } from '../session.ts';

/** GET /compose/poll-option - one more option input (helmjs "+ Add option"). */
export function getPollOption(ctx: Ctx): void {
    if (!requireLogin(ctx)) return;
    sendFragment(ctx, pollOptionRow());
}

// --- compose (kind 1068) ---------------------------------------------------

export async function postPoll(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const form = await readForm(ctx.req);
    const question = (form.get('content') ?? '').trim();
    const options = form.getAll('option').map((o) => o.trim()).filter(Boolean);
    const multiple = form.get('multiple') === '1';
    const days = POLL_DURATION_DAYS[Number(form.get('duration') ?? '0')] ?? null;
    const endsAt = days ? Math.floor(Date.now() / 1000) + days * 86400 : null;
    if (!question || options.length < 2) { redirect(ctx, '/compose?type=poll'); return; }
    const fromModal = form.get('inmodal') === '1'; // modal compose (stay put) vs the full /compose page (land on feed)

    // Polls are NOT schedulable: a poll's endsAt is baked at sign time, so holding a signed poll for a
    // future sweep would make its lifetime count from compose time (a "1 day" poll sent tomorrow arrives
    // already ended). Notes and articles schedule fine; polls always publish now.
    const opts = { question, options, multiple, endsAt };
    if (signsOnClient(s)) {
        const prepared = await signPoll(captureSigner, s.me, s.myRelays!, opts);
        // carry the relay-picker selection onto the publish continuation (nip07), like notes
        const q = new URLSearchParams();
        if (fromModal) q.set('inmodal', '1');
        appendRelayTargets(q, form);
        sendSignRequest(ctx, prepared.signed, `/poll/publish${q.toString() ? `?${q}` : ''}`);
        return;
    }
    try {
        const prepared = await signPoll(s.signer!, s.me, s.myRelays!, opts);
        prepared.writeTargets = chosenTargets(form, s); // relay-picker selection (top-level)
        if (await tryUndoWindow(ctx, s, prepared, { fromModal })) return; // hold + countdown toast (helmjs)
        await publishSigned(s.pool, prepared);
    } catch (err) {
        sendFragment(ctx, html`<div class="notice error">Couldn't post the poll: ${err instanceof Error ? err.message : String(err)}</div>`, {}, 502);
        return;
    }
    // JS: modal compose stays put; the full /compose page lands on the feed. Zero-JS redirects.
    if (ctx.isPartial) { if (fromModal) stayPutCloseModal(ctx); else await landOnFeed(ctx, s as Session & { me: string }); return; }
    redirect(ctx, '/'); // zero-JS: real navigation to the feed
}

/** nip07 continuation for a composed poll. */
export async function postPollPublish(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const signed = await requireSigned(ctx, s.me, KIND_POLL, 'the poll');
    if (!signed) return;
    const fromModal = ctx.query.get('inmodal') === '1';
    const prepared: Prepared = { signed, isReply: false, writeTargets: chosenTargets(ctx.query, s as Session & { me: string }), inboxTargets: [] };
    if (await tryUndoWindow(ctx, s as Session & { me: string }, prepared, { requirePartial: false, fromModal })) return; // nip07 = always JS
    try { await publishSigned(s.pool, prepared); } catch (err) {
        sendFragment(ctx, html`<div class="notice error">Couldn't publish: ${err instanceof Error ? err.message : String(err)}</div>`, {}, 502);
        return;
    }
    if (fromModal) stayPutCloseModal(ctx); else await landOnFeed(ctx, s as Session & { me: string });
}

// --- lazy hydrate ----------------------------------------------------------

/** GET /poll/<nevent> - fetch the poll + its responses, return the correct
 * ballot/results section (swapped into the note's poll box on load). */
export async function getPoll(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const entity = ctx.params.id ?? '';
    let id: string;
    let relays: string[] = [];
    try {
        const d = decode(entity);
        if (d.type === 'nevent') { id = d.data.id; relays = d.data.relays ?? []; }
        else if (d.type === 'note') id = d.data;
        else { notFound(ctx); return; }
    } catch { notFound(ctx); return; }

    const poll = await fetchPoll(s, id, relays);
    if (!poll) { sendFragment(ctx, html``); return; } // leave the instant ballot in place
    sendFragment(ctx, pollSection(poll, await tally(s, poll)));
}

// --- vote (kind 1018) ------------------------------------------------------

export async function postPollVote(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const pollId = ctx.params.pollid ?? '';
    if (!isHex64(pollId)) { notFound(ctx); return; }
    const form = await readForm(ctx.req);
    const poll = await fetchPoll(s, pollId, []);
    if (!poll) { sendFragment(ctx, html`<div class="notice error">Poll not found.</div>`, {}, 404); return; }
    // The votable ballot renders pre-hydration and zero-JS, so enforce the deadline server-side:
    // a vote after endsAt would publish but be dropped from the tally (vanishing silently).
    if (isPollEnded(poll)) { sendFragment(ctx, html`<div class="notice error">This poll has ended.</div>`, {}, 400); return; }
    const valid = new Set(parsePollOptions(poll).map((o) => o.id));
    const chosen = form.getAll('option').filter((o) => valid.has(o));
    if (chosen.length === 0) { sendFragment(ctx, html`<div class="notice error">Pick an option.</div>`, {}, 400); return; }

    const template: UnsignedEvent = { kind: KIND_POLL_RESPONSE, created_at: Math.floor(Date.now() / 1000), tags: buildResponseTags(pollId, chosen), content: '', pubkey: s.me };
    if (signsOnClient(s)) { sendSignRequest(ctx, template, `/poll/vote/${pollId}/publish`); return; }

    const signed = await s.signer!.signEvent(template);
    if (!await publishVote(s, poll, signed)) { sendFragment(ctx, html`<div class="notice error">Couldn't record your vote - no relay accepted it.</div>`, {}, 502); return; }
    // helmjs swaps the results into the poll box; zero-JS reloads the page (the
    // vote is published either way).
    if (!ctx.isPartial) { redirect(ctx, safeReferer(ctx)); return; }
    sendFragment(ctx, pollSection(poll, await tally(s, poll, signed)));
}

/** nip07 continuation for a vote. */
export async function postPollVotePublish(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const pollId = ctx.params.pollid ?? '';
    const signed = await readSignedEvent(ctx.req);
    if (!signed || signed.pubkey !== s.me || signed.kind !== KIND_POLL_RESPONSE
        || !signed.tags.some((t) => t[0] === 'e' && t[1] === pollId)) {
        sendFragment(ctx, html`<div class="notice error">Couldn't verify the vote.</div>`, {}, 400);
        return;
    }
    const poll = await fetchPoll(s, pollId, []);
    if (!poll) { sendFragment(ctx, html`<div class="notice error">Poll not found.</div>`, {}, 404); return; }
    if (isPollEnded(poll)) { sendFragment(ctx, html`<div class="notice error">This poll has ended.</div>`, {}, 400); return; }
    if (!await publishVote(s, poll, signed as NostrEvent)) { sendFragment(ctx, html`<div class="notice error">Couldn't record your vote - no relay accepted it.</div>`, {}, 502); return; }
    // Re-declare placement: the sign-request set H-Reswap:none (poisoning the
    // request's swap), so without these the results would never swap in (the gotcha
    // the like/act continuations also handle). Targets the same poll box, inner.
    sendFragment(ctx, pollSection(poll, await tally(s, poll, signed as NostrEvent)), { 'H-Reswap': 'inner', 'H-Retarget': `#poll-${pollId}` });
}

// --- helpers ---------------------------------------------------------------

function readRelays(s: Session, extra: string[] = []): string[] {
    return [...new Set([...extra, ...readRelaysFor(s.myRelays)])];
}

async function fetchPoll(s: Session & { me: string }, id: string, relays: string[]): Promise<NostrEvent | null> {
    const ev = await s.pool.get(readRelays(s, relays), { ids: [id] }).catch(() => null);
    return ev && ev.kind === KIND_POLL ? ev : null;
}

/** Publish a vote to the user's write relays + the poll's declared vote relays.
 * Returns true iff at least one relay accepted it (allSettled never rejects). */
async function publishVote(s: Session & { me: string }, poll: NostrEvent, signed: NostrEvent): Promise<boolean> {
    const targets = [...new Set([...writeRelays(s), ...parsePollRelays(poll)])];
    return published(s, signed, targets);
}

/** Tally a poll's responses; `mine` (a freshly-signed vote) is folded in so the
 * voter sees their result immediately, before relay propagation. */
async function tally(s: Session & { me: string }, poll: NostrEvent, mine?: NostrEvent) {
    const relays = [...new Set([...parsePollRelays(poll), ...INDEXER_RELAYS, ...(s.myRelays?.read ?? [])])];
    const responses = await fetchPollResponses(s.pool, poll.id, relays);
    return tallyResponses(mine ? [...responses, mine] : responses, s.me, poll);
}
