// Shared server-side image proxy: the cache-hit / fetch-miss / validate / serve+cache body that both
// /avatar and /media's image branch ran verbatim. Fetches an arbitrary third-party image once
// (Privacy-Mode-aware via torFetch, SSRF-guarded by the caller), caches it to the shared byte cache,
// and serves from cache thereafter - keeping the browser off third-party hosts.
//
// SECURITY-SENSITIVE (do not weaken): SVG is rejected (the one image type that's also a scriptable
// document - a stored-XSS vector in our own origin); the served type is pinned with nosniff +
// Content-Disposition: inline; and the per-call maxBytes/timeoutMs caps bound the fetch. On any
// failure (incl. strict Privacy Mode with Tor blocked) it fails closed to a 1x1 transparent GIF, so
// the <img>'s background-color (the colored fallback circle) shows through, never a broken image.

import { getAvatarBytes, putAvatarBytes } from '../data/avatar-cache.ts';
import { torFetch } from '../data/torfetch.ts';
import { sha256Hex } from '../upload.ts';
import type { Ctx } from '../http.ts';

const LONG_CACHE = 'public, max-age=604800, immutable'; // url-keyed → safe to cache hard
// 1x1 transparent GIF - the graceful fallback (lets the bg-color circle show).
const BLANK = Buffer.from('R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7', 'base64');
// Same-origin hardening: pin the declared type (nosniff), forbid render-as-document (inline).
const SAFE_IMG = { 'X-Content-Type-Options': 'nosniff', 'Content-Disposition': 'inline' };
// SVG is the one image type that's also a scriptable document - never a legitimate note image / avatar,
// and a stored-XSS vector in our own origin, so reject it.
const okImage = (ct: string): boolean => ct.startsWith('image/') && !/svg/i.test(ct);

function serveBlank(ctx: Ctx): void {
    ctx.res.writeHead(200, { 'Content-Type': 'image/gif', 'Cache-Control': 'public, max-age=300' });
    ctx.res.end(BLANK);
}

/** One bounded, Privacy-Mode-aware image fetch: a valid image's bytes+type, or null on any failure
 * (non-200, non-image, SVG, network/Tor error). */
async function fetchImage(url: string, maxBytes: number, timeoutMs: number): Promise<{ bytes: Buffer; ct: string } | null> {
    try {
        const r = await torFetch(url, timeoutMs, maxBytes);
        const ct = String(r.headers['content-type'] ?? '');
        return r.status === 200 && okImage(ct) ? { bytes: r.body, ct } : null;
    } catch { return null; }
}

/** Serve `url` as a proxied image (cache-first, then a bounded fetch), or the BLANK fallback on any
 * failure. The caller is responsible for the SSRF guard (isPublicHttpUrl) before calling. `heal` (NIP-B7)
 * is called ONLY when the primary fetch fails - it returns already-SSRF-guarded alternate urls (the same
 * Blossom file by hash on the author's other servers) to try before giving up. */
export async function serveProxiedImage(ctx: Ctx, url: string, { maxBytes, timeoutMs, heal, expectHash }: { maxBytes: number; timeoutMs: number; heal?: () => Promise<string[]>; expectHash?: string }): Promise<void> {
    const cached = await getAvatarBytes(url);
    if (cached) {
        if (!okImage(cached.ct)) { serveBlank(ctx); return; } // reject SVG even from an older cache
        ctx.res.writeHead(200, { 'Content-Type': cached.ct, 'Cache-Control': LONG_CACHE, ...SAFE_IMG });
        ctx.res.end(cached.bytes);
        return;
    }

    // Miss → fetch the primary; on failure, try the NIP-B7 heal urls (alternate Blossom servers). The
    // first image that comes back is served AND cached under the ORIGINAL url, so the next load (and the
    // lightbox, which requests the same original url) hits the cache - the heal happens at most once.
    let img = await fetchImage(url, maxBytes, timeoutMs);
    if (!img && heal) {
        for (const alt of await heal().catch(() => [] as string[])) {
            const cand = await fetchImage(alt, maxBytes, timeoutMs);
            // NIP-B7: a healed file MUST hash to the requested SHA-256 - else a server in the author's
            // list could serve mismatched bytes (and we'd cache them under the original url). Verified
            // bytes are by definition the genuine content-addressed file, so caching them is safe.
            if (cand && (!expectHash || (await sha256Hex(cand.bytes)) === expectHash)) { img = cand; break; }
        }
    }
    if (!img) { serveBlank(ctx); return; } // strict Privacy Mode with Tor blocked, or genuinely gone → blank
    ctx.res.writeHead(200, { 'Content-Type': img.ct, 'Cache-Control': LONG_CACHE, ...SAFE_IMG });
    ctx.res.end(img.bytes);
    void putAvatarBytes(url, img.bytes, img.ct); // cache after serving (don't slow the first hit)
}
