// SSRF guard for server-side fetches of USER-CONTROLLED urls (avatar proxy, lnurl /
// zap endpoints, nip05, …). A malicious profile - an avatar URL, a lightning address,
// a nip05 domain - must not be able to make this daemon fetch internal services. We
// block non-http(s) schemes and private / loopback / link-local hosts. `isPublicHttpUrl`
// screens IP LITERALS at request time; `safeLookup` (below) closes DNS rebinding at
// CONNECT time, for EXPOSED multi-user instances. Callers that follow redirects re-check
// each hop's url with isPublicHttpUrl too.

import net from 'node:net';
import { lookup as dnsLookup, type LookupAddress, type LookupOptions } from 'node:dns';

/** The shared private/loopback/link-local host screen, applied after each scheme's protocol check. One
 * copy so a new private-host rule (e.g. another metadata IP) can't be added to the http guard but missed on
 * the ws one. IP literals are validated here; a bare hostname is allowed (DNS rebinding is closed at CONNECT
 * time by safeLookup, not here - and under Tor the SOCKS proxy resolves remotely anyway). */
function screenHost(u: URL): boolean {
    // Node returns IPv6 hostnames wrapped in brackets ("[::1]"); strip them before any
    // check, or every IPv6 literal silently bypasses the guard.
    const host = u.hostname.toLowerCase().replace(/^\[|\]$/g, '');
    if (host === 'localhost' || host === '0.0.0.0' || host.endsWith('.local') || host.endsWith('.internal')) return false;
    if (net.isIPv4(host)) return isPublicIPv4(host);
    if (net.isIPv6(host)) return isPublicIPv6(host);
    return true; // a hostname (not an IP literal)
}

export function isPublicHttpUrl(raw: string): boolean {
    let u: URL;
    try { u = new URL(raw); } catch { return false; }
    if (u.protocol !== 'http:' && u.protocol !== 'https:') return false;
    return screenHost(u);
}

/** Like isPublicHttpUrl but for nostr relay urls (ws/wss): "browse a relay" lets the user aim the daemon
 * at an arbitrary relay, so the same host screen applies - it must not become a probe of internal services. */
export function isPublicWsUrl(raw: string): boolean {
    let u: URL;
    try { u = new URL(raw); } catch { return false; }
    if (u.protocol !== 'ws:' && u.protocol !== 'wss:') return false;
    return screenHost(u);
}

/** True iff the dotted-quad IPv4 is not loopback / private / link-local / unspecified. */
function isPublicIPv4(ip: string): boolean {
    return !/^(127\.|0\.|10\.|169\.254\.|192\.168\.|172\.(1[6-9]|2\d|3[01])\.)/.test(ip);
}

/** True iff the IPv6 literal is not loopback / ULA / link-local / unspecified, and -
 * for IPv4-mapped/compatible forms (dotted or hex) - the embedded IPv4 is public. */
function isPublicIPv6(ip: string): boolean {
    const h = ip.toLowerCase();
    if (h === '::1' || h === '::') return false; // loopback / unspecified
    // IPv4-mapped/compatible, dotted form: ::ffff:127.0.0.1 or ::127.0.0.1
    const dotted = h.match(/(\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3})$/);
    if (dotted && (h.startsWith('::ffff:') || h.startsWith('::'))) return isPublicIPv4(dotted[1]!);
    // IPv4-mapped, hex form (Node normalizes ::ffff:127.0.0.1 to ::ffff:7f00:1)
    const hexMapped = h.match(/^::ffff:([0-9a-f]{1,4}):([0-9a-f]{1,4})$/);
    if (hexMapped) {
        const hi = parseInt(hexMapped[1]!, 16), lo = parseInt(hexMapped[2]!, 16);
        return isPublicIPv4(`${(hi >> 8) & 0xff}.${hi & 0xff}.${(lo >> 8) & 0xff}.${lo & 0xff}`);
    }
    if (/^f[cd]/.test(h)) return false;   // fc00::/7 unique-local
    if (/^fe[89ab]/.test(h)) return false; // fe80::/10 link-local
    return true;
}

/** True iff an IP literal (v4 or v6) is public. A non-IP string → false (safeLookup only ever feeds IPs). */
function isPublicIP(host: string): boolean {
    if (net.isIPv4(host)) return isPublicIPv4(host);
    if (net.isIPv6(host)) return isPublicIPv6(host);
    return false;
}

/** A DNS `lookup` (drop-in for node http/https' `lookup` option) that REJECTS resolution to a private /
 * loopback / link-local address - the DNS-REBINDING defense. isPublicHttpUrl only screens IP LITERALS, so
 * a user-set hostname that resolves to 169.254.169.254 (cloud metadata) or a LAN IP would otherwise slip
 * through. Because the socket connects to the address THIS returns, validating here PINS it and closes the
 * check-vs-connect (TOCTOU) gap. Used only on the DIRECT (non-Tor) path; under Tor the SOCKS proxy resolves
 * remotely. Matters on an EXPOSED multi-user instance (a member's avatar/upload/lnurl host pointing inward). */
export function safeLookup(
    hostname: string,
    options: LookupOptions,
    callback: (err: NodeJS.ErrnoException | null, address: string | LookupAddress[], family?: number) => void,
): void {
    dnsLookup(hostname, { ...options, all: true }, (err, addresses) => {
        if (err) { callback(err, '', 0); return; }
        if (!addresses.length) { callback(new Error(`no address for ${hostname}`), '', 0); return; }
        const bad = addresses.find((a) => !isPublicIP(a.address));
        if (bad) { callback(new Error(`SSRF blocked: ${hostname} resolves to non-public ${bad.address}`), '', 0); return; }
        if (options.all) { callback(null, addresses); return; }
        const a = addresses[0]!;
        callback(null, a.address, a.family);
    });
}
