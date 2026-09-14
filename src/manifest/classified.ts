// NIP-99 classified listing (kind 30402), GRADUATED from the declarative engine to a hand-coded handler -
// the test case that proved the lifecycle: a kind starts generic (a data manifest), and when it earns a
// first-class experience it becomes code. The bespoke richness the engine couldn't give: PRICE formatting
// (["price", amount, currency, frequency] -> "100 USD / day"), a multi-IMAGE gallery, a SOLD badge, the
// Markdown body, and a "Message seller" DM action. Everything else still rides the shared cardShell.

import type { KindHandler } from './registry.ts';
import { type SatoriDeps, notWired } from './deps.ts';
import { html, type SafeHtml } from '../html.ts';
import { renderMarkdown, imageItems, mediaTiles, mediaOverlays } from '../render/content.ts';
import { cardShell, cardTitle, clampIfTall } from '../render/note.ts';
import { npub, type ProfileMap } from '../render/util.ts';
import { icon } from '../render/svg.ts';
import { parseImeta } from '../nostr/imeta.ts';
import { KIND_LISTING } from '../nostr/nip99.ts';
import { tag1 } from '../nostr/tags.ts';
import type { Session } from '../session.ts';
import type { NostrEvent } from '../nostr/types.ts';

/** ["price", amount, currency, frequency?] -> "100 USD" or "100 USD / day". The bespoke bit the generic
 * engine couldn't do: it only saw t[1]. Empty when there's no usable price. */
function formatPrice(ev: NostrEvent): string {
    const p = ev.tags.find((t) => t[0] === 'price');
    if (!p?.[1]) return '';
    const amount = p[1], currency = p[2] ?? '', freq = p[3] ?? '';
    return `${amount}${currency ? ' ' + currency : ''}${freq ? ' / ' + freq : ''}`;
}

/** ["location", country, subdivision?] -> "US, US-FL"; a single value passes through. */
function formatLocation(ev: NostrEvent): string {
    const t = ev.tags.find((tag) => tag[0] === 'location');
    return t ? t.slice(1).filter(Boolean).join(', ') : '';
}

/** All listing images: `image` tags first (NIP-99's primary), then any NIP-92 imeta urls, de-duped. */
function listingImages(ev: NostrEvent): string[] {
    const urls = ev.tags.filter((t) => t[0] === 'image' && t[1]).map((t) => t[1]!);
    for (const url of parseImeta(ev).keys()) urls.push(url);
    return [...new Set(urls)];
}

function listingBody(ev: NostrEvent, profiles: ProfileMap | undefined, s: Session | undefined, clamp: boolean, media: SafeHtml): SafeHtml {
    const title = tag1(ev, 'title');
    const price = formatPrice(ev);
    const location = formatLocation(ev);
    const sold = tag1(ev, 'status') === 'sold';
    const body = ev.content.trim() ? renderMarkdown(ev.content, profiles) : null;
    // "Message seller" opens a DM to the lister - the listing's reason to exist - unless it's your own.
    const contact = s && s.me !== ev.pubkey
        ? html`<a class="listing-contact" href="/messages/${npub(ev.pubkey)}" h-get h-scroll="top instant">Message seller</a>`
        : null;
    return html`
      ${cardTitle(title)}
      <div class="listing-meta">
        ${price ? html`<span class="listing-price">${price}</span>` : null}
        ${location ? html`<span class="listing-loc">${icon('map-pin')}${location}</span>` : null}
        ${sold ? html`<span class="listing-sold">Sold</span>` : null}
      </div>
      ${media}
      ${clampIfTall(body, ev.content, clamp, ev.id)}
      ${contact}`;
}

export const classifiedHandler: KindHandler<SatoriDeps> = {
    kinds: [KIND_LISTING],
    actions: ['reply', 'quote', 'like', 'zap', 'bookmark', 'pin'],
    ref: { as: 'listing', label: '↗ listing', path: (b) => `/a/${b}` }, // inline naddr → a listing embed card
    render(ev, surface, d) {
        if (surface === 'reader') return notWired(surface); // the card carries the listing; no separate reader
        // Reuse the note gallery + lightbox: tiles inline, overlays hoisted out of cardShell's container.
        // Embeds open the raw file (their lightbox can't escape the quote's containment), like note embeds.
        const compact = surface === 'embed';
        const items = imageItems(listingImages(ev));
        const body = listingBody(ev, d.profiles, d.s, surface !== 'focused', mediaTiles(items, !compact));
        return cardShell(ev, d.profiles, d.s, body, { compact, lightboxes: compact ? undefined : mediaOverlays(items) });
    },
};
