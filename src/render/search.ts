// The search page (NIP-50): a query box + People and Notes result sections. LIVE as-you-type -
// the input debounce-swaps just #search-results (helmjs h-trigger="input debounce:400", min 2 chars
// server-side so the relay fan-out is throttled); Enter / the button does a boosted GET to /search?q=
// so results stay shareable/bookmarkable, and it degrades to a plain submit with JS off. searchResults
// is the swappable region. See [[search-plan]].

import { html, join, raw, type SafeHtml } from '../html.ts';
import { icon, enso } from './svg.ts';
import { quote } from './quotes.ts';
import { avatar, displayName, npub, type ProfileMap } from './util.ts';
import { withEmoji } from './content.ts';
import { noteList } from './note.ts';
import type { Profile } from '../data/profiles.ts';
import type { NostrEvent } from '../nostr/types.ts';
import type { Session } from '../session.ts';

function personRow(pubkey: string, profile: Profile, profiles: ProfileMap): SafeHtml {
    return html`
      <li class="search-person">
        <a class="search-person-link" href="/u/${npub(pubkey)}" h-get h-prefetch="hover" h-scroll="top instant">
          ${avatar(pubkey, profile.picture, 'sm')}
          <span class="search-person-text">
            <span class="search-person-name">${withEmoji(displayName(pubkey, profiles), profile.emoji)}${profile.nip05Verified ? html`<span class="badge">✓</span>` : null}</span>
            ${profile.nip05 ? html`<span class="search-person-nip05">${profile.nip05}</span>` : null}
            ${profile.about ? html`<span class="search-person-about">${withEmoji(profile.about, profile.emoji)}</span>` : null}
          </span>
        </a>
      </li>`;
}

/** The swappable results region (empty/seek state OR the People/Notes tabset) - the live-as-you-type
 * target swapped into #search-results, and embedded in the full page below. Returns the INNER of
 * #search-results (h-swap="inner"). */
export function searchResults(
    q: string,
    people: { pubkey: string; profile: Profile }[],
    notes: NostrEvent[],
    s: Session & { me: string },
): SafeHtml {
    // On-theme empty states: a still ensō + a rotating Zen/Taoist line (the input's placeholder
    // carries the actual instruction). Pre-query / still-typing draws from the `seek` pool
    // (naming/seeking); no-results draws from `empty` with the query underneath.
    if (!q) return html`<div class="view-empty search-empty"><p class="search-quote">${quote('seek')}</p>${enso(40, true)}<p class="search-ops">Filters: #tag · by:name · p:name · has:image · site:domain · since:2024-01-01</p></div>`;
    if (people.length === 0 && notes.length === 0) {
        return html`<div class="view-empty search-empty"><p class="search-quote">${quote('empty')}</p>${enso(40, true)}<p class="search-noresults">Nothing found for “${q}”.</p></div>`;
    }
    // CSS radio-hack tabs: both result sets are already fetched, so switching is instant + zero-JS
    // (native radio arrow-key nav works). Count badges keep the other tab discoverable. Default to
    // whichever tab has results (People first).
    const peopleFirst = people.length > 0;
    return html`
      <div class="tabset">
        <input type="radio" name="stab" id="stab-people" class="tabset-radio"${peopleFirst ? raw(' checked') : raw('')}>
        <input type="radio" name="stab" id="stab-notes" class="tabset-radio"${peopleFirst ? raw('') : raw(' checked')}>
        <div class="tabset-list" role="tablist">
          <label for="stab-people" class="tabset-tab" role="tab">People <span class="tabset-count">${String(people.length)}</span></label>
          <label for="stab-notes" class="tabset-tab" role="tab">Notes <span class="tabset-count">${String(notes.length)}</span></label>
        </div>
        <section class="tabset-panel panel-people" role="tabpanel" aria-label="People results">
          ${people.length ? html`<ul class="search-people">${join(people.map((p) => personRow(p.pubkey, p.profile, s.profiles)))}</ul>` : html`<p class="status search-hint">No people found.</p>`}
        </section>
        <section class="tabset-panel panel-notes" role="tabpanel" aria-label="Note results">
          ${notes.length ? html`<ul class="feed">${noteList(notes, s.profiles, s)}</ul>` : html`<p class="status search-hint">No notes found.</p>`}
        </section>
      </div>`;
}

export function searchPage(
    q: string,
    people: { pubkey: string; profile: Profile }[],
    notes: NostrEvent[],
    s: Session & { me: string },
): SafeHtml {
    // The input debounce-swaps #search-results live (h-get + input trigger); the form's submit /
    // Enter is the boosted full nav (shareable URL). Both degrade to a plain GET with JS off.
    const form = html`
      <form class="search-form" action="/search" method="get" role="search">
        <input class="search-input" id="search-input" type="search" name="q" value="${q}"
               placeholder="Search people and notes…" aria-label="Search" autocomplete="off" autofocus
               h-get="/search" h-trigger="input debounce:400" h-include="#search-input"
               h-target="#search-results" h-swap="inner" h-push-url="false">
        <button type="submit" class="search-go" aria-label="Search">${icon('search')}</button>
      </form>`;
    return html`${form}<div id="search-results" aria-live="polite">${searchResults(q, people, notes, s)}</div>`;
}
