// Publish a long-form article (NIP-23, kind:30023). Mirrors postNote: bunker
// signs + publishes here (303 → the article reader); nip07 signs via the capture
// signer so the SAME signArticle builds the tags, the extension signs it, and
// POST /article/publish verifies + publishes + lands on the reader.

import { randomBytes } from 'node:crypto';
import { naddrEncode } from 'nostr-tools/nip19';
import { signArticle, publishSigned, captureSigner, type ArticleFields } from '../data/publish.ts';
import { KIND_ARTICLE } from '../nostr/nip23.ts';
import { articleReader } from '../render/note.ts';
import { articleComposePage, draftsView, draftsScreen, draftsSyncShell, type ArticleComposeCtx } from '../render/article-compose.ts';
import { chosenTargets, appendRelayTargets, parseScheduleAt } from '../actions.ts';
import { titleCount } from '../render/layout.ts';
import { saveDraft, listDrafts, getDraft, deleteDraft, type ArticleDraft, type Draft } from '../drafts.ts';
import { holdScheduled, SCHEDULE_FULL_MSG, listScheduled } from '../data/scheduled.ts';
import { syncDraft, unsyncDraft, fetchSyncedDrafts, draftToEvent, publishDraftWrap, fetchDraftWraps, draftFromDecrypted, applyDecryptedDraftRelays } from '../data/draft-sync.ts';
import { serializeDraft, draftWrapTemplate, KIND_DRAFT } from '../nostr/nip37.ts';
import { page } from '../render/layout.ts';
import { html, type SafeHtml } from '../html.ts';
import { readSignedEvent, readSignResult, verifySigned } from '../nip07.ts';
import { requireLogin, ensureProfiles, chromeFor } from './common.ts';
import { readForm, redirect, sendPage, sendFragment, sendSignRequest, readBatchResults, type Ctx } from '../http.ts';
import type { Session } from '../session.ts';
import { signsOnClient } from '../session.ts';
import type { NostrEvent } from '../nostr/types.ts';

function readFields(form: URLSearchParams): ArticleFields {
    return {
        identifier: (form.get('identifier') ?? '').trim() || randomBytes(8).toString('hex'),
        title: (form.get('title') ?? '').trim(),
        summary: (form.get('summary') ?? '').trim() || undefined,
        image: (form.get('image') ?? '').trim() || undefined,
        topics: (form.get('topics') ?? '').split(',').map((t) => t.trim()).filter(Boolean),
        body: (form.get('body') ?? '').trim(),
    };
}

const naddrOf = (s: Session & { me: string }, identifier: string): string => {
    try { return naddrEncode({ identifier, pubkey: s.me, kind: KIND_ARTICLE, relays: (s.myRelays?.write ?? []).slice(0, 2) }); }
    catch { return ''; }
};

