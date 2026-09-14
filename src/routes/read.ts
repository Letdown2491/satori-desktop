// Read views: GET /u/<npub> (profile), /t/<nevent|note> (thread), /a/<naddr>
// (article reader). Flat bech32 routing, mirroring Satori's entity URLs.

import { decode, neventEncode, naddrEncode } from 'nostr-tools/nip19';
import { pubkeyFromBech, neventFromId } from '../nostr/nip19.ts';
import { fetchEvent, fetchAddressable, fetchReplies, fetchAuthorNotes } from '../data/feeds.ts';
import { fetchPinnedItems, fetchAuthorArticles } from '../data/profile-extras.ts';
import { INDEXER_RELAYS, myRelayUrls } from '../nostr/nip65.ts';
import { fetchRelayLists } from '../data/relays.ts';
import { seenRelaysFor } from '../data/seen-relays.ts';
import { coordinateOf, isAddressable, tag1, HEX64, displayTime } from '../nostr/tags.ts';
import { fetchArticleComments } from '../data/comments.ts';
import { commentSection } from '../render/comments.ts';
import { buildThreadTree, type TreeNode } from '../nostr/thread.ts';
import { getFilters, compileFilters } from '../data/filters.ts';
import type { NostrEvent } from '../nostr/types.ts';
import { html, join, type SafeHtml } from '../html.ts';
import { profileHeader, noteCard, noteList, naddrFor, pagerSentinel, embedFallback, pinnedStrip, articlesStrip } from '../render/note.ts';
import { renderEvent, prepareEvents, handlerFor } from '../manifest/registry.ts';
import { emptyItem, quoteEmpty, gapNotice } from '../render/svg.ts';
import { localIsolated } from '../local-relay.ts';
import { quote } from '../render/quotes.ts';
import { npub, type ProfileMap } from '../render/util.ts';
import { requireLogin, ensureProfiles, notePubkeys, chromeFor } from './common.ts';
import { ensureLists, mutedPubkeys } from '../actions.ts';
import { pendingPrivateKinds, listPrimer } from './feed.ts';
import { ensureLikes } from '../likes.ts';
import { ensureEngaged, engageTarget } from '../engaged.ts';
import { ensureZaps } from '../zaps.ts';
import { ensureArticleReplies, replierPubkeys } from '../replies.ts';
import { sendPage, sendFragment, notFound, redirect, hasBatchCaps, readBatchResults, sendSignRequest, type Ctx } from '../http.ts';
import { privateRepliesFor, syntheticReply, type PrivateReply } from '../data/dms.ts';
import { privateRepliesForNip07, beginSync, applySeals, applyRumors, finalizeSync } from '../data/dms-nip07.ts';
import { signsOnClient, type Session } from '../session.ts';
import { kindLabel } from '../nostr/nip89.ts';
import { profileKinds } from '../data/content-prefs.ts';

const PAGE = 30;
const PROFILE_FILL = 4; // cap loop-fill fetches on a profile (mirrors the feed's MAX_FILL)
const MAX_DEPTH = 4;
function untilOf(ctx: Ctx): number | undefined {
    const u = ctx.query.get('until');
    return u && /^\d+$/.test(u) ? Number(u) : undefined;
}

// --- GET /u/<npub> ---------------------------------------------------------

