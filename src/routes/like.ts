// NIP-25 like toggle. POST /like/:target creates a kind:7 "+" (or deletes it via
// kind:5 when already liked); for nip07 it returns H-Nostr-Sign and POST
// /like/:target/publish is the continuation. Both end by swapping the updated
// heart in place (helmjs), or - zero-JS bunker - reloading the originating page.
// `:target` is a note id (hex, `e`-tag like) OR an article naddr (`a`-tag like); the
// cache key + DOM id stay the target string verbatim (the canonical naddr matches naddrFor).

import { html } from '../html.ts';
import { decodeNaddr } from '../nostr/nip19.ts';
import { likeTemplate, articleLikeTemplate, unlikeTemplate, pickReaction } from '../data/reactions.ts';
import { userEmojiCached } from '../data/emoji-sets.ts';
import { cachedLikeId, setLike, clearLike } from '../data/engagement-cache.ts';
import { seenRelaysFor } from '../data/seen-relays.ts';
import { published } from '../actions.ts';
import { likeButton } from '../render/actions.ts';
import { requireSigned } from '../nip07.ts';
import { requireLogin } from './common.ts';
import { readForm, redirect, safeReferer, sendFragment, sendSignRequest, notFound, type Ctx } from '../http.ts';
import { HEX64, coordParts } from '../nostr/tags.ts';
import { INDEXER_RELAYS } from '../nostr/nip65.ts';
import type { Session } from '../session.ts';
import { signsOnClient } from '../session.ts';

/** Resolve a like target: a note (hex id) or an article (naddr → address). Returns the author
 * (for the like's p-tag + the rendered button) and, for an article, the `a`-tag inputs. */
/** A reacted-kind from untrusted form/query input: a non-negative integer kind, else 1 (note). Guards
 * the NIP-25 `k` tag against garbage like "Infinity"/"-5"/"1.9". */
const kindFromForm = (v: string | null): number => { const n = Number(v); return Number.isInteger(n) && n >= 0 && n <= 65535 ? n : 1; };

function likeTarget(target: string, formAuthor: string): { author: string; addr?: { address: string; pubkey: string; kind: number; relays?: string[] } } | null {
    if (target.startsWith('naddr1')) {
        const d = decodeNaddr(target);
        if (!d) return null;
        return { author: d.pubkey, addr: { address: d.coord, pubkey: d.pubkey, kind: d.kind, relays: d.relays } };
    }
    if (!HEX64.test(target) || !HEX64.test(formAuthor)) return null;
    return { author: formAuthor };
}

/** A relay hint for the reacted event/author (NIP-25 SHOULD): the naddr's own hint, else the relays we've
 * empirically seen this author on. All in-memory - no network round-trip on the like path. */
function reactHint(author: string, addr?: { pubkey: string; relays?: string[] }): string {
    return addr?.relays?.[0] ?? seenRelaysFor(addr?.pubkey ?? author)[0] ?? '';
}

/** Resolve an addressable event's CURRENT event id (NIP-25 requires the reaction's `e` tag alongside
 * `a`; an naddr carries no id). Best-effort, 3s cap; returns undefined if not found so the reaction
 * falls back to a-only rather than blocking the like. Uses the naddr + empirical relay hints. */
async function addressableEventId(s: Session & { me: string }, addr: { address: string; pubkey: string; kind: number; relays?: string[] }): Promise<string | undefined> {
    const c = coordParts(addr.address);
    if (!c) return undefined;
    const relays = [...(addr.relays ?? []), ...seenRelaysFor(addr.pubkey), ...INDEXER_RELAYS].slice(0, 4);
    const ev = await s.pool.get(relays, { kinds: [c.kind], authors: [c.pubkey], ...(c.d ? { '#d': [c.d] } : {}) }, 3000).catch(() => null);
    return ev?.id;
}

function respond(ctx: Ctx, s: Session & { me: string }, noteId: string, author: string, kind: number): void {
    if (ctx.isPartial) sendFragment(ctx, likeButton(s, noteId, author, kind));
    else redirect(ctx, safeReferer(ctx));
}

