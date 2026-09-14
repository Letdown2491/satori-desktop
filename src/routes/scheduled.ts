// Scheduled posts now live in the Drafts page (a "Scheduled" section atop /drafts). This module
// keeps only the cancel action; /scheduled itself just redirects there. Scheduling happens in the
// compose flow (routes/note.ts); the daemon's sweep broadcasts queued posts at their time.

import { getScheduledPost, cancelScheduled, type ScheduledPost } from '../data/scheduled.ts';
import { saveDraft, newDraftId, type Draft } from '../drafts.ts';
import { KIND_ARTICLE } from '../nostr/nip23.ts';
import { requireLogin } from './common.ts';
import { redirect, type Ctx } from '../http.ts';

/** Rebuild an editable draft from a signed scheduled post (note or article - polls aren't schedulable).
 * Drops the fixed send-time but keeps the content, so cancelling a queued post reverts it to a draft
 * (the confirm promise). A note gets a fresh draft id; an article keeps its `d` slug so re-publishing
 * updates the same article. Returns null for an unknown kind (the post is just cancelled). */
function scheduledToDraft(p: ScheduledPost): Draft | null {
    const ev = p.signed;
    if (ev.kind === 1) {
        const cw = ev.tags.find((t) => t[0] === 'content-warning');
        return {
            type: 'note', id: newDraftId(), content: ev.content,
            imeta: ev.tags.filter((t) => t[0] === 'imeta'),
            cw: !!cw, cwReason: cw?.[1] ?? '', savedAt: Date.now(),
        };
    }
    if (ev.kind === KIND_ARTICLE) {
        const tag = (k: string): string => ev.tags.find((t) => t[0] === k)?.[1] ?? '';
        const id = tag('d') || newDraftId();
        return {
            type: 'article', id, identifier: id,
            title: tag('title'), summary: tag('summary'), image: tag('image'),
            topics: ev.tags.filter((t) => t[0] === 't' && t[1]).map((t) => t[1]).join(', '),
            body: ev.content, savedAt: Date.now(),
        };
    }
    return null;
}

/** GET /scheduled - merged into /drafts; redirect any old links/bookmarks there. */
export function getScheduled(ctx: Ctx): void {
    redirect(ctx, '/drafts');
}

/** POST /scheduled/cancel/:token - pull a queued post and revert it to a draft, then land on
 * /drafts (boosted form follows the 303 and swaps; full nav just navigates). */
export function postScheduledCancel(ctx: Ctx): void {
    const s = requireLogin(ctx);
    if (!s) return;
    const token = ctx.params.token ?? '';
    const p = getScheduledPost(s.me, token);
    if (p) {
        const d = scheduledToDraft(p);
        if (d) saveDraft(s.me, d);
        cancelScheduled(s.me, token);
    }
    redirect(ctx, '/drafts');
}
