// The timelines. Two tabs plus the promoted content-type timelines share one machinery:
//   following - your follows' notes, outbox-routed (their NIP-65 write relays).
//   followers - people who follow you, same outbox routing.
//   /timeline/<id> - your follows' events of ONE promoted content type (its kinds), same follows route. The
//     old hardcoded "longform" tab is now just the article timeline (/timeline/article, promoted by default).
// All paginate (?until cursor + infinite scroll); only Following carries the "new notes" poller / boundary.

import { buildFollowsRoute, buildFollowersRoute, fetchRoutedPage, fetchRelayPage } from '../data/feeds.ts';
import { getFavoriteRelays, toggleFavoriteRelay, normalizeRelayUrl, relayLabel } from '../data/relay-favorites.ts';
import { ensureRelayInfo, relayInfoCached } from '../data/relay-info.ts';
import { relayFeedBar, relayPicker, relayPickerBody, relayPickerPage, favStar } from '../render/relay-feed.ts';
import { html, join, type SafeHtml } from '../html.ts';
import { noteList, pagerSentinel, feedClearing, facesOOB } from '../render/note.ts';
import { ensureReplies, ensureArticleReplies, replierPubkeys } from '../replies.ts';
import { HEX64, displayTime } from '../nostr/tags.ts';
import { quoteEmpty } from '../render/svg.ts';
import { quote } from '../render/quotes.ts';
import { page, notesHome, feedSwitch } from '../render/layout.ts';
import { readAppearance } from '../theme.ts';
import { readReadState, advanceReadState } from '../read-state.ts';
import { requireLogin, ensureProfiles, notePubkeys, chromeFor } from './common.ts';
import { ensureLists, mutedPubkeys, PRIVATE_KINDS, actionKind } from '../actions.ts';
import { getFilters, compileFilters } from '../data/filters.ts';
import { cachedFeed, putCachedFeed } from '../data/feed-cache.ts';
import { foldPending, pendingNew } from '../data/feed-pending.ts';
import { recordFollowingLanding } from '../data/feed-metrics.ts';
import { ensureLikes } from '../likes.ts';
import { ensureEngaged, engageTarget } from '../engaged.ts';
import { ensureZaps } from '../zaps.ts';
import { sendPage, sendFragment, sendSignRequest, redirect, type Ctx } from '../http.ts';
import { readSignResult } from '../nip07.ts';
import type { Session } from '../session.ts';
import { myRelayUrls } from '../nostr/nip65.ts';
import { signsOnClient } from '../session.ts';
import { feedKinds, profileKinds, timelineKinds, timelineEntries, isContentType, CONTENT_TYPES } from '../data/content-prefs.ts';
import { prepareEvents } from '../manifest/registry.ts';
import type { FeedTab } from '../render/layout.ts';
import type { NostrEvent } from '../nostr/types.ts';

const PAGE = 30;
const TIMELINE_PAGE = 20;
// The new-notes indicators (both the off-feed dot and the on-feed mark) stay quiet
// until the user's newNotesThreshold (readAppearance) new notes have gathered - a
// calm nudge, not a live counter.
// Feed fetch-kinds come from the user's content-type prefs (data/content-prefs.ts: feedKinds / timelineKinds).

const TABS: FeedTab[] = ['following', 'followers'];

/** The private (NIP-44) list kinds still awaiting a nip07 decrypt this session.
 * Until decrypted they can't filter the feed (mutes) OR fill the bookmark glyph
 * (bookmarks) - the same cold-start gap for EVERY private list, not just mutes.
 * (Bunker decrypts server-side in ensureLists, so it's never pending.) */
export function pendingPrivateKinds(s: Session & { me: string }): number[] {
    if (!signsOnClient(s)) return []; // bunker decrypts server-side in ensureLists - never pending
    // Exclude kinds we've already dispatched a decrypt for this session: if the extension never answers
    // (no nip44 auto-approve), the list stays unattempted forever - this stops it re-firing every refresh.
    return [...PRIVATE_KINDS].filter((k) => !!s.lists.get(k)?.content && !s.privateTags.has(k) && !s.primerTried.has(k));
}

/** A one-shot, invisible primer that decrypts the pending private lists (nip07) on a
 * cold load, so mute/bookmark state is correct. Chains one decrypt per pending list,
 * then EITHER re-renders #feed (`tab`, the feed's in-place path) OR soft-reloads the
 * current page (`ret`, via H-Location - used on the profile/thread, which have no #feed
 * to swap). Reject a prompt and the page stays as-is (it already rendered; nothing strands). */
export function listPrimer(q: { tab?: FeedTab; ret?: string; boundary?: number }): SafeHtml {
    // Carry the Following boundary captured at THIS render so the primer's re-render uses the same one,
    // not a high-water that the clearing's mark-seen advanced in the meantime (which would empty the feed).
    const base = q.ret ? `ret=${encodeURIComponent(q.ret)}` : `tab=${q.tab ?? 'following'}`;
    const bnd = q.boundary != null && !q.ret ? `&bnd=${q.boundary}` : '';
    return html`<span id="list-primer" h-get="/notes/list-prime?${base}${bnd}" h-trigger="load" h-target="#list-primer" h-swap="none" h-push-url="false" aria-hidden="true"></span>`;
}

