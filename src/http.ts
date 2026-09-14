// Owned HTTP plumbing over Node's http: a small request context, cookie parsing,
// form-body reading, and response helpers (HTML, redirect, fragment). No framework.

import type { IncomingMessage, ServerResponse } from 'node:http';
import { gzipSync } from 'node:zlib';
import { renderToString, type SafeHtml } from './html.ts';
import { torStrict } from './privacy.ts';
import type { Session } from './session.ts';
import type { BatchResult } from './wire.ts';
export type { BatchResult };

export interface Ctx {
    req: IncomingMessage;
    res: ServerResponse;
    method: string;
    path: string;
    query: URLSearchParams;
    params: Record<string, string>;
    cookies: Record<string, string>;
    /** True when the request reached us over HTTPS (direct TLS or via a trusted proxy's
     * X-Forwarded-Proto) - so cookies get the `Secure` flag. Off for plain HTTP / .onion. */
    secure: boolean;
    session: Session | undefined;
    /** True when the request came from helmjs (so we return a fragment, not a full page). */
    isPartial: boolean;
    /** The helmjs target selector, if the client sent one. */
    hTarget: string | null;
}

export function parseCookies(header: string | undefined): Record<string, string> {
    const out: Record<string, string> = {};
    if (!header) return out;
    for (const part of header.split(';')) {
        const i = part.indexOf('=');
        if (i === -1) continue;
        const k = part.slice(0, i).trim();
        const v = part.slice(i + 1).trim();
        if (k) out[k] = decodeURIComponent(v);
    }
    return out;
}

/** Parse a multipart/form-data body's text fields into URLSearchParams. helmjs
 * submits forms as FormData, which fetch sends as multipart - so the enhanced
 * path needs this, while a native (no-JS) form POST is urlencoded. File parts
 * (those with a filename) are skipped: MVP forms are text-only. */
function parseMultipart(body: string, boundary: string): URLSearchParams {
    const params = new URLSearchParams();
    for (const part of body.split(`--${boundary}`)) {
        const sep = part.indexOf('\r\n\r\n');
        if (sep === -1) continue;
        const headers = part.slice(0, sep);
        const name = /name="([^"]*)"/i.exec(headers)?.[1];
        if (!name || /filename="/i.test(headers)) continue;
        let value = part.slice(sep + 4);
        if (value.endsWith('\r\n')) value = value.slice(0, -2); // trailing CRLF before next boundary
        params.append(name, value);
    }
    return params;
}

/** Read a form body into URLSearchParams. Handles both urlencoded (native no-JS
 * form POST) and multipart/form-data (helmjs FormData submit). Capped for memory. */
export function readForm(req: IncomingMessage, limit = 1024 * 1024): Promise<URLSearchParams> {
    return new Promise((resolve, reject) => {
        const chunks: Buffer[] = [];
        let size = 0;
        req.on('data', (chunk: Buffer) => {
            size += chunk.length;
            if (size > limit) { reject(new Error('body too large')); req.destroy(); return; }
            chunks.push(chunk);
        });
        req.on('end', () => {
            const body = Buffer.concat(chunks).toString('utf8');
            const ct = req.headers['content-type'] ?? '';
            const boundary = /boundary=(?:"([^"]+)"|([^;]+))/i.exec(ct);
            if (ct.includes('multipart/form-data') && boundary) {
                resolve(parseMultipart(body, (boundary[1] ?? boundary[2] ?? '').trim()));
            } else {
                resolve(new URLSearchParams(body));
            }
        });
        req.on('error', reject);
    });
}

export interface UploadedFile { filename: string; contentType: string; bytes: Buffer }

/** Read a multipart/form-data body binary-safe, extracting the first file part
 * (a part with a `filename`) plus any text fields. readForm corrupts binary (it
 * decodes the whole body as utf8), so /upload uses this instead. Capped large. */
