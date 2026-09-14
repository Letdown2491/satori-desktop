// NIP-57 zap routes. GET /zap?e=&p= opens the modal; POST /zap resolves the
// recipient's LNURL, builds the kind:9734 zap request (public → user signs via
// bunker/nip07; anonymous → a throwaway key here), fetches the bolt11 invoice,
// and shows it for an external wallet to pay (no spending credential server-side).
// nip07 public zaps sign-and-resubmit through POST /zap/invoice.

import { html, type SafeHtml } from '../html.ts';
import { resolveLnurlPay, zapRequestTemplate, anonZapRequest, fetchZapInvoice, zapLnurl, type ZapRef } from '../data/zap.ts';
import { writeRelays } from '../actions.ts';
import { INDEXER_RELAYS } from '../nostr/nip65.ts';
import { decodeNaddr } from '../nostr/nip19.ts';
import { HEX64 } from '../nostr/tags.ts';
import { zapModal, invoiceView, zappedToast, ZAP_MAX, type ZapCtx } from '../render/zap.ts';
import { zapButton, articleZapButton } from '../render/note.ts';
import { markZapped } from '../zaps.ts';
import { requireSigned } from '../nip07.ts';
import { requireLogin, ensureProfiles, chromeFor } from './common.ts';
import { readAppearance } from '../theme.ts';
import { readForm, readJson, sendPage, sendFragment, sendSignRequest, notFound, type Ctx } from '../http.ts';
import type { Session } from '../session.ts';
import { signsOnClient } from '../session.ts';

const zapRelays = (s: Session & { me: string }) => [...new Set([...writeRelays(s), ...INDEXER_RELAYS])].slice(0, 6);

/** A zap target's naddr → its `a`-tag address (`kind:pubkey:identifier`), or null if not a naddr. */
function naddrAddress(naddr: string): string | null {
    return decodeNaddr(naddr)?.coord ?? null;
}

/** Send the modal as a fragment (helmjs) or a full page (zero-JS baseline). */
function sendModal(ctx: Ctx, s: Session & { me: string }, body: ReturnType<typeof zapModal>, title = 'Zap', extra: Record<string, string> = {}): void {
    if (ctx.isPartial && ctx.hTarget === '#modal') sendFragment(ctx, body, extra);
    else sendPage(ctx, body, chromeFor(ctx, s, { title }));
}

/** One-tap WebLN headers: the bolt11 to pay + where to POST the preimage. The
 * lib only acts on these when window.webln exists; otherwise it falls back to
 * swapping the invoice view (the lightning: link). So we can always send them. */
const payHeaders = (bolt11: string, recipient: string, sats: number, eventId?: string, naddr?: string): Record<string, string> =>
    ({ 'H-Webln-Pay': bolt11, 'H-Webln-Continue': `/zap/paid?sats=${sats}&p=${recipient}${eventId ? `&e=${eventId}` : ''}${naddr ? `&a=${naddr}` : ''}` });

/** GET /zap?e=<noteId>&p=<authorHex> (or ?a=<naddr> for an article) - open the zap modal. */
export async function getZap(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const eventId = ctx.query.get('e') ?? undefined;
    const naddr = ctx.query.get('a') ?? undefined;
    const recipient = ctx.query.get('p') ?? '';
    if (!HEX64.test(recipient) || (eventId && !HEX64.test(eventId)) || (naddr && !naddrAddress(naddr))) { notFound(ctx); return; }
    await ensureProfiles(s, [recipient]);
    const presets = readAppearance(ctx).zapPresets;
    const lud16 = s.profiles.get(recipient)?.lud16;
    if (!lud16) { sendModal(ctx, s, zapModal(s, { recipient, lud16: '', eventId, naddr, presets, error: 'This account hasn’t set up a lightning address, so it can’t receive zaps.' })); return; }
    sendModal(ctx, s, zapModal(s, { recipient, lud16, eventId, naddr, presets }));
}

function parseAmount(form: URLSearchParams): number {
    // The hero input carries the amount; presets set it client-side (h-insert).
    return Math.floor(Number(form.get('sats')) || 0);
}

