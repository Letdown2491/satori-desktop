// YouTube privacy-facade routes. The browser makes ZERO requests to any Google domain
// on page render - everything below is served from our origin (the thumbnail proxied,
// the title server-fetched), and the player only loads on an explicit play click.
//   GET /yt/card/:id  - the lazy facade card (proxied thumb + oEmbed title + play btn)
//   GET /yt/thumb/:id - the thumbnail, proxied (Tor-with-fallback) + disk-cached
//   GET /yt/play/:id  - the click-to-load youtube-nocookie iframe (plays in the feed)
// Hosts are hardcoded and the id is validated (11-char), so there's no SSRF/open-proxy
// surface. Public (like /avatar) so <img>/<iframe> loads don't hit a login redirect.

import { html } from '../html.ts';
import { isYouTubeId, isYouTubePlaylist, youtubeWatchUrl, youtubeThumbUrl, youtubeEmbedUrl, youtubePlaylistUrl, youtubePlaylistEmbedUrl, fetchYouTubeTitle, fetchYouTubePlaylist } from '../data/youtube.ts';
import { torFetch } from '../data/torfetch.ts';
import { torStrict } from '../privacy.ts';
import { getAvatarBytes, putAvatarBytes } from '../data/avatar-cache.ts';
import { sendFragment, redirect, type Ctx } from '../http.ts';

const LONG_CACHE = 'public, max-age=604800, immutable'; // id-keyed → safe to cache hard
const MAX_THUMB = 8 * 1024 * 1024;
// 1x1 transparent GIF - graceful fallback when the thumbnail can't be fetched.
const BLANK = Buffer.from('R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7', 'base64');

const ytId = (ctx: Ctx): string | null => { const id = ctx.params.id ?? ''; return isYouTubeId(id) ? id : null; };
const ytList = (ctx: Ctx): string | null => { const list = ctx.params.list ?? ''; return isYouTubePlaylist(list) ? list : null; };
const startOf = (ctx: Ctx): number | undefined => Number(ctx.query.get('t')) || undefined;

function serveBlank(ctx: Ctx): void {
    ctx.res.writeHead(200, { 'Content-Type': 'image/gif', 'Cache-Control': 'public, max-age=300' });
    ctx.res.end(BLANK);
}

/** GET /yt/card/:id - the facade card, lazy-loaded by the placeholder in note content. */
export async function getYtCard(ctx: Ctx): Promise<void> {
    const id = ytId(ctx);
    if (!id) { redirect(ctx, '/'); return; }
    const start = startOf(ctx);
    const watch = youtubeWatchUrl(id, start);
    if (!ctx.isPartial) { redirect(ctx, watch); return; } // full nav → the clean YouTube page
    const title = await fetchYouTubeTitle(id);
    const playUrl = `/yt/play/${id}${start ? `?t=${start}` : ''}`;
    const inner = html`<img class="yt-thumb" src="/yt/thumb/${id}" alt="" loading="lazy"><span class="yt-logo">YouTube</span><span class="yt-playbtn" aria-hidden="true"></span>`;
    // Strict Privacy Mode: the inline nocookie player plays browser→Google (un-Tor'able),
    // so DON'T offer it - the poster is just an external link to the clean watch page
    // (clicking is an explicit, user-initiated exit). Off/Balanced: the poster's href is
    // the same-origin /yt/play (helmjs reads href for an <a>), so a click swaps the
    // iframe into #yt-<id>-frame in place; no-JS navigates /yt/play → 303 → YouTube.
    const poster = torStrict()
        ? html`<a class="yt-poster" href="${watch}" target="_blank" rel="noopener noreferrer" aria-label="Watch on YouTube (Strict Privacy Mode: opens YouTube directly)">${inner}</a>`
        : html`<a class="yt-poster" href="${playUrl}" h-target="#yt-${id}-frame" h-swap="inner" h-push-url="false" aria-label="Play video">${inner}</a>`;
    sendFragment(ctx, html`
      <div class="yt-frame" id="yt-${id}-frame">${poster}</div>
      ${title ? html`<div class="yt-title">${title}</div>` : html``}`);
}

/** GET /yt/playlist/:list - the facade card for a YouTube playlist: oEmbed title + a representative
 * proxied thumbnail (reusing /yt/thumb) + a play button that loads the nocookie videoseries player.
 * Same zero-Google-on-render guarantee as the video card. */
