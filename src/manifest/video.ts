// NIP-71 video events (21 normal, 22 short, 34235/34236 addressable). A SIMPLE kind on cardShell, like
// picture/podcast - the bespoke piece is the VIDEO, which is privacy-sensitive (a third-party <video src>
// streams browser->host and leaks the IP). We reuse the note's exact player via mediaTiles: poster +
// preload="none" off/balanced, and an "opens outside Tor" facade under strict Privacy Mode - all handled
// inside video()/mediaTiles, so this handler just feeds it the imeta. NIP-71 content-warning blurs the
// player behind the same tap-to-reveal notes use; the long description clamps off the focused thread.

import type { KindHandler } from './registry.ts';
import { type SatoriDeps, notWired } from './deps.ts';
import { html, type SafeHtml } from '../html.ts';
import { renderContent, videoItems, mediaTiles } from '../render/content.ts';
import { cardShell, cardTitle, clampIfTall, cwIfFlagged } from '../render/note.ts';
import type { ProfileMap } from '../render/util.ts';
import { parseImeta } from '../nostr/imeta.ts';
import { VIDEO_KINDS, isShortVideo } from '../nostr/nip71.ts';
import { tag1 } from '../nostr/tags.ts';
import type { NostrEvent } from '../nostr/types.ts';

/** The video BODY (title + player + description) for cardShell. The player is the PRIMARY imeta (the
 * first; further imeta are resolution variants of the same video, not a gallery), rendered through the
 * shared privacy-aware player. Description (NIP-71 content = summary) tokenizes + clamps off-focused. */
function videoBody(ev: NostrEvent, profiles: ProfileMap | undefined, clamp: boolean, inlineVideo: boolean): SafeHtml {
    const [primary] = [...parseImeta(ev)]; // [url, meta] of the first imeta
    // NIP-71: when the imeta carries no `dim`, the KIND decides orientation (a kind:22/34236 short is
    // vertical) - pass it as the aspect fallback so a poster-less short doesn't render landscape.
    const meta = primary && !primary[1].dim ? { ...primary[1], orient: isShortVideo(ev.kind) ? 'portrait' as const : 'landscape' as const } : primary?.[1];
    const player = primary ? mediaTiles(videoItems([{ url: primary[0], meta }]), true, inlineVideo) : null;
    const notes = ev.content.trim() ? renderContent(ev.content, profiles, false) : null;
    const visual = html`
      ${player}
      ${clampIfTall(notes, ev.content, clamp, ev.id)}`;
    return html`
      ${cardTitle(tag1(ev, 'title'))}
      ${cwIfFlagged(ev, visual)}`;
}

export const videoHandler: KindHandler<SatoriDeps> = {
    kinds: VIDEO_KINDS,
    actions: ['reply', 'quote', 'like', 'zap', 'bookmark', 'pin'],
    ref: { as: 'video', label: '↗ video', path: (b) => `/a/${b}` }, // inline naddr (34235/34236) → a video embed

    render(ev, surface, d) {
        if (surface === 'reader') return notWired(surface); // the card carries the video; no reader page
        return cardShell(ev, d.profiles, d.s, videoBody(ev, d.profiles, surface !== 'focused', d.s?.media?.inlineVideo ?? false), { compact: surface === 'embed' });
    },
};
