// NIP-17 DM routes. Bunker decrypts server-side in-process (the fast path); nip07
// drives the decrypt/encrypt/sign through the browser extension via the nip07-hateoas
// batch chain. For nip07, /messages and /messages/:peer render a shell whose boosted
// trigger fires a `…/sync` GET that returns the batch sign-chain; its terminal response
// swaps the decrypted list/thread into the shell. Send is an encrypt-batch -> sign-batch
// -> local-wrap chain. See [[nip17-dms-plan]].

import { pubkeyFromBech } from '../nostr/nip19.ts';
import { loadConversations, loadThread, sendDm, hasDmRelayList, hasUnprocessedWraps, cachedThread } from '../data/dms.ts';
import {
    beginSync, applySeals, applyRumors, finalizeSync, beginSend, sealStep, wrapStep, hasUnprocessedWrapsNip07, cachedInboxNip07, cachedThreadNip07, legacyBatch, applyLegacy, chainView, chainWarm,
} from '../data/dms-nip07.ts';
import { dmListPage, dmThreadPage, convList, reqList, listSyncShell, dmGate, dmBubble, threadInner, olderFragment, newMessageModal, newMessagePage, recipientResults } from '../render/dms.ts';
import { dmDotInner } from '../render/layout.ts';
import { searchPeople } from '../data/search.ts';
import { ensureDmBaseline, markDmRead } from '../data/dm-read.ts';
import { readAppearance } from '../theme.ts';
import { html } from '../html.ts';
import { requireLogin, ensureProfiles, chromeFor } from './common.ts';
import { readForm, redirect, sendPage, sendFragment, sendSignRequest, readBatchResults, hasBatchCaps, hasCap, notFound, type Ctx } from '../http.ts';
import type { Session } from '../session.ts';
import { signsOnClient, signsOnServer } from '../session.ts';
import type { DmInbox, DmMessage } from '../data/dms.ts';

type Session07 = Session & { me: string };

// Re-assert the swap placement on a chain's TERMINAL fragment: the batch sign-requests
// carried H-Reswap:none (so the JSON wasn't swapped), which mutates the request's swap
// to "none" - without these the decrypted result never lands (mirrors actions.ts). The
// trigger's own h-target/h-swap is restated here.
const PLACE_LIST = { 'H-Reswap': 'outer', 'H-Retarget': '#dm-sync' };
const PLACE_THREAD = { 'H-Reswap': 'inner', 'H-Retarget': '#dm-messages' };
const PLACE_APPEND = { 'H-Reswap': 'append', 'H-Retarget': '#dm-messages' };
const PLACE_OLDER = { 'H-Reswap': 'outer', 'H-Retarget': '#dm-older' }; // scroll-up prepend

/** Where a sync-chain error/timeout should land: thread chains at #dm-messages, list at #dm-sync. */
const errPlace = (view: 'inbox' | 'requests' | 'thread'): typeof PLACE_LIST | typeof PLACE_THREAD =>
    view === 'thread' ? PLACE_THREAD : PLACE_LIST;

/** A background prewarm (the chrome's #dm-prewarm, on every page) renders NOTHING: empty body +
 * H-Reswap:none, so the sync's placement headers can't land convList / a gate / an error onto whatever
 * page fired it. Without this, a cold-start prewarm dumps the whole DM list into the feed. */
const silentPrewarm = (ctx: Ctx): void => sendFragment(ctx, html``, { 'H-Reswap': 'none' });

/** Resolve a :peer path segment (hex pubkey / npub / nprofile) to a hex pubkey, or null. */
function resolvePeer(id: string): string | null {
    return pubkeyFromBech(id);
}

/** GET /messages - the Inbox (Messages tab). Full-width conversation list. nip07 renders the
 * decrypting shell when cold (its trigger fires /messages/sync). */