export async function getProfile(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const entity = ctx.params.npub ?? '';
    const pubkey = pubkeyFromBech(entity);
    if (!pubkey) { notFound(ctx, 'Not a profile'); return; }
    // Canonicalize a bare-hex pubkey to its npub (mirrors the /t/ thread route), so the address bar
    // self-heals to the app's bech32 convention instead of keeping the raw hex.
    if (HEX64.test(entity)) { redirect(ctx, `/u/${npub(pubkey)}`); return; }

    const until = untilOf(ctx);
    const meta = Promise.all([ensureProfiles(s, [pubkey]), ensureLists(s, ['follow', 'mute', 'bookmark', 'pin'])]);
    // Loop-fill the profile too: CONTENT filters apply here (a content filter is global intent),
    // but NOT author mutes - visiting a profile is intentional. Fetch successive windows until
    // ~PAGE visible so filtering doesn't leave short pages (mirrors the feed's fillPage).
    const filt = compileFilters(getFilters(s.me), 'profile');
    const profKinds = profileKinds(s.me); // the viewer's per-kind visibility prefs for profiles
    const notes: NostrEvent[] = [];
    // When filtering, the first window over-fetches (the feed's fillPage idiom) so a thinned page
    // usually fills in ONE round-trip instead of 2-4 serial ones. Events are consumed one-by-one up
    // to PAGE, the cursor advancing to the LAST CONSUMED event - so over-fetching never renders an
    // oversized page nor opens a pagination gap (the unconsumed tail is re-read by the next window).
    const OVERFETCH = 3;
    let cursor = until, oldest: number | undefined, hasMore = false;
    for (let i = 0; i < PROFILE_FILL && notes.length < PAGE; i++) {
        const lim = i === 0 && filt.active ? PAGE * OVERFETCH : PAGE;
        const page = await fetchAuthorNotes(s.pool, pubkey, profKinds, lim, cursor).catch(() => [] as NostrEvent[]);
        if (!page.length) { hasMore = false; break; }
        hasMore = page.length >= lim; // a full raw window → likely more history below it
        for (let j = 0; j < page.length; j++) {
            const e = page[j]!;
            oldest = e.created_at;
            if (!filt.hide(e)) notes.push(e);
            if (notes.length >= PAGE) { if (j < page.length - 1) hasMore = true; break; }
        }
        cursor = (oldest ?? 0) - 1;
        if (page.length < lim) break;
    }
    await meta;
    // prepareEvents warms reply-presence + replier avatars per kind (notes by id), so the profile
    // feed needs no `kind === 1` branch here; poll/picture rows just don't warm, as before.
    await Promise.all([ensureProfiles(s, notePubkeys(notes)), ensureLikes(s, notes.map((n) => n.id)), ensureEngaged(s, notes.map((n) => n.id)), ensureZaps(s), prepareEvents(notes, s)]);

    const more = hasMore && oldest != null ? pagerSentinel(`/u/${entity}?until=${oldest - 1}`) : null;
    // Render by DISPLAY time (published_at for long-form, else created_at); pagination stays created_at-based
    // (oldest/`more` above), matching the feed. Keeps a re-edited old article at its publish date.
    notes.sort((a, b) => displayTime(b) - displayTime(a));

    // Infinite-scroll partial (helmjs): just the next notes + a fresh sentinel.
    if (ctx.isPartial && ctx.hTarget === '#more') {
        sendFragment(ctx, html`${noteList(notes, s.profiles, s, { faces: true })}${more}`);
        return;
    }

    const profile = s.profiles.get(pubkey);
    const list = notes.length === 0
        ? html`<ul class="feed">${localIsolated() ? gapNotice() : quoteEmpty(quote('empty'))}</ul>`
        : html`<ul class="feed">${noteList(notes, s.profiles, s, { faces: true })}${more}</ul>`;

    // Pinned (NIP-51) + articles strips load LAZILY (helmjs `load`) so the pin-list
    // + article queries don't block the profile page; zero-JS shows header + notes.
    const content = html`
      ${profileHeader(pubkey, profile, s.profiles, s, pubkey === s.me)}
      ${pendingPrivateKinds(s).length ? listPrimer({ ret: `/u/${entity}` }) : html``}
      <div id="profile-extras" h-get="/u/${entity}/extras" h-trigger="load" h-target="#profile-extras" h-swap="outer" h-push-url="false"></div>
      ${list}`;

    sendPage(ctx, content, chromeFor(ctx, s, { active: 'profile', title: 'Profile' })); // header/tab say "Profile" (the name is the page's own heading)
}

