// Per-user CONTENT-TYPE visibility: which event kinds appear in the Feed vs on Profiles. The CATALOG
// (CONTENT_TYPES) is the curated list of renderable content types this client offers - colocated config
// NOT auto-derived from the registry (the registry has no labels, no grouping like
// calendar's two kinds under one row, no per-surface defaults). Adding a richly-rendered kind = one
// entry here, same as adding a handler. The per-user choices are stored server-side (mirrors filters.ts),
// so they never leave the daemon. The resolved kind sets drive the relay QUERY (fetch-time, not display).

import { join } from 'node:path';
import { jsonStore } from './json-store.ts';
import { KIND_POLL } from '../nostr/nip88.ts';
import { KIND_ARTICLE } from '../nostr/nip23.ts';
import { KIND_PICTURE } from '../nostr/nip68.ts';
import { KIND_PODCAST_EPISODE } from '../nostr/nipf4.ts';
import { KIND_CALENDAR_DATE, KIND_CALENDAR_TIME } from '../nostr/nip52.ts';
import { KIND_LISTING } from '../nostr/nip99.ts';
import { VIDEO_KINDS } from '../nostr/nip71.ts';
import { KIND_HIGHLIGHT } from '../nostr/nip84.ts';
import { KIND_COMMENT } from '../nostr/nip22.ts';
import { KIND_CUSTOM_NIP } from '../nostr/customnip.ts';
import { KIND_WIKI } from '../nostr/nip54.ts';
import { KIND_REPO } from '../nostr/nip34.ts';

export type Surface = 'feed' | 'profile' | 'timeline';

export interface ContentType {
    id: string;
    label: string;
    kinds: number[];
    feed: boolean;     // default in the timeline feed
    profile: boolean;  // default on profiles
    timeline: boolean; // default: promoted to its own timeline in the header switcher
}

// Pictures default feed-OFF on purpose: most people post pictures as kind:1 notes with the image inline,
// so pulling kind:20 into the feed is mostly redundant noise. Rich kinds are profile-on / feed-off - your
// profile shows your whole output; your main feed stays note-shaped unless you opt a kind in.
// A promoted `timeline` type gets its own entry in the header switcher (its kinds, from your follows). Only
// Articles is promoted by default (the old hardcoded "Longform" tab, now folded into this mechanism); every
// other type is opt-in from Settings > Content.
export const CONTENT_TYPES: ContentType[] = [
    // Notes + NIP-22 comments (kind:1111) ride together: a comment is a reply, the NIP-22 equivalent of a
    // kind:1 reply, so it's fetched with notes and shown unless you hide replies (see filters.ts hideReplies).
    { id: 'note', label: 'Notes', kinds: [1, KIND_COMMENT], feed: true, profile: true, timeline: false },
    { id: 'poll', label: 'Polls', kinds: [KIND_POLL], feed: true, profile: true, timeline: false },
    { id: 'picture', label: 'Pictures', kinds: [KIND_PICTURE], feed: false, profile: true, timeline: true },
    { id: 'video', label: 'Videos', kinds: VIDEO_KINDS, feed: false, profile: true, timeline: false },
    { id: 'article', label: 'Articles', kinds: [KIND_ARTICLE], feed: false, profile: true, timeline: true },
    { id: 'podcast', label: 'Podcasts', kinds: [KIND_PODCAST_EPISODE], feed: false, profile: true, timeline: false },
    { id: 'calendar', label: 'Calendar events', kinds: [KIND_CALENDAR_DATE, KIND_CALENDAR_TIME], feed: false, profile: true, timeline: false },
    { id: 'listing', label: 'Listings', kinds: [KIND_LISTING], feed: false, profile: true, timeline: false },
    { id: 'highlight', label: 'Highlights', kinds: [KIND_HIGHLIGHT], feed: false, profile: true, timeline: false },
    { id: 'customnip', label: 'Custom NIPs', kinds: [KIND_CUSTOM_NIP], feed: false, profile: true, timeline: false },
    { id: 'wiki', label: 'Wiki articles', kinds: [KIND_WIKI], feed: false, profile: true, timeline: false },
    { id: 'repo', label: 'Git repositories', kinds: [KIND_REPO], feed: false, profile: true, timeline: false },
];