export async function postArticle(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const form = await readForm(ctx.req);
    const f = readFields(form);

    const back = (error: string): void => {
        const c: ArticleComposeCtx = {
            identifier: form.get('identifier') || undefined, title: form.get('title') ?? '', summary: form.get('summary') ?? '',
            image: form.get('image') ?? '', topics: form.get('topics') ?? '', body: form.get('body') ?? '', error,
        };
        sendPage(ctx, articleComposePage(c), chromeFor(ctx, s, { active: 'compose', title: 'Article' }), 400);
    };
    if (!f.title) { back('Add a title.'); return; }
    if (!f.body) { back('Write something first.'); return; }

    // Schedule: sign now with created_at = the chosen time, hold on disk, the sweep broadcasts it then.
    // The editable draft is KEPT (a scheduled article can only be cancelled, not edited; cancelling reverts
    // to the draft), so we skip the draft-wrap retire dance here.
    let scheduledAt = 0;
    if (form.get('do') === 'schedule') {
        const r = parseScheduleAt(form.get('schedule') ?? '');
        if ('error' in r) { back('Pick a time in the future to schedule.'); return; }
        scheduledAt = r.at;
    }
    const fields: ArticleFields = scheduledAt ? { ...f, createdAt: scheduledAt } : f;

    // nip07: build the exact 30023 template; the extension signs it. If this draft was synced,
    // batch-sign the article + a BLANK wrap (one prompt) so publishing also retires the draft wrap
    // (else it would resurrect on the next /drafts load). Scheduling keeps the draft, so it skips that.
    // nip07 forwards the relay-picker selection on the publish-continuation URL (bunker reads the form
    // directly below); the continuation re-validates it centrally via chosenTargets.
    if (signsOnClient(s)) {
        const prepared = await signArticle(captureSigner, s.me, s.myRelays!, fields);
        const q = new URLSearchParams({ d: f.identifier });
        appendRelayTargets(q, form);
        if (scheduledAt) {
            q.set('schedule', String(scheduledAt));
            sendSignRequest(ctx, prepared.signed, `/article/publish?${q}`);
            return;
        }
        const draft = getDraft(s.me, f.identifier);
        if (draft?.synced) {
            const blank = draftWrapTemplate(s.me, f.identifier, draftToEvent(draft, s.me).kind, '');
            q.set('retire', '1');
            sendSignRequest(ctx, { templates: [prepared.signed, blank] }, `/article/publish?${q}`, 'sign_event_batch');
        } else {
            sendSignRequest(ctx, prepared.signed, `/article/publish?${q}`);
        }
        return;
    }

    // bunker: sign + publish (or hold for the sweep) here, then land on the reader (or /drafts).
    try {
        const prepared = await signArticle(s.signer!, s.me, s.myRelays!, fields);
        prepared.writeTargets = chosenTargets(form, s); // relay-picker selection
        if (scheduledAt) {
            if (!holdScheduled(s.me, prepared.signed, scheduledAt, prepared.writeTargets)) { back(SCHEDULE_FULL_MSG); return; }
            redirect(ctx, '/drafts');
            return;
        }
        await publishSigned(s.pool, prepared);
    } catch (err) {
        back(`Couldn't publish: ${err instanceof Error ? err.message : String(err)}`);
        return;
    }
    await clearDraftAndWrap(s, f.identifier); // published → clear its draft (+ retire the wrap)
    redirect(ctx, `/a/${naddrOf(s, f.identifier)}`);
}

/** POST /draft - save (or update) the article composer's draft, re-render the
 * composer with the id wired in (so the next save updates the same draft) + a
 * "saved" status. The Save-draft button posts here via `formaction`. */
export async function postDraft(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const form = await readForm(ctx.req);
    const identifier = (form.get('identifier') ?? '').trim() || randomBytes(8).toString('hex');
    const draft: ArticleDraft = {
        type: 'article', id: identifier, identifier,
        title: (form.get('title') ?? '').trim(),
        summary: (form.get('summary') ?? '').trim(),
        image: (form.get('image') ?? '').trim(),
        topics: (form.get('topics') ?? '').trim(),
        body: form.get('body') ?? '',
        savedAt: Date.now(),
        synced: getDraft(s.me, identifier)?.synced,
        syncedAt: getDraft(s.me, identifier)?.syncedAt,
    };
    saveDraft(s.me, draft);
    // bunker syncs inline + renders "saved"; nip07 runs the sync chain in this click's gesture, then OOB-
    // updates the composer status (the page stays put meanwhile). See saveDraftAndSync / composeSaved.
    await saveDraftAndSync(ctx, s, draft, (synced) =>
        sendPage(ctx, articleComposePage({ ...draft, status: savedStatus(synced) }), chromeFor(ctx, s, { active: 'compose', title: 'Article' })));
}

// --- drafts list + per-draft NIP-37 sync (bunker = inline; nip07 = encrypt/sign/decrypt chains) ---
type Signed = Session & { me: string; signer: NonNullable<Session['signer']> };
const canSync = (s: Session & { me: string }): s is Signed => s.mode === 'bunker' && !!s.signer;
const PLACE_LIST = { 'H-Reswap': 'outer', 'H-Retarget': '#drafts-view' };

/** #drafts-view re-render + an OOB refresh of the bar's "Drafts · N" count chip (the count lives in the
 * chrome, not the list). Used by the sync re-renders and the last-draft delete. */
function draftsListWithCount(me: string): SafeHtml {
    const drafts = listDrafts(me);
    return html`${draftsView(drafts, listScheduled(me).length > 0)}${titleCount(drafts.length, true)}`;
}

/** Post-delete swap. The row already collapsed in place (the delete button's h-optimistic added
 * `.deleting`; the #drafts-view CSS animates it out and it lingers at 0 height, like the mutes page).
 * So we DON'T swap the row (that would cut the transition) - just OOB-refresh the count. When it was
 * the LAST draft, re-render the whole view so the empty state shows instead of a blank list. */
