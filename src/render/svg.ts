// Server-side ports of Satori's ui/enso.ts and ui/icons.ts - emitting SafeHtml
// SVG instead of DOM nodes. Same geometry/path data, so the look is identical.

import { html, raw, type SafeHtml } from '../html.ts';

// --- 円相 Ensō ---------------------------------------------------------------

/** Brushstroke ensō path - replicated from ui/enso.ts (computed once at load). */
function buildEnsoPath(): string {
    const cx = 50, cy = 50, R = 35;
    const a0 = (284 * Math.PI) / 180;
    const sweep = (340 * Math.PI) / 180;
    const wMax = 18, N = 60;
    const outer: string[] = [], inner: string[] = [];
    for (let i = 0; i <= N; i++) {
        const t = i / N;
        const ang = a0 + sweep * t;
        const r = R + 2.4 * Math.sin(ang * 3 + 0.6);
        const taper = Math.pow(Math.sin(Math.PI * t), 0.6);
        const w = wMax * taper * (0.9 + 0.15 * Math.sin(ang * 5));
        const ro = r + w / 2, ri = r - w / 2;
        outer.push(`${(cx + ro * Math.cos(ang)).toFixed(2)} ${(cy + ro * Math.sin(ang)).toFixed(2)}`);
        inner.push(`${(cx + ri * Math.cos(ang)).toFixed(2)} ${(cy + ri * Math.sin(ang)).toFixed(2)}`);
    }
    return `M ${outer[0]} L ${outer.slice(1).join(' L ')} L ${inner.reverse().join(' L ')} Z`;
}

const ENSO_D = buildEnsoPath();

/** The ensō (Zen circle). `still` renders it static (wordmark / empty states). */
const ensoMemo = new Map<string, SafeHtml>();
export function enso(size = 44, still = false): SafeHtml {
    // Fully static per (size, still) - and every coverless article/wiki/NIP card renders one.
    const key = `${size}${still ? 's' : ''}`;
    const hit = ensoMemo.get(key);
    if (hit) return hit;
    const svg = html`<svg class="${still ? 'enso still' : 'enso'}" viewBox="0 0 100 100" width="${size}" height="${size}" aria-hidden="true"><path d="${ENSO_D}" filter="url(#enso-brush)" mask="url(#enso-dry)"></path></svg>`;
    ensoMemo.set(key, svg);
    return svg;
}

/** A quiet empty-state list item: a still ensō + a line. Mirrors Satori's
 * emptyItem() - used for empty feeds, no-notes, and not-found states. */
export function emptyItem(line: string, sub?: string): SafeHtml {
    return html`<li class="empty">${enso(40, true)}<span>${line}</span>${sub ? html`<span class="empty-sub">${sub}</span>` : null}</li>`;
}

/** An empty TIMELINE state in the calm "clearing" look (matches the notifications page): a contemplative
 * quote in quotation marks OVER a still ensō seal - vs emptyItem's ensō-over-a-utilitarian-line, which stays
 * for not-found / loading / count-zero messages. */
export function quoteEmpty(line: string): SafeHtml {
    return html`<li class="empty"><span>“${line}”</span>${enso(40, true)}</li>`;
}

/** Shown in place of an empty timeline / not-found card when the private relay is isolating reads (Only
 * mode, "fetch missing" off): the content probably exists elsewhere, so point at the toggle instead of a
 * dead end. Used only when localIsolated() is true. */
export function gapNotice(): SafeHtml {
    return html`<li class="empty gap-notice">${enso(40, true)}<span>Your private relay doesn't have this.</span><span class="empty-sub"><a href="/settings/relays/private">Turn on “Fetch events not on your private relay”</a> to load it from your normal relays.</span></li>`;
}

/** The brush-texture filter + dry-brush mask the ensō references. Inlined once
 * per page (position:absolute, zero-size). Verbatim from Satori's index.html. */