/** The mainstream content types shown by default in Settings > Content; the niche kinds sit behind a
 * "Show more" disclosure. Both groups render alphabetically. */
const CORE_TYPE_IDS = new Set(['note', 'poll', 'article', 'picture', 'video']);
export const isCoreType = (id: string): boolean => CORE_TYPE_IDS.has(id);

export type ContentPrefs = { feed: Record<string, boolean>; profile: Record<string, boolean>; timeline: Record<string, boolean> };

const FILE = process.env.SATORI_CONTENT_PREFS_FILE || join(process.cwd(), '.data', 'content-prefs.json');
type Store = Record<string, { feed?: Record<string, boolean>; profile?: Record<string, boolean>; timeline?: Record<string, boolean> }>;

const { readAll, writeAll } = jsonStore<Store>(FILE, 'content-prefs');

/** Resolve a surface's per-id booleans: a saved choice wins, else the catalog default. */
function resolve(saved: Record<string, boolean> | undefined, surface: Surface): Record<string, boolean> {
    return Object.fromEntries(CONTENT_TYPES.map((c) => [c.id, saved?.[c.id] ?? c[surface]]));
}

/** The user's content-type choices (catalog defaults where unset). */
export function getContentPrefs(me: string): ContentPrefs {
    const raw = readAll()[me];
    return { feed: resolve(raw?.feed, 'feed'), profile: resolve(raw?.profile, 'profile'), timeline: resolve(raw?.timeline, 'timeline') };
}

export function saveContentPrefs(me: string, prefs: ContentPrefs): void {
    const all = readAll();
    const pick = (o: Record<string, boolean>) => Object.fromEntries(CONTENT_TYPES.map((c) => [c.id, !!o[c.id]]));
    all[me] = { feed: pick(prefs.feed), profile: pick(prefs.profile), timeline: pick(prefs.timeline) };
    writeAll(all);
}

/** The enabled KINDS for a flattened surface (feed/profile) - drives the relay query. A promoted TIMELINE is
 * per-type (each its own feed with its own kinds), so it's NOT flattened here (see timelineKinds). */
function kindsFor(me: string, surface: 'feed' | 'profile'): number[] {
    const p = getContentPrefs(me)[surface];
    return CONTENT_TYPES.filter((c) => p[c.id]).flatMap((c) => c.kinds);
}
export const feedKinds = (me: string): number[] => kindsFor(me, 'feed');
export const profileKinds = (me: string): number[] => kindsFor(me, 'profile');

/** The content types the user has promoted to their own timeline - the extra entries in the header switcher. */
export function timelineTypes(me: string): ContentType[] {
    const p = getContentPrefs(me).timeline;
    // Alphabetical by label - the header switcher lists Following, Followers, then these below in order.
    return CONTENT_TYPES.filter((c) => p[c.id]).sort((a, b) => a.label.localeCompare(b.label));
}
/** The promoted timelines projected to the `{id,label}` shape the header switcher consumes - the one place
 * chromeFor, the content-save OOB rebuild, and the relay-picker rebuild all derive it. */
export function timelineEntries(me: string): { id: string; label: string }[] {
    return timelineTypes(me).map((c) => ({ id: c.id, label: c.label }));
}
/** The kinds a single promoted timeline queries (its content type's own kinds). */
export function timelineKinds(id: string): number[] {
    return CONTENT_TYPES.find((c) => c.id === id)?.kinds ?? [];
}
/** Is `id` a known content type? (Timeline-route validation.) */
export function isContentType(id: string): boolean {
    return CONTENT_TYPES.some((c) => c.id === id);
}