function sendAfterDelete(ctx: Ctx, me: string): void {
    const drafts = listDrafts(me);
    if (!drafts.length) { sendFragment(ctx, draftsListWithCount(me), PLACE_LIST); return; }
    sendFragment(ctx, titleCount(drafts.length, true), { 'H-Reswap': 'none' });
}
/** Merge fetched synced drafts into the local store (newest wins; mark local copies synced). */
function mergeSynced(me: string, synced: Draft[]): void {
    const local = new Map(listDrafts(me).map((d) => [d.id, d]));
    for (const r of synced) {
        const cur = local.get(r.id);
        if (!cur) saveDraft(me, r);                                  // new from another device
        else if (r.savedAt > cur.savedAt) saveDraft(me, r);         // relay copy is newer
        else if (!cur.synced) saveDraft(me, { ...cur, synced: true }); // mark the local one synced
    }
}

/** The composer draft-saved status. Shared by the bunker render path (via renderSaved) and the nip07
 * chain's terminal OOB (composeSaved), so both signing families land on the SAME honest wording:
 * "✓" when the relay wrap published, "· not synced" when only the local disk save succeeded. */
export const savedStatus = (synced: boolean): string => (synced ? 'Draft saved ✓' : 'Draft saved · not synced');

/** Save-then-sync for the composer, driven by the Save-draft click. BUNKER syncs the draft inline
 * (server-side, silent) then runs `renderSaved(synced)` (the caller's normal saved-composer render, with
 * the honest synced/not-synced status). NIP07 runs the encrypt/sign/publish chain FROM this request - the
 * click is a user gesture, so browser signing works (a background trigger can't sign, which was the
 * "syncing…" hang) - and does NOT render now: the composer stays put (the sign response is H-Reswap:none)
 * and the chain's final step (composeSaved) OOB-updates the status + draftid. The draft is assumed already
 * persisted by the caller (saveDraft). */
export async function saveDraftAndSync(ctx: Ctx, s: Session & { me: string }, d: Draft, renderSaved: (synced: boolean) => void): Promise<void> {
    if (canSync(s)) { // bunker: inline, silent
        let synced = false;
        try { synced = await syncDraft(s, d); } catch { /* best-effort */ }
        if (synced) saveDraft(s.me, { ...d, synced: true, syncedAt: d.savedAt });
        renderSaved(synced); // same honest status the nip07 chain lands on (composeSaved)
        return;
    }
    const inner = draftToEvent(d, s.me); // nip07: the chain runs in THIS request's gesture
    sendSignRequest(ctx, { pubkey: s.me, plaintext: serializeDraft(inner) }, `/draft/sync/${encodeURIComponent(d.id)}/wrap?kind=${inner.kind}`, 'nip44_encrypt');
}

/** The nip07 chain's terminal step for a compose save+sync: OOB-update the open composer (never re-rendered
 * during the H-Reswap:none sign flow) - the status ("Draft saved ✓", or "· not synced" if the relay publish
 * failed) + the id fields (draftid for note/poll, identifier for article), so a re-save updates THIS draft
 * rather than creating a duplicate. Whichever id field the current composer has gets swapped; the other is a
 * harmless no-op (no matching element). */
function composeSaved(ctx: Ctx, id: string, synced: boolean): void {
    sendFragment(ctx, html`<span id="compose-status" class="compose-status" h-oob="true">${savedStatus(synced)}</span><input type="hidden" id="compose-draftid" name="draftid" value="${id}" h-oob="true"><input type="hidden" id="compose-identifier" name="identifier" value="${id}" h-oob="true">`, { 'H-Reswap': 'none' });
}

/** GET /drafts - your saved drafts. Bunker merges synced inline; nip07 renders local now + a
 * decrypt-on-load shell that pulls your synced wraps in. */
export async function getDrafts(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    if (canSync(s)) {
        mergeSynced(s.me, await fetchSyncedDrafts(s).catch(() => [])); // merge BEFORE the count so it includes synced drafts
        const drafts = listDrafts(s.me);
        sendPage(ctx, draftsScreen(listScheduled(s.me), drafts), chromeFor(ctx, s, { active: 'drafts', title: 'Drafts', titleCount: drafts.length }));
        return;
    }
    // nip07: local count now; the decrypt-load re-render (postDraftsSyncApply) OOB-refreshes it once synced drafts merge.
    const drafts = listDrafts(s.me);
    sendPage(ctx, html`${draftsScreen(listScheduled(s.me), drafts)}${draftsSyncShell()}`, chromeFor(ctx, s, { active: 'drafts', title: 'Drafts', titleCount: drafts.length }));
}