export async function getMessages(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    ensureDmBaseline(s.me, Math.floor(Date.now() / 1000)); // forward-looking unread baseline
    const chrome = chromeFor(ctx, s, { title: 'Messages' });
    if (signsOnClient(s)) {
        const cached = cachedInboxNip07(s.me);
        if (cached) {
            await ensureProfiles(s, [...cached.conversations, ...cached.requests].map((c) => c.peer));
            sendPage(ctx, dmListPage({ tab: 'messages', list: convList(cached, s.profiles), unreadConvs: cached.conversations.some((c) => c.unread), unreadReqs: cached.requests.some((c) => c.unread) }), chrome); return;
        }
        sendPage(ctx, dmListPage({ tab: 'messages', list: listSyncShell('inbox') }), chrome); return;
    }
    const inbox = await loadConversations(s);
    if (!inbox) { sendPage(ctx, dmGate(), chrome); return; }
    await ensureProfiles(s, [...inbox.conversations, ...inbox.requests].map((c) => c.peer));
    const hasDmRelays = await hasDmRelayList(s);
    sendPage(ctx, dmListPage({ tab: 'messages', list: convList(inbox, s.profiles), unreadConvs: inbox.conversations.some((c) => c.unread), unreadReqs: inbox.requests.some((c) => c.unread), relayNudge: !hasDmRelays }), chrome);
}

/** GET /messages/requests - the Requests tab (strangers). */
export async function getRequests(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    ensureDmBaseline(s.me, Math.floor(Date.now() / 1000));
    const chrome = chromeFor(ctx, s, { title: 'Messages' });
    if (signsOnClient(s)) {
        const cached = cachedInboxNip07(s.me);
        if (cached) {
            await ensureProfiles(s, cached.requests.map((c) => c.peer));
            sendPage(ctx, dmListPage({ tab: 'requests', list: reqList(cached.requests, s.profiles), unreadConvs: cached.conversations.some((c) => c.unread), unreadReqs: cached.requests.some((c) => c.unread) }), chrome); return;
        }
        sendPage(ctx, dmListPage({ tab: 'requests', list: listSyncShell('requests') }), chrome); return;
    }
    const inbox = await loadConversations(s);
    const reqs = inbox?.requests ?? [];
    await ensureProfiles(s, reqs.map((c) => c.peer));
    sendPage(ctx, dmListPage({ tab: 'requests', list: reqList(reqs, s.profiles), unreadConvs: inbox?.conversations.some((c) => c.unread) ?? false, unreadReqs: reqs.some((c) => c.unread) }), chrome);
}

/** GET /messages/dot - the quiet unread poller (re-arms; lights on undecrypted wraps). */
export async function getMessagesDot(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    if (!ctx.isPartial) { redirect(ctx, '/messages'); return; }
    const unread = signsOnServer(s) ? await hasUnprocessedWraps(s) : await hasUnprocessedWrapsNip07(s);
    sendFragment(ctx, dmDotInner(unread, false));
}

/** GET /messages/:peer - the full-width thread. Renders from the warm cache when possible
 * (no spinner); else the bunker decrypts inline / nip07 runs the sync chain. Opening it marks
 * the conversation read. The boosted nav from the list carries the ink-wash transition. */
export async function getThread(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const peer = resolvePeer(ctx.params.peer ?? '');
    if (!peer) { notFound(ctx); return; }
    await ensureProfiles(s, [peer]);
    ensureDmBaseline(s.me, Math.floor(Date.now() / 1000));
    markDmRead(s.me, peer, Math.floor(Date.now() / 1000)); // opening clears its unread dot
    const chrome = chromeFor(ctx, s, { title: 'Messages' });

    if (signsOnClient(s)) {
        const warm = cachedThreadNip07(s.me, peer);
        sendPage(ctx, dmThreadPage(peer, warm ?? [], s.profiles, s.me, warm ? { cursor: null } : { sync: true }), chrome); return;
    }
    const warm = cachedThread(s, peer);
    if (warm) { sendPage(ctx, dmThreadPage(peer, warm, s.profiles, s.me, { cursor: null }), chrome); return; }
    const thread = await loadThread(s, peer);
    if (!thread) { sendPage(ctx, dmGate(), chrome); return; }
    sendPage(ctx, dmThreadPage(peer, thread.messages, s.profiles, s.me, { cursor: thread.cursor }), chrome);
}

/** GET /messages/new[?q=] - the New-message recipient picker. No `q` -> the modal shell;
 * with `q` -> just the people-search results (swapped into #dm-pick-results). */
