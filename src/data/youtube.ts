// YouTube link handling: a privacy facade (DuckDuckGo-style). The browser never
// loads Google's player or thumbnail on page render. Shared links are stripped down
// to just the video id (+ start time), DISCARDING tracking params (si, pp, utm, ...);
// every URL we emit is reconstructed clean. Title + thumbnail are fetched server-side
// (Tor-with-fallback, see torfetch); playback is a click-to-load youtube-nocookie
// iframe. So: zero Google contact from the browser until the user presses play.

import { torFetch } from './torfetch.ts';

const ID = /^[A-Za-z0-9_-]{11}$/;
const LIST = /^[A-Za-z0-9_-]{12,42}$/; // playlist ids (PL…/UU…/LL…/etc.), longer + variable vs the 11-char video id
const YT_HOSTS = new Set(['youtube.com', 'www.youtube.com', 'm.youtube.com', 'music.youtube.com', 'youtu.be', 'www.youtu.be']);

// A YouTube link is either a single VIDEO (id + optional start) or a PLAYLIST (list id). Both get the
// privacy facade; a playlist plays via the nocookie `videoseries` embed and its poster comes from oEmbed.
export type YouTubeRef = { kind: 'video'; id: string; start?: number } | { kind: 'playlist'; list: string };

export const isYouTubeId = (id: string): boolean => ID.test(id);
export const isYouTubePlaylist = (list: string): boolean => LIST.test(list);

/** Parse a start time (?t= / &start=, e.g. "90", "90s", "1m30s", "1h2m3s") to whole
 * seconds. This is a convenience param, not a tracker, so we keep it. */
function parseStart(v: string | null): number | undefined {
    if (!v) return undefined;
    if (/^\d+$/.test(v)) return Number(v) || undefined;
    const m = v.match(/^(?:(\d+)h)?(?:(\d+)m)?(?:(\d+)s)?$/);
    if (!m || !m[0]) return undefined;
    const s = Number(m[1] || 0) * 3600 + Number(m[2] || 0) * 60 + Number(m[3] || 0);
    return s || undefined;
}

/** Extract {id, start} from any YouTube URL form (watch / youtu.be / shorts / embed /
 * live), or null if it isn't a recognizable YouTube video link. Only v/path-id + the
 * start time are read; all other params (si, pp, feature, utm_*, list, ...) are dropped. */
// Small bounded memo: renderContent calls parseYouTube 2-3x per URL token (neighbor checks + the
// main loop), each doing a `new URL()` parse. Cache by raw url (CAP covers a feed page's links).
const ytMemo = new Map<string, YouTubeRef | null>();
const YT_MEMO_CAP = 128;
export function parseYouTube(raw: string): YouTubeRef | null {
    if (ytMemo.has(raw)) return ytMemo.get(raw)!;
    const ref = parseYouTubeImpl(raw);
    ytMemo.set(raw, ref);
    if (ytMemo.size > YT_MEMO_CAP) ytMemo.delete(ytMemo.keys().next().value as string);
    return ref;
}