/** GET /u/<npub>/extras - the lazily-loaded Pinned + Articles strips (the helmjs
 * `load` of the #profile-extras placeholder). A full nav redirects to the profile. */
export async function getProfileExtras(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const entity = ctx.params.npub ?? '';
    if (!ctx.isPartial) { redirect(ctx, `/u/${entity}`); return; }
    const pubkey = pubkeyFromBech(entity);
    if (!pubkey) { sendFragment(ctx, html``); return; }

    const [pinned, articles] = await Promise.all([
        fetchPinnedItems(s.pool, pubkey).catch(() => ({ notes: [] as NostrEvent[], articles: [] as NostrEvent[] })),
        fetchAuthorArticles(s.pool, pubkey).catch(() => [] as NostrEvent[]),
    ]);
    await Promise.all([
        ensureProfiles(s, [...notePubkeys(pinned.notes), ...pinned.articles.map((a) => a.pubkey), ...articles.map((a) => a.pubkey)]),
        ensureLists(s, ['bookmark', 'pin']), // the pinned notes' action bars
        ensureLikes(s, pinned.notes.map((n) => n.id)),
        ensureEngaged(s, [...pinned.notes.map((n) => n.id), ...pinned.articles.map(engageTarget), ...articles.map(engageTarget)]),
        ensureZaps(s), // one sync covers the pinned notes AND the article strips
        ensureArticleReplies(s, [...pinned.articles, ...articles].map(naddrFor)), // reply faces on the article rows
    ]);
    await ensureProfiles(s, replierPubkeys([...pinned.articles, ...articles].map(naddrFor))); // real avatars for the faces
    // Outer-swap replaces the placeholder with the strips (no loader → no re-fetch).
    sendFragment(ctx, html`${pinnedStrip(pinned.notes, pinned.articles, s.profiles, s)}${articlesStrip(articles, s.profiles)}`);
}

// --- GET /t/<nevent|note> --------------------------------------------------

function continueLink(parent: NostrEvent, depth: number): SafeHtml {
    let bech = parent.id;
    try { bech = neventEncode({ id: parent.id, author: parent.pubkey }); } catch { /* raw id */ }
    return html`<li class="continue-thread depth-${Math.min(depth, MAX_DEPTH)}"><a href="/t/${bech}" h-scroll="top instant">continue this thread →</a></li>`;
}

function renderReplyTree(nodes: TreeNode[], depth: number, profiles: ProfileMap, s: Session, inThread: string, privateIds: Set<string>): SafeHtml {
    const out: SafeHtml[] = [];
    for (const node of nodes) {
        out.push(noteCard(node.event, profiles, s, { hideParent: true, depth, inThread, isPrivate: privateIds.has(node.event.id) }));
        if (node.children.length === 0) continue;
        if (depth >= MAX_DEPTH) out.push(continueLink(node.event, depth + 1));
        else out.push(renderReplyTree(node.children, depth + 1, profiles, s, inThread, privateIds));
    }
    return join(out);
}

/** Decrypted private replies (NIP-59 gift-wrapped kind:1) to `noteId`, from whichever DM engine the
 * signing family uses. Surfaces what's already in the DM cache - bunker's is disk-backed; nip07's is
 * in-memory and warmed asynchronously (see the warm-on-thread routes below). */
function privateRepliesTo(s: Session, noteId: string): PrivateReply[] {
    return signsOnClient(s) ? privateRepliesForNip07(noteId, s.me!) : privateRepliesFor(s, noteId);
}