export function readUpload(req: IncomingMessage, limit = 25 * 1024 * 1024): Promise<{ fields: URLSearchParams; file: UploadedFile | null }> {
    return new Promise((resolve, reject) => {
        const chunks: Buffer[] = [];
        let size = 0;
        req.on('data', (chunk: Buffer) => {
            size += chunk.length;
            if (size > limit) { reject(new Error('file too large')); req.destroy(); return; }
            chunks.push(chunk);
        });
        req.on('end', () => {
            const fields = new URLSearchParams();
            const ct = req.headers['content-type'] ?? '';
            const m = /boundary=(?:"([^"]+)"|([^;]+))/i.exec(ct);
            if (!ct.includes('multipart/form-data') || !m) { resolve({ fields, file: null }); return; }
            const boundary = Buffer.from(`--${(m[1] ?? m[2] ?? '').trim()}`);
            const body = Buffer.concat(chunks);
            let file: UploadedFile | null = null;
            let start = body.indexOf(boundary);
            while (start !== -1) {
                start += boundary.length;
                if (body[start] === 0x2d && body[start + 1] === 0x2d) break; // closing "--"
                if (body[start] === 0x0d && body[start + 1] === 0x0a) start += 2; // CRLF after boundary
                const next = body.indexOf(boundary, start);
                if (next === -1) break;
                const part = body.subarray(start, next - 2); // drop the CRLF before the next boundary
                const sep = part.indexOf('\r\n\r\n');
                if (sep !== -1) {
                    const headers = part.subarray(0, sep).toString('utf8');
                    const name = /name="([^"]*)"/i.exec(headers)?.[1];
                    const filename = /filename="([^"]*)"/i.exec(headers)?.[1];
                    const partType = /content-type:\s*([^\r\n]+)/i.exec(headers)?.[1]?.trim();
                    const value = part.subarray(sep + 4);
                    if (filename !== undefined) {
                        if (filename && !file) file = { filename, contentType: partType || 'application/octet-stream', bytes: Buffer.from(value) };
                    } else if (name) {
                        fields.append(name, value.toString('utf8'));
                    }
                }
                start = next;
            }
            resolve({ fields, file });
        });
        req.on('error', reject);
    });
}

// A local daemon: keep it locked down. Both `script-src 'self'` AND `style-src 'self'` are strict (no
// 'unsafe-inline'): we emit ZERO inline styles - former dynamic values are class buckets (avatar hue
// .avatar-h0..35, poll width .pw-0..100, reply depth .depth-1..4) or HTML width/height attrs (media
// aspect-ratio). img-src 'self' data: - ALL images load same-origin via our proxies (/media, /avatar,
// /yt/thumb) or data:, so the browser physically can't fetch an off-origin image even if a bug emitted
// a raw src (enforces the no-leak guarantee).
//   media-src / frame-src are PER-PRIVACY-MODE: off/balanced let video stream direct on play (preload="none",
//   so nothing loads until the explicit play) and allow the YouTube nocookie player. STRICT suppresses both at
//   the render layer (videoSuppressed / the YT-player suppression) AND locks them here ('self' / 'none') as
//   the CSP-level enforcement of strict's no-leak guarantee - the browser physically can't stream off-origin
//   video or frame YouTube.
//   connect-src allows wss: in ALL modes for the NWC (NIP-47) browser->wallet-relay WebSocket. This socket is
//   browser-direct (it does NOT pass through the daemon's Tor routing), so in strict its IP privacy depends on
//   reaching Satori over Tor Browser, which tunnels it. (Parked: a daemon ws-proxy could route it over Tor so
//   any browser stays covered; see the wallet notes.) Same-origin fetches/sockets are always allowed via 'self'.
function securityHeaders(): Record<string, string> {
    const strict = torStrict();
    const mediaSrc = strict ? "'self'" : '*';
    const frameSrc = strict ? "'none'" : 'https://www.youtube-nocookie.com';
    const connectSrc = "'self' wss:"; // wss: for the browser's NWC wallet socket
    return {
        'Content-Security-Policy':
            `default-src 'self'; img-src 'self' data:; media-src ${mediaSrc}; connect-src ${connectSrc}; style-src 'self'; script-src 'self'; frame-src ${frameSrc}; form-action 'self'; base-uri 'none'; frame-ancestors 'none'`,
        'X-Content-Type-Options': 'nosniff',
        'Referrer-Policy': 'no-referrer',
    };
}

export function setCookie(ctx: Ctx, name: string, value: string, opts: { maxAge?: number } = {}): void {
    const res = ctx.res;
    const parts = [`${name}=${encodeURIComponent(value)}`, 'Path=/', 'HttpOnly', 'SameSite=Lax'];
    if (ctx.secure) parts.push('Secure'); // only over HTTPS; omitted on plain HTTP / .onion so the cookie still sets
    if (opts.maxAge !== undefined) parts.push(`Max-Age=${opts.maxAge}`);
    const prev = res.getHeader('Set-Cookie');
    const list = Array.isArray(prev) ? prev : prev ? [String(prev)] : [];
    list.push(parts.join('; '));
    res.setHeader('Set-Cookie', list);
}

