// Stateful action routes: POST /act/:action/:target toggles a follow/mute/
// bookmark/pin; for nip07 it returns H-Nostr-Sign and POST /act/:action/:target/
// publish is the continuation. Both end by swapping the updated button in place
// (helmjs), or - zero-JS bunker - by reloading the page the form was on.

import { html } from '../html.ts';
import {
    isActionName, isValidTarget, actionKind, ensureList, ensurePrivate, isOn, buildToggle, applyPublished, listKnown,
    isPrivateList, buildPrivateToggle, applyPrivatePublished, resolveTarget, addTag, writeRelays, published, type ActionName,
} from '../actions.ts';
import { actionButton } from '../render/actions.ts';
import { titleCount } from '../render/layout.ts';
import { emptyItem } from '../render/svg.ts';
import { listCount, listEmpty } from './saved.ts'; // single source of truth for a list's count + empty copy
import { readSignResult, requireSigned } from '../nip07.ts';
import { isHex64 } from '../nostr/tags.ts';
import { requireLogin } from './common.ts';
import { redirect, safeReferer, sendFragment, sendSignRequest, notFound, type Ctx } from '../http.ts';
import type { Session } from '../session.ts';
import { signsOnClient, signsOnServer } from '../session.ts';

/** Placement for a private-toggle continuation's button swap (the sign-request set
 * H-Reswap:none, which would otherwise no-op the swap). Keyed by action+target. */
const placeBtn = (action: ActionName, target: string) => ({ 'H-Reswap': 'outer', 'H-Retarget': `#act-${action}-${target}` });

/** Mute-from-a-row carries `?card=<eventId>`: on success we dismiss the whole card instead of
 * swapping the button - an empty body retargeted at `#card-<eventId>` (outer-swap = remove). */
function dismissCard(ctx: Ctx): string | null {
    const c = ctx.query.get('card');
    return c && isHex64(c) ? c : null;
}
const placeDismiss = (eventId: string) => ({ 'H-Reswap': 'outer', 'H-Retarget': `#card-${eventId}` });
/** Emit the card-dismissal (empty body retargeted at the card) when `?card=` is present. Used at the
 * end of a mute. No isPartial gate: the nip07 sign-continuation is the lib's own fetch (no H-Request,
 * so isPartial is false) yet still expects a swap via these headers. Returns true once handled. */
function emitDismiss(ctx: Ctx): boolean {
    const card = dismissCard(ctx);
    if (!card) return false;
    sendFragment(ctx, html``, placeDismiss(card));
    return true;
}
/** Bunker path: dismiss only for a helmjs (partial) request, so a zero-JS form POST falls through
 * to a normal redirect/reload instead of getting an empty body. */
function dismissedCard(ctx: Ctx): boolean {
    return ctx.isPartial && emitDismiss(ctx);
}

/** The dedicated list page for a private-list action - where a toggle-OFF should update the list in
 * place, not just swap the button. Other actions have no such page. */
const LIST_PAGE: Partial<Record<ActionName, string>> = { bookmark: '/bookmarks', mute: '/muted' };

/** True when this toggle-OFF fired from the action's own list page. helmjs sends the current location
 * in H-Current-URL (v0.14+), so a shared button needs no page-specific markup to behave differently here. */
function fromListPage(ctx: Ctx, action: ActionName, on: boolean): boolean {
    const cur = ctx.req.headers['h-current-url'];
    const path = typeof cur === 'string' ? cur.split('?')[0] : '';
    return !on && LIST_PAGE[action] === path;
}

/** Response after a toggle-OFF on a list page: swap the now-inactive button back in - which flips its
 * state class off, so the card/row collapses in place via the page's `:has()` CSS transition - plus an
 * OOB refresh of the header "· N" chip. When it was the last item, swap the empty state into the list
 * instead. Bookmarks and mutes share this exact shape (bookmarks collapse via the #list grid-rows rule,
 * mutes via .mute-row max-height); no per-card ids or DOM removal needed. */
function afterListToggle(ctx: Ctx, s: Session & { me: string }, action: ActionName, target: string): void {
    const kind = actionKind(action);
    // Unbookmarking from the list page removes a RESOLVED (visible) item, so the shown count drops by one.
    // (Mutes count pubkeys, which always resolve, so listCount already reflects the removal for them.)
    if (action === 'bookmark' && typeof s.bookmarkShown === 'number') s.bookmarkShown = Math.max(0, s.bookmarkShown - 1);
    const n = listCount(s, kind);
    const count = titleCount(n, true);
    if (n === 0) { sendFragment(ctx, html`${emptyItem(listEmpty(kind))}${count}`, { 'H-Reswap': 'inner', 'H-Retarget': `#list-${kind}` }); return; }
    sendFragment(ctx, html`${actionButton(s, action, target)}${count}`, placeBtn(action, target));
}

