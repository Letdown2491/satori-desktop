// HTTP GET that honors Privacy Mode for server-side fetches (YouTube preview, note
// media proxy, ...). off = direct; balanced = Tor with a direct fallback (fail-open);
// strict = Tor only, no fallback (fail-closed). Uses node http/https rather than
// global fetch, because the SOCKS proxy (TOR_SOCKS) is an http.Agent and undici/fetch
// has no SOCKS support. The .onion relay routing is the WS-layer analog (ws-tor.ts).

import http from 'node:http';
import https from 'node:https';
import { privacyMode, torStrict } from '../privacy.ts';
import { isPublicHttpUrl, safeLookup } from '../ssrf.ts';

export interface HttpResp { status: number; headers: http.IncomingHttpHeaders; body: Buffer; url: string }

// Lazily load the SocksProxyAgent class once (dynamic import so a missing dep can't
// crash the daemon - same resilience as ws-tor.ts), then build a SEPARATE agent PER
// destination host with a distinct SOCKS username. Tor's IsolateSOCKSAuth then gives
// each host its own circuit, so one exit can't correlate fetches to different hosts.
type SocksAgentCls = new (url: string) => unknown;
let spaP: Promise<SocksAgentCls | null> | null = null;
// Bounded: media/preview fetches hit arbitrary hosts, and each agent retains keep-alive sockets -
// unbounded, a long-lived daemon browsing media-heavy feeds accretes agents (and sockets) forever.
// FIFO eviction; an evicted host just pays a fresh circuit on its next fetch.
const AGENT_CAP = 64;
const agentByHost = new Map<string, unknown>();
async function socksAgent(host: string): Promise<unknown | null> {
    const proxy = process.env.TOR_SOCKS?.trim();
    if (!proxy) return null;
    if (!spaP) spaP = import('socks-proxy-agent').then((m) => m.SocksProxyAgent as unknown as SocksAgentCls).catch(() => null);
    const Cls = await spaP;
    if (!Cls) return null;
    const key = host || '_';
    let agent = agentByHost.get(key);
    if (!agent) {
        let purl: URL; try { purl = new URL(proxy); } catch { return null; }
        purl.username = `h-${key}`; purl.password = 'x'; // per-host SOCKS auth → isolated circuit
        try { agent = new Cls(purl.href); } catch { return null; }
        if (agentByHost.size >= AGENT_CAP) { const oldest = agentByHost.keys().next().value; if (oldest !== undefined) agentByHost.delete(oldest); }
        agentByHost.set(key, agent);
    }
    return agent;
}

// A plain, common browser UA so server-side fetches blend into the crowd instead of
// branding every request as a satori daemon (fingerprinting / Tor crowd-blending).
const UA = 'Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0';

function get(url: string, agent: unknown, timeoutMs: number, maxBytes: number, redirects = 3): Promise<HttpResp> {
    return new Promise((resolve, reject) => {
        const lib = url.startsWith('https:') ? https : http;
        let done = false;
        // A HARD wall-clock cap. The `timeout` option only covers socket inactivity
        // AFTER connect; a slow Tor SOCKS connect can hang well past it (seen: 30s),
        // so this guarantees we bail and fall back to a direct fetch on schedule.
        const killer = setTimeout(() => settle(() => reject(new Error('timeout'))), timeoutMs);
        const settle = (fn: () => void): void => { if (done) return; done = true; clearTimeout(killer); req.destroy(); fn(); };
        // DIRECT path only: a validating DNS lookup blocks resolution to a private/metadata IP (DNS
        // rebinding). Under Tor (agent set) the SOCKS proxy resolves remotely, so no local lookup runs.
        const req = lib.get(url, { agent: (agent as http.Agent) || undefined, timeout: timeoutMs, headers: { 'user-agent': UA, accept: '*/*' }, ...(agent ? {} : { lookup: safeLookup }) }, (res) => {
            const status = res.statusCode ?? 0;
            const loc = res.headers.location;
            if (status >= 300 && status < 400 && loc && redirects > 0) {
                res.resume();
                let next: string;
                try { next = new URL(loc, url).href; } catch { settle(() => reject(new Error('bad redirect'))); return; }
                // Re-check each redirect hop (a public url redirecting to an internal one
                // is the SSRF vector) - stronger than only checking the final url.
                if (!isPublicHttpUrl(next)) { settle(() => reject(new Error('redirect to non-public host'))); return; }
                if (done) return; done = true; clearTimeout(killer);
                get(next, agent, timeoutMs, maxBytes, redirects - 1).then(resolve, reject);
                return;
            }
            const chunks: Buffer[] = [];
            let total = 0;
            res.on('data', (c: Buffer) => {
                total += c.length;
                if (total > maxBytes) settle(() => reject(new Error('response too large')));
                else chunks.push(c);
            });
            res.on('end', () => settle(() => resolve({ status, headers: res.headers, body: Buffer.concat(chunks), url })));
        });
        req.on('timeout', () => settle(() => reject(new Error('timeout'))));
        req.on('error', (e) => settle(() => reject(e)));
    });
}

