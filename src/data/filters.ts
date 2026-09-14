// Server-side feed content filters (beyond NIP-51 per-pubkey mutes): hide notes whose
// content matches a keyword/regex, or that are replies / quote-posts / link-only. Applied
// as the feed renders (alongside muted-author filtering), so the noise never reaches the
// browser. LOCAL to this daemon (not a nostr standard, never published), per-pubkey on disk.
//
// Patterns: one per line. Plain text = case-insensitive substring match anywhere in the
// content. Wrapped in /slashes/ = a regex (optional trailing flags; case-insensitive by
// default, `g` stripped). ReDoS guard: the user authors the pattern but an ATTACKER authors
// the input (note content), so a careless catastrophic regex + a crafted note could hang the
// feed render. Without a linear-time engine (RE2 = the documented future hardening) we (a)
// refuse dangerous nested-quantifier shapes at compile time and (b) cap the matched content
// length. Compile-once + try/catch + count/length caps still apply.

import { join } from 'node:path';
import { jsonStore } from './json-store.ts';
import { replyParent } from '../nostr/nip10.ts';
import { KIND_COMMENT } from '../nostr/nip22.ts';
import type { NostrEvent } from '../nostr/types.ts';

/** The structural toggles, set independently per surface (feed vs profile). */
export interface SurfaceFlags {
    hideReplies: boolean;
    hideQuotes: boolean;      // notes carrying a NIP-18 `q` tag (the feed's "repost"-like content)
    hideLinkOnly: boolean;    // content is only url(s)/whitespace, no prose
}
export type Surface = 'feed' | 'profile';

export interface FeedFilters {
    patterns: string[];       // keyword (substring) or /regex/ per line - GLOBAL (every surface)
    feed: SurfaceFlags;       // structural toggles for the timeline feed
    profile: SurfaceFlags;    // structural toggles for profile pages
}

const noFlags = (): SurfaceFlags => ({ hideReplies: false, hideQuotes: false, hideLinkOnly: false });
const MAX_PATTERNS = 50, MAX_LEN = 200;
const MAX_MATCH_LEN = 8192; // cap note content fed to user regexes (ReDoS: bound the input)

const FILE = process.env.SATORI_FILTERS_FILE || join(process.cwd(), '.data', 'filters.json');
type Store = Record<string, unknown>;

// mtime-cached read / 0o600 write (getFilters() runs on every feed render, so avoid a read+JSON.parse
// each time - jsonStore re-parses only when filters.json actually changed).
const { readAll, writeAll } = jsonStore<Store>(FILE, 'filters');

const flagsOf = (o: unknown): SurfaceFlags => {
    const x = (o ?? {}) as Partial<SurfaceFlags>;
    return { hideReplies: !!x.hideReplies, hideQuotes: !!x.hideQuotes, hideLinkOnly: !!x.hideLinkOnly };
};

/** Read + normalize. Migrates the pre-per-surface shape (flat hideReplies/... that applied to
 * both) by copying those flags onto BOTH surfaces, preserving prior behavior on upgrade. */
export function getFilters(me: string): FeedFilters {
    const raw = readAll()[me] as (Partial<FeedFilters> & Partial<SurfaceFlags>) | undefined;
    if (!raw) return { patterns: [], feed: noFlags(), profile: noFlags() };
    const patterns = Array.isArray(raw.patterns) ? raw.patterns : [];
    if (raw.feed || raw.profile) return { patterns, feed: flagsOf(raw.feed), profile: flagsOf(raw.profile) };
    const flat = flagsOf(raw); // legacy flat flags → both surfaces
    return { patterns, feed: { ...flat }, profile: { ...flat } };
}
export function saveFilters(me: string, f: FeedFilters): void {
    const all = readAll();
    all[me] = {
        patterns: f.patterns.map((p) => p.slice(0, MAX_LEN)).slice(0, MAX_PATTERNS),
        feed: flagsOf(f.feed), profile: flagsOf(f.profile),
    } satisfies FeedFilters;
    writeAll(all);
}

