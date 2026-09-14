// NIP-52 calendar events. The two RENDERABLE kinds are the events themselves: a DATE-based event
// (31922, all-day/multi-day, `start`/`end` are "YYYY-MM-DD") and a TIME-based event (31923, `start`/
// `end` are unix seconds with optional IANA `start_tzid`). Both are addressable (reached via /a/<naddr>).
// Acting on an event = an RSVP (31925, accepted/declined/tentative). The other NIP-52 kind, calendar
// collection 31924, isn't an event card. Kind homes live in their NIP, like KIND_PICTURE (nip68).

import type { NostrEvent, UnsignedEvent } from './types.ts';
import { tag1, coordinateOf } from './tags.ts';

export const KIND_CALENDAR_DATE = 31922;
export const KIND_CALENDAR_TIME = 31923;
export const KIND_CALENDAR_RSVP = 31925;

export type RsvpStatus = 'accepted' | 'declined' | 'tentative';
export function isRsvpStatus(x: unknown): x is RsvpStatus {
    return x === 'accepted' || x === 'declined' || x === 'tentative';
}

/** The addressable coordinate `kind:pubkey:dtag` for an event - the RSVP's `a` target + stable `d`. */
export const eventCoordinate = coordinateOf;

/** Build the RSVP (31925) template. `d` = the event coordinate, so a user's RSVP to a given event is a
 * SINGLE addressable event that REPLACES on change (no pile-up of stale RSVPs); `a` points at the event,
 * `p` notifies the organizer. `fb` (free/busy) is set busy when going, omitted otherwise (and per spec
 * must be omitted when declined). The relay hint rides the `a` tag when we have one. */
export function buildRsvp(me: string, coord: string, organizer: string, status: RsvpStatus, relayHint?: string): UnsignedEvent {
    const tags: string[][] = [
        relayHint ? ['a', coord, relayHint] : ['a', coord],
        ['d', coord],
        ['status', status],
        ['p', organizer],
    ];
    if (status === 'accepted') tags.push(['fb', 'busy']);
    return { kind: KIND_CALENDAR_RSVP, created_at: Math.floor(Date.now() / 1000), pubkey: me, content: '', tags };
}

// --- iCalendar (.ics) export -----------------------------------------------

/** Escape a TEXT value per RFC 5545 (backslash, semicolon, comma, newline). */
function icsEscape(s: string): string {
    return s.replace(/\\/g, '\\\\').replace(/;/g, '\\;').replace(/,/g, '\\,').replace(/\r?\n/g, '\\n');
}

/** Fold a content line at 75 octets (approximated by chars; our content is effectively ASCII after
 * escaping), continuation lines starting with a space - strict parsers require this for long lines. */
function icsFold(line: string): string {
    if (line.length <= 75) return line;
    const out: string[] = [];
    let i = 0;
    while (i < line.length) { out.push((i ? ' ' : '') + line.slice(i, i + (i ? 74 : 75))); i += i ? 74 : 75; }
    return out.join('\r\n');
}

/** unix seconds → iCal UTC stamp "YYYYMMDDTHHMMSSZ". The unix time is absolute, so emitting UTC is
 * timezone-correct in any calendar app without needing a VTIMEZONE block. */
function icsStampUTC(unixSec: number): string {
    return new Date(unixSec * 1000).toISOString().replace(/[-:]/g, '').replace(/\.\d{3}/, '');
}

/** "YYYY-MM-DD" → iCal DATE "YYYYMMDD"; optional day offset (iCal all-day DTEND is EXCLUSIVE). */
function icsDateOnly(iso: string, addDays = 0): string {
    const m = /^(\d{4})-(\d{2})-(\d{2})/.exec(iso);
    if (!m) return '';
    const d = new Date(Date.UTC(+m[1]!, +m[2]! - 1, +m[3]! + addDays));
    return `${d.getUTCFullYear()}${String(d.getUTCMonth() + 1).padStart(2, '0')}${String(d.getUTCDate()).padStart(2, '0')}`;
}

/** Build a single-VEVENT iCalendar document for a calendar event (31922/31923). Date-based events use
 * VALUE=DATE all-day fields (DTEND made exclusive); time-based use UTC timestamps. */
export function buildIcs(ev: NostrEvent): string {
    const title = tag1(ev, 'title') || 'Event';
    const start = tag1(ev, 'start');
    const end = tag1(ev, 'end');
    const location = tag1(ev, 'location');
    const lines: string[] = ['BEGIN:VCALENDAR', 'VERSION:2.0', 'PRODID:-//Satori//NIP-52//EN', 'CALSCALE:GREGORIAN', 'BEGIN:VEVENT'];
    lines.push(`UID:${eventCoordinate(ev)}`);
    lines.push(`DTSTAMP:${icsStampUTC(ev.created_at)}`);
    if (ev.kind === KIND_CALENDAR_DATE) {
        if (start) lines.push(`DTSTART;VALUE=DATE:${icsDateOnly(start)}`);
        lines.push(`DTEND;VALUE=DATE:${icsDateOnly(end || start, 1)}`); // exclusive: day AFTER the last day
    } else {
        const s = Number(start);
        if (Number.isFinite(s)) lines.push(`DTSTART:${icsStampUTC(s)}`);
        const e = Number(end);
        if (Number.isFinite(e) && e > s) lines.push(`DTEND:${icsStampUTC(e)}`);
    }
    lines.push(`SUMMARY:${icsEscape(title)}`);
    if (ev.content.trim()) lines.push(`DESCRIPTION:${icsEscape(ev.content.trim())}`);
    if (location) lines.push(`LOCATION:${icsEscape(location)}`);
    lines.push('END:VEVENT', 'END:VCALENDAR');
    return lines.map(icsFold).join('\r\n') + '\r\n';
}