export async function postLike(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    if (!s.reactions) { notFound(ctx); return; } // reactions disabled in settings → no like emission
    const noteId = ctx.params.target ?? '';
    const form = await readForm(ctx.req);
    const t = likeTarget(noteId, (form.get('author') ?? '').trim());
    if (!t) { notFound(ctx); return; }
    const author = t.author;

    const existing = cachedLikeId(s.me, noteId) ?? null; // current state (+ reaction id to unlike) from the cache
    const op = existing ? 'unlike' : 'like';
    // Which reaction to add: a palette unicode emoji or one of the user's own custom emoji (the url is
    // resolved server-side from their set, never trusted from the client). Ignored on the unlike path.
    const reaction = pickReaction(form.get('emoji'), userEmojiCached(s.me));
    // The reacted event's kind (for the `k` tag): the naddr carries it; a note carries it in a hidden
    // field (the like button knows the rendered event's kind). Defaults to 1 if absent/garbled.
    const kind = t.addr ? t.addr.kind : kindFromForm(form.get('k'));
    const hint = reactHint(author, t.addr);
    // NIP-25: an addressable reaction MUST carry an `e` tag (the specific event id) alongside `a`. The like
    // button already knows the id and passes it as a hidden `eid` (no round-trip); resolve it best-effort
    // only if that's missing. On a like only, not an unlike.
    const formEid = (form.get('eid') ?? '').trim();
    const eventId = t.addr && !existing ? (HEX64.test(formEid) ? formEid : await addressableEventId(s, t.addr)) : undefined;
    const template = existing ? unlikeTemplate(s.me, existing)
        : t.addr ? articleLikeTemplate(s.me, { ...t.addr, relayHint: hint, eventId }, reaction) : likeTemplate(s.me, { id: noteId, pubkey: author, kind, relayHint: hint }, reaction);

    // nip07: the extension signs; the continuation publishes + updates state. Carry the shortcode/char
    // only - the url is re-resolved server-side from the user's set in the continuation.
    if (signsOnClient(s)) { sendSignRequest(ctx, template, `/like/${noteId}/publish?author=${author}&op=${op}&emoji=${encodeURIComponent(reaction.emoji)}&k=${kind}`); return; }

    // bunker: sign + publish here.
    try {
        const signed = await s.signer!.signEvent(template);
        if (!await published(s, signed)) throw new Error('no relay accepted it');
        if (op === 'like') setLike(s.me, noteId, signed.id, reaction.emoji, reaction.url); else clearLike(s.me, noteId);
    } catch (err) {
        sendFragment(ctx, html`<div class="notice error">Couldn't ${op}: ${err instanceof Error ? err.message : String(err)}</div>`, {}, 502);
        return;
    }
    respond(ctx, s, noteId, author, kind);
}

/** nip07 continuation: verify the extension-signed reaction/deletion, publish, swap. */
export async function postLikePublish(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    if (!s.reactions) { notFound(ctx); return; } // reactions disabled → reject the continuation too
    const noteId = ctx.params.target ?? '';
    const author = (ctx.query.get('author') ?? '').trim();
    const op = ctx.query.get('op') === 'unlike' ? 'unlike' : 'like';
    const kind = kindFromForm(ctx.query.get('k'));
    const reaction = pickReaction(ctx.query.get('emoji'), userEmojiCached(s.me));
    // Decode-validate (not just the `naddr1` prefix): a non-canonical target would otherwise reach
    // the `H-Retarget: #like-<target>` header below. likeTarget() rejects anything that isn't a hex
    // note id or a real naddr, so only the safe bech32 charset can flow into the selector.
    if (!likeTarget(noteId, author)) { notFound(ctx); return; }

    const signed = await requireSigned(ctx, s.me, op === 'like' ? 7 : 5, 'the like');
    if (!signed) return;
    if (!await published(s, signed)) {
        sendFragment(ctx, html`<div class="notice error">Couldn't ${op === 'like' ? 'like' : 'unlike'} that - no relay accepted it.</div>`, {}, 502);
        return;
    }
    if (op === 'like') setLike(s.me, noteId, signed.id, reaction.emoji, reaction.url); else clearLike(s.me, noteId);
    // Declare placement: the sign-request set H-Reswap:none (so a non-plugin client
    // won't swap the JSON template), which mutates the request's swap to "none". The
    // continuation must re-assert the swap or the heart never updates (only a reload
    // would). Mirrors the note-publish flow's LAND_ON_FEED headers.
    sendFragment(ctx, likeButton(s, noteId, author, kind), { 'H-Reswap': 'outer', 'H-Retarget': `#like-${noteId}` });
}