export async function getNewMessage(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const q = ctx.query.get('q');
    const query = (q ?? '').trim();
    // Live-search results swapped into #dm-pick-results (typing in the picker).
    if (q !== null && ctx.isPartial) {
        const people = query ? await searchPeople(s.pool, readAppearance(ctx).searchProfileRelays, query).catch(() => []) : [];
        for (const p of people) s.profiles.set(p.pubkey, p.profile);
        sendFragment(ctx, recipientResults(query, people, s.profiles));
        return;
    }
    // The modal shell (opener targets #modal).
    if (ctx.isPartial) { sendFragment(ctx, newMessageModal('', [], s.profiles)); return; }
    // No-JS full navigation -> a real page (search form + results) with chrome, so the picker
    // degrades: the search form is a GET and recipient rows are plain links.
    const people = query ? await searchPeople(s.pool, readAppearance(ctx).searchProfileRelays, query).catch(() => []) : [];
    for (const p of people) s.profiles.set(p.pubkey, p.profile);
    sendPage(ctx, newMessagePage(query, people, s.profiles), chromeFor(ctx, s, { title: 'Messages' }));
}

/** POST /messages/read-all - clear the unread dots for the current tab's conversations at
 * once (mark each read), then re-swap the list. `view=requests` does just the Requests bucket. */
export async function postReadAll(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const form = await readForm(ctx.req);
    const view = (form.get('view') ?? '') === 'requests' ? 'requests' : 'messages';
    const inbox = signsOnClient(s) ? cachedInboxNip07(s.me) : await loadConversations(s);
    if (!inbox) { notFound(ctx); return; }
    const bucket = view === 'requests' ? inbox.requests : inbox.conversations;
    const now = Math.floor(Date.now() / 1000);
    for (const c of bucket) { markDmRead(s.me, c.peer, now); c.unread = false; }
    await ensureProfiles(s, bucket.map((c) => c.peer));
    sendFragment(ctx, view === 'requests' ? reqList(inbox.requests, s.profiles) : convList(inbox, s.profiles));
}

/** GET /messages/:peer/older?until=<ts> - one older window, prepended above the thread.
 * The intersect sentinel (#dm-older) fires this; bunker decrypts inline, nip07 runs the
 * decrypt chain (terminal lands at #dm-older). Both return [re-armed sentinel + bubbles]. */
export async function getThreadOlder(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const peer = resolvePeer(ctx.params.peer ?? '');
    if (!peer) { notFound(ctx); return; }
    const until = Number(ctx.query.get('until'));
    if (!Number.isFinite(until)) { notFound(ctx); return; }
    if (signsOnClient(s)) { await startSync(ctx, s, 'thread', peer, until); return; }
    const thread = await loadThread(s, peer, until);
    if (!thread) { notFound(ctx); return; }
    await ensureProfiles(s, [peer]);
    sendFragment(ctx, olderFragment(peer, thread.messages, s.me, thread.cursor));
}

// --- nip07 read chain: layer-1 (wrap->seal) -> triage -> layer-2 (seal->rumor) ----

/** Shared start: gate non-batch clients, fetch wraps, emit the layer-1 decrypt batch
 * (or finalize straight from cache when nothing is uncached). */
async function startSync(ctx: Ctx, s: Session07, view: 'inbox' | 'requests' | 'thread', peer?: string, until?: number, warm = false): Promise<void> {
    const place = until != null ? PLACE_OLDER : view === 'thread' ? PLACE_THREAD : PLACE_LIST;
    if (!hasBatchCaps(ctx)) { if (warm) silentPrewarm(ctx); else sendFragment(ctx, dmGate(), place); return; }
    const { chainId, items } = await beginSync(s, view, peer, until, hasCap(ctx, 'nip04'), warm);
    if (items.length === 0) { await continueOrFinalize(ctx, s, chainId, view, warm); return; }
    sendSignRequest(ctx, { items }, `/messages/sync/seals?chain=${chainId}`, 'nip44_decrypt_batch');
}

/** The NIP-17 terminal: if the chain has legacy (kind-4) queued, emit a 3rd nip04 decrypt batch
 * (-> /sync/legacy); otherwise render from the cache. Reached once, after the NIP-17 layers. */
