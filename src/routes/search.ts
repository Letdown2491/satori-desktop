// GET /search?q= - NIP-50 search over dedicated search relays (data/search.ts),
// rendering People (kind:0) + Notes (kind:1). The form is a boosted GET so it's a
// snappy partial nav with JS and a real navigation without.

import { pubkeyFromBech } from '../nostr/nip19.ts';
import { searchNotes, searchPeople, parseSearchQuery } from '../data/search.ts';
import { searchPage, searchResults } from '../render/search.ts';
import { requireLogin, ensureProfiles, notePubkeys, chromeFor } from './common.ts';
import { ensureLists, mutedPubkeys } from '../actions.ts';
import { ensureLikes } from '../likes.ts';
import { ensureEngaged, engageTarget } from '../engaged.ts';
import { ensureZaps } from '../zaps.ts';
import { readAppearance } from '../theme.ts';
import { sendPage, sendFragment, type Ctx } from '../http.ts';
import type { Session } from '../session.ts';
import type { Profile } from '../data/profiles.ts';

/** How well a profile matches the typed name: exact name/display_name (4) + nip05 local-part (2) +
 * verified (1). Used to pick the RIGHT "jack" out of many accounts named jack. */
function nameScore(p: Profile, q: string): number {
    const name = (p.name ?? '').toLowerCase(), disp = (p.display_name ?? '').toLowerCase();
    const local = (p.nip05 ?? '').toLowerCase().split('@')[0];
    return (name === q || disp === q ? 4 : 0) + (local === q ? 2 : 0) + (p.nip05Verified ? 1 : 0);
}

/** Resolve a by:/p: identifier to a pubkey: hex / npub / nprofile decode, else an exact-ish match in
 * the profile cache, else a NIP-50 people search RANKED by name match (so a bare NAME resolves to the
 * right person even when unfollowed). Null only when nothing matches at all. */
async function resolvePubkey(s: Session & { me: string }, relays: string[], raw: string): Promise<string | null> {
    const direct = pubkeyFromBech(raw); // hex / npub / nprofile
    if (direct) return direct;
    const q = raw.toLowerCase();
    let sub: string | null = null;
    for (const [pk, p] of s.profiles) {
        const name = (p.name ?? p.display_name ?? '').toLowerCase();
        const nip05 = (p.nip05 ?? '').toLowerCase();
        if (name === q || nip05 === q || nip05.split('@')[0] === q) return pk;
        if (!sub && name.includes(q)) sub = pk;
    }
    if (sub) return sub;
    // Network: fetch a candidate pool and pick the BEST match (a stable sort keeps the relays'
    // relevance order as the tiebreaker), so "jack" → the prominent jack, not a random one.
    const found = await searchPeople(s.pool, relays, raw, 20).catch(() => []);
    const best = found.map((r) => ({ r, score: nameScore(r.profile, q) })).sort((a, b) => b.score - a.score)[0]?.r;
    if (best) { s.profiles.set(best.pubkey, best.profile); return best.pubkey; }
    return null;
}

export async function getSearch(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const chrome = () => chromeFor(ctx, s, { active: 'search', title: 'Search' });
    const live = ctx.isPartial && ctx.hTarget === '#search-results'; // live-as-you-type input swap
    const q = (ctx.query.get('q') ?? '').trim();
    // Live + too-short query: swap to the seek state with NO relay fan-out (skip 0-1 char queries;
    // the 400ms input debounce already throttles). Full-nav empty query: the seek page.
    if (live && q.length < 2) { sendFragment(ctx, searchResults('', [], [], s)); return; }
    if (!q) { sendPage(ctx, searchPage('', [], [], s), chrome()); return; }

    // Parse operators (by:/p:/#tag/has:/site:/since:/until:); resolve by:/p: → pubkeys. People
    // search uses only the FREE text (operators are note-scoped); a pure-operator query skips it.
    const sq = parseSearchQuery(q);
    const a = readAppearance(ctx);
    // Independent of the search results: warm the lists in parallel with the fan-out instead of
    // serializing a cold session's list round-trip behind it (the await below joins these).
    void ensureLists(s, ['bookmark', 'pin', 'mute']).catch(() => {});
    const [authors, mentions] = await Promise.all([
        Promise.all(sq.by.map((r) => resolvePubkey(s, a.searchProfileRelays, r))).then((xs) => xs.filter((x): x is string => !!x)),
        Promise.all(sq.p.map((r) => resolvePubkey(s, a.searchProfileRelays, r))).then((xs) => xs.filter((x): x is string => !!x)),
    ]);
    const [people, notesRaw] = await Promise.all([
        sq.text ? searchPeople(s.pool, a.searchProfileRelays, sq.text) : Promise.resolve([] as { pubkey: string; profile: Profile }[]),
        searchNotes(s.pool, a.searchNoteRelays, sq, authors, mentions),
    ]);
    // Cache the searched profiles so displayName/avatar render for strangers.
    for (const { pubkey, profile } of people) s.profiles.set(pubkey, profile);

    // Mute list must be loaded before filtering (people are left unfiltered - you
    // may be searching to find/unmute someone; only the note feed hides mutes).
    await ensureLists(s, ['bookmark', 'pin', 'mute']);
    const muted = mutedPubkeys(s);
    const notes = notesRaw.filter((e) => !muted.has(e.pubkey));
    await Promise.all([
        ensureProfiles(s, notePubkeys(notes)),
        ensureLikes(s, notes.map((e) => e.id)),
        ensureEngaged(s, notes.map(engageTarget)),
        ensureZaps(s),
    ]);
    if (live) { sendFragment(ctx, searchResults(q, people, notes, s)); return; }
    sendPage(ctx, searchPage(q, people, notes, s), chrome());
}
