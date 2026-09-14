// Server-side render helpers - the string-output counterparts of Satori's
// ui/util.ts. A ProfileMap (pubkey → profile) is threaded through render calls so
// author/mention labels resolve to names where known, and short npub otherwise.

import { npubEncode } from 'nostr-tools/nip19';
import { html, safeUrl, type SafeHtml } from '../html.ts';
import type { Profile } from '../data/profiles.ts';

export type ProfileMap = Map<string, Profile>;

/** The `min` attribute for a schedule datetime-local input: ~1 minute from now in the
 * local tz (the daemon's tz == the single user's). Shared by the note, poll, and article
 * composers so "schedule for later" can't be set to a past time. */
export function minScheduleValue(): string {
    const p = (n: number) => String(n).padStart(2, '0');
    const d = new Date(Date.now() + 60_000);
    return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}T${p(d.getHours())}:${p(d.getMinutes())}`;
}

/** Avatar image (Satori markup): the picture if safe, over a deterministic ink
 * tint so a missing/loading image still reads as a filled disc. */
export function avatar(pubkey: string, picture: string | undefined, size?: 'sm' | 'xs' | 'lg'): SafeHtml {
    // Deterministic placeholder colour as a HUE-BUCKET CLASS (no inline CSS, so the
    // strict style-src holds): the per-pubkey hue is quantized to 10° buckets (avatar-h0..35),
    // each defined in the stylesheet. Visually identical to the old exact hsl().
    const cls = `avatar avatar-h${avatarHueBucket(pubkey)}${size ? ` avatar-${size}` : ''}`;
    const src = avatarSrc(picture);
    // No picture (or not loaded yet) → a filled colored circle, not a srcless <img>
    // (which collapses to an empty box - it looked like a missing avatar).
    return src
        ? html`<img class="${cls}" src="${src}" alt="" loading="lazy">`
        : html`<span class="${cls} avatar-blank" aria-hidden="true"></span>`;
}

/** Route http(s) avatars through our caching proxy (privacy + no slow-host pop-in);
 * pass data: URIs through; reject anything else → the colored fallback circle. */
function avatarSrc(picture: string | undefined): string | null {
    if (!picture) return null;
    if (/^https?:\/\//i.test(picture)) return `/avatar?u=${encodeURIComponent(picture)}`;
    if (picture.startsWith('data:')) { const s = safeUrl(picture); return s === '#' ? null : s; }
    return null;
}

export function npub(pubkey: string): string {
    try { return npubEncode(pubkey); } catch { return pubkey; }
}

/** A URL's display text: drop the http(s):// scheme + trailing slash for a clean look. Shared by every
 * "show a link tidily" surface (profile website, repo web urls, and future issue/patch metadata). */
export const stripScheme = (url: string): string => url.replace(/^https?:\/\//i, '').replace(/\/+$/, '');

export function shortNpub(pubkey: string): string {
    const n = npub(pubkey);
    return `${n.slice(0, 12)}…${n.slice(-6)}`;
}

/** Display name for a pubkey: profile display_name/name, else short npub. */
export function displayName(pubkey: string, profiles?: ProfileMap): string {
    const p = profiles?.get(pubkey);
    return p?.display_name || p?.name || shortNpub(pubkey);
}

/** A short, stable base36 id from a string (for element ids that the page links to
 * by `#id` - lightbox slides, relay-score chips). Collisions are cosmetic. */
export function shortHash(s: string): string {
    let h = 0;
    for (let i = 0; i < s.length; i++) h = (Math.imul(31, h) + s.charCodeAt(i)) | 0;
    return (h >>> 0).toString(36);
}

/** Deterministic placeholder hue bucket (0..35, 10° each) for an avatar, from the
 * pubkey. The matching `.avatar-h<n>` rule sets the background colour (see styles.css);
 * a class instead of inline CSS keeps `style-src 'self'` strict. */
export function avatarHueBucket(pubkey: string): number {
    return Math.floor((parseInt(pubkey.slice(0, 6), 16) % 360) / 10);
}

export function timeAgo(ts: number): string {
    const secs = Math.floor(Date.now() / 1000) - Math.floor(ts); // floor ts too: drafts pass a fractional (ms→s) timestamp
    if (secs < 60) return `${Math.max(0, secs)}s ago`;
    if (secs < 3600) return `${Math.floor(secs / 60)}m ago`;
    if (secs < 86400) return `${Math.floor(secs / 3600)}h ago`;
    const days = Math.floor(secs / 86400);
    if (days < 7) return `${days}d ago`;
    if (days < 30) return `${Math.floor(days / 7)}w ago`;
    if (days < 365) return `${Math.floor(days / 30)}mo ago`;
    return `${Math.floor(days / 365)}y ago`;
}

/** A relay url without the wss:// scheme and trailing slash (Satori's shortRelay). */
export function shortRelay(url: string): string {
    return url.replace(/^wss?:\/\//, '').replace(/\/$/, '');
}
