// GET /notifications - replies, mentions, zaps, and poll-votes addressed to you,
// newest first, paged by ?until (intersect-once infinite scroll, like the feed).
// Opening it marks everything seen (the high-water that drives the unread dot).

import { html, type SafeHtml } from '../html.ts';
import { fetchMyPollIds, fetchNotifications, parseZapReceipt, type Notif } from '../data/notifications.ts';
import { notifList, notifCaughtUp } from '../render/notifications.ts';
import { pagerSentinel } from '../render/note.ts';
import { notifBell } from '../render/layout.ts';
import { ensureLists, mutedPubkeys } from '../actions.ts';
import { ensureLikes } from '../likes.ts';
import { ensureEngaged } from '../engaged.ts';
import { ensureZaps } from '../zaps.ts';
import { requireLogin, ensureProfiles, notePubkeys, chromeFor } from './common.ts';
import { allPrivateReplies, syntheticReply, type PrivateReply } from '../data/dms.ts';
import { allPrivateRepliesNip07 } from '../data/dms-nip07.ts';
import { signsOnClient, type Session } from '../session.ts';
import { readReadState, advanceReadState } from '../read-state.ts';
import { sendPage, sendFragment, redirect, type Ctx } from '../http.ts';
import { coalesceOne } from '../data/coalesce.ts';

const PAGE = 30;

// Single-flight the kind:1068 poll-id lookup per session: the page open and the bell poller land
// near-simultaneously on a cold session, and caching only the RESOLVED array (s.myPollIds) let both
// fire the same relay query.
const pollIdsInflight = new Map<string, Promise<string[]>>();
function myPollIds(s: Session & { me: string }): Promise<string[]> {
    if (s.myPollIds) return Promise.resolve(s.myPollIds);
    return coalesceOne(pollIdsInflight, s.id, () =>
        fetchMyPollIds(s.pool, s.me, s.myRelays).then((ids) => (s.myPollIds = ids)).catch(() => [] as string[]));
}

/** Who acted: the sender for a zap, the event author otherwise (null = unknown). */
function actorOf(n: Notif): string | null {
    return n.type === 'zap' ? parseZapReceipt(n.event).sender : n.event.pubkey;
}

/** Private replies to your notes, from the local DM cache, within `win`. Not on relays, so they're
 * merged client-side; the same time bounds keep pagination consistent. */
function cachedPrivateReplies(s: Session & { me: string }, win: { since?: number; until?: number }): PrivateReply[] {
    const raw = signsOnClient(s) ? allPrivateRepliesNip07(s.me) : allPrivateReplies(s);
    return raw.filter((r) => (win.until === undefined || r.at < win.until) && (win.since === undefined || r.at > win.since));
}

/** Those private replies as Notif rows (synthetic kind:1 events) for merging into the notifications feed. */
function cachedPrivateReplyNotifs(s: Session & { me: string }, win: { since?: number; until?: number }): Notif[] {
    return cachedPrivateReplies(s, win).map((r) => ({ type: 'privateReply' as const, event: syntheticReply(r) }));
}

/** Pubkeys to resolve (the actors). */
function authorsOf(items: Notif[]): string[] {
    return items.map(actorOf).filter((p): p is string => !!p);
}

/** Filter muted actors + hydrate (profiles, lists, likes, engagement, zaps). */
async function prepareNotifs(s: Session & { me: string }, items: Notif[]): Promise<Notif[]> {
    await ensureLists(s, ['mute']); // need the mute set before filtering
    const muted = mutedPubkeys(s);
    const visible = items.filter((n) => { const a = actorOf(n); return !a || !muted.has(a); });
    const noteEvents = visible.filter((n) => n.type === 'reply' || n.type === 'mention').map((n) => n.event);
    const noteIds = noteEvents.map((e) => e.id);
    await Promise.all([ensureProfiles(s, [...authorsOf(visible), ...notePubkeys(noteEvents)]), ensureLists(s, ['bookmark', 'pin']), ensureLikes(s, noteIds), ensureEngaged(s, noteIds), ensureZaps(s)]);
    return visible;
}