/** nip07 sync chain, step 1 continuation (started by saveDraftAndSync): encrypted content in -> sign the
 * wrap. On a failed encrypt the draft is still saved locally; report that ("· not synced"). */
export async function postDraftSyncWrap(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const id = ctx.params.id ?? '';
    const kind = Number(ctx.query.get('kind')) || 1;
    const ciphertext = await readSignResult(ctx.req);
    const cur = getDraft(s.me, id);
    if (typeof ciphertext !== 'string' || !cur) { composeSaved(ctx, id, false); return; }
    sendSignRequest(ctx, draftWrapTemplate(s.me, id, kind, ciphertext), `/draft/sync/${encodeURIComponent(id)}/publish`, 'sign_event');
}

/** nip07 sync chain, final step: signed wrap in -> publish it, mark synced, OOB-update the composer. */
export async function postDraftSyncPublish(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const id = ctx.params.id ?? '';
    const signed = await readSignedEvent(ctx.req);
    // Assert the draft-wrap kind (KIND_DRAFT/31234), not just the pubkey: without it the signer could
    // return any kind for us to publish.
    let ok = false;
    if (signed && signed.pubkey === s.me && signed.kind === KIND_DRAFT) {
        ok = await publishDraftWrap(s, signed).catch(() => false);
        const cur = getDraft(s.me, id);
        if (ok && cur) saveDraft(s.me, { ...cur, synced: true, syncedAt: cur.savedAt });
    }
    composeSaved(ctx, id, ok);
}

// nip07 decrypt-on-load chain state: chainId -> the wrap identifiers (order-matched to the
// decrypt batch we send) + whether item 0 is the 10013 draft-relay ciphertext, TTL'd so an
// abandoned chain self-evicts.
const draftChains = new Map<string, { ids: string[]; relayItem: boolean; at: number }>();
const CHAIN_TTL = 2 * 60_000;
function newChain(ids: string[], relayItem: boolean): string {
    const now = Date.now();
    for (const [k, v] of draftChains) if (now - v.at > CHAIN_TTL) draftChains.delete(k);
    const chainId = randomBytes(9).toString('base64url');
    draftChains.set(chainId, { ids, relayItem, at: now });
    return chainId;
}

/** nip07 GET /drafts/sync - fetch your wraps, batch-decrypt their contents via the extension.
 * A pending 10013 draft-relay ciphertext (NIP-37 keeps that list encrypted; the server can't
 * decrypt for nip07) rides the same batch as item 0, so this load also resolves WHERE future
 * saves/unsyncs publish. */
export async function getDraftsSync(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    if (!signsOnClient(s)) { sendFragment(ctx, draftsListWithCount(s.me), PLACE_LIST); return; }
    const wraps = await fetchDraftWraps(s).catch(() => []);
    const relayCipher = s.draftRelaysCipher; // set by draftRelays() during the fetch above, if any
    if (!wraps.length && !relayCipher) { sendFragment(ctx, draftsListWithCount(s.me), PLACE_LIST); return; }
    const chainId = newChain(wraps.map((w) => w.tags.find((t) => t[0] === 'd')?.[1] ?? ''), !!relayCipher);
    const items = [
        ...(relayCipher ? [{ pubkey: s.me, ciphertext: relayCipher }] : []),
        ...wraps.map((w) => ({ pubkey: s.me, ciphertext: w.content })),
    ];
    sendSignRequest(ctx, { items }, `/drafts/sync/apply?chain=${chainId}`, 'nip44_decrypt_batch');
}

/** nip07 continuation: decrypted contents in -> merge into local, re-render the list. */
export async function postDraftsSyncApply(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const key = ctx.query.get('chain') ?? '';
    const chain = draftChains.get(key);
    draftChains.delete(key);
    let results = await readBatchResults(ctx.req);
    if (chain && results) {
        if (chain.relayItem) {
            const r = results[0];
            if (r?.ok && typeof r.value === 'string') applyDecryptedDraftRelays(s, r.value);
            results = results.slice(1); // the rest line up with chain.ids
        }
        const synced: Draft[] = [];
        results.forEach((r, i) => {
            const id = chain.ids[i];
            if (r.ok && id && typeof r.value === 'string') {
                const d = draftFromDecrypted(r.value, id);
                if (d) synced.push(d);
            }
        });
        mergeSynced(s.me, synced);
    }
    sendFragment(ctx, draftsListWithCount(s.me), PLACE_LIST);
}

