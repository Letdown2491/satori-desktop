// NIP-09 event deletion. POST /delete/:id swaps the trash glyph for an inline confirm
// (nothing is signed yet); POST /delete/:id/confirm builds the kind:5 - bunker signs and
// publishes here, nip07 gets H-Nostr-Sign with POST /delete/:id/publish as the continuation;
// POST /delete/:id/cancel restores the glyph. Only your own events: the glyph only renders on
// them, ownership is re-checked against the event cache when possible, and the kind:5 is
// signed by YOUR key regardless - relays (and our tombstone lookups) both pubkey-match, so a
// forged target could never delete someone else's event anyway.

import { html, type SafeHtml } from '../html.ts';
import { icon } from '../render/svg.ts';
import { deleteActRaw } from '../render/actions.ts';
import { published } from '../actions.ts';
import { cachedEvent } from '../data/feeds.ts';
import { requireSigned } from '../nip07.ts';
import { requireLogin } from './common.ts';
import { readForm, redirect, safeReferer, sendFragment, sendPage, sendSignRequest, notFound, type Ctx } from '../http.ts';
import { chromeFor } from './common.ts';
import { HEX64, coordParts, nowSec, tag1 } from '../nostr/tags.ts';
import type { UnsignedEvent } from '../nostr/types.ts';
import { signsOnClient } from '../session.ts';

interface DelTarget { id: string; k: number; coord: string; back: string }

/** Validate the delete target from the form/query: a hex event id, an integer kind, and - for
 * an addressable kind - a coordinate that PARSES, matches the kind, and names YOUR pubkey (an
 * `a` tag for someone else's address is a cross-author deletion attempt; reject the request).
 * `back` is the page to return to on the zero-JS path, captured at arm time and carried through
 * the chain; validated to a same-origin relative path so it can't become an open redirect. */
function parseDelete(ctx: Ctx, me: string, p: { get(n: string): string | null }): DelTarget | null {
    const id = (ctx.params.id ?? '').toLowerCase();
    if (!HEX64.test(id)) return null;
    const k = Number(p.get('k'));
    if (!Number.isInteger(k) || k < 0 || k > 65535) return null;
    const coord = (p.get('a') ?? '').trim();
    if (coord) {
        const c = coordParts(coord);
        if (!c || c.kind !== k || c.pubkey !== me) return null;
    }
    const rawBack = (p.get('back') ?? '').trim();
    const back = /^\/(?!\/)/.test(rawBack) ? rawBack : '';
    // Ownership re-check when the event is at hand (the UI only renders the glyph on own posts,
    // but the POST is forgeable): a cached copy that isn't yours kills the request. An uncached
    // id proceeds - the kind:5 is signed by you, so it can't touch anyone else's event.
    const cached = cachedEvent(id);
    if (cached && cached.pubkey !== me) return null;
    return { id, k, coord, back };
}

/** The kind:5 deletion request: `e` = the event, `a` beside it for an addressable kind
 * (NIP-09 recommends both), `k` = the deleted event's kind. */
function deleteTemplate(me: string, t: DelTarget): UnsignedEvent {
    const tags = [['e', t.id], ...(t.coord ? [['a', t.coord]] : []), ['k', String(t.k)]];
    return { kind: 5, created_at: nowSec(), pubkey: me, content: '', tags };
}

const hiddenFields = (t: DelTarget): SafeHtml =>
    html`<input type="hidden" name="k" value="${String(t.k)}">${t.coord ? html`<input type="hidden" name="a" value="${t.coord}">` : null}${t.back ? html`<input type="hidden" name="back" value="${t.back}">` : null}`;

/** The armed state: confirm + keep, two sibling forms swapping the same slot (a form can't
 * nest, and one form with two formactions doesn't survive the helmjs submit path). */
function confirmFragment(t: DelTarget): SafeHtml {
    return html`<span id="del-${t.id}" class="del-confirm">
        <form class="act-form" action="/delete/${t.id}/confirm" method="post" h-post h-target="#del-${t.id}" h-swap="outer">${hiddenFields(t)}
          <button type="submit" class="note-act delete active" title="Yes, delete it - relays that honor deletions will drop it" aria-label="Yes, delete it">${icon('trash', true)}</button></form>
        <form class="act-form" action="/delete/${t.id}/cancel" method="post" h-post h-target="#del-${t.id}" h-swap="outer">${hiddenFields(t)}
          <button type="submit" class="note-act" title="Keep it" aria-label="Keep it">${icon('back')}</button></form>
      </span>`;
}

