// NIP-57 zaps (send side), mirroring Satori's data/zap-send.ts. Resolve a
// recipient's LNURL-pay endpoint, build a kind:9734 zap request, and fetch the
// bolt11 invoice. Split into template-builders + an invoice fetch so both signing
// modes work (bunker/nip07 sign the public request; anonymous uses a throwaway
// key here). NO wallet/spending credential is involved - the invoice is paid by
// an external wallet (lightning: link). Private mode (inner kind:9733) deferred.

import { finalizeEvent, generateSecretKey } from 'nostr-tools/pure';
import { decodeLnurl, encodeLnurl } from '../nostr/lnurl.ts';
import { isPublicHttpUrl } from '../ssrf.ts';
import { torFetch } from './torfetch.ts';
import type { UnsignedEvent, NostrEvent } from '../nostr/types.ts';

export type ZapPrivacy = 'public' | 'anonymous';
export interface LnurlPay { callback: string; allowsNostr: boolean; nostrPubkey?: string; min: number; max: number }

/** The LNURL-pay URL for a recipient address: lud16 (user@domain), lud06 lnurl,
 * or a raw https URL. */
function lnurlPayUrl(addr: string): string | null {
    const a = addr.trim();
    if (a.includes('@')) { const [name, domain] = a.split('@'); return name && domain ? `https://${domain}/.well-known/lnurlp/${name}` : null; }
    if (/^lnurl1/i.test(a)) return decodeLnurl(a);
    if (/^https?:\/\//i.test(a)) return a;
    return null;
}

/** The NIP-57 `lnurl` tag value for a recipient: the pay url, bech32-encoded (LUD-06). Returns
 * undefined for an address we can't resolve to a url (the tag is recommended, so we just skip it). */
export function zapLnurl(addr: string): string | undefined {
    const url = lnurlPayUrl(addr);
    return url ? encodeLnurl(url) : undefined;
}

/** Resolve a recipient address (lud16/lud06/lnurl) to its LNURL-pay parameters. */
export async function resolveLnurlPay(addr: string): Promise<LnurlPay> {
    const url = lnurlPayUrl(addr);
    if (!url) throw new Error('invalid lightning address');
    if (!isPublicHttpUrl(url)) throw new Error('lightning address points to a non-public host'); // SSRF guard (lud16 domain is from someone else's profile)
    const res = await torFetch(url, 10000, 512 * 1024).catch(() => null); // Privacy-Mode-aware
    if (!res || res.status !== 200) throw new Error('could not reach the lightning address');
    let j: { callback?: string; allowsNostr?: boolean; nostrPubkey?: string; minSendable?: number; maxSendable?: number } | null = null;
    try { j = JSON.parse(res.body.toString('utf8')); } catch { /* not JSON */ }
    if (!j?.callback) throw new Error('lightning address has no pay endpoint');
    return { callback: j.callback, allowsNostr: !!j.allowsNostr, nostrPubkey: j.nostrPubkey, min: j.minSendable ?? 1000, max: j.maxSendable ?? 1e11 };
}

export interface ZapRef { recipientPubkey: string; eventId?: string; address?: string; amountMsats: number; comment: string; relays: string[]; lnurl?: string }

/** The shared 9734 tags (NIP-57): relays, amount, the recommended `lnurl`, recipient `p`, then the
 * target `e` (event) or `a` (addressable). `extra` appends before the target (e.g. the `anon` tag). */
function zapTags(o: ZapRef, extra: string[][] = []): string[][] {
    const tags: string[][] = [['relays', ...o.relays], ['amount', String(o.amountMsats)]];
    if (o.lnurl) tags.push(['lnurl', o.lnurl]);
    tags.push(['p', o.recipientPubkey], ...extra);
    if (o.eventId) tags.push(['e', o.eventId]); else if (o.address) tags.push(['a', o.address]);
    return tags;
}

/** The unsigned kind:9734 zap request (public mode - signed by the user). */
export function zapRequestTemplate(me: string, o: ZapRef): UnsignedEvent {
    return { kind: 9734, created_at: Math.floor(Date.now() / 1000), pubkey: me, content: o.comment, tags: zapTags(o) };
}

/** A fully-signed anonymous kind:9734 (throwaway key + `anon` tag - unlinkable). */
export function anonZapRequest(o: ZapRef): NostrEvent {
    return finalizeEvent({ kind: 9734, created_at: Math.floor(Date.now() / 1000), content: o.comment, tags: zapTags(o, [['anon', '']]) }, generateSecretKey()) as NostrEvent;
}

/** Call the LNURL callback with the (signed) zap request → bolt11 invoice. */
export async function fetchZapInvoice(info: LnurlPay, amountMsats: number, signedZapRequest: NostrEvent): Promise<string> {
    if (!info.allowsNostr || !info.nostrPubkey) throw new Error('this recipient doesn’t support zaps');
    if (amountMsats < info.min || amountMsats > info.max) throw new Error('amount is outside the recipient’s allowed range');
    if (!isPublicHttpUrl(info.callback)) throw new Error('invalid pay endpoint'); // SSRF guard - callback is whatever the lnurl server returned
    const sep = info.callback.includes('?') ? '&' : '?';
    const url = `${info.callback}${sep}amount=${amountMsats}&nostr=${encodeURIComponent(JSON.stringify(signedZapRequest))}`;
    const res = await torFetch(url, 10000, 512 * 1024).catch(() => null); // Privacy-Mode-aware
    if (!res || res.status !== 200) throw new Error('could not get an invoice from the recipient');
    let j: { pr?: string; reason?: string } | null = null;
    try { j = JSON.parse(res.body.toString('utf8')); } catch { /* not JSON */ }
    if (!j?.pr) throw new Error(j?.reason || 'no invoice returned');
    return j.pr;
}