/** The ?ret= soft-reload target - a LOCAL path only (no open redirect), else ''. */
function retParam(ctx: Ctx): string {
    const r = ctx.query.get('ret') ?? '';
    return /^\/[^/]/.test(r) ? r : '';
}

/** Resolve the ?tab= query to a known feed tab (defaults to following). */
function tabParam(ctx: Ctx): FeedTab {
    const t = ctx.query.get('tab');
    return t && (TABS as string[]).includes(t) ? (t as FeedTab) : 'following';
}

/** A valid private-list kind from ?kind=, else null. */
function privKindParam(ctx: Ctx): number | null {
    const n = Number(ctx.query.get('kind'));
    return PRIVATE_KINDS.has(n) ? n : null;
}

/** The Following boundary captured at the original render (?b=), so the primer's feed re-render is stable
 * against a mark-seen that advanced the high-water mid-load. null when absent (then recompute). */
function boundaryParam(ctx: Ctx): number | null {
    const n = Number(ctx.query.get('bnd'));
    return Number.isFinite(n) && n > 0 ? n : null;
}

const PATHS: Record<FeedTab, string> = { following: '/', followers: '/followers' };

// A feed SOURCE: one of the routed tabs, a single relay's timeline ("browse a relay"), OR a promoted content
// type's timeline (its kinds, from your follows). fillPage/buildFeed are source-agnostic; only the source-
// specific bits below (fetch, cache key, kinds, pagesize, sentinel) branch. A relay source is plain infinite
// scroll, kinds = the user's feed prefs, NOT cached (no TTL for its key → cachedFeed/putCachedFeed simply no-
// op), and shows the stranger-feed `mute` glyph. A type source is the old longform machinery generalized:
// same follows route, one content type's kinds, rich-content page size. No caught-up boundary - that's a
// Following-only concept.
export type FeedSource = { tab: FeedTab } | { relay: string } | { type: string };
const isRelay = (src: FeedSource): src is { relay: string } => 'relay' in src;
const isType = (src: FeedSource): src is { type: string } => 'type' in src;
const srcKey = (src: FeedSource): string => (isRelay(src) ? `relay:${src.relay}` : isType(src) ? `type:${src.type}` : src.tab);
const srcPageSize = (src: FeedSource): number => (isType(src) ? TIMELINE_PAGE : PAGE); // promoted-timeline page size (like the old longform feed)
// An explicit relay browse shows that relay's CONTENT across every renderable kind (profileKinds), not the
// narrow home-feed set - otherwise a long-form-only relay (articles/wikis, no kind:1) reads as empty. This
// also lets seen-relays learn addressable-event authors from a relay you visit (bootstrapping outbox reads).
// A type source queries exactly that content type's kinds.
const srcKinds = (src: FeedSource, me: string): number[] => (isRelay(src) ? profileKinds(me) : isType(src) ? timelineKinds(src.type) : feedKinds(me));
function srcSentinel(src: FeedSource, until: number): SafeHtml {
    if (isRelay(src)) return pagerSentinel(`/relay?r=${encodeURIComponent(src.relay)}&until=${until}`);
    if (isType(src)) return pagerSentinel(`/timeline/${src.type}?until=${until}`);
    return sentinel(src.tab, until);
}
async function srcFetch(s: Session & { me: string }, src: FeedSource, until?: number, limit?: number, budget?: 'page' | 'adaptive'): Promise<NostrEvent[]> {
    if (isRelay(src)) return fetchRelayPage(s.pool, src.relay, limit ?? PAGE, until, srcKinds(src, s.me)).catch(() => [] as NostrEvent[]);
    if (isType(src)) {
        // Same follows route as Following (same authors), just this content type's kinds - exactly what the
        // old longform tab did, now per promoted type.
        const route = await routeFor(s, 'following');
        return fetchRoutedPage(s.pool, route, limit ?? TIMELINE_PAGE, until, timelineKinds(src.type), undefined, budget).catch(() => [] as NostrEvent[]);
    }
    return fetchPage(s, src.tab, until, limit, budget);
}

/** The outbox route for a tab (cached on the session). */
async function routeFor(s: Session & { me: string }, tab: FeedTab): Promise<Map<string, Set<string>>> {
    if (tab === 'followers') {
        if (!s.followersRoute) s.followersRoute = await buildFollowersRoute(s.pool, s.me, s.myRelays!).catch(() => ({ authors: [], route: new Map() }));
        return s.followersRoute.route;
    }
    // following + longform share the follows route (same authors, different kinds)
    if (!s.followsRoute) s.followsRoute = await buildFollowsRoute(s.pool, s.me, s.myRelays!).catch(() => ({ authors: [], route: new Map() }));
    return s.followsRoute.route;
}