/** Post-delete state: an inert glyph. The card itself drops on the next page load - the
 * tombstone is already recorded, so every future render filters the event out. */
const deletedFragment = (id: string): SafeHtml =>
    html`<span id="del-${id}" class="note-act delete deleted" title="Deleted - relays that honor deletion requests will drop it">${icon('trash', true)}<span class="sr-only">Deleted</span></span>`;

function respondFragment(ctx: Ctx, body: SafeHtml, id: string, back = ''): void {
    if (ctx.isPartial) sendFragment(ctx, body, { 'H-Reswap': 'outer', 'H-Retarget': `#del-${id}` });
    else redirect(ctx, back || safeReferer(ctx));
}

/** Arm: swap the glyph for the inline confirm (helmjs), or - zero-JS - render a small confirm
 * PAGE with the same two forms, which work without any client JS. The `back` path captured
 * here rides the chain so confirm/keep can land on the originating page (the interstitial's
 * own URL is POST-only, so the referer would be a dead end). */
export async function postDelete(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const t = parseDelete(ctx, s.me, await readForm(ctx.req));
    if (!t) { notFound(ctx); return; }
    if (ctx.isPartial) { respondFragment(ctx, confirmFragment(t), t.id); return; }
    const armed: DelTarget = { ...t, back: t.back || safeReferer(ctx) };
    sendPage(ctx, html`<div class="view-pad">
        <h2 class="page-title">Delete this post?</h2>
        <p class="filter-help">This publishes a deletion request to your relays. Relays that honor deletions will drop the post; copies may persist on relays that don't.</p>
        ${confirmFragment(armed)}
      </div>`, chromeFor(ctx, s, { active: 'feed', title: 'Delete' }));
}

/** Disarm: put the original glyph back. */
export async function postDeleteCancel(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const t = parseDelete(ctx, s.me, await readForm(ctx.req));
    if (!t) { notFound(ctx); return; }
    respondFragment(ctx, deleteActRaw(t.id, t.k, t.coord), t.id, t.back);
}

/** Confirmed: build the kind:5. Bunker signs + publishes here; nip07 hands the template to the
 * extension with /delete/:id/publish as the continuation. */
export async function postDeleteConfirm(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const t = parseDelete(ctx, s.me, await readForm(ctx.req));
    if (!t) { notFound(ctx); return; }
    const template = deleteTemplate(s.me, t);

    if (signsOnClient(s)) {
        sendSignRequest(ctx, template, `/delete/${t.id}/publish?k=${t.k}${t.coord ? `&a=${encodeURIComponent(t.coord)}` : ''}`);
        return;
    }
    try {
        const signed = await s.signer!.signEvent(template);
        if (!await published(s, signed)) throw new Error('no relay accepted it');
    } catch (err) {
        sendFragment(ctx, html`<div class="notice error">Couldn't delete: ${err instanceof Error ? err.message : String(err)}</div>`, {}, 502);
        return;
    }
    respondFragment(ctx, deletedFragment(t.id), t.id, t.back);
}

/** nip07 continuation: verify the extension-signed kind:5, publish, swap in the deleted state. */
export async function postDeletePublish(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const t = parseDelete(ctx, s.me, ctx.query);
    if (!t) { notFound(ctx); return; }
    const signed = await requireSigned(ctx, s.me, 5, 'the deletion');
    if (!signed) return;
    // Bind the signed event to THIS target: requireSigned proves your key signed a kind:5, but
    // not that it deletes the event this continuation claims - a substituted/replayed deletion
    // of a different own event would otherwise publish while the UI reports this one deleted.
    if (tag1(signed, 'e').toLowerCase() !== t.id) {
        sendFragment(ctx, html`<div class="notice error">Couldn't delete: the signed deletion doesn't match this post.</div>`, {}, 400);
        return;
    }
    if (!await published(s, signed)) {
        sendFragment(ctx, html`<div class="notice error">Couldn't delete: no relay accepted it.</div>`, {}, 502);
        return;
    }
    // The continuation is the lib's own fetch; re-assert placement like the other sign
    // continuations (the sign-request set H-Reswap:none).
    sendFragment(ctx, deletedFragment(t.id), { 'H-Reswap': 'outer', 'H-Retarget': `#del-${t.id}` });
}