// --- compile + match -------------------------------------------------------
type Matcher = (content: string) => boolean;
const URL_RE = /https?:\/\/\S+/gi;
const SLASH_RE = /^\/(.+)\/([a-z]*)$/;

/** Heuristic ReDoS guard: a quantifier applied to a group whose body itself contains a
 * quantifier or alternation is the classic catastrophic-backtracking shape (star-height > 1) -
 * e.g. `(\w+\s?)*`, `(a+)+`, `(a|a)*`. We can't bound an untrusted regex's time without a
 * linear-time engine, so we REFUSE these shapes (treated like an invalid pattern → ignored),
 * which stops a crafted note from hanging the render. May over-reject exotic-but-safe patterns;
 * acceptable for a single-user filter (simplify the pattern). */
function isDangerousRegex(src: string): boolean {
    return /\([^()]*[+*{][^()]*\)[+*{]|\([^()]*\|[^()]*\)[+*{]/.test(src);
}

/** One pattern line → a matcher, or null (blank / invalid / dangerous regex → ignored). */
function compilePattern(line: string): Matcher | null {
    const t = line.trim();
    if (!t) return null;
    const rx = SLASH_RE.exec(t);
    if (rx) {
        if (isDangerousRegex(rx[1]!)) return null;          // refuse catastrophic-backtracking shapes
        let flags = (rx[2] ?? '').replace(/[^imsuy]/g, ''); // valid, non-stateful flags only (drop g)
        if (!flags.includes('i')) flags += 'i';             // case-insensitive by default
        try { const re = new RegExp(rx[1]!, flags); return (c) => re.test(c); }
        catch { return null; }
    }
    const needle = t.toLowerCase();
    return (c) => c.toLowerCase().includes(needle);
}

/** A kind:1 note whose ENTIRE content is a JSON object/array - an app/bot using kind:1 as a data
 * transport (e.g. chess-over-nostr `{"type":"move",…}`), never human discussion. Always dropped
 * from feeds (built-in, no user config). Human notes virtually never parse as a bare JSON object,
 * and we require the `{`/`[` lead so prose that merely contains JSON isn't touched. */
export function isMachineNote(ev: NostrEvent): boolean {
    if (ev.kind !== 1) return false;
    const t = ev.content.trim();
    if (t.length < 2 || (t[0] !== '{' && t[0] !== '[')) return false;
    try { const v = JSON.parse(t); return typeof v === 'object' && v !== null; } catch { return false; }
}

export interface CompiledFilters { hide(ev: NostrEvent): boolean; active: boolean }

/** Compile a filter set for a SURFACE ONCE per render; `hide(ev)` is then a cheap per-note
 * check. Patterns apply on every surface; the structural toggles are read for `surface`. */
export function compileFilters(f: FeedFilters, surface: Surface): CompiledFilters {
    const matchers = f.patterns.map(compilePattern).filter((m): m is Matcher => !!m);
    const sf = f[surface];
    const active = matchers.length > 0 || sf.hideReplies || sf.hideQuotes || sf.hideLinkOnly;
    return {
        active,
        hide(ev: NostrEvent): boolean {
            if (isMachineNote(ev)) return true; // app/bot JSON-transport notes - never human discussion (built-in)
            // A NIP-22 comment (kind:1111) is ALWAYS a reply; replyParent only reads NIP-10 (lowercase `e`
            // with a marker / no 4th slot), which a 1111's pubkey-in-slot-4 `e` tag never matches - so treat
            // the kind itself as the reply signal. Keeps "hide replies" covering both reply forms.
            if (sf.hideReplies && (ev.kind === KIND_COMMENT || replyParent(ev))) return true;
            if (sf.hideQuotes && ev.tags.some((t) => t[0] === 'q')) return true;
            if (sf.hideLinkOnly && ev.content.replace(URL_RE, '').trim() === '') return true;
            if (matchers.length) {
                const c = ev.content.length > MAX_MATCH_LEN ? ev.content.slice(0, MAX_MATCH_LEN) : ev.content;
                for (const m of matchers) if (m(c)) return true;
            }
            return false;
        },
    };
}