async function continueOrFinalize(ctx: Ctx, s: Session07, chainId: string, view: 'inbox' | 'requests' | 'thread', warm = false): Promise<void> {
    const legacy = legacyBatch(chainId);
    if (legacy) { sendSignRequest(ctx, { items: legacy }, `/messages/sync/legacy?chain=${chainId}`, 'nip04_decrypt_batch'); return; }
    await renderTerminal(ctx, s, finalizeSync(s, chainId), view, warm);
}

/** GET /messages/sync - inbox (or ?view=requests) decrypt start. */
export async function getDmSync(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return; if (!signsOnClient(s)) { notFound(ctx); return; }
    await startSync(ctx, s, ctx.query.get('view') === 'requests' ? 'requests' : 'inbox', undefined, undefined, ctx.query.get('warm') === '1');
}

/** GET /messages/:peer/sync - thread decrypt start. */
export async function getThreadSync(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return; if (!signsOnClient(s)) { notFound(ctx); return; }
    const peer = resolvePeer(ctx.params.peer ?? '');
    if (!peer) { notFound(ctx); return; }
    await startSync(ctx, s, 'thread', peer);
}

/** POST /messages/sync/seals - layer-1 results in; triage and emit layer-2, or finalize. */
export async function postDmSeals(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return; if (!signsOnClient(s)) { notFound(ctx); return; }
    const chainId = ctx.query.get('chain') ?? '';
    const view = chainView(chainId) ?? 'inbox'; // capture before finalize consumes the chain
    const warm = chainWarm(chainId);
    const results = await readBatchResults(ctx.req);
    if (!results) { if (warm) silentPrewarm(ctx); else sendFragment(ctx, html`<div class="notice error">Couldn’t read the decrypted messages.</div>`, errPlace(view)); return; }
    const next = await applySeals(s, chainId, results);
    if (next) { sendSignRequest(ctx, { items: next.items }, `/messages/sync/rumors?chain=${chainId}`, 'nip44_decrypt_batch'); return; }
    await continueOrFinalize(ctx, s, chainId, view, warm);
}

/** POST /messages/sync/rumors - layer-2 results in; cache, then the legacy step or finalize. */
export async function postDmRumors(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return; if (!signsOnClient(s)) { notFound(ctx); return; }
    const chainId = ctx.query.get('chain') ?? '';
    const view = chainView(chainId) ?? 'inbox';
    const warm = chainWarm(chainId);
    const results = await readBatchResults(ctx.req);
    if (!results) { if (warm) silentPrewarm(ctx); else sendFragment(ctx, html`<div class="notice error">Couldn’t read the decrypted messages.</div>`, errPlace(view)); return; }
    applyRumors(s, chainId, results);
    await continueOrFinalize(ctx, s, chainId, view, warm);
}

/** POST /messages/sync/legacy - nip04 results in; cache the decrypted kind-4s, then finalize.
 * The optional 3rd step in the chain (NIP-04 read), reached only when legacy was queued. */
export async function postDmLegacy(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return; if (!signsOnClient(s)) { notFound(ctx); return; }
    const chainId = ctx.query.get('chain') ?? '';
    const view = chainView(chainId) ?? 'inbox';
    const warm = chainWarm(chainId);
    const results = await readBatchResults(ctx.req);
    if (!results) { if (warm) silentPrewarm(ctx); else sendFragment(ctx, html`<div class="notice error">Couldn’t read the decrypted messages.</div>`, errPlace(view)); return; }
    applyLegacy(s, chainId, results);
    await renderTerminal(ctx, s, finalizeSync(s, chainId), view, warm);
}

/** Render a completed sync from the cache, placing it where its trigger lives. A thread
 * scroll-up (`older`) lands at #dm-older (replacing the sentinel with [re-armed sentinel +
 * older bubbles]); the initial thread load fills #dm-messages with the sentinel + bubbles. */
