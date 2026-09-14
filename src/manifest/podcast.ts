// NIP-F4 podcast EPISODE (kind 54) - rendered when ENCOUNTERED (a quote/embed or a thread anchor), not
// pulled into any feed (feed-off in CONTENT_TYPES). A second SIMPLE kind after picture: same B-pattern (a body
// dropped into the shared cardShell). The new piece vs picture is AUDIO, which is privacy-sensitive like
// video - so it gets the same treatment: preload="none" (no fetch until the user hits play) off/balanced,
// suppressed to an "opens outside Tor" link under strict Privacy Mode. The privacy logic lives here in the
// renderer (where it would live in a declarative engine's audio field-renderer too), never in the data.

import type { KindHandler } from './registry.ts';
import { type SatoriDeps, notWired } from './deps.ts';
import { html, type SafeHtml } from '../html.ts';
import { imgSrc, renderContent } from '../render/content.ts';
import { cardShell, cardTitle, collapse, isTallText } from '../render/note.ts';
import type { ProfileMap } from '../render/util.ts';
import { icon } from '../render/svg.ts';
import { torStrict } from '../privacy.ts';
import { KIND_PODCAST_EPISODE } from '../nostr/nipf4.ts';
import { tag1 } from '../nostr/tags.ts';
import type { NostrEvent } from '../nostr/types.ts';

/** The `["audio", url, type?]` tags, keeping only safe http(s) urls (a bad/cross-scheme src is dropped). */
function audioSources(ev: NostrEvent): { url: string; type: string }[] {
    return ev.tags
        .filter((t) => t[0] === 'audio' && t[1] && /^https?:\/\//i.test(t[1]))
        .map((t) => ({ url: t[1]!, type: t[2] ?? '' }));
}

/** The episode BODY (cover + title + audio player + show-notes) - the piece that goes inside cardShell.
 * Order is deliberate: the AUDIO is the payload, so it sits right under the cover, above the notes (an
 * episode's show-notes can run to thousands of chars - burying the player under them, as a naive title→
 * description→audio order did, hid the one thing a podcast card is for). The description renders through
 * renderContent (embeds=false, the reply-preview path) so its URLs are real links, not dead plain text.
 * `clamp` (true off the focused thread) FULLY collapses long show-notes behind a zero-JS Show-more that
 * sits right under the player - the audio is the payload, so a partial preview is just a wall of text;
 * better to hide the notes entirely until asked. The focused thread shows them in full. */
function episodeBody(ev: NostrEvent, profiles: ProfileMap | undefined, clamp: boolean): SafeHtml {
    const title = tag1(ev, 'title');
    const image = tag1(ev, 'image');
    const desc = tag1(ev, 'description');
    const audio = audioSources(ev);
    // Audio = video's privacy class: under strict Tor, suppress the <audio src> (it'd stream browser→host
    // and leak the IP) and offer a deliberate "leaves Tor" open link, reusing the video-facade styling.
    const player = audio.length === 0 ? null
        : torStrict()
            ? html`<a class="video-facade strict" href="${audio[0]!.url}" target="_blank" rel="noreferrer noopener" aria-label="Play episode (leaves Tor)" title="Play episode (leaves Tor)">${icon('play', true)}<span class="video-strict-note">opens outside Tor</span></a>`
            : html`<audio class="audio-player" controls preload="none">${audio.map((a) => a.type
                ? html`<source src="${a.url}" type="${a.type}">`
                : html`<source src="${a.url}">`)}</audio>`;
    const notes = desc ? renderContent(desc, profiles, false) : null;
    const showNotes = notes && clamp && isTallText(desc) ? collapse(notes, ev.id) : notes;
    return html`
      ${cardTitle(title)}
      ${image ? html`<img class="media" src="${imgSrc(image)}" alt="${title}" loading="lazy">` : null}
      ${player}
      ${showNotes}`;
}

export const podcastEpisodeHandler: KindHandler<SatoriDeps> = {
    kinds: [KIND_PODCAST_EPISODE],
    actions: ['reply', 'quote', 'like', 'zap', 'bookmark', 'pin'],
    render(ev, surface, d) {
        if (surface === 'reader') return notWired(surface); // the markdown show-notes reader is not wired (code, like the article reader); the card is enough
        // Clamp the long show-notes everywhere EXCEPT the focused thread (where the full episode belongs).
        return cardShell(ev, d.profiles, d.s, episodeBody(ev, d.profiles, surface !== 'focused'), { compact: surface === 'embed' });
    },
};
