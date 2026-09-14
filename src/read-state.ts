// Read position (the new-notes-dot + unread-bell high-waters) lives in a CLIENT cookie,
// not server session state - so every request carries it and is self-contained
// (stateless; the HATEOAS-correct home for session/application state). The cookie is a
// PER-PUBKEY map { pk: {f,n} }, so multiple accounts in the same browser each keep
// their own position; it's LRU-capped (most-recently-advanced accounts) so it can't
// grow unbounded. Mirrors the satori-appearance cookie + survives restarts client-side
// (replacing the old sessions.json persistence). High-waters only advance forward.

import { setCookie, type Ctx } from './http.ts';

const COOKIE = 'satori-read';
const MAX_AGE = 60 * 60 * 24 * 365; // ~1 year
const CAP = 10;                     // accounts kept (keeps the cookie well under 4KB)

export interface ReadState { feed: number; notif: number }
interface Entry { f: number; n: number } // feed, notif high-waters (short keys = smaller cookie)
type Stored = Record<string, Entry>;

function parse(ctx: Ctx): Stored {
    const raw = ctx.cookies[COOKIE];
    if (!raw) return {};
    try { const m = JSON.parse(raw); return m && typeof m === 'object' && !Array.isArray(m) ? m as Stored : {}; }
    catch { return {}; }
}

/** The read high-waters for `me` from the cookie (0/0 if this account has none yet). */
export function readReadState(ctx: Ctx, me: string): ReadState {
    const e = parse(ctx)[me];
    return { feed: Number(e?.f) || 0, notif: Number(e?.n) || 0 };
}

/** Advance one/both high-waters for `me` (monotonic) and Set-Cookie if anything changed.
 * MUST run before the response head is written (it sets a header). No-op if nothing
 * advances. The updated account moves to most-recent; over CAP, the oldest is dropped. */
export function advanceReadState(ctx: Ctx, me: string, next: Partial<ReadState>): void {
    const m = parse(ctx);
    const cur = m[me] ?? { f: 0, n: 0 };
    const f = Math.max(cur.f || 0, next.feed ?? 0);
    const n = Math.max(cur.n || 0, next.notif ?? 0);
    if (f === (cur.f || 0) && n === (cur.n || 0)) return; // nothing advanced
    delete m[me]; m[me] = { f, n }; // re-insert last = most-recently-advanced (LRU order)
    const keys = Object.keys(m);
    if (keys.length > CAP) for (const k of keys.slice(0, keys.length - CAP)) delete m[k];
    setCookie(ctx, COOKIE, JSON.stringify(m), { maxAge: MAX_AGE });
}
