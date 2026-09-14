// NIP-22 generic comments (kind 1111). A comment scopes itself with UPPERCASE root tags (E/A/I + K) and
// names its IMMEDIATE parent with lowercase tags (e/a/i + k). We render the immediate parent as the "in
// reply to" link on the comment card.

import { tag1, quotedIds } from './tags.ts';
import type { NostrEvent } from './types.ts';

export const KIND_COMMENT = 1111;

/** The immediate parent a NIP-22 comment replies to: an event (`e`), addressable (`a`), or external id
 * (`i`), plus the parent's kind (`k`) and - for an `e` parent - the NIP-22 relay hint + author pubkey it
 * carries (`["e", id, relay, pubkey]`), so a link/embed to the parent can actually resolve it. null when
 * none of the lowercase parent tags is present. */
export interface CommentParent { type: 'e' | 'a' | 'i'; value: string; kind: string; relay?: string; pubkey?: string }
export function commentParent(ev: NostrEvent): CommentParent | null {
    const kind = tag1(ev, 'k');
    // A NIP-22 `e` tag carries no marker (index 3 is the author pubkey), so a `q`-quoted event can't be told
    // apart from the parent by shape - exclude quoted ids so a quote in the comment isn't mistaken for its
    // parent. (A NIP-27 mention e-tag still can't be distinguished; we rely on compliant clients emitting the
    // parent `e` first, as this find() picks the first surviving `e`.)
    const quoted = quotedIds(ev);
    const e = ev.tags.find((t) => t[0] === 'e' && t[1] && !quoted.has(t[1])); if (e) return { type: 'e', value: e[1]!, kind, relay: e[2] || undefined, pubkey: e[3] || undefined };
    const a = ev.tags.find((t) => t[0] === 'a' && t[1]); if (a) return { type: 'a', value: a[1]!, kind, relay: a[2] || undefined };
    const i = ev.tags.find((t) => t[0] === 'i' && t[1]); if (i) return { type: 'i', value: i[1]!, kind };
    return null;
}

/** The ROOT scope a NIP-22 comment is anchored to: the uppercase E/A/I + K (+ P) tags. Unlike the
 * immediate parent, the root is the ORIGINAL thread subject, carried unchanged through every nested
 * level - so a reply to a comment inherits this verbatim. Mirror of commentParent for the root tags. */
export interface CommentRootScope { type: 'E' | 'A' | 'I'; value: string; kind: string; pubkey?: string }
export function commentRoot(ev: NostrEvent): CommentRootScope | null {
    const kind = tag1(ev, 'K');
    const pubkey = tag1(ev, 'P') || undefined;
    const e = ev.tags.find((t) => t[0] === 'E' && t[1]); if (e) return { type: 'E', value: e[1]!, kind, pubkey };
    const a = ev.tags.find((t) => t[0] === 'A' && t[1]); if (a) return { type: 'A', value: a[1]!, kind, pubkey };
    const i = ev.tags.find((t) => t[0] === 'I' && t[1]); if (i) return { type: 'I', value: i[1]!, kind, pubkey };
    return null;
}