async function renderTerminal(
    ctx: Ctx, s: Session07,
    fin: { view: 'inbox' | 'requests' | 'thread'; peer?: string; inbox?: DmInbox; messages?: DmMessage[]; cursor?: number | null; older?: boolean } | null,
    fallbackView: 'inbox' | 'requests' | 'thread',
    warm = false,
): Promise<void> {
    if (warm) { silentPrewarm(ctx); return; } // background prewarm: cache is filled, render nothing
    const view = fin?.view ?? fallbackView;
    if (!fin) {
        sendFragment(ctx, html`<div class="notice error">That took too long. Reload Messages to retry.</div>`, view === 'thread' ? PLACE_THREAD : PLACE_LIST);
        return;
    }
    if (fin.view === 'thread') {
        await ensureProfiles(s, [fin.peer!]);
        const cursor = fin.cursor ?? null;
        if (fin.older) { sendFragment(ctx, olderFragment(fin.peer!, fin.messages!, s.me, cursor), PLACE_OLDER); return; }
        sendFragment(ctx, threadInner(fin.peer!, fin.messages!, s.me, cursor), PLACE_THREAD);
        return;
    }
    if (fin.view === 'requests') {
        const reqs = fin.inbox!.requests;
        await ensureProfiles(s, reqs.map((c) => c.peer));
        sendFragment(ctx, reqList(reqs, s.profiles), PLACE_LIST);
        return;
    }
    const inbox = fin.inbox!;
    await ensureProfiles(s, [...inbox.conversations, ...inbox.requests].map((c) => c.peer));
    sendFragment(ctx, convList(inbox, s.profiles), PLACE_LIST);
}

// --- send -------------------------------------------------------------------------

/** POST /messages/:peer - send a DM. Bunker signs+wraps inline; nip07 starts the
 * encrypt-batch chain. Both append the sent bubble optimistically. */
export async function postSend(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const peer = resolvePeer(ctx.params.peer ?? '');
    if (!peer) { notFound(ctx); return; }
    const form = await readForm(ctx.req);
    const text = (form.get('text') ?? '').trim();
    if (!text) { notFound(ctx); return; }

    if (signsOnClient(s)) {
        const r = beginSend(s, peer, text);
        if (!r) { notFound(ctx); return; }
        sendSignRequest(ctx, { items: r.items }, `/messages/${ctx.params.peer}/seal?chain=${r.chainId}`, 'nip44_encrypt_batch');
        return;
    }

    const ok = await sendDm(s, peer, text);
    if (!ok) {
        // A no-JS POST must not get a bare <li> as the whole page; reload the thread instead. The 502
        // routes through helmjs's error branch (append the notice, but DON'T fire h-reset) so the typed
        // text is preserved to retry; only a real send (200) clears the compose.
        if (ctx.isPartial) sendFragment(ctx, dmBubble({ id: 'err', from: s.me, at: Math.floor(Date.now() / 1000), text: 'Could not send.' }, 'none'), PLACE_APPEND, 502);
        else redirect(ctx, `/messages/${ctx.params.peer}`);
        return;
    }
    if (ctx.isPartial) sendFragment(ctx, dmBubble({ id: `s${Date.now()}`, from: s.me, at: Math.floor(Date.now() / 1000), text }, s.me));
    else redirect(ctx, `/messages/${ctx.params.peer}`);
}

/** POST /messages/:peer/seal - encrypt results in; build the seal templates, sign-batch. */
export async function postSendSeal(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return; if (!signsOnClient(s)) { notFound(ctx); return; }
    const chainId = ctx.query.get('chain') ?? '';
    const results = await readBatchResults(ctx.req);
    const next = results ? sealStep(s, chainId, results) : null;
    if (!next) { sendFragment(ctx, dmBubble({ id: 'err', from: s.me, at: Math.floor(Date.now() / 1000), text: 'Could not send.' }, 'none'), PLACE_APPEND, 502); return; }
    sendSignRequest(ctx, { templates: next.templates }, `/messages/${ctx.params.peer}/wrap?chain=${chainId}`, 'sign_event_batch');
}

/** POST /messages/:peer/wrap - signed seals in; wrap locally, publish, append the bubble. */
export async function postSendWrap(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return; if (!signsOnClient(s)) { notFound(ctx); return; }
    const chainId = ctx.query.get('chain') ?? '';
    const results = await readBatchResults(ctx.req);
    const msg = results ? await wrapStep(s, chainId, results) : null;
    if (!msg) { sendFragment(ctx, dmBubble({ id: 'err', from: s.me, at: Math.floor(Date.now() / 1000), text: 'Could not send.' }, 'none'), PLACE_APPEND, 502); return; }
    sendFragment(ctx, dmBubble(msg, s.me), PLACE_APPEND);
}