export async function getThread(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const entity = ctx.params.id ?? '';
    let id: string;
    let relays: string[] = [];
    let author: string | undefined; // from the nevent, so fetchEvent can use the author's outbox relays
    try {
        const d = decode(entity);
        if (d.type === 'note') id = d.data;
        else if (d.type === 'nevent') { id = d.data.id; relays = d.data.relays ?? []; author = d.data.author; }
        else { notFound(ctx, 'Not a note'); return; }
    } catch {
        // A bare 64-hex event id (hand-typed, or copied from a client/njump that uses raw hex): canonicalize
        // to an nevent and redirect, so the address bar self-heals to the app's bech32 convention.
        const nev = neventFromId(entity);
        if (nev) { redirect(ctx, `/t/${nev}`); return; }
        notFound(ctx, 'Bad note id'); return;
    }

    // Reader-side of the outbox model: also query the viewer's OWN relays. An author-less nevent with a
    // stale relay hint (event not actually on the hinted relay, no author to resolve outbox from) would
    // otherwise only hit INDEXER_RELAYS - missing notes that live on a relay you're connected to.
    const hints = [...new Set([...relays, ...myRelayUrls(s.myRelays)])];
    const [focused, replies] = await Promise.all([
        fetchEvent(s.pool, id, hints, author),
        fetchReplies(s.pool, id, hints).catch(() => [] as NostrEvent[]),
    ]);

    if (!focused) {
        sendPage(ctx, html`<ul class="feed">${localIsolated() ? gapNotice() : emptyItem('Note not found.')}</ul>`, chromeFor(ctx, s, { title: 'Thread' }));
        return;
    }

    // Private replies (gift-wrapped) you've received to this note, folded in as synthetic events so they
    // nest and render like public replies, badged with a lock. Self-copies of ones you sent show too.
    // Bunker's decrypt cache is disk-backed + pre-warmed, so fold them in now. nip07's cache is in-memory
    // (nothing persists) and may be cold, so for it we render public-only here and WARM it asynchronously
    // (the #thread-warm trigger below) - the warm step appends the private replies once decrypted.
    const privates = signsOnClient(s) ? [] : privateRepliesTo(s, id).map(syntheticReply);
    const privateIds = new Set(privates.map((e) => e.id));
    const allReplies = [...replies, ...privates];

    await Promise.all([ensureProfiles(s, notePubkeys([focused, ...allReplies])), ensureLists(s, ['bookmark', 'pin', 'mute']), ensureLikes(s, [focused.id, ...replies.map((r) => r.id)]), ensureEngaged(s, [focused.id, ...replies.map((r) => r.id)]), ensureZaps(s)]);

    // Hide muted authors' replies (the focused note itself stays - you navigated to it).
    const muted = mutedPubkeys(s);
    // `inThread` (this thread's nevent) lets every reply button append back here.
    // The #thread <ul> is where an optimistic reply is appended (helmjs `append`).
    const tree = renderReplyTree(buildThreadTree(allReplies.filter((r) => !muted.has(r.pubkey)), id), 0, s.profiles, s, entity, privateIds);
    // nip07: an invisible load-trigger that decrypts gift-wraps and appends any private replies (once).
    const warm = signsOnClient(s) ? html`<div id="thread-warm" h-get="/t/${entity}/private" h-trigger="load" h-swap="none" h-push-url="false"></div>` : null;
    const content = html`<ul class="feed" id="thread">${renderEvent(focused, 'focused', { profiles: s.profiles, s, inThread: entity })}${tree}</ul>${warm}`;
    sendPage(ctx, content, chromeFor(ctx, s, { title: 'Thread' }));
}

// Append decrypted private replies into the open thread's #thread list (helmjs append override).
const APPEND_THREAD = { 'H-Retarget': '#thread', 'H-Reswap': 'append' };

/** Decode a thread entity (note/nevent) to its note id - shared by the warm-chain routes. */
function threadNoteId(entity: string): string | null {
    try { const d = decode(entity); if (d.type === 'note') return d.data; if (d.type === 'nevent') return d.data.id; } catch { /* */ }
    return null;
}

// After warming, skip a fresh inbox sync if one ran within this window (avoids a 500-wrap relay query on
// every thread open in a session; a new private reply still surfaces on the next thread open past the TTL).
const WARM_TTL_MS = 45_000;

/** Render this note's private replies (lock-badged) from the warm cache and append them into #thread.
 * Empty fragment when there are none (clears the trigger). */