export async function postZap(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const form = await readForm(ctx.req);
    const recipient = (form.get('recipient') ?? '').trim();
    const lud16 = (form.get('lud16') ?? '').trim();
    const eventId = (form.get('event') ?? '').trim() || undefined;
    const naddr = (form.get('naddr') ?? '').trim() || undefined;
    const address = naddr ? naddrAddress(naddr) ?? undefined : undefined; // article target → `a`-tag
    const comment = (form.get('comment') ?? '').trim();
    const anonymous = form.get('privacy') === 'anonymous';
    const sats = parseAmount(form);
    if (!HEX64.test(recipient) || (naddr && !address)) { notFound(ctx); return; }

    const back = (error: string): void => { const c: ZapCtx = { recipient, lud16, eventId, naddr, error, presets: readAppearance(ctx).zapPresets }; sendModal(ctx, s, zapModal(s, c)); };
    if (sats <= 0) { back('Enter an amount.'); return; }
    if (sats > ZAP_MAX) { back(`That’s over the ${ZAP_MAX.toLocaleString()} sats limit.`); return; }

    const ref: ZapRef = { recipientPubkey: recipient, eventId, address, amountMsats: sats * 1000, comment, relays: zapRelays(s), lnurl: zapLnurl(lud16) };

    let info;
    try { info = await resolveLnurlPay(lud16); } catch (e) { back(e instanceof Error ? e.message : 'Could not reach the lightning address.'); return; }

    // public + client-signs: hand the 9734 to the extension/app; the continuation fetches the invoice.
    if (!anonymous && signsOnClient(s)) {
        const q = new URLSearchParams({ recipient, lud16, sats: String(sats) });
        if (eventId) q.set('event', eventId);
        if (naddr) q.set('naddr', naddr);
        sendSignRequest(ctx, zapRequestTemplate(s.me, ref), `/zap/invoice?${q}`);
        return;
    }

    try {
        const signed = anonymous ? anonZapRequest(ref) : await s.signer!.signEvent(zapRequestTemplate(s.me, ref));
        const invoice = await fetchZapInvoice(info, ref.amountMsats, signed);
        sendModal(ctx, s, invoiceView(s, recipient, sats, invoice), 'Zap', payHeaders(invoice, recipient, sats, eventId, naddr));
    } catch (e) {
        back(e instanceof Error ? e.message : 'Could not create the invoice.');
    }
}

/** nip07 continuation: take the extension-signed 9734 + fetch the invoice. */
export async function postZapInvoice(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const recipient = ctx.query.get('recipient') ?? '';
    const lud16 = ctx.query.get('lud16') ?? '';
    const eventId = ctx.query.get('event') ?? undefined;
    const naddr = ctx.query.get('naddr') ?? undefined;
    const sats = Math.floor(Number(ctx.query.get('sats') || 0));
    // Validate the target ids before they ride into the pay headers (mirrors getZap:49) - defense in
    // depth: same-origin relative path + Node's CRLF guard + /zap/paid re-validation already contain it.
    if (!HEX64.test(recipient) || sats <= 0 || (eventId && !HEX64.test(eventId)) || (naddr && !naddrAddress(naddr))) { notFound(ctx); return; }

    const signed = await requireSigned(ctx, s.me, 9734, 'the zap request', { 'H-Reswap': 'inner', 'H-Retarget': '#modal' });
    if (!signed) return;
    try {
        const info = await resolveLnurlPay(lud16);
        const invoice = await fetchZapInvoice(info, sats * 1000, signed);
        // Re-assert placement (the sign-request's H-Reswap:none poisons the swap);
        // plus the one-tap pay headers, so a WebLN client chains sign → pay.
        sendFragment(ctx, invoiceView(s, recipient, sats, invoice), { 'H-Reswap': 'inner', 'H-Retarget': '#modal', ...payHeaders(invoice, recipient, sats, eventId, naddr) });
    } catch (e) {
        sendFragment(ctx, html`<div class="notice error">${e instanceof Error ? e.message : 'Could not create the invoice.'}</div>`, { 'H-Reswap': 'inner', 'H-Retarget': '#modal' }, 502);
    }
}

/** WebLN one-tap continuation: the extension paid the invoice and POSTs the
 * preimage here. We don't need to verify it (the zap receipt is published by the
 * recipient's service); just confirm in the modal. */
export async function postZapPaid(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const recipient = ctx.query.get('p') ?? '';
    const eventId = ctx.query.get('e') ?? '';
    const naddr = ctx.query.get('a') ?? '';
    const sats = Math.floor(Number(ctx.query.get('sats') || 0));
    await readJson(ctx.req).catch(() => null); // { preimage } - proof of payment, not needed server-side
    // Mark the target zapped (a UI flag) and OOB-fill its button behind the modal, so it lights
    // up immediately and survives a refresh. Note id (`e`) → note button; article naddr (`a`) → article button.
    let oob: SafeHtml | null = null;
    if (HEX64.test(eventId) && HEX64.test(recipient)) {
        markZapped(s, eventId);
        oob = zapButton(eventId, recipient, true, true);
    } else if (naddr.startsWith('naddr1') && HEX64.test(recipient)) {
        markZapped(s, naddr);
        oob = articleZapButton(naddr, recipient, true, true);
    }
    sendFragment(ctx, html`${zappedToast(s, recipient, sats)}${oob}`, { 'H-Reswap': 'inner', 'H-Retarget': '#modal' });
}