async function fetchPage(s: Session & { me: string }, tab: FeedTab, until?: number, limit?: number, budget?: 'page' | 'adaptive'): Promise<NostrEvent[]> {
    const route = await routeFor(s, tab);
    return fetchRoutedPage(s.pool, route, limit ?? PAGE, until, feedKinds(s.me), undefined, budget).catch(() => [] as NostrEvent[]);
}

/** Infinite-scroll sentinel for a tab (delegates to the shared pager sentinel). */
function sentinel(tab: FeedTab, until: number): SafeHtml {
    const sep = PATHS[tab].includes('?') ? '&' : '?';
    return pagerSentinel(`${PATHS[tab]}${sep}until=${until}`);
}

/** Build a feed page's content + the newest-note timestamp (for the bar's notes poller). */
const MAX_FILL = 4;   // cap loop-fill fetches so an aggressive filter can't fan out unbounded
const OVERFETCH = 3;  // when filtering, the first window pulls this many × target raw, so the
                      // common case fills the page in ONE round-trip instead of several sequential

/** Fetch until ~pageSize VISIBLE notes accumulate (muted authors + content filters can thin a
 * raw window well below target). When filtering, the first window over-fetches (OVERFETCH×) so
 * one round-trip usually suffices; MAX_FILL bounds the fallback loop. No extra cost when nothing
 * is filtered. We consume events one-by-one and stop at `target`, advancing the cursor to the
 * LAST CONSUMED event - so over-fetching never renders too many notes nor opens a pagination gap
 * (the un-consumed tail is simply re-read by the next window). Returns the visible notes, the raw
 * events consumed (for the list-primer's private-list check), a `more` sentinel, and the newest
 * raw timestamp (the notes-dot high-water). */
async function fillPage(s: Session & { me: string }, src: FeedSource, until?: number, extra?: NostrEvent[], budget?: 'page' | 'adaptive'): Promise<{ visible: NostrEvent[]; allRaw: NostrEvent[]; more: SafeHtml | null; newestRaw: number; recovered: number }> {
    // Load the lists CONCURRENTLY with the first page fetch, not serially before it: mute is only
    // needed once the page returns (filtering), bookmark/pin only at render. Awaited inside the loop
    // after the fetch (so it overlaps it) - saves ~1 round-trip on a cold paint (a full ~12s on Tor).
    const listsReady = ensureLists(s, ['mute', 'bookmark', 'pin']);
    const filt = compileFilters(getFilters(s.me), 'feed');
    let muted = new Set<string>(); // filled once listsReady resolves (first iteration), before any keep()
    const keep = (e: NostrEvent): boolean => !muted.has(e.pubkey) && !filt.hide(e);
    const target = srcPageSize(src);
    const overfetch = filt.active; // over-fetch a bigger first window only when a filter will thin it
    const visible: NostrEvent[] = [];
    const allRaw: NostrEvent[] = [];
    let cursor = until;
    let newestRaw = 0, exhausted = false;
    for (let i = 0; i < MAX_FILL && visible.length < target; i++) {
        const lim = i === 0 && overfetch ? target * OVERFETCH : target;
        // Landing window (until=undefined): serve the brief cache to skip the relay query on a
        // re-visit; cache only on a miss (so the TTL stays bounded, not refreshed on every visit).
        let page: NostrEvent[];
        if (i === 0 && until === undefined) {
            const cached = cachedFeed(s.me, srcKey(src));
            if (cached) page = cached;
            // Cache only a NON-EMPTY result: a transient empty (e.g. a relay returning nothing on a cold
            // pooled connection) must not get cached, or it'd blank the tab for the whole TTL. An empty
            // page just falls through to a fresh fetch on the next visit.
            else { page = await srcFetch(s, src, cursor, lim, budget); if (page.length) putCachedFeed(s.me, srcKey(src), page); }
        } else {
            page = await srcFetch(s, src, cursor, lim, budget);
        }
        if (i === 0) { await listsReady; muted = mutedPubkeys(s); } // lists overlapped the fetch above; ready before any keep()
        if (!page.length) { exhausted = true; break; }
        if (!newestRaw) newestRaw = page[0]!.created_at; // newest raw of the first window
        for (const e of page) {
            allRaw.push(e);
            cursor = e.created_at - 1; // advance per-event so a mid-window stop leaves no gap
            if (keep(e)) { visible.push(e); if (visible.length >= target) break; }
        }
        if (page.length < lim) { exhausted = true; break; } // short window → end of the feed
    }
    // Fold in `extra` (the background backfill buffer for the following landing): late-arriving events the
    // fast paint's relays didn't deliver in time. Merge post-loop so they ride the SAME keep()/enrich path -
    // dedup against what we already have, filter (mute/content), then re-sort newest-first. This is what makes
    // the new-notes dot and the load consistent: the dot counted these, and here they actually render.
    let recovered = 0; // notes the tight first-paint fetch missed that the backfill buffer (`extra`) recovers - see feed-metrics.ts
    if (extra?.length) {
        const have = new Set(allRaw.map((e) => e.id));
        for (const e of extra) {
            if (have.has(e.id) || !keep(e)) continue;
            allRaw.push(e); visible.push(e); recovered++;
        }
        if (visible.length) newestRaw = Math.max(newestRaw, ...visible.map((e) => e.created_at)); // high-water covers folded events (order-independent)
    }
    // Enrich only what we render (not the filtered-out raw). prepareEvents fans the reply-presence
    // prefetch out per kind (notes warm by id, articles by naddr) and hydrates the replier avatars,
    // so this stays free of `kind ===` branching.
    await Promise.all([ensureProfiles(s, [s.me, ...notePubkeys(visible)]), ensureLikes(s, visible.map((e) => e.id)), ensureEngaged(s, visible.map(engageTarget)), ensureZaps(s), prepareEvents(visible, s)]);
    const more = !exhausted ? srcSentinel(src, cursor!) : null;
    // Order the RENDERED page by DISPLAY time (published_at for long-form, else created_at). Pagination stays
    // created_at-based (the cursor/`more` above are computed from the raw fetch order and untouched here); only
    // the visible order flips, so a re-edited old article sits at its publish date instead of pinning to the
    // top of the feed. A no-op for notes, which carry no published_at.
    visible.sort((a, b) => displayTime(b) - displayTime(a));
    return { visible, allRaw, more, newestRaw, recovered };
}