/** Clear a draft after publish (or explicit delete): drop the local copy, and if it was synced,
 * retire its relay wrap. Bunker blanks the wrap inline; nip07 wrap-retirement is deferred (no
 * server-side key to sign the blank), so a nip07 wrap lingers until a manual un-sync. */
async function clearDraftAndWrap(s: Session & { me: string }, id: string): Promise<void> {
    const d = getDraft(s.me, id);
    deleteDraft(s.me, id);
    if (d?.synced && canSync(s)) { try { await unsyncDraft(s, id, draftToEvent(d, s.me).kind); } catch { /* best-effort */ } }
}

export async function postDraftDelete(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const id = ctx.params.id ?? '';
    const d = getDraft(s.me, id);
    // nip07 + synced: must retire the relay wrap (blank it) or it resurrects on the next /drafts
    // load. Can't sign server-side, so chain a sign-blank, then delete in the continuation.
    if (d?.synced && !canSync(s) && ctx.isPartial) {
        const kind = draftToEvent(d, s.me).kind;
        sendSignRequest(ctx, draftWrapTemplate(s.me, id, kind, ''), `/draft/delete/${encodeURIComponent(id)}/finish`, 'sign_event');
        return;
    }
    await clearDraftAndWrap(s, id); // bunker (inline blank) / unsynced / no-JS best-effort
    if (ctx.isPartial) sendAfterDelete(ctx, s.me);
    else redirect(ctx, '/drafts');
}

/** nip07 continuation: signed blank wrap in -> publish it (retire the relay copy), drop local. */
export async function postDraftDeleteFinish(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const id = ctx.params.id ?? '';
    const signed = await readSignedEvent(ctx.req);
    // Assert the draft-wrap kind (KIND_DRAFT/31234), not just the pubkey, before publishing the blank.
    if (signed && signed.pubkey === s.me && signed.kind === KIND_DRAFT) await publishDraftWrap(s, signed).catch(() => false);
    deleteDraft(s.me, id);
    sendAfterDelete(ctx, s.me); // collapse handled optimistically; refresh count (or re-render empty view if last)
}

/** nip07 continuation for a SYNCED article: the batch [signed article, signed blank wrap] comes
 * back; publish the article, retire the draft wrap, drop the local draft, land on the reader. */
async function articlePublishRetire(ctx: Ctx, s: Session & { me: string }): Promise<void> {
    const results = await readBatchResults(ctx.req);
    const article = results && results[0]?.ok ? verifySigned(results[0].value) : null;
    const landError = (error: string, status: number): void => {
        const c: ArticleComposeCtx = { ...fieldsFromSigned(article), error };
        sendFragment(ctx, page(articleComposePage(c), chromeFor(ctx, s, { active: 'compose', title: 'Article' })),
            { 'H-Retarget': 'body', 'H-Reselect': 'body', 'H-Reswap': 'inner' }, status);
    };
    if (!article || article.pubkey !== s.me || article.kind !== KIND_ARTICLE) { landError("Couldn't verify the signed article.", 400); return; }
    try {
        await publishSigned(s.pool, { signed: article, isReply: false, writeTargets: chosenTargets(ctx.query, s), inboxTargets: [] });
    } catch (err) { landError(`Couldn't publish: ${err instanceof Error ? err.message : String(err)}`, 502); return; }
    const wrap = results && results[1]?.ok ? verifySigned(results[1].value) : null; // the blank wrap (retire the draft)
    if (wrap && wrap.pubkey === s.me) await publishDraftWrap(s, wrap).catch(() => false);
    const d = article.tags.find((t) => t[0] === 'd')?.[1] ?? '';
    deleteDraft(s.me, d);
    await ensureProfiles(s, [s.me]);
    const naddr = naddrOf(s, d);
    sendFragment(ctx, page(articleReader(article, s.profiles, s), chromeFor(ctx, s, { title: 'Article' })),
        { 'H-Push-Url': `/a/${naddr}`, 'H-Retarget': 'body', 'H-Reselect': 'body', 'H-Reswap': 'inner' });
}

