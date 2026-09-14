// Stateful action button fragments. Each is a <form> whose POST returns the new
// button (helmjs swaps it in place via the form's h-target/h-swap; with JS off the
// server 303-redirects to the same page for a full reload). The continuation
// returns the identical form with toggled state, so the swap is seamless.

import { html, type SafeHtml } from '../html.ts';
import { icon, type IconName } from './svg.ts';
import { isOn, type ActionName } from '../actions.ts';
import { isAddressable, coordinateOf } from '../nostr/tags.ts';
import type { NostrEvent } from '../nostr/types.ts';
import { cachedReaction } from '../data/engagement-cache.ts';
import { REACTIONS } from '../data/reactions.ts';
import { userEmojiCached } from '../data/emoji-sets.ts';
import { imgSrc } from './content.ts';
import type { Session } from '../session.ts';

const CUSTOM_CAP = 15; // how many of the user's custom emoji the picker shows - a calm palette, not a keyboard

/** A stable element id for an action+target, used as the swap target. */
function actId(action: ActionName, target: string): string {
    return `act-${action}-${target}`;
}

/** An icon action (bookmark / pin) on a note - the `.note-act` ink glyph that
 * fills + accents when active. The form targets itself and outer-swaps. */
function iconAction(action: ActionName, target: string, on: boolean, name: IconName, title: string): SafeHtml {
    const id = actId(action, target);
    // h-optimistic flips `.active` on the glyph the instant you click (the response
    // swap reconciles to server truth; an h:error - failed publish or rejected
    // signature - reverts it). The target is the .note-act button, since that's what
    // the `.active` styling keys off.
    return html`<form id="${id}" class="act-form" action="/act/${action}/${target}" method="post" h-post h-target="#${id}" h-swap="outer" h-optimistic="class:active" h-optimistic-target="#${id} .note-act">
        <button type="submit" class="note-act ${action} ${on ? 'active' : ''}" title="${on ? `${title} ✓` : title}" aria-label="${title}" aria-pressed="${on ? 'true' : 'false'}">${icon(name, on)}</button>
      </form>`;
}

export function bookmarkButton(s: Session, noteId: string): SafeHtml {
    return iconAction('bookmark', noteId, isOn(s, 'bookmark', noteId), 'bookmark', 'Bookmark');
}

export function pinButton(s: Session, noteId: string): SafeHtml {
    return iconAction('pin', noteId, isOn(s, 'pin', noteId), 'pin', 'Pin to profile');
}

/** Mute-this-person glyph for stranger-facing rows (notifications, relay timelines). Unlike the
 * note-keyed toggles it targets the AUTHOR; on success it removes the whole card (the route
 * returns an empty body retargeted at `#card-<eventId>`), so muting dismisses the item. The
 * glyph flips optimistically the instant you click, then the card drops once mute persists. */
export function muteAct(s: Session, pubkey: string, eventId: string): SafeHtml {
    const id = actId('mute', pubkey);
    const on = isOn(s, 'mute', pubkey);
    return html`<form id="${id}" class="act-form" action="/act/mute/${pubkey}?card=${eventId}" method="post" h-post h-target="#card-${eventId}" h-swap="outer" h-transition h-optimistic="class:active" h-optimistic-target="#${id} .note-act">
        <button type="submit" class="note-act mute ${on ? 'active' : ''}" title="${on ? 'Muted ✓' : 'Mute this person'}" aria-label="Mute this person" aria-pressed="${on ? 'true' : 'false'}">${icon('mute', on)}</button>
      </form>`;
}

/** A reaction's glyph: a custom NIP-30 image when there's a url, a plain heart for '+'/'' (the default
 * like), else the chosen unicode emoji character. */
function reactionGlyph(emoji: string, url?: string): SafeHtml {
    if (typeof url === 'string' && url) return html`<img class="emoji" src="${imgSrc(url)}" alt=":${emoji}:" loading="lazy">`;
    return emoji === '+' || emoji === '' ? icon('like', true) : html`<span class="react-emoji">${emoji}</span>`;
}

/** The reaction control (NIP-25). A like is a per-note kind:7 event, so state comes from the engagement
 * cache (not a list). Two states:
 *  - reacted: a single button showing YOUR emoji; clicking it retracts (the route sees an existing
 *    reaction and emits the kind:5 delete - so the submitted emoji is irrelevant there).
 *  - not reacted: a one-click heart PLUS a CSS-revealed palette (zero-JS via a hidden checkbox, like the
 *    compose toggles) of the rest of the curated set; each emoji is a submit button carrying its value.
 * No counts of others are ever shown - your reaction is a personal gesture, not a scoreboard. */