async function buildFeed(s: Session & { me: string }, src: FeedSource, until?: number, emptyLine?: string): Promise<{ content: SafeHtml; newestTs: number | undefined; events: NostrEvent[] }> {
    const { visible, allRaw, more, newestRaw } = await fillPage(s, src, until);
    // nip07: kick off the private-list decrypt as the feed lands, then it re-renders. Tab feeds only - the
    // primer re-renders a TAB's #feed; a relay/type feed skips it (private mutes just apply once you visit a tab).
    const primer = !isRelay(src) && !isType(src) && pendingPrivateKinds(s).length ? listPrimer({ tab: src.tab }) : null;
    const body = visible.length === 0 && allRaw.length === 0
        ? quoteEmpty(emptyLine ?? quote('empty'))
        : html`${noteList(visible, s.profiles, s, { mute: isRelay(src), faces: true })}${more}`;
    return {
        content: html`<ul class="feed" id="feed">${body}</ul>${primer}`,
        newestTs: newestRaw || Math.floor(Date.now() / 1000),
        events: allRaw, // raw (pre-filter) so the list-primer can detect private-muted authors to hide
    };
}

/** Does the decrypted private mute/bookmark state actually change how THIS page of notes
 * renders? True iff a now-muted author or a now-bookmarked note id is on the page. Lets the
 * list-primer skip its #feed re-swap (the post-login "glitch") when nothing visible changes. */
function privateAffectsPage(s: Session, events: NostrEvent[]): boolean {
    const privMuted = new Set((s.privateTags.get(actionKind('mute')) ?? []).filter((t) => t[0] === 'p').map((t) => t[1]));
    const privMarked = new Set((s.privateTags.get(actionKind('bookmark')) ?? []).filter((t) => t[0] === 'e').map((t) => t[1]));
    return events.some((e) => privMuted.has(e.pubkey) || privMarked.has(e.id));
}

/** Shared handler for the feed tabs (and their infinite-scroll partials). */
async function serveFeed(ctx: Ctx, tab: FeedTab): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const untilParam = ctx.query.get('until');
    const until = untilParam && /^\d+$/.test(untilParam) ? Number(untilParam) : undefined;

    // Following carries the "caught up" boundary (the original Satori's design-for-exit): a feed that ends.
    if (tab === 'following') { await serveFollowing(ctx, s, until); return; }

    // Infinite-scroll partial: just the next notes + a fresh sentinel. Loop-fill keeps each
    // scroll increment ~pageSize even when filters/mutes thin the raw windows.
    if (ctx.isPartial && ctx.hTarget === '#more') {
        const { visible, more } = await fillPage(s, { tab }, until);
        sendFragment(ctx, html`${noteList(visible, s.profiles, s, { faces: true })}${more}`);
        return;
    }

    const { content, newestTs } = await buildFeed(s, { tab }, until);
    if (until === undefined && newestTs) markFeedSeen(ctx, s, tab, newestTs); // viewing the feed = seen
    sendPage(ctx, content, chromeFor(ctx, s, { active: 'feed', feedTab: tab, notesSince: newestTs }));
}

