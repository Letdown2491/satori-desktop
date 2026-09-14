// NIP-30 custom emoji for the reaction picker. READ-ONLY: we consume whatever emoji the user already
// curated (in any client) via their kind:10030 "emoji list" - which references kind:30030 "emoji sets"
// (and may carry direct `emoji` tags) - and flatten it to a shortcode->url map. Managing/adding emoji
// sets inside Satori is out of scope. Cached per session, TTL'd. The DISPLAY of custom emoji already
// works (nostr/nip30.ts + render withEmoji); this is purely the SOURCE for the picker + send path.

import type { Session } from '../session.ts';
import type { NostrEvent } from '../nostr/types.ts';
import { INDEXER_RELAYS } from '../nostr/nip65.ts';
import { coalesceOne } from './coalesce.ts';
import { emojiFromTags, type EmojiMap } from '../nostr/nip30.ts';

export const KIND_USER_EMOJI = 10030; // the user's emoji list (a/emoji tags)
export const KIND_EMOJI_SET = 30030;  // an addressable emoji set (emoji tags)

const TTL_MS = 5 * 60 * 1000; // balance: a just-added emoji set appears within a few minutes, not a long wait
const cache = new Map<string, { map: EmojiMap; at: number }>();
const inflight = new Map<string, Promise<void>>();

/** A broad relay set: the user's own (their 10030 lives here) plus indexers (third-party 30030 sets
 * often do). Best-effort - a set we can't reach just means those emoji don't appear (graceful). */
function relaysFor(s: Session): string[] {
    return [...new Set([...(s.myRelays?.read ?? []), ...(s.myRelays?.write ?? []), ...INDEXER_RELAYS])].slice(0, 12);
}

/** The user's custom emoji (shortcode -> url), from cache. Empty until warmed (ensureUserEmoji). */
export function userEmojiCached(me: string): EmojiMap {
    return cache.get(me)?.map ?? {};
}

/** Fetch + cache the user's NIP-30 emoji: 10030 list -> referenced 30030 sets -> one flat shortcode->url
 * map. Idempotent + TTL'd + single-flight, so calling it on every reaction-enabled render is cheap. */
export async function ensureUserEmoji(s: Session): Promise<void> {
    if (!s.me) return;
    const me = s.me;
    const hit = cache.get(me);
    if (hit && Date.now() - hit.at < TTL_MS) return;
    await coalesceOne(inflight, me, async () => {
        const relays = relaysFor(s);
        const lists = await s.pool.query(relays, { authors: [me], kinds: [KIND_USER_EMOJI], limit: 1 }).catch(() => [] as NostrEvent[]);
        const list = lists.sort((a, b) => b.created_at - a.created_at)[0];
        const map: EmojiMap = {};
        if (list) {
            Object.assign(map, emojiFromTags(list.tags) ?? {}); // direct emoji tags on the 10030 itself
            // `a` tags = referenced sets, coords "30030:pubkey:identifier".
            const coords = list.tags.filter((t) => t[0] === 'a' && t[1]?.startsWith(`${KIND_EMOJI_SET}:`)).map((t) => t[1]!);
            if (coords.length) {
                const want = new Set(coords);
                const authors = [...new Set(coords.map((c) => c.split(':')[1]).filter((x): x is string => !!x))];
                const ds = [...new Set(coords.map((c) => c.split(':')[2]).filter((x): x is string => !!x))];
                const sets = await s.pool.query(relays, { kinds: [KIND_EMOJI_SET], authors, '#d': ds, limit: 50 }).catch(() => [] as NostrEvent[]);
                const newest = new Map<string, NostrEvent>(); // newest per referenced coord
                for (const ev of sets) {
                    const d = ev.tags.find((t) => t[0] === 'd')?.[1] ?? '';
                    const coord = `${KIND_EMOJI_SET}:${ev.pubkey}:${d}`;
                    if (!want.has(coord)) continue; // ignore sets we didn't ask for
                    const prev = newest.get(coord);
                    if (!prev || ev.created_at > prev.created_at) newest.set(coord, ev);
                }
                for (const ev of newest.values()) Object.assign(map, emojiFromTags(ev.tags) ?? {});
            }
        }
        cache.set(me, { map, at: Date.now() });
    });
}

/** Clear on logout (the next account must not inherit it). */
export function clearUserEmoji(me?: string): void { if (me) cache.delete(me); else cache.clear(); }