/** Reconstruct the composer fields from a signed kind-30023 (the nip07 continuation has no
 * form to read back), so a verify/publish error can re-render the composer without data loss. */
function fieldsFromSigned(ev: NostrEvent | null): ArticleComposeCtx {
    if (!ev || !Array.isArray(ev.tags)) return {};
    const tag = (k: string): string => ev.tags.find((t) => t[0] === k)?.[1] ?? '';
    return {
        identifier: tag('d') || undefined,
        title: tag('title'),
        summary: tag('summary'),
        image: tag('image'),
        topics: ev.tags.filter((t) => t[0] === 't' && t[1]).map((t) => t[1]).join(', '),
        body: ev.content ?? '',
    };
}

/** nip07 continuation for a SCHEDULED article: verify the extension-signed 30023, hold it for the sweep,
 * and land on /drafts. The editable draft is kept (a scheduled post reverts to it on cancel). */
async function articleSchedule(ctx: Ctx, s: Session & { me: string }, schedule: number): Promise<void> {
    const signed = await readSignedEvent(ctx.req);
    const fail = (error: string, status: number): void =>
        sendFragment(ctx, page(articleComposePage({ ...fieldsFromSigned(signed as NostrEvent | null), error }), chromeFor(ctx, s, { active: 'compose', title: 'Article' })),
            { 'H-Retarget': 'body', 'H-Reselect': 'body', 'H-Reswap': 'inner' }, status);
    if (!signed || signed.pubkey !== s.me || signed.kind !== KIND_ARTICLE) { fail("Couldn't verify the signed article.", 400); return; }
    if (!holdScheduled(s.me, signed as NostrEvent, schedule, chosenTargets(ctx.query, s))) { fail(SCHEDULE_FULL_MSG, 400); return; }
    sendFragment(ctx, page(draftsScreen(listScheduled(s.me), listDrafts(s.me)), chromeFor(ctx, s, { active: 'drafts', title: 'Drafts' })),
        { 'H-Push-Url': '/drafts', 'H-Retarget': 'body', 'H-Reselect': 'body', 'H-Reswap': 'inner' });
}

/** nip07 continuation: verify the extension-signed article, publish, land on it. */
export async function postArticlePublish(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const schedule = Number(ctx.query.get('schedule')) || 0;
    if (schedule > Math.floor(Date.now() / 1000)) { await articleSchedule(ctx, s, schedule); return; } // hold for the sweep
    if (ctx.query.get('retire')) { await articlePublishRetire(ctx, s); return; } // batched [article, blank wrap]
    const signed = await readSignedEvent(ctx.req);
    // The sign request set H-Reswap:none, so an error fragment with no retarget would nest the
    // whole composer inside the inline error box. Swap the body and re-render the composer with
    // the submitted fields (recovered from the signed event) so a failed publish loses nothing.
    const composerError = (error: string, status: number): void => {
        const c: ArticleComposeCtx = { ...fieldsFromSigned(signed), error };
        sendFragment(ctx, page(articleComposePage(c), chromeFor(ctx, s, { active: 'compose', title: 'Article' })),
            { 'H-Retarget': 'body', 'H-Reselect': 'body', 'H-Reswap': 'inner' }, status);
    };
    if (!signed || signed.pubkey !== s.me || signed.kind !== KIND_ARTICLE) {
        composerError("Couldn't verify the signed article.", 400);
        return;
    }
    const writeTargets = chosenTargets(ctx.query, s); // relay-picker selection (forwarded on the URL)
    try {
        await publishSigned(s.pool, { signed: signed as NostrEvent, isReply: false, writeTargets, inboxTargets: [] });
    } catch (err) {
        composerError(`Couldn't publish: ${err instanceof Error ? err.message : String(err)}`, 502);
        return;
    }
    await ensureProfiles(s, [s.me]);
    const d = signed.tags.find((t) => t[0] === 'd')?.[1] ?? '';
    await clearDraftAndWrap(s, d); // published → clear its draft (+ retire the wrap; nip07 lingers)
    const naddr = naddrOf(s, d);
    // Land on the new article (body swap + push the reader URL).
    sendFragment(ctx, page(articleReader(signed, s.profiles, s), chromeFor(ctx, s, { title: 'Article' })),
        { 'H-Push-Url': `/a/${naddr}`, 'H-Retarget': 'body', 'H-Reselect': 'body', 'H-Reswap': 'inner' });
}