// The "new since you left" set looks back at most this far - a long absence stays bounded + calm,
// the rest waits behind "see earlier". (The original Satori had this; per-author cap dropped per user.)
const LOOKBACK_S = 7 * 24 * 60 * 60;
const FIRST_VISIT_S = 24 * 60 * 60; // no high-water yet → a gentle last-24h window, not a 7-day wall

/** The boundary = your last-visit high-water, floored to the look-back so a long absence stays bounded;
 * a first-ever visit (no high-water) gets a calm 24h window. Notes newer than this are the "new since
 * you left" set; older ones are already-read history behind the clearing. */
function followingBoundary(ctx: Ctx, me: string): number {
    const seen = readReadState(ctx, me).feed;
    const now = Math.floor(Date.now() / 1000);
    return seen > 0 ? Math.max(seen, now - LOOKBACK_S) : now - FIRST_VISIT_S;
}

/** The Following LANDING view: the new-since-your-last-visit set, capped to ONE window (never auto-paged
 * past it), then a clearing. Reaching the clearing marks you caught up (its intersect); "View older posts"
 * loads the next batch on a deliberate tap. Shared by the route and the nip07 list-primer re-render. */
async function followingFirstView(s: Session & { me: string }, boundary: number): Promise<{ inner: SafeHtml; newestTs: number; events: NostrEvent[] }> {
    // Fold in the background-backfill buffer (slow-relay events the dot found since the fast paint) so the
    // landing SHOWS exactly what the dot COUNTED - fillPage's keep() applies mute/content filters to them.
    const { visible, allRaw, more, newestRaw, recovered } = await fillPage(s, { tab: 'following' }, undefined, pendingNew(s.me, 'following', boundary), 'page');
    const newVisible = visible.filter((e) => e.created_at > boundary);
    // Mark everything we're showing as `shown` so the next dot poll (which re-fetches with the adaptive budget)
    // doesn't recount on-screen notes as "new".
    foldPending(s.me, 'following', newVisible, true);
    recordFollowingLanding(newVisible.length, recovered); // measure how much the slow-relay backfill actually rescued
    const reached = visible.some((e) => e.created_at <= boundary) || more === null; // saw all the new in one window
    const moreExists = more !== null || visible.length > newVisible.length; // more pages, or older notes held in this window
    // Continue from the OLDEST created_at shown (relays page by created_at). visible is display-sorted now,
    // so take the min explicitly rather than the last element.
    const contFrom = (newVisible.length ? Math.min(...newVisible.map((e) => e.created_at)) : newestRaw) - 1;
    const clearing = feedClearing({ caughtUp: reached, markTs: newestRaw || undefined, more: moreExists && contFrom > 0 ? contFrom : undefined });
    return { inner: html`${noteList(newVisible, s.profiles, s, { faces: true })}${clearing}`, newestTs: newestRaw, events: allRaw };
}

/** GET / (Following) with the original Satori "caught up" boundary - now in BATCHES (model B): one window
 * + a clearing, and each "View older posts" (a deliberate, click-only tap) loads exactly one more batch
 * that also ends in a clearing. The scroll never runs away; you choose each step. Mark-seen happens on
 * CATCH-UP (the first clearing's intersect → /feed/seen), never on load, so "new since you left" stays true. */
async function serveFollowing(ctx: Ctx, s: Session & { me: string }, until?: number): Promise<void> {
    const batch = ctx.query.get('b') === '1';
    const inPageSwap = ctx.isPartial && ctx.hTarget === '#feed-clearing';
    const boundary = followingBoundary(ctx, s.me); // captured ONCE; threaded to the primer so its re-render matches
    const wrapPage = (frag: SafeHtml, newestTs?: number): void => {
        if (inPageSwap) { sendFragment(ctx, frag); return; }
        const primer = pendingPrivateKinds(s).length ? listPrimer({ tab: 'following', boundary }) : null;
        sendPage(ctx, html`<ul class="feed" id="feed">${frag}</ul>${primer ?? html``}`, chromeFor(ctx, s, { active: 'feed', feedTab: 'following', notesSince: newestTs }));
    };

    // CONTINUE: a deliberate next batch (one window + its own clearing + the next "Continue"). No mark-seen
    // (the first clearing already did); no new/old split (you've chosen to keep reading older notes).
    if (batch) {
        const { visible, more } = await fillPage(s, { tab: 'following' }, until);
        const oldest = visible.length ? Math.min(...visible.map((e) => e.created_at)) : undefined; // oldest created_at shown (visible is display-sorted)
        wrapPage(html`${noteList(visible, s.profiles, s, { faces: true })}${feedClearing({ caughtUp: false, more: more !== null && oldest ? oldest - 1 : undefined })}`);
        return;
    }

    // FIRST view (the new-since-your-last-visit window).
    const { inner, newestTs } = await followingFirstView(s, boundary);
    wrapPage(inner, newestTs);
}

/** GET /feed/seen?ts= - the caught-up clearing's intersect target: reaching the end of the new set
 * advances your last-visit high-water AND clears the new-notes dot in the chrome right away (an OOB
 * swap of #notes-home), instead of waiting for its next 60s poll. The clearing itself is untouched
 * (its own h-swap="none"); the OOB element is the only thing that lands. */
