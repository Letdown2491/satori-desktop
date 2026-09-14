// NIP-F4: Nostr-native podcasts. The renderable feed event is the EPISODE (kind 54): title/image/
// description/audio tags + markdown show-notes in content. (The NIP also defines podcast metadata
// 10154, authored 10064, and favorites 10054 - not rendered as cards, so not constants here yet.)
// Kind constant lives here, its NIP module, mirroring KIND_PICTURE (nip68) / KIND_POLL (nip88).
export const KIND_PODCAST_EPISODE = 54;

import type { NostrEvent } from './types.ts';

/** A kind:54 event is only a real podcast episode if it carries an `audio` tag. Kind 54 is contested - it's
 * also used in the wild for lightning-gated content (e.g. zap.cooking recipes) that has no audio - so an
 * event that claims the kind but has no audio is NOT a podcast and must not render as one. */
export const isPodcast = (ev: NostrEvent): boolean =>
    ev.kind === KIND_PODCAST_EPISODE && ev.tags.some((t) => t[0] === 'audio' && !!t[1]);

/** Filter predicate: keep everything EXCEPT a kind:54 event with no audio (a non-podcast masquerading as one). */
export const notFakePodcast = (ev: NostrEvent): boolean => ev.kind !== KIND_PODCAST_EPISODE || isPodcast(ev);
