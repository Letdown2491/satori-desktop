// Relay timelines, dark-wisp style: browsing a relay is entirely inline (no settings). The feed switcher's
// "Browse a relay…" opens relayPicker (a modal): type any relay URL, or pick a favorite / one of your own
// relays. While viewing a relay, relayFeedBar shows it + a star to favorite it (the lightweight "save").

import { html, raw, type SafeHtml } from '../html.ts';
import { icon } from './svg.ts';
import { modalClose } from './compose.ts';
import { relayLabel, type SavedRelay } from '../data/relay-favorites.ts';
import { relayInfoCached } from '../data/relay-info.ts';

/** A favorite-toggle star. `target`/`swap` define what the toggle re-renders; `id` is set on the relay-bar
 * star (so it can self-swap) and omitted on picker rows. Shared by the bar and the picker list. */
function starButton(fav: boolean, cls: string, post: string, target: string, swap: string, id?: string): SafeHtml {
    const lbl = fav ? 'Remove from favorites' : 'Add to favorites';
    return html`<button${id ? raw(` id="${id}"`) : raw('')} class="${cls}${fav ? ' on' : ''}" h-post="${post}" h-target="${target}" h-swap="${swap}" h-push-url="false" title="${lbl}" aria-label="${lbl}" aria-pressed="${fav ? 'true' : 'false'}">${fav ? '★' : '☆'}</button>`;
}

/** The favorite star for the current relay (the relay-feed bar): posts a toggle and re-renders itself. */
export function favStar(url: string, fav: boolean): SafeHtml {
    return starButton(fav, 'relay-fav', `/relay/favorite?r=${encodeURIComponent(url)}`, '#relay-fav', 'outer', 'relay-fav');
}

/** The bar above a relay timeline: the relay's label + a favorite star. With a cached NIP-11
 * document the relay's self-declared name leads and its description runs beside it, so "whose
 * relay am I reading" is answered by the relay itself. The real HOST stays visible next to a
 * self-declared name (not tooltip-only): the name is the relay's word, and a hostile relay
 * could otherwise present as some other relay entirely. */
export function relayFeedBar(url: string, fav: boolean): SafeHtml {
    const info = relayInfoCached(url);
    return html`<div class="relay-bar"><span class="relay-bar-name" title="${url}">${icon('globe')} ${relayLabel(url, info?.name)}</span>${info?.name ? html`<span class="relay-bar-host">${relayLabel(url)}</span>` : null}${info?.description ? html`<span class="relay-bar-desc" title="${info.description}">${info.description}</span>` : null}${favStar(url, fav)}</div>`;
}

/** The "type a relay URL → Browse" form, shared by the modal picker and the full-page fallback. */
function pickGoForm(): SafeHtml {
    return html`<form class="relay-pick-go" action="/relay" method="get">
        <input type="text" id="relay-pick-url" name="r" placeholder="wss://relay.example.com" autocomplete="off" spellcheck="false">
        <button type="submit" class="ghost">Browse</button>
      </form>`;
}

/** The relay picker modal - opened from the switcher's "Browse a relay…". Type any relay URL to browse, or
 * pick a favorite / one of your own relays. Everything inline; no settings involved. */
export function relayPicker(favorites: SavedRelay[], myRelays: string[]): SafeHtml {
    return html`
      <div class="modal-overlay" id="relay-pick">
        <div class="modal">
          <div class="modal-head"><span class="modal-title">Browse a relay</span>${modalClose()}</div>
          ${pickGoForm()}
          <div id="relay-picker-body">${relayPickerBody(favorites, myRelays)}</div>
        </div>
      </div>`;
}

/** Zero-JS / direct-nav fallback: the same picker as a full page (no overlay), so hitting /relay/pick
 * without helmjs still works (the modal path is the JS enhancement). */
export function relayPickerPage(favorites: SavedRelay[], myRelays: string[]): SafeHtml {
    return html`
      <div class="view-pad relay-pick-page">
        <h2 class="page-title">Browse a relay</h2>
        ${pickGoForm()}
        <div id="relay-picker-body">${relayPickerBody(favorites, myRelays)}</div>
      </div>`;
}

/** The picker's list body (favorites + your relays) - re-rendered alone when a star is toggled here. */
export function relayPickerBody(favorites: SavedRelay[], myRelays: string[]): SafeHtml {
    const favUrls = new Set(favorites.map((r) => r.url));
    const others = myRelays.filter((u) => !favUrls.has(u));
    const row = (url: string, name: string | undefined, fav: boolean): SafeHtml => {
        const enc = encodeURIComponent(url);
        return html`<li class="relay-pick-row">
          <a class="relay-pick-name" href="/relay?r=${enc}" h-get h-scroll="top instant" title="${url}">${icon('globe')} ${relayLabel(url, name ?? relayInfoCached(url)?.name)}${!name && relayInfoCached(url)?.name ? html` <span class="relay-pick-host">${relayLabel(url)}</span>` : null}</a>
          ${starButton(fav, 'relay-fav-mini', `/relay/favorite?r=${enc}&from=pick`, '#relay-picker-body', 'inner')}
        </li>`;
    };
    return html`
      ${favorites.length ? html`<div class="relay-pick-head">Favorites</div><ul class="relay-pick-list">${favorites.map((r) => row(r.url, r.name, true))}</ul>` : null}
      ${others.length ? html`<div class="relay-pick-head">Your relays</div><ul class="relay-pick-list">${others.map((u) => row(u, undefined, false))}</ul>` : null}
      ${!favorites.length && !others.length ? html`<p class="relay-pick-empty">Type a relay URL above to browse its timeline.</p>` : null}`;
}