export async function getFeedSeen(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const ts = Number(ctx.query.get('ts'));
    if (ctx.isPartial && Number.isFinite(ts) && ts > 0) {
        markFeedSeen(ctx, s, 'following', ts); // sets the cookie - MUST run before the response head below
        sendFragment(ctx, notesHome(false, false, true)); // OOB-clear the dot (re-arms the poller, no `load`)
        return;
    }
    ctx.res.writeHead(204);
    ctx.res.end();
}

/** The new-notes dot tracks the FOLLOWING timeline (your "home"); viewing/loading it
 * advances the high-water in the client cookie (read-state.ts; monotonic) so a reload/
 * restart doesn't relight the dot. */
function markFeedSeen(ctx: Ctx, s: Session & { me: string }, tab: FeedTab, ts: number): void {
    if (tab !== 'following' || ts <= 0) return;
    advanceReadState(ctx, s.me, { feed: ts });
}

/** GET /notes/dot - the ambient new-notes poller (off the feed): light the dot when
 * the Following timeline has notes newer than your seen high-water. */
export async function getNotesDot(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    if (!ctx.isPartial) { redirect(ctx, '/'); return; }
    const seen = readReadState(ctx, s.me).feed;
    const threshold = readAppearance(ctx).newNotesThreshold;
    const muted = mutedPubkeys(s);
    const filt = compileFilters(getFilters(s.me), 'feed');
    // Respect the same calm-nudge threshold as the on-feed mark: the ambient dot lights only
    // once enough new notes have gathered. Query TIGHTLY - only events newer than `seen`, capped
    // near the threshold (+slack for muted) - instead of pulling a full timeline page every 60s.
    // Content-filtered notes don't count toward the dot (they won't show in the feed anyway).
    const route = await routeFor(s, 'following');
    // Background poll: latency is invisible off-feed, so fetch on the GENEROUS adaptive budget - it catches
    // the slow / connect-laggard relays the fast landing paint can't wait for - and fold the result into the
    // pending buffer. Counting the BUFFER (not just this fetch) means the dot reflects what the landing will
    // actually render (fillPage folds the same buffer), so the count can't promise notes the load then misses.
    const events = await fetchRoutedPage(s.pool, route, threshold + 5, undefined, feedKinds(s.me), seen, 'adaptive').catch(() => [] as NostrEvent[]);
    foldPending(s.me, 'following', events, false);
    // Count against the SAME boundary the landing uses (not raw `seen`), so the dot counts exactly the set the
    // landing will surface as "new" - after a >7d absence `boundary` floors to 7d, and events older than that
    // are history below the clearing, not "new". Keeps the dot honest against the load in the edge case too.
    const boundary = followingBoundary(ctx, s.me);
    const count = pendingNew(s.me, 'following', boundary).filter((e) => !muted.has(e.pubkey) && !filt.hide(e)).length;
    sendFragment(ctx, notesHome(count >= threshold));
}

export const getFeed = (ctx: Ctx) => serveFeed(ctx, 'following');
export const getFollowers = (ctx: Ctx) => serveFeed(ctx, 'followers');
// Back-compat: the old /longform URL is now just the article timeline (promoted by default).
export const getLongform = (ctx: Ctx) => redirect(ctx, '/timeline/article');

/** GET /timeline/:id - a promoted content type's own timeline: that type's kinds (data/content-prefs.ts) from
 * the people you follow (the SAME outbox follows route as Following, just different kinds - the generalized
 * longform tab). Plain paginated infinite scroll through the shared fill/filter/enrich machinery; no caught-up
 * boundary, no mark-seen, no new-notes dot - those stay Following-only. An unknown content-type id falls back
 * to the home feed. A rare-kind timeline (e.g. Custom NIPs) is often empty; it shows a calm empty message. */
export async function getTimeline(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const id = ctx.params.id ?? '';
    if (!isContentType(id)) { redirect(ctx, '/'); return; }
    const src: FeedSource = { type: id };
    const untilParam = ctx.query.get('until');
    const until = untilParam && /^\d+$/.test(untilParam) ? Number(untilParam) : undefined;
    const label = CONTENT_TYPES.find((c) => c.id === id)?.label ?? 'posts';

    if (ctx.isPartial && ctx.hTarget === '#more') {
        const { visible, more } = await fillPage(s, src, until);
        sendFragment(ctx, html`${noteList(visible, s.profiles, s, { faces: true })}${more}`);
        return;
    }
    const { content, newestTs } = await buildFeed(s, src, until, `No ${label.toLowerCase()} from people you follow yet.`);
    sendPage(ctx, content, chromeFor(ctx, s, { active: 'feed', activeTimeline: id, notesSince: newestTs }));
}

