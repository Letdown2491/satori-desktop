// Shared undo-window step for the publish handlers (note/reply/quote/poll, both
// signing modes). If the undo window is on, hold the signed event server-side and
// answer with the feed + a countdown toast instead of publishing now; the caller
// returns. A reply made FROM a thread (`inThread`) instead appends an optimistic
// pending reply into the open thread (and closes the modal). See undo.ts for why a
// closed tab leaves the event unpublished (it matches Satori).

import { holdPublish } from '../undo.ts';
import { undoToast } from '../render/compose.ts';
import { noteCard } from '../render/note.ts';
import { html, type SafeHtml } from '../html.ts';
import { readAppearance } from '../theme.ts';
import { feedDocument } from './feed.ts';
import { LAND_ON_FEED } from './common.ts';
import { sendFragment, type Ctx } from '../http.ts';
import type { NostrEvent } from '../nostr/types.ts';
import type { Prepared } from '../data/publish.ts';
import type { Session } from '../session.ts';

// helmjs swap vocab: `append` = insert as the last child (it maps to
// insertAdjacentHTML("beforeend") internally). NOT "beforeend" - that fails
// helmjs's H-Reswap validation, leaving the swap at the sign-request's poisoned
// "none" so the reply card is never inserted.
const APPEND_THREAD = { 'H-Retarget': '#thread', 'H-Reswap': 'append' };
// Float the undo toast into the layout's persistent #undo-slot without touching the feed/page.
const FLOAT_UNDO = { 'H-Retarget': '#undo-slot', 'H-Reswap': 'inner' };
// OOB-close the compose modal: `#modal:empty` hides it (styles.css). Used as a secondary swap alongside a
// primary one (a thread append / a floated toast), where the modal isn't the request's main target.
export const CLOSE_MODAL_OOB = html`<div id="modal" h-oob="true"></div>`;

/** Append a reply note (pending or confirmed) into the open thread's `#thread` list
 * and close the compose modal (OOB). The append target/swap are server-driven so
 * the boosted form's default body-swap is overridden. */
export function sendReplyToThread(
    ctx: Ctx, s: Session & { me: string }, ev: NostrEvent, inThread: string,
    pending?: { token: string; seconds: number }, isPrivate = false,
): void {
    const card = noteCard(ev, s.profiles, s, { hideParent: true, depth: 0, inThread, pending, isPrivate });
    sendFragment(ctx, html`${card}${CLOSE_MODAL_OOB}`, APPEND_THREAD);
}

/** Undo OFF, modal compose, JS path: the note is already published, so just close the compose modal and
 * stay put - no feed re-render (which bounced you to the boundary view). The full-page /compose path lands
 * on the feed instead (landOnFeed); zero-JS callers redirect. */
export function stayPutCloseModal(ctx: Ctx): void {
    sendFragment(ctx, html``, { 'H-Retarget': '#modal', 'H-Reswap': 'inner' });
}

/** Land a freshly-published note on the Following feed (the full-page /compose path, JS): body-swap the feed
 * document, optionally carrying the undo toast. This is a single FIRST render (you weren't already viewing
 * the timeline), so it can't trip the empty-timeline double-render that the modal stay-put path avoids. */
export async function landOnFeed(ctx: Ctx, s: Session & { me: string }, toast?: SafeHtml): Promise<void> {
    sendFragment(ctx, await feedDocument(ctx, s, toast), LAND_ON_FEED);
}

interface UndoOpts { requirePartial?: boolean; inThread?: string; fromModal?: boolean }

/** Hold the publish + show the countdown: appended into an open thread (`inThread`), floated over the page
 * (modal compose, stay put), or landed on the feed (full-page compose). Returns true (caller should return),
 * or false if the window is off and the caller should publish now. `requirePartial` (default true) means a
 * zero-JS form post skips the window; nip07 passes false. */
export async function tryUndoWindow(
    ctx: Ctx, s: Session & { me: string }, prepared: Prepared, opts: UndoOpts = {},
): Promise<boolean> {
    const { requirePartial = true, inThread, fromModal = false } = opts;
    const a = readAppearance(ctx);
    if (!a.undoEnabled || (requirePartial && !ctx.isPartial)) return false;
    const token = holdPublish(s.pool, prepared, a.undoSeconds, inThread ? { inThread } : undefined);
    if (inThread) { sendReplyToThread(ctx, s, prepared.signed, inThread, { token, seconds: a.undoSeconds }); return true; }
    // Top-level note/poll. From the modal: float the toast into #undo-slot + OOB-close the modal so you stay
    // exactly where you were (a feed re-render here bounced you to the boundary view + marked everything seen).
    // From the full /compose page: land on the feed with the toast (a fresh first render, so no blank).
    if (fromModal) { sendFragment(ctx, html`${undoToast(token, a.undoSeconds)}${CLOSE_MODAL_OOB}`, FLOAT_UNDO); return true; }
    await landOnFeed(ctx, s, undoToast(token, a.undoSeconds));
    return true;
}
