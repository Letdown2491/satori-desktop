// Shared route helpers: a login guard and a profile-cache primer.

import { fetchProfiles } from '../data/profiles.ts';
import { fetchRelayLists } from '../data/relays.ts';
import { isReadDead } from '../data/relay-latency.ts';
import { getCachedProfile, isProfileStale, putProfile, inflightProfile, registerInflight } from '../data/profile-cache.ts';
import { INDEXER_RELAYS } from '../nostr/nip65.ts';
import { isLoggedIn, type Session } from '../session.ts';
import { redirect, type Ctx } from '../http.ts';
import { npub, displayName } from '../render/util.ts';
import { mentionPubkeys } from '../render/content.ts';
import { readAppearance } from '../theme.ts';
import { timelineEntries } from '../data/content-prefs.ts';
import type { ChromeOpts, Me, ActiveView, FeedTab } from '../render/layout.ts';

/** Build the chrome (Sumi-e bar + theme) for a logged-in view. */
/** The chrome's Me (avatar/name) from the session - shared by chromeFor and the
 * account-hub OOB swap (e.g. when a Privacy Mode toggle flips the avatar to a shield). */
export function meFor(s: Session & { me: string }): Me {
    return { npub: npub(s.me), hex: s.me, name: displayName(s.me, s.profiles), picture: s.profiles.get(s.me)?.picture };
}

export function chromeFor(
    ctx: Ctx, s: Session & { me: string },
    opts: { active?: ActiveView; title?: string; feedTab?: FeedTab; notesSince?: number; contentH1?: boolean; relayLabel?: string; titleCount?: number; activeTimeline?: string } = {},
): ChromeOpts {
    const a = readAppearance(ctx);
    // Every page's switcher lists the user's promoted timelines (so you can jump to one from anywhere).
    const timelines = timelineEntries(s.me);
    return { loggedIn: true, me: meFor(s), theme: a.theme, timelines, localAuthNeeded: s.pool.localAuthMissing(), ...opts };
}

/** Chrome for logged-out pages (sign-in) - no bar, but honor the theme cookie. */
export function guestChrome(ctx: Ctx, opts: { title?: string } = {}): ChromeOpts {
    const a = readAppearance(ctx);
    return { loggedIn: false, theme: a.theme, ...opts };
}

/** Response headers that make a sign-and-resubmit continuation land the client on
 * the feed via the re-enterable-swap seam (which doesn't follow redirect headers):
 * select the returned document's <body>, swap it into <body>, and push "/". */
export const LAND_ON_FEED = { 'H-Push-Url': '/', 'H-Retarget': 'body', 'H-Reselect': 'body', 'H-Reswap': 'inner' };

/** Resolve the session or bounce to /login. Returns the logged-in session or null
 * (after sending a redirect - the caller must stop). */
export function requireLogin(ctx: Ctx): (Session & { me: string }) | null {
    if (!isLoggedIn(ctx.session)) { redirect(ctx, '/login'); return null; }
    // Refresh the per-request appearance prefs from the cookie so renderers can read them off the
    // session (media autoload; whether the reactions/like button shows; reactions-in-notifications).
    const a = readAppearance(ctx);
    ctx.session.media = { autoLoad: a.autoLoadMedia, inlineVideo: a.inlineVideo };
    ctx.session.reactions = a.reactions;
    ctx.session.reactionNotifs = a.reactionNotifs;
    return ctx.session;
}

/** Fetch + cache kind:0 profiles for any pubkeys we don't have yet. Best-effort:
 * relays that fail just leave those authors showing a short npub. */
/** Author + in-content mention pubkeys for a batch of notes - pass to ensureProfiles
 * so @mentions resolve to names instead of @npub1… (Satori hydrates both). */
export function notePubkeys(events: { pubkey: string; content: string }[]): string[] {
    const out: string[] = [];
    for (const e of events) { out.push(e.pubkey); for (const pk of mentionPubkeys(e.content)) out.push(pk); }
    return out;
}

export async function ensureProfiles(s: Session, pubkeys: Iterable<string>): Promise<void> {
    const want = [...new Set(pubkeys)].filter((pk) => pk && !s.profiles.has(pk));
    if (want.length === 0) return;
    // Stale-while-revalidate: serve cached profiles instantly; only the genuinely
    // missing ones block on a relay query; stale ones refresh in the background.
    const missing: string[] = [];
    const stale: string[] = [];
    for (const pk of want) {
        const cached = getCachedProfile(pk);
        if (cached) { s.profiles.set(pk, cached); if (isProfileStale(pk)) stale.push(pk); }
        else missing.push(pk);
    }
    // Outbox: a profile (kind:0) lives on its OWNER's write relays, so resolve those for the pubkeys we're
    // about to fetch and prefer them (indexers + your inbox as fallback). The relay-list cache makes this
    // free for anyone already routed (your follows); strangers cost one batched kind:10002 lookup.
    const relaysFor = async (pks: string[]): Promise<string[]> => {
        const lists = pks.length ? await fetchRelayLists(s.pool, INDEXER_RELAYS, pks).catch(() => null) : null;
        // Read-dead relays (blasters in people's write lists) dropped: they refuse REQs, so they can
        // only slow the profile fetch down.
        const writes = lists ? [...new Set(pks.flatMap((pk) => lists.get(pk)?.write ?? []))].filter((u) => !isReadDead(u)) : [];
        return [...new Set([...writes, ...INDEXER_RELAYS, ...(s.myRelays?.read ?? [])])];
    };
    if (missing.length) {
        // Coalesce against concurrent renders: pubkeys already being fetched -> await that
        // shared promise; only the rest hit relays now (still one batched query), registered
        // so a parallel render reuses them. The shared promise owns putProfile; we read back.
        const already = missing.filter((pk) => inflightProfile(pk));
        const fresh = missing.filter((pk) => !inflightProfile(pk));
        if (fresh.length) {
            const p = relaysFor(fresh)
                .then((relays) => fetchProfiles(s.pool, relays, fresh))
                .then((m) => { for (const [pk, prof] of m) putProfile(pk, prof); })
                .catch(() => { /* relays failed; leave as npub */ });
            registerInflight(fresh, p);
            await p;
        }
        if (already.length) await Promise.all(already.map((pk) => inflightProfile(pk) ?? Promise.resolve()));
        for (const pk of missing) { const c = getCachedProfile(pk); if (c) s.profiles.set(pk, c); }
    }
    if (stale.length) {
        // Fire-and-forget refresh; update both the shared cache and this session so a later render in
        // the same session also sees the fresh data. Registered in the in-flight map like the missing
        // branch: a landing fires several near-simultaneous partials, and unregistered refreshes had
        // each re-querying the same stale set.
        const refresh = stale.filter((pk) => !inflightProfile(pk));
        if (refresh.length) {
            const p = relaysFor(refresh)
                .then((relays) => fetchProfiles(s.pool, relays, refresh))
                .then((m) => { for (const [pk, prof] of m) { putProfile(pk, prof); s.profiles.set(pk, prof); } })
                .catch(() => { /* keep serving the stale copy */ });
            registerInflight(refresh, p);
        }
    }
}
