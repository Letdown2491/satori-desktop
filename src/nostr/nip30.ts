// NIP-30 custom emoji. Events (kind:1 notes, kind:1111 comments, kind:0 metadata)
// carry ["emoji", "<shortcode>", "<url>"] tags; a `:shortcode:` in the content (or in
// a profile's name/about) then renders as that image. Pure data; the render side
// (render/content.ts withEmoji) turns matches into <img class="emoji"> proxied via /media.

import { safeUrl } from '../html.ts';
import type { NostrEvent } from './types.ts';

export type EmojiMap = Record<string, string>; // shortcode (no colons) → image url

const CODE = /^[a-zA-Z0-9_-]+$/;

/** Map shortcode → url from a tag list's NIP-30 `emoji` tags (valid code + safe url
 * only). Returns undefined when there are none, so callers can skip the work. */
export function emojiFromTags(tags: string[][]): EmojiMap | undefined {
    let map: EmojiMap | undefined;
    for (const tag of tags) {
        if (tag[0] !== 'emoji' || !tag[1] || !tag[2] || !CODE.test(tag[1]) || safeUrl(tag[2]) === '#') continue;
        // Null-prototype so a `:__proto__:`/`:constructor:` shortcode can't collide with inherited members
        // when this map is later indexed by an attacker-supplied key (see pickReaction).
        (map ??= Object.create(null) as EmojiMap)[tag[1]] = tag[2];
    }
    return map;
}

/** As above, from an event (kind:1 notes, kind:1111 comments). */
export const parseEmojiTags = (ev: NostrEvent): EmojiMap | undefined => emojiFromTags(ev.tags);