/** GET /relay?r=<wss url> - browse a single relay's timeline (saved or ad-hoc). Plain infinite scroll
 * through the shared fill/filter/enrich machinery; no caught-up boundary, no mark-seen (the dot tracks
 * Following only). An invalid / disallowed relay url falls back to the home feed. */
export async function getRelay(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const url = normalizeRelayUrl(ctx.query.get('r') ?? '');
    if (!url) { redirect(ctx, '/'); return; }
    const src: FeedSource = { relay: url };
    const untilParam = ctx.query.get('until');
    const until = untilParam && /^\d+$/.test(untilParam) ? Number(untilParam) : undefined;
    // One favorites read: derive both the title's saved-name and the bar's star state (url is normalized).
    const fav = getFavoriteRelays(s.me).find((r) => r.url === url);

    if (ctx.isPartial && ctx.hTarget === '#more') {
        // Infinite-scroll partial: no header renders here, so never wait on the NIP-11 fetch.
        const { visible, more } = await fillPage(s, src, until);
        sendFragment(ctx, html`${noteList(visible, s.profiles, s, { mute: true, faces: true })}${more}`);
        return;
    }
    // NIP-11: fetch the relay's identity document (capped - a relay with no document must not
    // stall its own timeline). Your saved nickname outranks the relay's self-declared name.
    await ensureRelayInfo([url], 1500);
    const title = relayLabel(url, fav?.name ?? relayInfoCached(url)?.name);
    const { content } = await buildFeed(s, src, until);
    const body = html`${relayFeedBar(url, !!fav)}${content}`;
    sendPage(ctx, body, chromeFor(ctx, s, { active: 'feed', title, relayLabel: title }));
}

/** GET /faces?keys=<note-id|naddr,...> - the lazy post-paint reply-faces hydrate. A feed/profile page paints
 * with empty face slots (presence is best-effort + usually cold off Following); this fires once on load,
 * warms presence for the page's COLD keys (blocking, even on Tor - paint already happened), and returns an
 * OOB faces swap for each key that has repliers. Partial-only; a direct hit is a 204 no-op. */
export async function getFaces(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    if (!ctx.isPartial) { ctx.res.writeHead(204); ctx.res.end(); return; }
    const keys = (ctx.query.get('keys') ?? '').split(',').map((k) => k.trim()).filter(Boolean).slice(0, 100);
    const noteIds = keys.filter((k) => HEX64.test(k));
    const naddrs = keys.filter((k) => k.startsWith('naddr'));
    await Promise.all([ensureReplies(s, noteIds, 'lazy'), ensureArticleReplies(s, naddrs, 'lazy')]);
    await ensureProfiles(s, replierPubkeys(keys)).catch(() => {});
    const els = keys.map((k) => facesOOB(k, s)).filter((x): x is SafeHtml => x !== null);
    sendFragment(ctx, els.length ? join(els) : html``);
}

/** GET /relay/pick - the relay picker (the switcher's "Browse a relay…"): type any relay URL to browse, or
 * pick a favorite / one of your own relays. helmjs path = a modal fragment + an OOB that closes the switcher
 * <details> (a partial swap leaves the native dropdown open behind the modal otherwise); direct nav / zero-JS
 * = the same picker as a full page. Everything inline - no settings. */
export async function getRelayPick(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const mine = myRelayUrls(s.myRelays);
    const favs = getFavoriteRelays(s.me);
    // NIP-11 names for the picker rows, warmed in the background - the modal opens instantly
    // and unnamed rows pick up their relay's name on the next open.
    void ensureRelayInfo([...mine, ...favs.map((f) => f.url)]);
    if (ctx.isPartial) {
        // Rebuild the switcher CLOSED (OOB) for whichever header opened the picker: a non-timeline page
        // carries ?title (+ ?tc); a timeline carries ?tab (+ ?rl). Reconstruct the matching one, keeping the
        // promoted-timeline entries so canceling the picker doesn't strip them from the menu.
        const timelines = timelineEntries(s.me);
        const title = ctx.query.get('title');
        const sw = title !== null
            ? feedSwitch({ title, titleCount: Number(ctx.query.get('tc')) || undefined, timelines, oob: true })
            : feedSwitch({ active: tabParam(ctx), relayLabel: ctx.query.get('rl') || undefined, timelines, oob: true });
        sendFragment(ctx, html`${relayPicker(favs, mine)}${sw}`);
    } else {
        sendPage(ctx, relayPickerPage(favs, mine), chromeFor(ctx, s, { active: 'feed', title: 'Browse a relay' }));
    }
}

/** POST /relay/favorite?r=<url>[&from=pick] - toggle a relay favorite (the star). Re-renders the picker
 * list (from=pick) or the relay-bar star (default). */