export function likeButton(s: Session, noteId: string, author: string, kind: number, eventId?: string): SafeHtml {
    const id = `like-${noteId}`;
    const mine = s.me ? cachedReaction(s.me, noteId) : undefined;
    // h-optimistic adds `.reacting` the instant you submit (pick or retract), so the gesture registers
    // before the relay round-trip: the picker collapses + the smiley takes the accent on a pick, the heart
    // empties on a retract. The response swap reconciles to the real glyph; an h:error reverts the class.
    // `k` carries the reacted event's kind so the kind:7's NIP-25 `k` tag is accurate (not hardcoded 1).
    // `eid` (addressable targets only) carries the concrete event id so the required `e` tag needs no
    // extra relay fetch; postLike falls back to resolving it only when absent.
    const head = html`<form id="${id}" class="act-form react" action="/like/${noteId}" method="post" h-post h-target="#${id}" h-swap="outer" h-optimistic="class:reacting" h-optimistic-target="#${id}">
        <input type="hidden" name="author" value="${author}">
        <input type="hidden" name="k" value="${String(kind)}">
        ${eventId ? html`<input type="hidden" name="eid" value="${eventId}">` : null}`;
    if (mine) {
        return html`${head}
        <button type="submit" name="emoji" value="${mine.emoji}" class="note-act like active" title="Remove reaction" aria-label="Remove reaction" aria-pressed="true">${reactionGlyph(mine.emoji, mine.url)}</button>
      </form>`;
    }
    // ONE control: a smiley that reveals the whole palette (heart included, shown as ❤️). Zero-JS via the
    // hidden-checkbox toggle. Picking any emoji reacts; there's no separate one-click heart button. The
    // user's own NIP-30 custom emoji (capped) follow the curated unicode set as small images.
    const custom = s.me ? Object.entries(userEmojiCached(s.me)).slice(0, CUSTOM_CAP) : [];
    return html`${head}
        <input type="checkbox" id="pal-${noteId}" class="react-toggle">
        <label class="note-act react-more" for="pal-${noteId}" title="React" aria-label="React">${icon('smile')}</label>
        <span class="react-palette">${REACTIONS.map((e) => { const g = e === '+' ? '❤️' : e; return html`<button type="submit" name="emoji" value="${e}" class="react-opt" title="React ${g}" aria-label="React ${g}">${g}</button>`; })}${custom.map(([code, url]) => html`<button type="submit" name="emoji" value="${code}" class="react-opt" title="React :${code}:" aria-label="React ${code}"><img class="emoji" src="${imgSrc(url)}" alt=":${code}:" loading="lazy"></button>`)}</span>
      </form>`;
}

/** Delete-your-own-post glyph (NIP-09). Not part of any kind's action vocabulary: deletion is
 * an OWNERSHIP affordance, so noteActions/articleActions append it for every own event
 * regardless of kind. The POST returns an inline confirm in place of the glyph (nothing is
 * signed yet); only the confirm builds + publishes the kind:5. `coord` rides along for
 * addressable kinds so the deletion carries the `a` tag beside the `e`. */
export function deleteActRaw(id: string, kind: number, coord: string): SafeHtml {
    return html`<form id="del-${id}" class="act-form" action="/delete/${id}" method="post" h-post h-target="#del-${id}" h-swap="outer">
        <input type="hidden" name="k" value="${String(kind)}">
        ${coord ? html`<input type="hidden" name="a" value="${coord}">` : null}
        <button type="submit" class="note-act delete" title="Delete" aria-label="Delete">${icon('trash')}</button>
      </form>`;
}

export function deleteAct(ev: NostrEvent): SafeHtml {
    return deleteActRaw(ev.id, ev.kind, isAddressable(ev.kind) ? coordinateOf(ev) : '');
}

/** A text action (follow / mute) on a profile - `.follow-btn` / `.mute-btn`. The
 * on/off labels are BOTH rendered (shown by CSS via the state class), so an
 * optimistic toggle of that class flips the look AND the text instantly; the
 * response swap reconciles, an h:error reverts. */
function textAction(action: ActionName, target: string, on: boolean, cls: string, stateClass: string, onLabel: string, offLabel: string): SafeHtml {
    const id = actId(action, target);
    return html`<form id="${id}" class="act-form" action="/act/${action}/${target}" method="post" h-post h-target="#${id}" h-swap="outer" h-optimistic="class:${stateClass}" h-optimistic-target="#${id} button">
        <button type="submit" class="${cls} ${on ? stateClass : ''}" aria-pressed="${on ? 'true' : 'false'}">
          <span class="label-off">${offLabel}</span><span class="label-on">${onLabel}</span>
        </button>
      </form>`;
}

export function followButton(s: Session, pubkey: string): SafeHtml {
    return textAction('follow', pubkey, isOn(s, 'follow', pubkey), 'follow-btn', 'following', 'Following', 'Follow');
}

export function muteButton(s: Session, pubkey: string): SafeHtml {
    return textAction('mute', pubkey, isOn(s, 'mute', pubkey), 'mute-btn ghost', 'muted', 'Muted', 'Mute');
}

/** The right fragment to return after a toggle, by action name. */
export function actionButton(s: Session, action: ActionName, target: string): SafeHtml {
    switch (action) {
        case 'bookmark': return bookmarkButton(s, target);
        case 'pin': return pinButton(s, target);
        case 'follow': return followButton(s, target);
        case 'mute': return muteButton(s, target);
    }
}