function parse(ctx: Ctx): { action: ActionName; target: string } | null {
    const action = ctx.params.action ?? '';
    const target = ctx.params.target ?? '';
    if (!isActionName(action) || !isValidTarget(action, target)) return null;
    return { action, target };
}

/** The private-chain intent, carried in each continuation URL (STATELESS - no server
 * token store). `on` (the toggle direction) is added once step 1 (decrypt) computes it. */
function parsePrivate(ctx: Ctx): { action: ActionName; target: string; on?: boolean; relist?: boolean } | null {
    const action = ctx.query.get('action') ?? '';
    const target = ctx.query.get('target') ?? '';
    if (!isActionName(action) || !isValidTarget(action, target)) return null;
    const on = ctx.query.get('on');
    return { action, target, on: on == null ? undefined : on === '1', relist: ctx.query.get('relist') === '1' };
}
function privQuery(action: ActionName, target: string, on?: boolean, card?: string | null, relist?: boolean): string {
    const q = new URLSearchParams({ action, target });
    if (on !== undefined) q.set('on', on ? '1' : '0');
    if (card) q.set('card', card);   // ride the card-dismiss intent through the stateless chain
    if (relist) q.set('relist', '1'); // …and the bookmarks-list removal intent (H-Current-URL isn't on chain fetches)
    return q.toString();
}

/** Swap the updated button (helmjs) - or reload the originating page (zero-JS). */
function respond(ctx: Ctx, s: Session & { me: string }, action: ActionName, target: string): void {
    if (ctx.isPartial) sendFragment(ctx, actionButton(s, action, target));
    else redirect(ctx, safeReferer(ctx));
}

export async function postAction(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const p = parse(ctx);
    if (!p) { notFound(ctx); return; }
    const { action, target } = p;

    const prev = await ensureList(s, actionKind(action), { retry: true }); // bypass the unknown sentinel: a toggle deserves a fresh read
    // Read-before-write guard: a truncated-empty read leaves the list UNKNOWN (ensureList
    // caches nothing). Building a toggle from "nothing" would publish a one-entry list over
    // the real one, so refuse and let the retry re-read. Covers all three write paths below.
    if (!listKnown(s, actionKind(action))) {
        sendFragment(ctx, html`<div class="notice error">Couldn't ${action}: your current list didn't load from your relays, so nothing was changed. Try again.</div>`, {}, 502);
        return;
    }
    if (isPrivateList(action)) await ensurePrivate(s, actionKind(action)); // so isOn sees private state
    const on = !isOn(s, action, target); // desired next state

    // Private (bunker) write: mute/bookmark go NIP-44-encrypted to self, like Satori.
    // (nip07 falls through to the public path for now - its key isn't on this server.)
    if (isPrivateList(action) && signsOnServer(s)) {
        try {
            const template = await buildPrivateToggle(s, action, target, on);
            const signed = await s.signer!.signEvent(template);
            await s.pool.publish(writeRelays(s), signed);
            applyPrivatePublished(s, signed, action, target, on);
        } catch (err) {
            const msg = err instanceof Error ? err.message : String(err);
            sendFragment(ctx, html`<div class="notice error">Couldn't ${action}: ${msg}</div>`, {}, 502);
            return;
        }
        if (fromListPage(ctx, action, on)) { afterListToggle(ctx, s, action, target); return; } // unbookmark/unmute on its list page → drop the item
        if (dismissedCard(ctx)) return; // mute-from-a-row: drop the card instead of swapping the button
        respond(ctx, s, action, target);
        return;
    }

    // Private (nip07) write: a 3-step extension chain - decrypt current private →
    // modify → encrypt → sign. STATELESS: the {action,target,on} intent rides in each
    // continuation URL (no server token store). When there's no private content yet,
    // skip the decrypt - `on` is already known.
    if (isPrivateList(action) && signsOnClient(s)) {
        const kind = actionKind(action);
        const content = s.lists.get(kind)?.content;
        const card = dismissCard(ctx);
        const relist = fromListPage(ctx, action, on); // detect the page NOW (chain fetches lack H-Current-URL)
        if (content) {
            sendSignRequest(ctx, { pubkey: s.me, ciphertext: content }, `/act/private/dec?${privQuery(action, target, undefined, card, relist)}`, 'nip44_decrypt');
        } else {
            s.privateTags.set(kind, []); // nothing private yet → on is known, encrypt directly
            sendSignRequest(ctx, { pubkey: s.me, plaintext: JSON.stringify(on ? [addTag(action, target)] : []) }, `/act/private/enc?${privQuery(action, target, on, card, relist)}`, 'nip44_encrypt');
        }
        return;
    }

    // Public path: follow/pin always.
    const template = buildToggle(action, prev, target, on, s.me);
    if (signsOnClient(s)) { sendSignRequest(ctx, template, `/act/${action}/${target}/publish`); return; }

    // bunker: sign + publish here.
    try {
        const signed = await s.signer!.signEvent(template);
        if (!await published(s, signed)) throw new Error('no relay accepted it');
        applyPublished(s, signed);
        if (signed.kind === 3) { s.followsRoute = null; s.followersRoute = null; } // follow set changed → rebuild feed + DM routing
    } catch (err) {
        const msg = err instanceof Error ? err.message : String(err);
        sendFragment(ctx, html`<div class="notice error">Couldn't ${action}: ${msg}</div>`, {}, 502);
        return;
    }
    respond(ctx, s, action, target);
}

