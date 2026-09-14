// GET /handlers/:id?k=<kind> - lazy NIP-89 discovery for an unknown-kind card. The fallback card
// renders a working njump link immediately; this swaps in "open in <app>" links for apps that
// declare support for the kind (kind:31990). JS-only (h-get); a no-JS user just uses the njump
// link already in the card, so a direct navigation here just bounces to njump.

import { decode } from 'nostr-tools/nip19';
import { fetchHandlers } from '../data/nip89.ts';
import { handlerLinks } from '../manifest/fallback.ts';
import { INDEXER_RELAYS } from '../nostr/nip65.ts';
import { requireLogin } from './common.ts';
import { sendFragment, redirect, type Ctx } from '../http.ts';

export async function getHandlers(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const bech = ctx.params.id ?? '';
    const kind = Number(ctx.query.get('k'));
    if (!ctx.isPartial || !bech || !Number.isFinite(kind)) { redirect(ctx, `https://njump.me/${encodeURIComponent(bech)}`); return; }
    let entity = '';
    try { entity = decode(bech).type; } catch { /* raw id / bad bech - entity stays '' */ }
    const relays = [...new Set([...(s.myRelays?.read ?? []), ...INDEXER_RELAYS])];
    const handlers = await fetchHandlers(s.pool, relays, kind);
    sendFragment(ctx, handlerLinks(handlers, bech, entity));
}