/** The app injects its full-page shell (chrome) renderer here at boot, so this HTTP kernel
 * stays app-agnostic and does NOT import the app's layout. `sendPage` wraps page content
 * with it. `C` is the app's chrome-options type (opaque to the kernel). */
export type PageRenderer<C = unknown> = (content: SafeHtml, chrome: C) => SafeHtml;
let pageRenderer: PageRenderer | null = null;
export function setPageRenderer<C>(fn: PageRenderer<C>): void { pageRenderer = fn as PageRenderer; }

/** Send a full HTML page (or just the fragment, for a helmjs partial request). */
export function sendPage<C>(ctx: Ctx, content: SafeHtml, chrome: C, status = 200): void {
    // For a boosted/h-boost request, helmjs swaps the <body>, so a full document is
    // fine either way - it extracts <body>. We always send the full page; it both
    // degrades (no JS) and boosts. Targeted partials use sendFragment instead.
    //
    // On a 4xx/5xx, helmjs auto-routes the response into an [h-error] region when
    // the page has one - which would nest this whole page inside that small region.
    // We reserve [h-error] for the client's own (nip07) error text, so a full-page
    // error re-renders the <body> instead: H-Retarget points the error swap at the
    // body (classic "re-render the form with the error + preserved input"). The
    // header is ignored on a no-JS full navigation, which just renders this page.
    const errorHeaders: Record<string, string> = status >= 400 ? { 'H-Retarget': 'body', 'H-Reswap': 'inner' } : {};
    if (!pageRenderer) throw new Error('sendPage: no page renderer registered (call setPageRenderer at boot).');
    const body = renderToString(pageRenderer(content, chrome));
    writeHtml(ctx, status, { 'Content-Type': 'text/html; charset=utf-8', ...securityHeaders(), ...errorHeaders }, body);
}

/** Send a bare HTML fragment (helmjs partial swap target), with optional H-* headers. */
export function sendFragment(ctx: Ctx, content: SafeHtml, extraHeaders: Record<string, string> = {}, status = 200): void {
    writeHtml(ctx, status, { 'Content-Type': 'text/html; charset=utf-8', ...securityHeaders(), ...extraHeaders }, renderToString(content));
}

// Dynamic HTML is unique per request, so unlike serveStatic's precompressed cache it's gzipped
// inline. Feed/thread pages gzip 5-10x and the .onion deployment feels every KB; a couple of ms
// of zlib beats hundreds of KB on the wire. The size floor skips bodies too small to pay for it.
const GZIP_MIN_BYTES = 1024;
function writeHtml(ctx: Ctx, status: number, headers: Record<string, string>, body: string): void {
    const buf = Buffer.from(body);
    if (buf.byteLength >= GZIP_MIN_BYTES && /\bgzip\b/.test(String(ctx.req.headers['accept-encoding'] ?? ''))) {
        ctx.res.writeHead(status, { ...headers, 'Content-Encoding': 'gzip', 'Vary': 'Accept-Encoding' });
        ctx.res.end(gzipSync(buf));
        return;
    }
    ctx.res.writeHead(status, headers);
    ctx.res.end(buf);
}

export function redirect(ctx: Ctx, location: string, status = 303): void {
    ctx.res.writeHead(status, { Location: location, ...securityHeaders() });
    ctx.res.end();
}

/** Send a text body as a file download (Content-Disposition: attachment). `filename` is sanitized to a
 * safe ASCII subset so it can't break the header or smuggle directives. */
export function sendDownload(ctx: Ctx, body: string, filename: string, contentType: string): void {
    const safe = filename.replace(/[^A-Za-z0-9._-]/g, '_').slice(0, 100) || 'download';
    ctx.res.writeHead(200, {
        'Content-Type': contentType,
        'Content-Disposition': `attachment; filename="${safe}"`,
        ...securityHeaders(),
    });
    ctx.res.end(body);
}

/** The originating page to bounce back to after an action, but ONLY if it's our own
 * origin - returned as a relative path so a forged Referer can't drive an open
 * redirect to an external site. Falls back to "/". */
