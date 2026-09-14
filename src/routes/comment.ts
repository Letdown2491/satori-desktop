// NIP-22 comment routes. POST /comment signs a kind:1111 (bunker here / nip07
// sign-and-resubmit via /comment/publish) and re-renders the whole #comment-section
// (re-fetched + the new comment merged) - one swap target, correct threading.
// GET /comment/form loads an inline reply form under a comment.

import { html } from '../html.ts';
import { signComment, publishSigned, captureSigner, type CommentTarget, type CommentRef } from '../data/publish.ts';
import { fetchArticleComments } from '../data/comments.ts';
import { KIND_ARTICLE } from '../nostr/nip23.ts';
import { KIND_COMMENT } from '../nostr/nip22.ts';
import { commentUpdate, commentForm } from '../render/comments.ts';
import { writeRelays } from '../actions.ts';
import { requireSigned } from '../nip07.ts';
import { requireLogin, ensureProfiles, notePubkeys } from './common.ts';
import { readForm, redirect, safeReferer, sendFragment, sendSignRequest, notFound, type Ctx } from '../http.ts';
import { HEX64, coordParts } from '../nostr/tags.ts';
import type { Session } from '../session.ts';
import { signsOnClient } from '../session.ts';
import type { NostrEvent } from '../nostr/types.ts';
import type { SafeHtml } from '../html.ts';
// Success: morph the list in place (no avatar reflash / preserved scroll); the count
// and top-level form ride along OOB. Errors replace the whole section with the notice.
const PLACE = { 'H-Reswap': 'morph', 'H-Retarget': '#comment-list' };
const ERR_PLACE = { 'H-Reswap': 'inner', 'H-Retarget': '#comment-section' };

/** Parse the comment form's hidden refs into a NIP-22 target (root = the article;
 * parent = the article for a top-level comment, or a comment for a reply). */
function targetFrom(form: URLSearchParams): { ra: string; rp: string; target: CommentTarget } | null {
    const ra = (form.get('ra') ?? '').trim();  // article address kind:pubkey:d
    const rp = (form.get('rp') ?? '').trim();   // article author
    const pi = (form.get('pi') ?? '').trim();   // parent comment id (empty = top-level)
    const pp = (form.get('pp') ?? '').trim();   // parent author
    if (!ra || !HEX64.test(rp)) return null;
    // The root kind is the first segment of the `kind:pubkey:d` address, so a comment on a custom NIP (or
    // any addressable) carries the correct root `K` tag - not always the article kind.
    const root: CommentRef = { kind: coordParts(ra)?.kind ?? KIND_ARTICLE, pubkey: rp, address: ra };
    const parent: CommentRef = pi && HEX64.test(pi) ? { kind: KIND_COMMENT, pubkey: pp, id: pi } : root;
    return { ra, rp, target: { root, parent } };
}

async function renderSection(s: Session & { me: string }, ra: string, rp: string, extra?: NostrEvent): Promise<SafeHtml> {
    const comments = await fetchArticleComments(s.pool, ra, rp).catch(() => [] as NostrEvent[]);
    if (extra && !comments.some((c) => c.id === extra.id)) comments.push(extra); // optimistic: relays may lag
    await ensureProfiles(s, notePubkeys(comments)); // comment authors + their @mentions
    return commentUpdate(ra, rp, comments, s.profiles);
}

export async function postComment(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const form = await readForm(ctx.req);
    const content = (form.get('content') ?? '').trim();
    const t = targetFrom(form);
    if (!t) { notFound(ctx); return; }
    // Empty after trim (whitespace-only satisfies `required` then trims away). A no-JS POST
    // must get a real page back, not a chrome-less fragment - reload the article it came from.
    if (!content) {
        if (ctx.isPartial) sendFragment(ctx, await renderSection(s, t.ra, t.rp), PLACE);
        else redirect(ctx, safeReferer(ctx));
        return;
    }

    // client-signs: the extension/app signs the 1111; the continuation publishes + re-renders.
    if (signsOnClient(s)) {
        const prepared = await signComment(captureSigner, s.pool, s.me, s.myRelays!, { content, comment: t.target });
        sendSignRequest(ctx, prepared.signed, `/comment/publish?ra=${encodeURIComponent(t.ra)}&rp=${t.rp}`);
        return;
    }

    // bunker: sign + publish here.
    try {
        const prepared = await signComment(s.signer!, s.pool, s.me, s.myRelays!, { content, comment: t.target });
        await publishSigned(s.pool, prepared);
        if (ctx.isPartial) sendFragment(ctx, await renderSection(s, t.ra, t.rp, prepared.signed));
        else redirect(ctx, safeReferer(ctx));
    } catch (err) {
        sendFragment(ctx, html`<div class="notice error">Couldn't post the comment: ${err instanceof Error ? err.message : String(err)}</div>`, ERR_PLACE, 502);
    }
}

/** nip07 continuation: verify the extension-signed comment, publish, re-render. */
export async function postCommentPublish(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const ra = ctx.query.get('ra') ?? '';
    const rp = ctx.query.get('rp') ?? '';
    if (!HEX64.test(rp)) { sendFragment(ctx, html`<div class="notice error">Couldn't verify the comment.</div>`, ERR_PLACE, 400); return; }
    const signed = await requireSigned(ctx, s.me, KIND_COMMENT, 'the comment', ERR_PLACE);
    if (!signed) return;
    await publishSigned(s.pool, { signed, isReply: true, writeTargets: writeRelays(s), inboxTargets: [] }).catch(() => { /* best effort */ });
    sendFragment(ctx, await renderSection(s, ra, rp, signed), PLACE);
}

/** GET /comment/form - an inline reply form under a comment (helmjs). */
export function getCommentForm(ctx: Ctx): void {
    const s = requireLogin(ctx);
    if (!s) return;
    sendFragment(ctx, commentForm(ctx.query.get('ra') ?? '', ctx.query.get('rp') ?? '', ctx.query.get('pi') ?? '', ctx.query.get('pp') ?? '', 'Write a reply…'));
}