async function appendPrivateCards(ctx: Ctx, s: Session & { me: string }, privates: PrivateReply[], entity: string): Promise<void> {
    if (!privates.length) { sendFragment(ctx, html``); return; }
    await ensureProfiles(s, privates.map((p) => p.from));
    const cards = privates.map((p) => noteCard(syntheticReply(p), s.profiles, s, { hideParent: true, depth: 0, inThread: entity, isPrivate: true }));
    sendFragment(ctx, join(cards), APPEND_THREAD);
}

/** Terminal of the nip07 thread-warm chain: the decrypt cache is now warm, so consume the chain, stamp
 * the warm time (for the recency gate), and append this note's private replies into #thread. */
async function appendThreadPrivates(ctx: Ctx, s: Session & { me: string }, chainId: string, noteId: string, entity: string): Promise<void> {
    finalizeSync(s, chainId); // consume the chain; mem is warm (we ignore the inbox aggregate it returns)
    s.lastDmWarm = Date.now();
    await appendPrivateCards(ctx, s, privateRepliesForNip07(noteId, s.me!), entity);
}

/** GET /t/:id/private - start the nip07 warm: decrypt recent gift-wraps so this note's private replies
 * land in the in-memory cache, then append them. Reuses the DM decrypt chain (no plaintext persisted). */
export async function getThreadPrivate(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const entity = ctx.params.id ?? '';
    const noteId = threadNoteId(entity);
    // Only nip07 needs warming (bunker already inlined). No batch caps => can't decrypt; skip silently.
    if (!noteId || !signsOnClient(s) || !hasBatchCaps(ctx)) { sendFragment(ctx, html``); return; }
    // Recently warmed? Skip the relay query + decrypt round-trips and just render from the warm cache.
    if (s.lastDmWarm && Date.now() - s.lastDmWarm < WARM_TTL_MS) {
        await appendPrivateCards(ctx, s as Session & { me: string }, privateRepliesForNip07(noteId, s.me!), entity);
        return;
    }
    const { chainId, items } = await beginSync(s, 'inbox');
    if (items.length === 0) { await appendThreadPrivates(ctx, s as Session & { me: string }, chainId, noteId, entity); return; }
    sendSignRequest(ctx, { items }, `/t/${entity}/private/seals?chain=${chainId}`, 'nip44_decrypt_batch');
}

/** POST /t/:id/private/seals - layer-1 results in; emit layer-2 (seal->rumor) or finalize+append. */
export async function postThreadPrivateSeals(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return; if (!signsOnClient(s)) { sendFragment(ctx, html``); return; }
    const entity = ctx.params.id ?? '';
    const noteId = threadNoteId(entity);
    const chainId = ctx.query.get('chain') ?? '';
    const results = await readBatchResults(ctx.req);
    const next = results ? await applySeals(s, chainId, results) : null;
    if (next) { sendSignRequest(ctx, { items: next.items }, `/t/${entity}/private/rumors?chain=${chainId}`, 'nip44_decrypt_batch'); return; }
    if (noteId) await appendThreadPrivates(ctx, s as Session & { me: string }, chainId, noteId, entity); else sendFragment(ctx, html``);
}

/** POST /t/:id/private/rumors - layer-2 results in; cache the rumors, then finalize+append. */
export async function postThreadPrivateRumors(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return; if (!signsOnClient(s)) { sendFragment(ctx, html``); return; }
    const entity = ctx.params.id ?? '';
    const noteId = threadNoteId(entity);
    const chainId = ctx.query.get('chain') ?? '';
    const results = await readBatchResults(ctx.req);
    if (results) applyRumors(s, chainId, results);
    if (noteId) await appendThreadPrivates(ctx, s as Session & { me: string }, chainId, noteId, entity); else sendFragment(ctx, html``);
}

// --- GET /a/<naddr> --------------------------------------------------------