export function safeReferer(ctx: Ctx): string {
    const ref = ctx.req.headers.referer;
    const host = ctx.req.headers.host;
    if (!ref || !host) return '/';
    try { const u = new URL(ref); if (u.host === host) return u.pathname + u.search; } catch { /* malformed */ }
    return '/';
}

export function notFound(ctx: Ctx, message = 'Not found'): void {
    ctx.res.writeHead(404, { 'Content-Type': 'text/plain; charset=utf-8', ...securityHeaders() });
    ctx.res.end(message);
}

// --- NIP-07 sign-and-resubmit (the nip07-hateoas wire contract) ------------

/** Capability tokens the client advertised in H-Nostr-Caps. Tolerant of comma OR whitespace
 * separators (HTTP token-lists vary; the plugin currently joins with commas), so the contract
 * never hinges on the delimiter. */
function caps(ctx: Ctx): string[] {
    const h = ctx.req.headers['h-nostr-caps'];
    return typeof h === 'string' ? h.split(/[\s,]+/).filter(Boolean) : [];
}

/** Does the client advertise a given capability token (e.g. 'batch', 'nip04')? */
export function hasCap(ctx: Ctx, token: string): boolean {
    return caps(ctx).includes(token);
}

/** True when the client advertised batch-method support (H-Nostr-Caps: batch). The plugin
 * sends this on every boosted request; it lets us collapse a decrypt-heavy NIP-17 sync into
 * 2 batch round-trips instead of hundreds. A client without it falls back to the bunker gate. */
export function hasBatchCaps(ctx: Ctx): boolean {
    return hasCap(ctx, 'batch');
}

/** Read the `{ results: [...] }` a `*_batch` continuation POSTs back - the order- and
 * length-preserving array the plugin returns (one slot per item we sent). Large limit:
 * a mailbox-sized decrypt batch returns hundreds of plaintext rumors. */
export async function readBatchResults(req: IncomingMessage): Promise<BatchResult[] | null> {
    const body = await readJson(req, 8 * 1024 * 1024).catch(() => null);
    const r = body && typeof body === 'object' ? (body as { results?: unknown }).results : null;
    return Array.isArray(r) ? r as BatchResult[] : null;
}

/** Read a JSON request body (the continuation posts a signed event as JSON). */
export function readJson(req: IncomingMessage, limit = 256 * 1024): Promise<unknown> {
    return new Promise((resolve, reject) => {
        let body = '';
        let size = 0;
        req.on('data', (chunk: Buffer) => {
            size += chunk.length;
            if (size > limit) { reject(new Error('body too large')); req.destroy(); return; }
            body += chunk.toString('utf8');
        });
        req.on('end', () => { try { resolve(JSON.parse(body || 'null')); } catch (e) { reject(e); } });
        req.on('error', reject);
    });
}

/** Serialize an unsigned event template as JSON, escaping `&`/`<`/`>` to their
 * \\uXXXX forms so the payload survives any HTML-oriented response processing in
 * the client. helmjs treats a boosted form's response as a document and runs the
 * body through DOMParser/innerHTML before h:before-swap, which would turn `&`
 * into `&amp;` and parse `<b>` as an element - corrupting (or breaking) the JSON.
 * JSON.parse decodes the \\uXXXX escapes transparently, so the signed event keeps
 * the user's exact content. (The wire contract's escaping rule, completed with &.) */
export function signRequestBody(template: unknown): string {
    return JSON.stringify(template)
        .replace(/&/g, '\\u0026')
        .replace(/</g, '\\u003c')
        .replace(/>/g, '\\u003e');
}

/** Respond "please sign this": the unsigned event in the body + H-Nostr-Sign with
 * the continuation URL. H-Reswap:none keeps a non-plugin client from swapping the
 * JSON; the nip07-hateoas plugin cancels the swap and drives the round-trip. */
export function sendSignRequest(
    ctx: Ctx, template: unknown, continuationUrl: string,
    method: 'sign_event' | 'nip44_encrypt' | 'nip44_decrypt' | 'nip04_decrypt'
        | 'sign_event_batch' | 'nip44_encrypt_batch' | 'nip44_decrypt_batch' | 'nip04_decrypt_batch' = 'sign_event',
): void {
    ctx.res.writeHead(200, {
        'Content-Type': 'application/json; charset=utf-8',
        'H-Nostr-Sign': continuationUrl,
        'H-Nostr-Method': method,
        'H-Reswap': 'none',
        ...securityHeaders(),
    });
    ctx.res.end(signRequestBody(template));
}