/** nip07 continuation: verify the extension-signed list event, publish, swap. */
export async function postActionPublish(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const p = parse(ctx);
    if (!p) { notFound(ctx); return; }
    const { action, target } = p;

    const signed = await requireSigned(ctx, s.me, actionKind(action), `the signed ${action}`);
    if (!signed) return;
    if (!await published(s, signed)) {
        sendFragment(ctx, html`<div class="notice error">Couldn't ${action}: no relay accepted it.</div>`, {}, 502);
        return;
    }
    applyPublished(s, signed);
    if (signed.kind === 3) { s.followsRoute = null; s.followersRoute = null; } // follow set changed → rebuild feed + DM routing
    // The continuation is the lib's own fetch (no H-Request); the lib swaps it into
    // the originating form via the seam. Re-assert placement: the sign-request set
    // H-Reswap:none (don't swap the JSON template), which mutates the request's swap
    // to "none" - without these headers the toggled button never swaps in (only a
    // reload would). Mirrors the note-publish LAND_ON_FEED headers.
    sendFragment(ctx, actionButton(s, action, target), placeBtn(action, target));
}

// --- nip07 private-toggle chain (decrypt → encrypt → sign) ------------------

/** Step 1 result: the decrypted current private tags. Cache them, compute the
 * toggle direction, then ask the extension to encrypt the new private set. */
export async function postActPrivateDec(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const p = parsePrivate(ctx);
    if (!p) { sendFragment(ctx, html`<div class="notice error">Couldn’t complete that action. Try again.</div>`, {}, 400); return; }
    let priv: string[][];
    try { const j = JSON.parse(String(await readSignResult(ctx.req))); if (!Array.isArray(j)) throw new Error(); priv = j as string[][]; }
    catch { sendFragment(ctx, html`<div class="notice error">Couldn’t read your private list, so it wasn’t changed.</div>`, {}, 400); return; }
    const kind = actionKind(p.action);
    s.privateTags.set(kind, priv);
    const on = !isOn(s, p.action, p.target);
    const { tag, value } = resolveTarget(p.action, p.target);
    const next = priv.filter((t) => !(t[0] === tag && t[1] === value));
    if (on) next.push(addTag(p.action, p.target));
    sendSignRequest(ctx, { pubkey: s.me, plaintext: JSON.stringify(next) }, `/act/private/enc?${privQuery(p.action, p.target, on, dismissCard(ctx), p.relist)}`, 'nip44_encrypt');
}

/** Step 2 result: the re-encrypted private content. Build the list event (public
 * tags minus the target, new encrypted content) and ask the extension to sign it. */
export async function postActPrivateEnc(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const p = parsePrivate(ctx);
    if (!p) { sendFragment(ctx, html`<div class="notice error">Couldn’t complete that action. Try again.</div>`, {}, 400); return; }
    const ciphertext = await readSignResult(ctx.req);
    if (typeof ciphertext !== 'string') { sendFragment(ctx, html`<div class="notice error">Encryption failed.</div>`, {}, 400); return; }
    const kind = actionKind(p.action);
    const { tag, value } = resolveTarget(p.action, p.target);
    const publicTags = (s.lists.get(kind)?.tags ?? []).filter((t) => !(t[0] === tag && t[1] === value));
    sendSignRequest(ctx, { kind, created_at: Math.floor(Date.now() / 1000), tags: publicTags, content: ciphertext, pubkey: s.me }, `/act/private/sign?${privQuery(p.action, p.target, p.on, dismissCard(ctx), p.relist)}`, 'sign_event');
}

/** Step 3 result: the signed list event. Publish it, update caches, swap the button. */
export async function postActPrivateSign(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const p = parsePrivate(ctx);
    if (!p) { sendFragment(ctx, html`<div class="notice error">Couldn’t complete that action. Try again.</div>`, {}, 400); return; }
    const signed = await requireSigned(ctx, s.me, actionKind(p.action), `the signed ${p.action}`);
    if (!signed) return;
    await s.pool.publish(writeRelays(s), signed).catch(() => {});
    applyPrivatePublished(s, signed, p.action, p.target, p.on ?? true);
    if (p.relist) { afterListToggle(ctx, s, p.action, p.target); return; } // unbookmark/unmute on its list page → drop the item + refresh the count
    if (emitDismiss(ctx)) return; // mute-from-a-row (nip07): drop the card instead of swapping the button
    sendFragment(ctx, actionButton(s, p.action, p.target), placeBtn(p.action, p.target));
}