export async function getNotifications(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    // Don't block on the poll-id lookup: hand fetchNotifications a promise so its tagged-stream query
    // starts immediately and the poll-id round-trip overlaps it (cached on the session once resolved).
    const pollIdsP = myPollIds(s);
    // The three list reads are independent of the fetched notifications: warm them in parallel with
    // the tagged-stream query instead of serializing behind it (prepareNotifs' awaits join these
    // in-flight fetches via ensureList's coalescing). Cold sessions drop from 3 round-trips to ~1.
    void ensureLists(s, ['mute', 'bookmark', 'pin']).catch(() => {});

    const untilParam = ctx.query.get('until');
    const until = untilParam && /^\d+$/.test(untilParam) ? Number(untilParam) : undefined;
    const seenMode = ctx.query.get('seen') === '1'; // "See older": the already-seen history below the boundary
    // Only an in-page swap (the #more pager or the #see-older control) gets a bare fragment; a boosted
    // navigation is also isPartial but needs the full page (chrome), so gate on the swap target.
    const inPageSwap = ctx.isPartial && (ctx.hTarget === '#more' || ctx.hTarget === '#notif-clearing');
    const wrap = (frag: SafeHtml): void => {
        if (inPageSwap) sendFragment(ctx, frag);
        else sendPage(ctx, html`<ul class="feed" id="feed">${frag}</ul>`, chromeFor(ctx, s, { active: 'notifications', title: 'Notifications' }));
    };

    const fetched = await fetchNotifications(s.pool, s.me, s.myRelays, pollIdsP, { until, limit: PAGE }, !!s.reactionNotifs);
    // Fold in private replies from the local DM cache (not on relays), re-sort, and re-slice to a page so
    // the `oldest` cursor and pager stay consistent across the merged stream.
    const items = [...fetched, ...cachedPrivateReplyNotifs(s, { until })].sort((a, b) => b.event.created_at - a.event.created_at).slice(0, PAGE);
    const visible = await prepareNotifs(s, items);
    // The pager cursor anchors on the OLDEST RAW item (not the muted-filtered `visible`), so
    // filtering never shortens the window and skips events. A full page → there may be more.
    const oldest = items.length ? items[items.length - 1]!.event.created_at : undefined;
    const fullPage = items.length >= PAGE;

    // SEEN history: one continuous, newest-first list paginated by `until`. No boundary logic.
    if (seenMode) {
        const more = fullPage && oldest ? pagerSentinel(`/notifications?seen=1&until=${String(oldest - 1)}`) : html``;
        // The tappable ensō (#notif-clearing) is swapped OUTER with this seen history, so the "caught up"
        // clearing is replaced in place - no separate OOB clear needed.
        wrap(html`${notifList(visible, s.profiles, s)}${more}`);
        return;
    }

    // NEW set: everything since your last visit. The boundary is your read high-water - carried
    // explicitly through pager URLs (`nb`) so it survives the high-water advancing on first load.
    const nbParam = ctx.query.get('nb');
    const boundary = nbParam && /^\d+$/.test(nbParam) ? Number(nbParam) : readReadState(ctx, s.me).notif;

    // First page only: opening marks everything seen (raw newest, so muted ones still advance the
    // high-water). MUST run before the response head (it sets a cookie). Captured `boundary` above.
    if (until === undefined && items.length) advanceReadState(ctx, s.me, { notif: items[0]!.event.created_at });

    const newVisible = visible.filter((n) => n.event.created_at > boundary);
    const reachedBoundary = items.some((n) => n.event.created_at <= boundary); // seen history begins in this page

    // Still all-new with more pages → keep paging the NEW set (carry the boundary forward). Otherwise
    // close it: the "caught up" clearing, plus "See older" when there's already-seen history to reveal.
    const tail = !reachedBoundary && fullPage && oldest
        ? pagerSentinel(`/notifications?until=${String(oldest - 1)}&nb=${String(boundary)}`)
        : notifCaughtUp(reachedBoundary ? boundary : undefined);
    wrap(html`${notifList(newVisible, s.profiles, s)}${tail}`);
}

/** GET /notifications/unread - the bell poller: lights the dot when anything is
 * newer than your seen high-water. A full navigation here just goes to the page. */
export async function getNotifUnread(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    if (!ctx.isPartial) { redirect(ctx, '/notifications'); return; }
    const pollIdsP = myPollIds(s);
    const [items] = await Promise.all([
        fetchNotifications(s.pool, s.me, s.myRelays, pollIdsP, { since: readReadState(ctx, s.me).notif + 1, limit: 10 }, !!s.reactionNotifs),
        ensureLists(s, ['mute']),
    ]);
    const muted = mutedPubkeys(s);
    const since = readReadState(ctx, s.me).notif;
    // A relay notification OR an unmuted private reply newer than the high-water lights the dot. The
    // private-reply check skips building synthetic events - the poll only needs existence, not the rows.
    const hasUnread = items.some((n) => { const a = actorOf(n); return !a || !muted.has(a); })
        || cachedPrivateReplies(s, { since }).some((r) => !muted.has(r.from));
    sendFragment(ctx, notifBell(hasUnread));
}