/** Like get() but method+body+header capable (for PUT uploads). No redirect-following:
 * an upload that 3xx-redirects is rejected rather than silently re-PUT elsewhere. */
function request(url: string, opts: { method: string; agent: unknown; timeoutMs: number; maxBytes: number; headers?: Record<string, string>; body?: Buffer }): Promise<HttpResp> {
    return new Promise((resolve, reject) => {
        const lib = url.startsWith('https:') ? https : http;
        let done = false;
        const killer = setTimeout(() => settle(() => reject(new Error('timeout'))), opts.timeoutMs);
        const settle = (fn: () => void): void => { if (done) return; done = true; clearTimeout(killer); req.destroy(); fn(); };
        const req = lib.request(url, { method: opts.method, agent: (opts.agent as http.Agent) || undefined, timeout: opts.timeoutMs, headers: { 'user-agent': UA, accept: '*/*', ...(opts.headers ?? {}) }, ...(opts.agent ? {} : { lookup: safeLookup }) }, (res) => {
            const status = res.statusCode ?? 0;
            const chunks: Buffer[] = [];
            let total = 0;
            res.on('data', (c: Buffer) => {
                total += c.length;
                if (total > opts.maxBytes) settle(() => reject(new Error('response too large')));
                else chunks.push(c);
            });
            res.on('end', () => settle(() => resolve({ status, headers: res.headers, body: Buffer.concat(chunks), url })));
        });
        req.on('timeout', () => settle(() => reject(new Error('timeout'))));
        req.on('error', (e) => settle(() => reject(e)));
        if (opts.body) req.write(opts.body);
        req.end();
    });
}

/** GET via Tor when TOR_SOCKS is set (best-effort), else direct. A Tor failure or a
 * blocked-exit (>=400) falls back to a direct fetch, so previews keep working when
 * Google throttles the Tor network. */
const TOR_ATTEMPT_MS = 5000; // balanced: bail off a slow/blocked exit fast, then go direct.

/** Fetch honoring Privacy Mode:
 *   off      - direct (no Tor).
 *   balanced - try Tor (short cap), fall back to a direct fetch (fail-open).
 *   strict   - Tor only, full timeout, NO direct fallback (fail-closed: throws if Tor
 *              is unavailable or the exit is blocked, so nothing leaks).
 */
export async function torFetch(url: string, timeoutMs = 8000, maxBytes = 10 * 1024 * 1024, redirects = 3): Promise<HttpResp> {
    if (privacyMode() !== 'off') {
        const strict = torStrict();
        const host = (() => { try { return new URL(url).hostname; } catch { return ''; } })();
        const agent = await socksAgent(host);
        if (agent) {
            const torTimeout = strict ? timeoutMs : Math.min(timeoutMs, TOR_ATTEMPT_MS);
            try { const r = await get(url, agent, torTimeout, maxBytes, redirects); if (r.status > 0 && r.status < 400) return r; } catch { /* handled below */ }
        }
        if (strict) throw new Error('Tor required (strict Privacy Mode) but unavailable or blocked');
        // balanced: fall through to a direct fetch
    }
    return get(url, null, timeoutMs, maxBytes, redirects);
}

/** A method+body request honoring Privacy Mode - the outbound-write analog of torFetch,
 * for media uploads. Strict = Tor only, fail-closed (throws, never PUTs direct, so the
 * clearnet IP never leaks). Balanced = Tor first (full timeout, since an upload is user-
 * initiated and worth completing over Tor), direct fallback only when Tor is unreachable.
 * Off = direct. maxBytes bounds the RESPONSE (Blossom returns small JSON), not the body. */
export async function torRequest(url: string, opts: { method: string; headers?: Record<string, string>; body?: Buffer; timeoutMs?: number; maxBytes?: number }): Promise<HttpResp> {
    const timeoutMs = opts.timeoutMs ?? 30000;
    const maxBytes = opts.maxBytes ?? 1024 * 1024;
    const base = { method: opts.method, timeoutMs, maxBytes, headers: opts.headers, body: opts.body };
    if (privacyMode() !== 'off') {
        const strict = torStrict();
        const host = (() => { try { return new URL(url).hostname; } catch { return ''; } })();
        const agent = await socksAgent(host);
        if (agent) {
            try { return await request(url, { ...base, agent }); } catch { /* handled below */ }
        }
        if (strict) throw new Error('Tor required (strict Privacy Mode) but unavailable or blocked');
        // balanced: fall through to a direct request
    }
    return request(url, { ...base, agent: null });
}
