// Profile (kind:0) fetching + publishing. Caching lives in the app store.

import type { Pool } from './pool.ts';
import type { NostrEvent } from '../nostr/types.ts';
import { emojiFromTags, type EmojiMap } from '../nostr/nip30.ts';

export interface Profile {
    name?: string;
    display_name?: string;
    nip05?: string;
    picture?: string;
    about?: string;
    lud16?: string; // lightning address (for zaps)
    website?: string; // NIP-24: a web URL related to the author
    banner?: string;  // NIP-24: wide profile background image
    bot?: boolean;    // NIP-24: account is (partly) automated
    nip05Verified?: boolean; // derived (not from kind:0): set by the background NIP-05 check
    emoji?: EmojiMap; // NIP-30 shortcode→url from the kind:0 event's `emoji` tags (for name/about)
}

/** A lightning address from kind:0 metadata - lud16, or lud06→? (lud16 only). */
function lightningAddress(meta: { lud16?: unknown }): string | undefined {
    return typeof meta.lud16 === 'string' && meta.lud16.includes('@') ? meta.lud16.trim() : undefined;
}

/** Parse a kind:0 `content` JSON string into a Profile (null if malformed). Pass the
 * event's `tags` to capture NIP-30 custom emoji declared for the name/about. */
export function parseProfile(content: string, tags?: string[][]): Profile | null {
    try {
        const meta = JSON.parse(content);
        return {
            name: meta.name,
            display_name: meta.display_name ?? meta.displayName,
            nip05: (typeof meta.nip05 === 'string' && meta.nip05.includes('@')) ? meta.nip05.trim() : undefined,
            picture: typeof meta.picture === 'string' ? meta.picture : undefined,
            about: typeof meta.about === 'string' ? meta.about : undefined,
            lud16: lightningAddress(meta),
            website: typeof meta.website === 'string' && meta.website.trim() ? meta.website.trim() : undefined,
            banner: typeof meta.banner === 'string' && meta.banner.trim() ? meta.banner.trim() : undefined,
            bot: meta.bot === true ? true : undefined,
            emoji: tags ? emojiFromTags(tags) : undefined,
        };
    } catch { return null; }
}

/** Fetch kind:0 profiles for many pubkeys, newest per author. */
export async function fetchProfiles(pool: Pool, relays: string[], pubkeys: string[]): Promise<Map<string, Profile>> {
    const map = new Map<string, Profile>();
    if (pubkeys.length === 0) return map;
    const newest = new Map<string, number>();
    const events = await pool.query(relays, { kinds: [0], authors: pubkeys }, { fast: true, resolve: true });
    for (const ev of events) {
        if ((newest.get(ev.pubkey) ?? -1) >= ev.created_at) continue;
        const prof = parseProfile(ev.content, ev.tags);
        if (prof) { newest.set(ev.pubkey, ev.created_at); map.set(ev.pubkey, prof); }
    }
    return map;
}

/** Fetch one pubkey's latest kind:0 RAW content (all fields), for editing - so we
 * preserve fields the app doesn't render (banner, website, lud06, …). */
export async function fetchProfileContent(pool: Pool, relays: string[], pubkey: string): Promise<Record<string, unknown>> {
    const events = await pool.query(relays, { kinds: [0], authors: [pubkey] }, { resolve: true }).catch(() => []);
    let newest: NostrEvent | null = null;
    for (const ev of events) if (!newest || ev.created_at > newest.created_at) newest = ev;
    if (!newest) return {};
    try {
        const c = JSON.parse(newest.content);
        return (c && typeof c === 'object') ? c as Record<string, unknown> : {};
    } catch { return {}; }
}
