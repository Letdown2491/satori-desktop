// NIP-52 calendar event ACTIONS - the interactive bits of the event card that the manifest handler and
// the routes both render: an RSVP control (Going / Maybe / Can't go, posting a kind:31925) and an
// "Add to calendar" .ics download. Mirrors render/poll.ts: the RSVP box renders a zero-JS votable form
// immediately, then lazily hydrates (h-trigger="intersect once") to reflect your CURRENT RSVP - so the
// card paints without blocking on a relay round-trip, and corrects the active button once state loads.

import { html, join, type SafeHtml } from '../html.ts';
import type { RsvpStatus } from '../nostr/nip52.ts';

/** A stable DOM id from the naddr (djb2 → base36). The same id is computed in the routes for H-Retarget,
 * so it must depend only on the naddr (which both sides have), not the event's per-version id. */
export function rsvpId(naddr: string): string {
    let h = 5381;
    for (let i = 0; i < naddr.length; i++) h = ((h << 5) + h + naddr.charCodeAt(i)) >>> 0;
    return `rsvp-${h.toString(36)}`;
}

const CHOICES: { status: RsvpStatus; label: string }[] = [
    { status: 'accepted', label: 'Going' },
    { status: 'tentative', label: 'Maybe' },
    { status: 'declined', label: "Can't go" },
];

/** The three RSVP buttons with `mine` highlighted - the inner content the GET hydrate + POST both swap
 * into the box. Each is a real form POST (works zero-JS); helmjs swaps the updated buttons in place. The
 * going count rides INSIDE the "Going" button (events are about gathering, so the scale is real info,
 * not a vanity scoreboard) - deliberately NOT avatar faces, which already mean "conversation" here. */
export function rsvpButtons(naddr: string, mine: RsvpStatus | null, going = 0): SafeHtml {
    const id = rsvpId(naddr);
    return html`<div class="rsvp-buttons">${join(CHOICES.map(({ status, label }) => html`
      <form class="rsvp-form" action="/cal/rsvp/${naddr}" method="post" h-post h-target="#${id}" h-swap="inner" h-push-url="false">
        <button type="submit" name="status" value="${status}" class="rsvp-btn rsvp-${status}${mine === status ? ' active' : ''}"${mine === status ? html` aria-pressed="true"` : html``}>${label}${status === 'accepted' && going > 0 ? html`<span class="rsvp-count">${String(going)}</span>` : null}</button>
      </form>`))}</div>`;
}

/** The RSVP box: an instant ballot that lazily hydrates to your RSVP + the going count via /cal/rsvp. */
export function rsvpBox(naddr: string): SafeHtml {
    const id = rsvpId(naddr);
    return html`<div class="rsvp" id="${id}" h-get="/cal/rsvp/${naddr}" h-trigger="intersect once" h-target="#${id}" h-swap="inner" h-push-url="false">${rsvpButtons(naddr, null)}</div>`;
}

/** "Add to calendar": downloads the event as an .ics (local, no signing). */
export function icsLink(naddr: string): SafeHtml {
    return html`<a class="cal-ics" href="/cal/ics/${naddr}" download>Add to calendar</a>`;
}

/** The calendar event's action block (RSVP + .ics), shown under the body when logged in. */
export function calActions(naddr: string): SafeHtml {
    return html`<div class="cal-actions">${rsvpBox(naddr)}${icsLink(naddr)}</div>`;
}
