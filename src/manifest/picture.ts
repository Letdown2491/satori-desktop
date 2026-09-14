// The NIP-68 "picture" kind (20), added entirely in the manifest layer - a new SIMPLE kind is this file +
// its registration in satori.ts + one CONTENT_TYPES entry, no core surgery. Pictures put their image(s) in
// NIP-92 `imeta` tags (not the content), so the note fallback would show only the caption; this handler
// renders the images. The shared `.note` shell (avatar + head + action row) now comes from `cardShell`
// (render/note.ts), so this file is just picture-specific extraction + the body that goes inside it - the
// template every future simple kind follows.

import type { KindHandler } from './registry.ts';
import { type SatoriDeps, notWired } from './deps.ts';
import { html, type SafeHtml } from '../html.ts';
import { renderContent, imageItems, mediaTiles, mediaOverlays } from '../render/content.ts';
import { cardShell, cardTitle, cwIfFlagged, pictureFigure } from '../render/note.ts';
import type { ProfileMap } from '../render/util.ts';
import { parseImeta } from '../nostr/imeta.ts';
import { tag1 } from '../nostr/tags.ts';
import { KIND_PICTURE, firstCaptionLine } from '../nostr/nip68.ts';
import type { NostrEvent } from '../nostr/types.ts';

/** The picture-specific BODY (image-forward), dropped into the shared cardShell. The images now render
 * through the SAME gallery + lightbox a note uses (mediaTiles, fed imeta url+alt+dim so dimStyle still
 * reserves each slot - no layout shift), instead of a re-rolled stack. The caption (.content = NIP-68
 * "description of post") renders through renderContent so its links / mentions are live; a `location`
 * tag shows as a quiet dateline. NIP-68 content-warning (NSFW) blurs the visual behind the same zero-JS
 * tap-to-reveal notes use - the title stays visible (context for the reveal); the imagery is hidden. */
function pictureBody(ev: NostrEvent, profiles: ProfileMap | undefined, media: SafeHtml): SafeHtml {
    // The image(s) already render as the figure (from the imeta tags). Some picture clients ALSO put the
    // image URL in the content, so renderContent would show it a SECOND time as an inline image below the
    // caption. Strip the imeta urls from the caption text so it stays just the description.
    let text = ev.content;
    for (const [url] of parseImeta(ev)) text = text.split(url).join('');
    text = text.trim();
    const caption = text ? renderContent(text, profiles, false) : null;
    const visual = pictureFigure(media, caption, tag1(ev, 'location'));
    // A blank-title picture derives its NIP-68 `title` tag from the caption's first line (see signPicture),
    // so the tag is present for other clients. Showing it HERE would duplicate that first line above the
    // caption, so we suppress a title that just repeats it (applies to any kind-20, whatever the source).
    const title = tag1(ev, 'title');
    const heading = title && title !== firstCaptionLine(ev.content) ? title : '';
    return html`
      ${cardTitle(heading)}
      ${cwIfFlagged(ev, visual)}`;
}

export const pictureHandler: KindHandler<SatoriDeps> = {
    kinds: [KIND_PICTURE],
    // Declared control vocabulary (noteActions reads this to build the row): picture affords pin (your
    // own) but not row-mute.
    actions: ['reply', 'quote', 'like', 'zap', 'bookmark', 'pin'],
    render(ev, surface, d) {
        if (surface === 'reader') return notWired(surface); // pictures have no reader page
        const compact = surface === 'embed';
        const items = imageItems([...parseImeta(ev)].map(([url, meta]) => ({ url, meta })));
        const body = pictureBody(ev, d.profiles, mediaTiles(items, !compact));
        return cardShell(ev, d.profiles, d.s, body, { compact, lightboxes: compact ? undefined : mediaOverlays(items) });
    },
};
