// GET /avatar?u=<url> - server-side avatar proxy. Fetches the image once, caches it
// to disk, serves from cache thereafter. Keeps the browser off arbitrary third-party
// hosts (privacy) and smooths slow-host pop-in. On any failure it serves a 1x1
// transparent GIF, so the <img>'s background-color (the colored fallback circle)
// shows through instead of a broken image. The proxy body is shared with /media.

import { serveProxiedImage } from './image-proxy.ts';
import { isPublicHttpUrl } from '../ssrf.ts';
import type { Ctx } from '../http.ts';

const MAX_BYTES = 8 * 1024 * 1024;  // avatars over this are almost certainly not avatars
const TIMEOUT_MS = 8000;

export async function getAvatar(ctx: Ctx): Promise<void> {
    const url = ctx.query.get('u') ?? '';
    if (!url || !isPublicHttpUrl(url)) { serveBlank(ctx); return; }
    await serveProxiedImage(ctx, url, { maxBytes: MAX_BYTES, timeoutMs: TIMEOUT_MS });
}

// 1x1 transparent GIF fallback for a missing/invalid url (before the proxy is reached).
const BLANK = Buffer.from('R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7', 'base64');
function serveBlank(ctx: Ctx): void {
    ctx.res.writeHead(200, { 'Content-Type': 'image/gif', 'Cache-Control': 'public, max-age=300' });
    ctx.res.end(BLANK);
}