function parseYouTubeImpl(raw: string): YouTubeRef | null {
    let u: URL;
    try { u = new URL(raw); } catch { return null; }
    if (u.protocol !== 'http:' && u.protocol !== 'https:') return null;
    const host = u.hostname.toLowerCase();
    if (!YT_HOSTS.has(host)) return null;
    let id: string | null = null;
    if (host === 'youtu.be' || host === 'www.youtu.be') id = u.pathname.slice(1).split('/')[0] || null;
    else if (u.pathname === '/watch') id = u.searchParams.get('v');
    else { const m = u.pathname.match(/^\/(?:shorts|embed|live|v)\/([^/?#]+)/); if (m) id = m[1]!; }
    if (id && ID.test(id)) return { kind: 'video', id, start: parseStart(u.searchParams.get('t') || u.searchParams.get('start')) };
    // No video id, but a playlist link (/playlist?list=, /embed/videoseries?list=, or watch?list= with
    // no v) → a playlist facade. A /watch?v=…&list=… stays a VIDEO (handled above; the list is dropped).
    const list = u.searchParams.get('list');
    if (list && LIST.test(list)) return { kind: 'playlist', list };
    return null;
}

// --- Clean canonical URLs (always reconstructed, never the original) --------
export function youtubeWatchUrl(id: string, start?: number): string {
    return `https://www.youtube.com/watch?v=${id}${start ? `&t=${start}` : ''}`;
}
export function youtubeThumbUrl(id: string): string {
    return `https://i.ytimg.com/vi/${id}/hqdefault.jpg`;
}
export function youtubeEmbedUrl(id: string, start?: number): string {
    return `https://www.youtube-nocookie.com/embed/${id}?autoplay=1&rel=0${start ? `&start=${start}` : ''}`;
}
export function youtubePlaylistUrl(list: string): string {
    return `https://www.youtube.com/playlist?list=${list}`;
}
export function youtubePlaylistEmbedUrl(list: string): string {
    return `https://www.youtube-nocookie.com/embed/videoseries?list=${list}&autoplay=1&rel=0`;
}

// --- Title (oEmbed), cached process-wide (cross-user, public data) ----------
const titles = new Map<string, { title: string | null; exp: number }>();
const TITLE_TTL = 24 * 60 * 60 * 1000;
const TITLE_CAP = 5000; // bound the cache: one entry per distinct video id, else it grows forever

/** Drop expired entries; if still over cap, evict oldest-inserted (Map preserves order). */
function pruneTitles(): void {
    const now = Date.now();
    for (const [id, e] of titles) if (e.exp <= now) titles.delete(id);
    if (titles.size <= TITLE_CAP) return;
    let over = titles.size - TITLE_CAP;
    for (const id of titles.keys()) { if (over-- <= 0) break; titles.delete(id); }
}

/** Best-effort video title via oEmbed (server-side, Tor-with-fallback, cached). Null
 * when unavailable (blocked Tor exit, private/removed video) - the card still renders,
 * just without a title. No API key needed. */
export async function fetchYouTubeTitle(id: string): Promise<string | null> {
    if (!ID.test(id)) return null;
    const hit = titles.get(id);
    if (hit && hit.exp > Date.now()) return hit.title;
    let title: string | null = null;
    try {
        const url = `https://www.youtube.com/oembed?url=https%3A%2F%2Fwww.youtube.com%2Fwatch%3Fv%3D${id}&format=json`;
        const r = await torFetch(url, 6000, 256 * 1024);
        if (r.status === 200) {
            const j = JSON.parse(r.body.toString('utf8')) as { title?: string };
            if (typeof j.title === 'string') title = j.title;
        }
    } catch { /* leave null */ }
    titles.set(id, { title, exp: Date.now() + TITLE_TTL });
    pruneTitles();
    return title;
}

// --- Playlist (oEmbed), same cache discipline as titles ---------------------
const VIDEO_IN_THUMB = /\/vi\/([A-Za-z0-9_-]{11})\//; // oEmbed's thumbnail_url is a representative video thumb
const playlists = new Map<string, { title: string | null; thumbId: string | null; exp: number }>();

function prunePlaylists(): void {
    const now = Date.now();
    for (const [k, e] of playlists) if (e.exp <= now) playlists.delete(k);
    if (playlists.size <= TITLE_CAP) return;
    let over = playlists.size - TITLE_CAP;
    for (const k of playlists.keys()) { if (over-- <= 0) break; playlists.delete(k); }
}

/** Best-effort playlist title + a representative thumbnail VIDEO id via oEmbed (server-side,
 * Tor-with-fallback, cached). The id is pulled from oEmbed's i.ytimg thumbnail_url so the poster
 * reuses the proxied /yt/thumb path. Both fields null when unavailable - the card still renders
 * (the videoseries player works regardless of oEmbed). No API key needed. */
export async function fetchYouTubePlaylist(list: string): Promise<{ title: string | null; thumbId: string | null }> {
    if (!LIST.test(list)) return { title: null, thumbId: null };
    const hit = playlists.get(list);
    if (hit && hit.exp > Date.now()) return { title: hit.title, thumbId: hit.thumbId };
    let title: string | null = null, thumbId: string | null = null;
    try {
        const url = `https://www.youtube.com/oembed?url=https%3A%2F%2Fwww.youtube.com%2Fplaylist%3Flist%3D${list}&format=json`;
        const r = await torFetch(url, 6000, 256 * 1024);
        if (r.status === 200) {
            const j = JSON.parse(r.body.toString('utf8')) as { title?: string; thumbnail_url?: string };
            if (typeof j.title === 'string') title = j.title;
            const m = typeof j.thumbnail_url === 'string' ? j.thumbnail_url.match(VIDEO_IN_THUMB) : null;
            if (m) thumbId = m[1]!;
        }
    } catch { /* leave null */ }
    playlists.set(list, { title, thumbId, exp: Date.now() + TITLE_TTL });
    prunePlaylists();
    return { title, thumbId };
}