export const ENSO_DEFS: SafeHtml = raw(`<svg width="0" height="0" class="enso-defs" aria-hidden="true">
  <filter id="enso-brush" x="-25%" y="-25%" width="150%" height="150%" color-interpolation-filters="sRGB">
    <feTurbulence type="fractalNoise" baseFrequency="0.045 0.06" numOctaves="3" seed="6" result="n"/>
    <feDisplacementMap in="SourceGraphic" in2="n" scale="6.5" xChannelSelector="R" yChannelSelector="G"/>
  </filter>
  <mask id="enso-dry" maskUnits="userSpaceOnUse" x="0" y="0" width="100" height="100">
    <rect width="100" height="100" fill="#fff"/>
    <g fill="none" stroke="#000" stroke-linecap="round">
      <circle cx="50" cy="50" r="43.5" stroke-width="1.5" stroke-dasharray="22 12 9 30" transform="rotate(8 50 50)"/>
      <circle cx="50" cy="50" r="39" stroke-width="1.1" stroke-dasharray="14 9 28 16" transform="rotate(64 50 50)"/>
      <circle cx="50" cy="50" r="35" stroke-width="1.4" stroke-dasharray="30 14 10 22" transform="rotate(146 50 50)"/>
      <circle cx="50" cy="50" r="31" stroke-width="1.0" stroke-dasharray="12 10 24 14" transform="rotate(214 50 50)"/>
      <circle cx="50" cy="50" r="27.5" stroke-width="1.3" stroke-dasharray="20 16 8 26" transform="rotate(298 50 50)"/>
    </g>
  </mask>
</svg>`);

// --- monochrome line icons (Feather-style, ported from ui/icons.ts) ---------

export type IconName =
    | 'reply' | 'zap' | 'like' | 'quote' | 'bookmark' | 'pin' | 'more' | 'bell'
    | 'play' | 'globe' | 'image' | 'alert' | 'home' | 'gear' | 'back' | 'notes' | 'thread' | 'search' | 'shield' | 'compose' | 'lock' | 'clock' | 'mute' | 'smile' | 'calendar' | 'map-pin' | 'git' | 'trash';