export async function getArticle(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const entity = ctx.params.naddr ?? '';
    let kind: number, pubkey: string, identifier: string, relays: string[];
    try {
        const d = decode(entity);
        if (d.type !== 'naddr') { notFound(ctx, 'Not an article address'); return; }
        ({ kind, pubkey, identifier, relays } = { ...d.data, relays: d.data.relays ?? [] });
    } catch { notFound(ctx, 'Bad naddr'); return; }

    const writes = (await fetchRelayLists(s.pool, INDEXER_RELAYS, [pubkey]).catch(() => null))?.get(pubkey)?.write ?? [];
    const queryRelays = [...new Set([...relays, ...writes, ...seenRelaysFor(pubkey), ...myRelayUrls(s.myRelays), ...INDEXER_RELAYS])]; // author's write relays (outbox) + where we've seen them + your own, then indexers
    const ev = await s.pool.get(queryRelays, { kinds: [kind], authors: [pubkey], '#d': [identifier] }).catch(() => null);

    if (!ev) {
        sendPage(ctx, html`<ul class="feed">${emptyItem('Event not found.')}</ul>`, chromeFor(ctx, s, { title: 'Article' }));
        return;
    }
    // Not a long-form reader kind? Dispatch through the manifest rather than hardcoding "article not
    // found". /a/ is the addressable-event route; the markdown long-form kinds (article + custom NIP) earn
    // the rich reader + comments below. Any other recognized addressable kind renders its focused card; an
    // unknown one hits the honest fallback ("Satori doesn't render this yet · open in app") - never a
    // misleading not-found for a valid event we simply fetched.
    if (!handlerFor(ev.kind)?.reader) {
        await ensureProfiles(s, notePubkeys([ev]));
        sendPage(ctx, html`<ul class="feed">${renderEvent(ev, 'focused', { profiles: s.profiles, s })}</ul>`, chromeFor(ctx, s, { title: kindLabel(ev.kind) }));
        return;
    }

    const ra = coordinateOf(ev);
    // Hydrate the article's own author/mentions in parallel with fetching comments + their
    // authors, rather than serially - the comment round-trips were needlessly gating the body.
    // (Concurrent ensureProfiles is safe: it coalesces overlapping pubkeys in-flight.)
    const [, comments] = await Promise.all([
        Promise.all([ensureProfiles(s, notePubkeys([ev])), ensureLists(s, ['bookmark', 'pin']), ensureEngaged(s, [engageTarget(ev)]), ensureZaps(s)]), // author + body @mentions
        (async (): Promise<NostrEvent[]> => {
            const c = await fetchArticleComments(s.pool, ra, ev.pubkey, relays).catch(() => [] as NostrEvent[]);
            await ensureProfiles(s, notePubkeys(c)); // comment authors + their @mentions
            return c;
        })(),
    ]);
    // nip07 cold-decrypt of private lists (bookmark/mute) so the bookmark heart reflects saved
    // state - the article page was missing the primer that the feed/profile already have.
    const primer = pendingPrivateKinds(s).length ? listPrimer({ ret: `/a/${entity}` }) : html``;
    sendPage(ctx, html`${renderEvent(ev, 'reader', { profiles: s.profiles, s })}${commentSection(s, ra, ev.pubkey, comments, s.profiles)}${primer}`,
        chromeFor(ctx, s, { title: kindLabel(ev.kind), contentH1: true })); // the reader title is the page <h1> (kindLabel now covers Article)
}

/** GET /embed/<bech>?as=reply|quote|article - a compact preview lazily swapped
 * into an embed card (reply-context / quoted note / article). Falls back to a
 * labelled link when the target can't be loaded, so the card never sits empty. */
// A quoted/embedded preview is decorative: if the relays don't have it in 3s, show the fallback link
// rather than holding a connection for the full 6s. The result (hit or miss) is then cached by fetchEvent.
const EMBED_MAX_WAIT = 3000;

