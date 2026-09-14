// The UNKNOWN-KIND fallback: the NATEOAS frontier. A kind with no registered handler used to render
// AS A NOTE (author + content + reply/like/zap row) - a masquerade: it claimed Satori understood an
// event it doesn't. This renders an honest, calm card instead - it names the kind, shows the raw
// content if any, and offers "open in an app that supports this", with the handler DISCOVERED from
// the network via NIP-89 (kind:31990) rather than assumed. Graceful degradation extended to the
// manifest layer: Satori says what it can't do and points elsewhere, instead of faking it.

import { neventEncode } from 'nostr-tools/nip19';
import { html, join, safeUrl, type SafeHtml } from '../html.ts';
import { tag1 } from '../nostr/tags.ts';
import { avatar, displayName, npub, shortNpub, timeAgo } from '../render/util.ts';
import { kindLabel, handlerUrl, type HandlerInfo } from '../nostr/nip89.ts';
import type { NostrEvent } from '../nostr/types.ts';
import { type SatoriDeps, notWired } from './deps.ts';
import type { KindHandler, Surface } from './registry.ts';

const RAW_CAP = 500; // unknown kinds may carry huge JSON in content; show a bounded, escaped preview
const ALT_CAP = 300; // NIP-31 alt is meant as a short summary; cap defensively (untrusted free text)

/** Best bech32 reference for this event: the entity the user referenced (embed/thread), else a
 * freshly-encoded nevent, else the raw id (njump still resolves it). */
function bechFor(ev: NostrEvent, d: SatoriDeps): string {
    if (d.bech) return d.bech;
    if (d.inThread) return d.inThread;
    try { return neventEncode({ id: ev.id, author: ev.pubkey }); } catch { return ev.id; }
}

/** njump opens any entity in a browser - the universal fallback chip when no NIP-89 handler exists. */
function njumpChip(bech: string): SafeHtml {
    return html`<a class="event-chip njump" href="https://njump.me/${bech}" target="_blank" rel="noopener noreferrer">njump</a>`;
}

/** The discovery fragment (lazy /handlers response): NIP-89 web handlers as app chips, then njump.
 * Handler URLs are from STRANGERS' events, so each is safeUrl-gated (drop unsafe). */
export function handlerLinks(handlers: HandlerInfo[], bech: string, entity: string): SafeHtml {
    const chips: SafeHtml[] = [];
    for (const h of handlers.slice(0, 4)) {
        const href = safeUrl(handlerUrl(h, bech, entity) ?? '');
        if (href === '#') continue; // unsafe scheme / no template - never trust a stranger's URL blindly
        chips.push(html`<a class="event-chip" href="${href}" target="_blank" rel="noopener noreferrer">${h.name || shortNpub(h.pubkey)}</a>`);
    }
    chips.push(njumpChip(bech));
    return join(chips);
}

/** The lazy "open in" row: a muted lead + app chips. njump shows immediately (always works);
 * NIP-89-discovered handlers swap into the chips slot on intersect (poll-box lazy pattern). */
function openInSlot(ev: NostrEvent, bech: string): SafeHtml {
    const id = `open-${ev.id.slice(0, 16)}`; // ev.id is hex → dom-safe
    // The intersect trigger lives on the DIV, not the chips span: the span is display:contents (no
    // layout box) so an IntersectionObserver can't watch it. Trigger on the box, swap into the span.
    return html`<div class="event-open" h-get="/handlers/${bech}?k=${String(ev.kind)}" h-trigger="intersect once" h-target="#${id}" h-swap="inner" h-push-url="false">
          <span class="event-open-lead">Open in</span>
          <span class="event-open-apps" id="${id}">${njumpChip(bech)}</span>
        </div>`;
}

/** The honest unknown-event card. Embed = a compact quote-style card; timeline/focused = the note
 * shell MINUS the action row (reply/like/zap on an unknown kind is the masquerade we're removing). */
function unsupportedCard(ev: NostrEvent, surface: Surface, d: SatoriDeps): SafeHtml {
    const { profiles } = d;
    const bech = bechFor(ev, d);
    const label = kindLabel(ev.kind);
    // NIP-31: the author's own "what this event is" summary for clients that can't render the
    // kind. When present it IS the card body - it reads better than a raw JSON preview, so the
    // preview only shows without it. Plain escaped text, no entity resolution (untrusted).
    const alt = tag1(ev, 'alt').trim().slice(0, ALT_CAP);
    const raw = ev.content?.trim();
    // Clamp + fade only when it's long enough to actually overflow; short content shows in full.
    const long = !!raw && raw.length > 300;
    const preview = alt ? html`<div class="content event-alt">${alt}</div>`
        : raw ? html`<div class="content event-raw${long ? ' clamped' : ''}">${raw.slice(0, RAW_CAP)}</div>` : null;

    if (surface === 'embed') {
        return html`<a class="quote-label" href="https://njump.me/${bech}" target="_blank" rel="noopener noreferrer">↗ ${label}</a
            ><div class="quote-head"><a class="quote-author-link" href="/u/${npub(ev.pubkey)}" h-scroll="top instant">${avatar(ev.pubkey, profiles?.get(ev.pubkey)?.picture, 'xs')}<span class="quote-author">${displayName(ev.pubkey, profiles)}</span></a></div
            ><div class="quote-body event-unsupported">${alt || `Satori doesn't render this kind yet.`}</div>`;
    }
    return html`
      <li class="note">
        <a href="/u/${npub(ev.pubkey)}" aria-label="author" h-scroll="top instant">${avatar(ev.pubkey, profiles?.get(ev.pubkey)?.picture)}</a>
        <div class="note-body">
          <div class="note-head">
            <a class="author" href="/u/${npub(ev.pubkey)}" h-scroll="top instant">${displayName(ev.pubkey, profiles)}</a>
            <span class="time">${timeAgo(ev.created_at)}</span>
          </div>
          <div class="event-kind">Unsupported event · ${label}</div>
          ${preview}
          ${openInSlot(ev, bech)}
        </div>
      </li>`;
}

export const fallbackHandler: KindHandler<SatoriDeps> = {
    kinds: [],
    // No action vocabulary: reply/quote/like/zap on a kind Satori doesn't understand is the masquerade.
    render(ev, surface, d) {
        if (surface === 'timeline' || surface === 'focused' || surface === 'embed') return unsupportedCard(ev, surface, d);
        return notWired(surface);
    },
};