export async function getYtPlaylistCard(ctx: Ctx): Promise<void> {
    const list = ytList(ctx);
    if (!list) { redirect(ctx, '/'); return; }
    const watch = youtubePlaylistUrl(list);
    if (!ctx.isPartial) { redirect(ctx, watch); return; } // full nav → the clean YouTube playlist page
    const { title, thumbId } = await fetchYouTubePlaylist(list);
    const playUrl = `/yt/playlist/${list}/play`;
    const thumb = thumbId
        ? html`<img class="yt-thumb" src="/yt/thumb/${thumbId}" alt="" loading="lazy">`
        : html`<span class="yt-thumb yt-thumb-blank" aria-hidden="true"></span>`; // oEmbed had no thumb; player still works
    const inner = html`${thumb}<span class="yt-logo">Playlist</span><span class="yt-playbtn" aria-hidden="true"></span>`;
    const poster = torStrict()
        ? html`<a class="yt-poster" href="${watch}" target="_blank" rel="noopener noreferrer" aria-label="Open playlist on YouTube (Strict Privacy Mode: opens YouTube directly)">${inner}</a>`
        : html`<a class="yt-poster" href="${playUrl}" h-target="#yt-pl-${list}-frame" h-swap="inner" h-push-url="false" aria-label="Play playlist">${inner}</a>`;
    sendFragment(ctx, html`
      <div class="yt-frame" id="yt-pl-${list}-frame">${poster}</div>
      ${title ? html`<div class="yt-title">${title}</div>` : html``}`);
}

/** GET /yt/playlist/:list/play - the click-to-load nocookie `videoseries` player for a playlist. */
export function getYtPlaylistPlay(ctx: Ctx): void {
    const list = ytList(ctx);
    if (!list) { redirect(ctx, '/'); return; }
    if (!ctx.isPartial) { redirect(ctx, youtubePlaylistUrl(list)); return; } // no-JS → clean YouTube page
    if (torStrict()) { sendFragment(ctx, html`<a class="yt-poster yt-strict-link" href="${youtubePlaylistUrl(list)}" target="_blank" rel="noopener noreferrer">▶ Open playlist on YouTube</a>`); return; }
    sendFragment(ctx, html`<iframe class="yt-iframe" src="${youtubePlaylistEmbedUrl(list)}" title="YouTube playlist player" allow="autoplay; encrypted-media; picture-in-picture; fullscreen" allowfullscreen loading="lazy" referrerpolicy="origin"></iframe>`);
}

/** GET /yt/thumb/:id - the proxied thumbnail (Tor-with-fallback), disk-cached so the
 * browser never touches i.ytimg.com directly. Falls back to a blank pixel. */
export async function getYtThumb(ctx: Ctx): Promise<void> {
    const id = ytId(ctx);
    if (!id) { serveBlank(ctx); return; }
    const url = youtubeThumbUrl(id);
    const cached = await getAvatarBytes(url); // url-keyed image byte cache (shared with avatars)
    if (cached) { ctx.res.writeHead(200, { 'Content-Type': cached.ct, 'Cache-Control': LONG_CACHE }); ctx.res.end(cached.bytes); return; }
    try {
        const r = await torFetch(url, 8000, MAX_THUMB);
        const ct = String(r.headers['content-type'] ?? '');
        if (r.status !== 200 || !ct.startsWith('image/')) { serveBlank(ctx); return; }
        ctx.res.writeHead(200, { 'Content-Type': ct, 'Cache-Control': LONG_CACHE });
        ctx.res.end(r.body);
        void putAvatarBytes(url, r.body, ct); // cache after serving
    } catch { serveBlank(ctx); }
}

/** GET /yt/play/:id - the click-to-load player. Only here does anything from Google
 * enter the page, and only on an explicit play click (cookies deferred by nocookie). */
export function getYtPlay(ctx: Ctx): void {
    const id = ytId(ctx);
    if (!id) { redirect(ctx, '/'); return; }
    const start = startOf(ctx);
    if (!ctx.isPartial) { redirect(ctx, youtubeWatchUrl(id, start)); return; } // no-JS → clean YouTube page
    // Strict Privacy Mode: never serve the inline player (it would play browser→Google).
    // Defensive - the Strict card poster is already an external link, not an h-get here.
    if (torStrict()) { sendFragment(ctx, html`<a class="yt-poster yt-strict-link" href="${youtubeWatchUrl(id, start)}" target="_blank" rel="noopener noreferrer">▶ Watch on YouTube</a>`); return; }
    // referrerpolicy="origin" (overriding the page's global no-referrer) sends ONLY
    // our origin (scheme+host, not the path/note) to YouTube. The nocookie player
    // needs a Referer to validate the embedding origin - with none it errors out
    // ("Error 153"). Origin-only is the minimum it needs and leaks nothing beyond
    // the host, only on an explicit play (when your IP already reaches Google).
    sendFragment(ctx, html`<iframe class="yt-iframe" src="${youtubeEmbedUrl(id, start)}" title="YouTube video player" allow="autoplay; encrypted-media; picture-in-picture; fullscreen" allowfullscreen loading="lazy" referrerpolicy="origin"></iframe>`);
}
