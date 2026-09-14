// NIP-52 calendar EVENTS (kind 31922 date-based, 31923 time-based) - addressable, so reached via
// /a/<naddr> (and rendered as an encounter card, not pulled into a feed). A SIMPLE kind on the
// cardShell pattern, like picture/podcast; the NEW piece here is WHEN/WHERE rendering: a date-based
// event's start/end are "YYYY-MM-DD" strings, a time-based event's are unix seconds with an optional
// IANA `start_tzid`. We format the time-based event IN ITS OWN timezone (an event happens in its
// locale, not the viewer's) via Intl + the tzid, falling back to the daemon's local tz when absent.

import type { KindHandler } from './registry.ts';
import { type SatoriDeps, notWired } from './deps.ts';
import { html, type SafeHtml } from '../html.ts';
import { imgSrc, renderContent, extLink } from '../render/content.ts';
import { cardShell, cardTitle, clampWrap, isTallText, naddrFor } from '../render/note.ts';
import { calActions } from '../render/calendar.ts';
import { icon } from '../render/svg.ts';
import type { ProfileMap } from '../render/util.ts';
import { KIND_CALENDAR_DATE, KIND_CALENDAR_TIME } from '../nostr/nip52.ts';
import { tag1 } from '../nostr/tags.ts';
import type { NostrEvent } from '../nostr/types.ts';

/** Date-based (31922): "YYYY-MM-DD" parsed as a bare calendar date (UTC, so no tz shift moves the day),
 * formatted "Sat, Jun 28, 2026"; a distinct `end` becomes a range. */
function formatDateRange(start: string, end: string): string {
    const fmt = (iso: string): string => {
        const m = /^(\d{4})-(\d{2})-(\d{2})/.exec(iso);
        if (!m) return iso;
        return new Date(Date.UTC(+m[1]!, +m[2]! - 1, +m[3]!))
            .toLocaleDateString('en-US', { weekday: 'short', month: 'short', day: 'numeric', year: 'numeric', timeZone: 'UTC' });
    };
    const s = fmt(start);
    return !end || end === start ? s : `${s} - ${fmt(end)}`;
}

/** The short timezone label (e.g. "EDT") for a date in a given IANA zone; falls back to the raw tzid
 * if it's unknown/invalid (Intl throws), so a bad tzid never breaks the card. */
function tzLabel(date: Date, tz: string): string {
    try {
        return new Intl.DateTimeFormat('en-US', { timeZone: tz, timeZoneName: 'short' })
            .formatToParts(date).find((p) => p.type === 'timeZoneName')?.value ?? tz;
    } catch { return tz; }
}

/** Time-based (31923): unix seconds + optional IANA tzid → "Sat, Jun 28, 2026, 7:00 PM - 9:00 PM EDT",
 * collapsing to one date when start/end share a day. With no tzid we format in the daemon's local tz
 * (best effort) and omit the label. An invalid tzid degrades to local time rather than throwing. */
function formatTimeRange(start: string, end: string, tzid: string): string {
    const startN = Number(start), endN = Number(end);
    if (!Number.isFinite(startN)) return '';
    let tz: string | undefined = tzid || undefined;
    const sd = new Date(startN * 1000);
    try { if (tz) new Intl.DateTimeFormat('en-US', { timeZone: tz }); } catch { tz = undefined; } // invalid tzid → local
    const dateOf = (d: Date): string => d.toLocaleDateString('en-US', { weekday: 'short', month: 'short', day: 'numeric', year: 'numeric', timeZone: tz });
    const timeOf = (d: Date): string => d.toLocaleTimeString('en-US', { hour: 'numeric', minute: '2-digit', timeZone: tz });
    const label = tz ? ` ${tzLabel(sd, tz)}` : '';
    const head = `${dateOf(sd)}, ${timeOf(sd)}`;
    if (!Number.isFinite(endN) || endN <= startN) return `${head}${label}`;
    const ed = new Date(endN * 1000);
    const sameDay = dateOf(sd) === dateOf(ed);
    return sameDay ? `${head} - ${timeOf(ed)}${label}` : `${head}${label} - ${dateOf(ed)}, ${timeOf(ed)}${label}`;
}

/** The human "when" line for either calendar kind. */
function whenOf(ev: NostrEvent): string {
    const start = tag1(ev, 'start'), end = tag1(ev, 'end');
    if (!start) return '';
    return ev.kind === KIND_CALENDAR_DATE ? formatDateRange(start, end) : formatTimeRange(start, end, tag1(ev, 'start_tzid'));
}

/** A `location` is free-form (address / room / video-call link); render a url as a link, else text. */
function locationLine(loc: string): SafeHtml {
    return /^https?:\/\//i.test(loc) ? extLink(loc, loc) : html`${loc}`;
}

/** The calendar event BODY (title + when + where + cover + description) for the shared cardShell. The
 * when/where meta sits right under the title - it's the payload of an event, always shown. The flyer +
 * write-up are the BULK, so in a list (any non-focused surface) they collapse TOGETHER behind a Show-more
 * (unlike podcast, where the long text was the bulk and the cover stayed - here the IMAGE is the height,
 * so clamping just the text wouldn't shrink the card). A flyer alone usually overflows, so an image OR
 * tall text arms the clamp; the focused view shows everything in full. */
function eventBody(ev: NostrEvent, profiles: ProfileMap | undefined, clamp: boolean, actions: SafeHtml | null): SafeHtml {
    const title = tag1(ev, 'title');
    const when = whenOf(ev);
    const location = tag1(ev, 'location');
    const image = tag1(ev, 'image');
    const notes = ev.content.trim() ? renderContent(ev.content, profiles, false) : null;
    const detail = html`
      ${image ? html`<img class="media" src="${imgSrc(image)}" alt="${title}" loading="lazy">` : null}
      ${notes}`;
    const collapsible = clamp && (!!image || (!!notes && isTallText(ev.content)));
    return html`
      ${cardTitle(title)}
      <div class="cal-meta">
        ${when ? html`<div class="cal-line">${icon('calendar')}<span>${when}</span></div>` : null}
        ${location ? html`<div class="cal-line">${icon('map-pin')}<span>${locationLine(location)}</span></div>` : null}
      </div>
      ${actions}
      ${collapsible ? clampWrap(detail, ev.id) : detail}`;
}

export const calendarEventHandler: KindHandler<SatoriDeps> = {
    kinds: [KIND_CALENDAR_DATE, KIND_CALENDAR_TIME],
    actions: ['reply', 'quote', 'like', 'zap', 'bookmark', 'pin'],
    ref: { as: 'event', label: '↗ event', path: (b) => `/a/${b}` }, // inline naddr → an event embed card
    render(ev, surface, d) {
        if (surface === 'reader') return notWired(surface); // no dedicated reader page; the card carries the event
        // RSVP + Add-to-calendar sit under the meta (always visible, above the collapsible flyer), but only
        // when logged in and not inside an embed (a quoted event stays a plain preview).
        const actions = d.s && surface !== 'embed' ? calActions(naddrFor(ev)) : null;
        return cardShell(ev, d.profiles, d.s, eventBody(ev, d.profiles, surface !== 'focused', actions), { compact: surface === 'embed' });
    },
};
