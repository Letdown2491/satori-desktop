// GET /media?u=<url> - proxy a note's inline IMAGE through the daemon, so the browser
// never connects to the third-party host (no IP/cookies/referrer leak), and the
// daemon's own fetch honors Privacy Mode via torFetch (direct off / Tor on). Disk-
// cached (the shared image byte cache) + SSRF-guarded (the url is from arbitrary note
// content). Mirrors /avatar; video is NOT proxied yet (needs Range/streaming) so it
// stays direct. On any failure: a 1x1 transparent GIF (so the <img> just shows blank).

import { isPublicHttpUrl } from '../ssrf.ts';
import { serveProxiedImage } from './image-proxy.ts';
import { sendFragment, type Ctx } from '../http.ts';
import { html, safeUrl } from '../html.ts';
import { torStrict } from '../privacy.ts';
import { videoEmbed } from '../render/content.ts';
import { fetchBlossomServers } from '../upload.ts';
import { isHex64 } from '../nostr/tags.ts';
import type { Pool } from '../data/pool.ts';

const MAX_BYTES = 25 * 1024 * 1024;  // generous for images, but bounded
const TIMEOUT_MS = 12000;            // images over a cold Tor circuit can be slow
const BLANK = Buffer.from('R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7', 'base64');

function serveBlank(ctx: Ctx): void {
    ctx.res.writeHead(200, { 'Content-Type': 'image/gif', 'Cache-Control': 'public, max-age=300' });
    ctx.res.end(BLANK);
}

// The author's Blossom server list (kind:10063) is fetched only when an image fails to heal, and cached
// briefly so a feed with several dead images from one author triggers a single lookup.
const SERVERS_TTL = 10 * 60 * 1000;
const SERVERS_CAP = 500; // bound the map (keyed by author pubkey, only added on a heal) - evict the oldest
const serversCache = new Map<string, { servers: string[]; at: number }>();
async function blossomServersFor(pool: Pool, pubkey: string): Promise<string[]> {
    const hit = serversCache.get(pubkey);
    if (hit && Date.now() - hit.at < SERVERS_TTL) return hit.servers;
    const servers = await fetchBlossomServers(pool, pubkey, null).catch(() => [] as string[]); // null relays → INDEXER lookup
    if (serversCache.size >= SERVERS_CAP) { const oldest = [...serversCache].sort((a, b) => a[1].at - b[1].at)[0]; if (oldest) serversCache.delete(oldest[0]); }
    serversCache.set(pubkey, { servers, at: Date.now() });
    return servers;
}

/** A Blossom-style url whose last path segment is a SHA-256 (optionally with an extension) → {hash, ext}. */
function blossomTarget(url: string): { hash: string; ext: string } | null {
    try {
        const seg = new URL(url).pathname.split('/').pop() ?? '';
        const m = /^([0-9a-f]{64})(\.[a-z0-9]+)?$/i.exec(seg);
        return m ? { hash: m[1]!.toLowerCase(), ext: m[2] ?? '' } : null;
    } catch { return null; }
}

/** NIP-B7 healing candidates: the same Blossom file (by hash) on the AUTHOR's other servers (kind:10063).
 * Only for a hash-named url with a valid author and a session pool; each candidate is SSRF-guarded. */
async function healUrls(ctx: Ctx, url: string, author: string): Promise<string[]> {
    if (!isHex64(author)) return [];
    const tgt = blossomTarget(url);
    const pool = ctx.session?.pool;
    if (!tgt || !pool) return [];
    const servers = await blossomServersFor(pool, author);
    return servers
        .map((s) => `${s.replace(/\/+$/, '')}/${tgt.hash}${tgt.ext}`)
        .filter((u) => u !== url && isPublicHttpUrl(u));
}

export async function getMedia(ctx: Ctx): Promise<void> {
    const url = ctx.query.get('u') ?? '';
    if (!url || !isPublicHttpUrl(url)) { serveBlank(ctx); return; } // SSRF guard (url is from a note)
    const author = ctx.query.get('author') ?? '';
    // heal is a lazy callback: serveProxiedImage only calls it if the primary url fails (so the common
    // case pays nothing, and the kind:10063 lookup happens only for genuinely-dead media). expectHash
    // makes the proxy verify a healed file's SHA-256 (NIP-B7 integrity) before serving/caching it.
    const heal = author ? () => healUrls(ctx, url, author) : undefined;
    const expectHash = author ? blossomTarget(url)?.hash : undefined;
    await serveProxiedImage(ctx, url, { maxBytes: MAX_BYTES, timeoutMs: TIMEOUT_MS, heal, expectHash });
}

/** GET /video?u=<url>&dim=<dim> - the autoplaying <video> for a poster-less video facade,
 * swapped in (helmjs) when the user clicks the play placeholder. Render-only (the <video src>
 * still streams direct, like all video); the facade's <a href> is the no-JS fallback. In strict
 * Privacy Mode the facade never offers this h-get, but guard defensively - return the raw-file open
 * link (a deliberate "leaves Tor" tab) instead of an inline streaming <video>. */
export function getVideoEmbed(ctx: Ctx): void {
    const url = ctx.query.get('u') ?? '';
    if (torStrict()) { sendFragment(ctx, html`<a class="video-facade-play strict-link" href="${safeUrl(url)}" target="_blank" rel="noreferrer noopener">▶ Open video (leaves Tor)</a>`); return; }
    sendFragment(ctx, videoEmbed(url, ctx.query.get('dim') ?? undefined));
}