// Each shape is "<tag attr=\"v\" …/>" raw markup (geometry only - no user data).
const ICONS: Record<IconName, string[]> = {
    reply: ['<path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/>'],
    zap: ['<polygon points="13 2 3 14 12 14 11 22 21 10 12 10 13 2"/>'],
    like: ['<path d="M20.84 4.61a5.5 5.5 0 0 0-7.78 0L12 5.67l-1.06-1.06a5.5 5.5 0 0 0-7.78 7.78l1.06 1.06L12 21.23l7.78-7.78 1.06-1.06a5.5 5.5 0 0 0 0-7.78z"/>'],
    quote: ['<polyline points="17 1 21 5 17 9"/>', '<path d="M3 11V9a4 4 0 0 1 4-4h14"/>', '<polyline points="7 23 3 19 7 15"/>', '<path d="M21 13v2a4 4 0 0 1-4 4H3"/>'],
    bookmark: ['<path d="M19 21l-7-5-7 5V5a2 2 0 0 1 2-2h10a2 2 0 0 1 2 2z"/>'],
    pin: ['<line x1="12" y1="17" x2="12" y2="22"/>', '<path d="M9 10.76a2 2 0 0 1-1.11 1.79l-1.78.9A2 2 0 0 0 5 15.24V16a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1v-.76a2 2 0 0 0-1.11-1.79l-1.78-.9A2 2 0 0 1 15 10.76V7a1 1 0 0 1 1-1 2 2 0 0 0 0-4H8a2 2 0 0 0 0 4 1 1 0 0 1 1 1z"/>'],
    more: ['<circle cx="5" cy="12" r="1.5"/>', '<circle cx="12" cy="12" r="1.5"/>', '<circle cx="19" cy="12" r="1.5"/>'],
    bell: ['<path d="M18 8a6 6 0 0 0-12 0c0 7-3 9-3 9h18s-3-2-3-9"/>', '<path d="M13.73 21a2 2 0 0 1-3.46 0"/>'],
    play: ['<polygon points="8 5 19 12 8 19"/>'],
    globe: ['<circle cx="12" cy="12" r="10"/>', '<line x1="2" y1="12" x2="22" y2="12"/>', '<path d="M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z"/>'],
    image: ['<rect x="3" y="3" width="18" height="18" rx="2" ry="2"/>', '<circle cx="8.5" cy="8.5" r="1.5"/>', '<polyline points="21 15 16 10 5 21"/>'],
    alert: ['<path d="M10.29 3.86L1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z"/>', '<line x1="12" y1="9" x2="12" y2="13"/>', '<line x1="12" y1="17" x2="12.01" y2="17"/>'],
    clock: ['<circle cx="12" cy="12" r="9"/>', '<polyline points="12 7 12 12 15 14"/>'],
    home: ['<path d="M3 9l9-7 9 7v11a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/>', '<polyline points="9 22 9 12 15 12 15 22"/>'],
    notes: ['<line x1="8" y1="6" x2="21" y2="6"/>', '<line x1="8" y1="12" x2="21" y2="12"/>', '<line x1="8" y1="18" x2="21" y2="18"/>', '<line x1="3" y1="6" x2="3.01" y2="6"/>', '<line x1="3" y1="12" x2="3.01" y2="12"/>', '<line x1="3" y1="18" x2="3.01" y2="18"/>'],
    // Two overlapping speech bubbles - "view conversation/thread", distinct from the
    // single-bubble `reply` action icon (Tabler-style `messages`).
    thread: ['<path d="M21 14l-3-3h-7a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1h9a1 1 0 0 1 1 1v10z"/>', '<path d="M14 15v2a1 1 0 0 1-1 1H6l-3 3V11a1 1 0 0 1 1-1h2"/>'],
    search: ['<circle cx="11" cy="11" r="8"/>', '<line x1="21" y1="21" x2="16.65" y2="16.65"/>'],
    // Speaker with a slash (Feather "volume-x") - mute this person.
    mute: ['<polygon points="11 5 6 9 2 9 2 15 6 15 11 19 11 5"/>', '<line x1="23" y1="9" x2="17" y2="15"/>', '<line x1="17" y1="9" x2="23" y2="15"/>'],
    shield: ['<path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z"/>'],
    back: ['<line x1="19" y1="12" x2="5" y2="12"/>', '<polyline points="12 19 5 12 12 5"/>'],
    compose: ['<path d="M12 20h9"/>', '<path d="M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4z"/>'],
    lock: ['<rect x="3" y="11" width="18" height="11" rx="2" ry="2"/>', '<path d="M7 11V7a5 5 0 0 1 10 0v4"/>'],
    // A quiet smiley - the "more reactions" affordance next to the one-click heart (Feather "smile").
    smile: ['<circle cx="12" cy="12" r="10"/>', '<path d="M8 14s1.5 2 4 2 4-2 4-2"/>', '<line x1="9" y1="9" x2="9.01" y2="9"/>', '<line x1="15" y1="9" x2="15.01" y2="9"/>'],
    // Feather "calendar" (when) + "map-pin" (where) - the calendar event's two meta lines.
    calendar: ['<rect x="3" y="4" width="18" height="18" rx="2" ry="2"/>', '<line x1="16" y1="2" x2="16" y2="6"/>', '<line x1="8" y1="2" x2="8" y2="6"/>', '<line x1="3" y1="10" x2="21" y2="10"/>'],
    'map-pin': ['<path d="M21 10c0 7-9 13-9 13s-9-6-9-13a9 9 0 0 1 18 0z"/>', '<circle cx="12" cy="10" r="3"/>'],
    // Feather "git-branch" - the NIP-34 repository glyph.
    git: ['<line x1="6" y1="3" x2="6" y2="15"/>', '<circle cx="18" cy="6" r="3"/>', '<circle cx="6" cy="18" r="3"/>', '<path d="M18 9a9 9 0 0 1-9 9"/>'],
    // Feather "trash-2" - delete your own post (NIP-09).
    trash: ['<polyline points="3 6 5 6 21 6"/>', '<path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/>', '<line x1="10" y1="11" x2="10" y2="17"/>', '<line x1="14" y1="11" x2="14" y2="17"/>'],
    gear: ['<circle cx="12" cy="12" r="3"/>', '<path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"/>'],
};

/** A line icon. `filled` fills the shape (active like/bookmark); `more` is solid. Memoized: the
 * output is fully static per (name, filled), and one feed page calls this hundreds of times. */
const iconMemo = new Map<string, SafeHtml>();
export function icon(name: IconName, filled = false): SafeHtml {
    const key = filled ? `${name}+f` : name;
    const hit = iconMemo.get(key);
    if (hit) return hit;
    const dots = name === 'more';
    const fill = dots || filled ? 'currentColor' : 'none';
    const stroke = dots ? 'none' : 'currentColor';
    const svg = raw(`<svg class="icon" viewBox="0 0 24 24" width="18" height="18" aria-hidden="true" fill="${fill}" stroke="${stroke}" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round">${ICONS[name].join('')}</svg>`);
    iconMemo.set(key, svg);
    return svg;
}