export async function postRelayFavorite(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const url = normalizeRelayUrl(ctx.query.get('r') ?? '');
    if (!url) { ctx.res.writeHead(204); ctx.res.end(); return; }
    const fav = toggleFavoriteRelay(s.me, url);
    if (ctx.query.get('from') === 'pick') {
        sendFragment(ctx, relayPickerBody(getFavoriteRelays(s.me), myRelayUrls(s.myRelays)));
    } else {
        sendFragment(ctx, favStar(url, fav));
    }
}

/** The full Following-feed document - used by sign-in / post-note continuations. Renders the BATCHED
 * "caught up" boundary view (followingFirstView), the same as the GET / landing - NOT buildFeed's infinite
 * scroll, which would flatten the boundary (a sign-in / post-note must not turn the feed back into an
 * endless reverse-chron stream). `extra` (e.g. the undo toast) is placed in the body so a body-swap keeps it. */
export async function feedDocument(ctx: Ctx, s: Session & { me: string }, extra?: SafeHtml): Promise<SafeHtml> {
    const boundary = followingBoundary(ctx, s.me); // captured once; threaded so the primer re-render matches this view
    const { inner, newestTs } = await followingFirstView(s, boundary);
    const primer = pendingPrivateKinds(s).length ? listPrimer({ tab: 'following', boundary }) : null;
    return page(html`<ul class="feed" id="feed">${inner}</ul>${extra ?? html``}${primer ?? html``}`, chromeFor(ctx, s, { active: 'feed', feedTab: 'following', notesSince: newestTs }));
}

/** GET /notes/list-prime - kick off the private-list decrypt chain (nip07). Returns
 * a nip44_decrypt request for the first pending list; 204 if nothing's pending. */
export async function getListPrime(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    if (!ctx.isPartial) { redirect(ctx, '/'); return; }
    await ensureLists(s, ['mute', 'bookmark']);
    const [kind] = pendingPrivateKinds(s);
    const content = kind != null ? s.lists.get(kind)?.content : undefined;
    if (kind == null || !content) { ctx.res.writeHead(204); ctx.res.end(); return; }
    s.primerTried.add(kind); // mark dispatched: a decrypt that never returns won't re-fire on the next load
    const bnd = boundaryParam(ctx);
    const carry = (retParam(ctx) ? `ret=${encodeURIComponent(retParam(ctx))}` : `tab=${tabParam(ctx)}`) + (bnd != null ? `&bnd=${bnd}` : '');
    sendSignRequest(ctx, { pubkey: s.me, ciphertext: content }, `/notes/list-primed?${carry}&kind=${kind}`, 'nip44_decrypt');
}

/** POST /notes/list-primed - cache one decrypted list, then either chain to the next
 * pending list or, when none remain, re-render #feed (now filtered + glyphs correct).
 * A failed/rejected decrypt caches null so the chain won't retry that list. */
export async function postListPrimed(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    await ensureLists(s, ['mute', 'bookmark']);
    const kind = privKindParam(ctx);
    if (kind != null) {
        let priv: string[][] | null = null;
        try { const j = JSON.parse(String(await readSignResult(ctx.req))); if (Array.isArray(j)) priv = j as string[][]; } catch { /* unreadable */ }
        s.privateTags.set(kind, priv);
    }
    const tab = tabParam(ctx);
    const ret = retParam(ctx);
    const bnd = boundaryParam(ctx);
    const carry = (ret ? `ret=${encodeURIComponent(ret)}` : `tab=${tab}`) + (bnd != null ? `&bnd=${bnd}` : '');
    const [next] = pendingPrivateKinds(s);
    const nextContent = next != null ? s.lists.get(next)?.content : undefined;
    if (next != null && nextContent) {
        s.primerTried.add(next); // same dispatched-once guard as getListPrime
        sendSignRequest(ctx, { pubkey: s.me, ciphertext: nextContent }, `/notes/list-primed?${carry}&kind=${next}`, 'nip44_decrypt');
        return;
    }
    // Done. Profile/thread: soft-reload the current page (H-Location) so its mute/
    // bookmark state re-renders correctly. Feed: swap #feed in place.
    if (ret) { ctx.res.writeHead(200, { 'H-Location': ret }); ctx.res.end(); return; }
    // Following re-renders with the caught-up boundary CAPTURED AT THE ORIGINAL RENDER (`bnd`), not a fresh
    // followingBoundary - the clearing's mark-seen may have advanced the high-water by now, which would empty
    // the new set and flash the feed blank. Other tabs use the standard build. Both re-swap #feed only when the
    // decrypted lists actually change a visible note, so the feed doesn't flash for nothing.
    const { content, events } = tab === 'following'
        ? await followingFirstView(s, bnd ?? followingBoundary(ctx, s.me)).then((r) => ({ content: html`<ul class="feed" id="feed">${r.inner}</ul>`, events: r.events }))
        : await buildFeed(s, { tab });
    if (!privateAffectsPage(s, events)) { ctx.res.writeHead(204); ctx.res.end(); return; }
    sendFragment(ctx, content, { 'H-Reswap': 'outer', 'H-Retarget': '#feed' });
}