export async function getEmbed(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const entity = ctx.params.id ?? '';
    let decoded;
    try { decoded = decode(entity); } catch { /* unparseable below */ }
    const isAddr = decoded?.type === 'naddr';
    // Lazy-load fragment only; a full navigation goes to the underlying view: addressable → /a/, else /t/.
    if (!ctx.isPartial) { redirect(ctx, isAddr ? `/a/${entity}` : `/t/${entity}`); return; }
    if (!decoded) { sendFragment(ctx, embedFallback(`/t/${entity}`, '↗ link')); return; }

    // naddr → embed ANY addressable kind via its handler: the article reader-card, a calendar/video/
    // listing card, or the honest fallback. (Was hardcoded to article only - a parity gap for the other
    // addressable kinds, which have `ref` descriptors so they reference inline as embeds.)
    if (decoded.type === 'naddr') {
        const { kind, pubkey, identifier, relays } = { ...decoded.data, relays: decoded.data.relays ?? [] };
        const writes = (await fetchRelayLists(s.pool, INDEXER_RELAYS, [pubkey]).catch(() => null))?.get(pubkey)?.write ?? [];
        const queryRelays = [...new Set([...relays, ...writes, ...seenRelaysFor(pubkey), ...myRelayUrls(s.myRelays), ...INDEXER_RELAYS])]; // author's write relays (outbox) + where we've seen them + your own, then indexers
        // Cached + coalesced (fetchAddressable): a feed's naddr embeds re-fire on every revisit, and the
        // uncached direct get here was the naddr twin of the note-embed storm.
        const ev = await fetchAddressable(s.pool, kind, pubkey, identifier, queryRelays, { maxWait: EMBED_MAX_WAIT }).catch(() => null);
        if (!ev) { sendFragment(ctx, embedFallback(`/a/${entity}`, `↗ ${kindLabel(kind).toLowerCase()}`)); return; }
        await ensureProfiles(s, notePubkeys([ev]));
        sendFragment(ctx, renderEvent(ev, 'embed', { profiles: s.profiles, bech: entity, naddr: entity }));
        return;
    }

    // note / nevent → a note embed.
    const as = ctx.query.get('as') === 'quote' ? 'quote' : 'reply';
    const fb = () => embedFallback(`/t/${entity}`, as === 'quote' ? '↗ quoted note' : '↩ in reply to an earlier note');
    let id: string;
    let relays: string[] = [];
    let author: string | undefined;
    if (decoded.type === 'note') id = decoded.data;
    else if (decoded.type === 'nevent') { id = decoded.data.id; relays = decoded.data.relays ?? []; author = decoded.data.author; }
    else { sendFragment(ctx, fb()); return; }
    const hints = [...new Set([...relays, ...myRelayUrls(s.myRelays)])]; // + viewer's own relays (see getThread)
    const ev = await fetchEvent(s.pool, id, hints, author, { maxWait: EMBED_MAX_WAIT }).catch(() => null);
    if (!ev) { sendFragment(ctx, fb()); return; }
    // Hydrate the author AND any @mentioned pubkeys in the embed's content, so
    // in-content mentions resolve to @names instead of falling back to @npub.
    await ensureProfiles(s, notePubkeys([ev]));
    // An addressable event (article, custom NIP, ...) can be quoted by event-id (nevent), not just naddr.
    // Its handler renders the clean reference card (title + summary), NOT a raw-markdown note body, linking
    // via a freshly-encoded naddr (its addressable form). A plain note falls through to the note embed
    // (the registry fallback) - exactly the old article-vs-everything-else branch, now dispatched per kind.
    let naddr: string | undefined;
    if (isAddressable(ev.kind)) {
        const identifier = tag1(ev, 'd');
        naddr = naddrEncode({ kind: ev.kind, pubkey: ev.pubkey, identifier, relays });
    }
    sendFragment(ctx, renderEvent(ev, 'embed', { profiles: s.profiles, bech: entity, naddr, label: as === 'quote' ? '↗ quoted note' : '↩ in reply to' }));
}
