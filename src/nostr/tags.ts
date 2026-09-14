// Generic Nostr tag + value helpers (kind-agnostic). These were redefined per-file across routes/data/
// nostr; one home now. Imports only ./types.ts, so it's cycle-safe for any caller.

import type { NostrEvent } from './types.ts';

/** The first value of the first tag named `name` (`t[1]`), or '' when absent. */
export const tag1 = (ev: NostrEvent, name: string): string => ev.tags.find((t) => t[0] === name)?.[1] ?? '';

/** ALL values of every tag named `name`, flattened + de-blanked. Handles both single-tag-multi-value
 * (`["clone", a, b]`) and repeated-tag (`["clone", a], ["clone", b]`) shapes - NIP-34 uses both. */
export const tagValues = (ev: NostrEvent, name: string): string[] =>
    ev.tags.filter((t) => t[0] === name).flatMap((t) => t.slice(1)).filter(Boolean);

/** A 64-char hex id / pubkey. Case-INSENSITIVE (some sources emit uppercase); callers should lowercase
 * at the boundary if using the value as a tag/key. One canonical validator for every route. */
export const HEX64 = /^[0-9a-f]{64}$/i;
export const isHex64 = (s: string): boolean => HEX64.test(s);

/** Current unix time in SECONDS (Nostr's `created_at` unit). */
export const nowSec = (): number => Math.floor(Date.now() / 1000);

/** The timestamp to DISPLAY / order an event by. Long-form kinds (NIP-23 articles, NIP-54 wikis, custom
 * NIPs) carry a `published_at` tag = the original first-publish time, which stays FIXED across edits while
 * `created_at` bumps on every re-sign. Ordering a timeline by this keeps a re-edited old article at its
 * publish date instead of jumping to the top of the feed on every edit. Plain events (notes) have no such
 * tag, so this is simply their created_at. Mirrors the fallback in parseArticle/parseCustomNip/parseWiki. */
export const displayTime = (ev: NostrEvent): number => {
    const p = Number(tag1(ev, 'published_at'));
    return Number.isFinite(p) && p > 0 ? p : ev.created_at;
};

/** The addressable coordinate `kind:pubkey:dtag` for an event. */
export const coordinateOf = (ev: NostrEvent): string => `${ev.kind}:${ev.pubkey}:${tag1(ev, 'd')}`;

/** Parse an addressable coordinate `kind:pubkey:dtag` back into its parts, or null if malformed. Splits on
 * the FIRST TWO colons only, so a `d` identifier that itself contains colons survives intact. The inverse
 * of coordinateOf - one parser for every site that used to hand-split with `.split(':')` indices. */
export const coordParts = (coord: string): { kind: number; pubkey: string; d: string } | null => {
    const i1 = coord.indexOf(':'), i2 = coord.indexOf(':', i1 + 1);
    if (i1 < 0 || i2 < 0) return null;
    const kind = Number(coord.slice(0, i1)), pubkey = coord.slice(i1 + 1, i2);
    if (!Number.isInteger(kind) || !HEX64.test(pubkey)) return null;
    return { kind, pubkey, d: coord.slice(i2 + 1) };
};

/** An addressable (parameterized-replaceable) kind: 30000-39999 (NIP-01). These are referenced by their
 * `kind:pubkey:d` coordinate (naddr), so engagement/like state keys off the coordinate, not the event id. */
export const isAddressable = (kind: number): boolean => kind >= 30000 && kind < 40000;

/** The `relay` tag urls, de-duped and capped (default 8). Shared by the DM / draft / poll relay parsers. */
export const relayTags = (ev: NostrEvent, cap = 8): string[] =>
    [...new Set(ev.tags.filter((t) => t[0] === 'relay' && t[1]).map((t) => t[1]!))].slice(0, cap);

/** The set of event ids this event `q`-quotes (NIP-18 quote tags). Excluded from reply/comment PARENT
 * resolution (a quote isn't the thing you're replying to) and enumerated as reposts by the engagement
 * cache - the one definition of "the ids this event quotes", shared so the exclusion can't drift. */
export const quotedIds = (ev: NostrEvent): Set<string> =>
    new Set(ev.tags.filter((t) => t[0] === 'q' && t[1]).map((t) => t[1]!));
