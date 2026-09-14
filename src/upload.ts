// Server-side media upload (Blossom only - NIP-96 is effectively dead; Blossom is
// the de-facto standard and supports multiple servers). The Blossom auth (kind:24242,
// BUD-02) is built as a template, signed either by the bunker (server) or the
// extension (nip07 sign-and-resubmit), then THIS server PUTs the blob to every server
// in your kind:10063 list (first success wins). Counts / notifying others are out of
// scope, matching Satori.

import type { Pool } from './data/pool.ts';
import type { Signer } from './data/signer.ts';
import type { RelayList, NostrEvent, UnsignedEvent } from './nostr/types.ts';
import { INDEXER_RELAYS, writeRelaysFor } from './nostr/nip65.ts';
import { nowSec } from './nostr/tags.ts';
import { torRequest } from './data/torfetch.ts';
import { isPublicHttpUrl } from './ssrf.ts';

const KIND_BLOSSOM_LIST = 10063;
// Fallback when you haven't set a kind:10063 media-server list, so uploads work
// out-of-box. Override by adding your own in Settings → Media servers.
export const DEFAULT_BLOSSOM_SERVER = 'https://blossom.primal.net';
const now = nowSec;
const trim = (u: string) => u.replace(/\/+$/, '');
const serverWriteTargets = (r: RelayList | null): string[] => writeRelaysFor(r);

export interface Upload { url: string; imeta: string[] } // a single NIP-92 imeta tag

/** Your Blossom server list (kind:10063 `server` tags), newest wins. */
export async function fetchBlossomServers(pool: Pool, me: string, myRelays: RelayList | null): Promise<string[]> {
    const relays = [...new Set([...(myRelays?.read ?? []), ...(myRelays?.write ?? []), ...INDEXER_RELAYS])];
    const events = await pool.query(relays, { kinds: [KIND_BLOSSOM_LIST], authors: [me] }).catch(() => []);
    const ev = events.sort((a, b) => b.created_at - a.created_at)[0];
    return ev ? ev.tags.filter((t) => t[0] === 'server' && t[1]).map((t) => trim(t[1]!)) : [];
}

/** Build the unsigned kind:10063 Blossom server list (no signing/publishing). */
export function serverListTemplate(me: string, servers: string[]): UnsignedEvent {
    return { kind: KIND_BLOSSOM_LIST, created_at: now(), pubkey: me, content: '', tags: servers.map((s) => ['server', trim(s)]) };
}

/** Publish a signed kind:10063 to your write relays (nip07: extension signed). */
export async function publishServerListSigned(pool: Pool, signed: NostrEvent, myRelays: RelayList | null): Promise<void> {
    const results = await pool.publish(serverWriteTargets(myRelays), signed);
    if (!results.some((r) => r.status === 'fulfilled')) throw new Error('no relay accepted the media server list');
}

/** Sign (bunker) + publish a kind:10063 server list. */
export async function publishServerList(pool: Pool, signer: Signer, me: string, myRelays: RelayList | null, servers: string[]): Promise<void> {
    const signed = await signer.signEvent(serverListTemplate(me, servers)) as NostrEvent;
    await publishServerListSigned(pool, signed, myRelays);
}

export async function sha256Hex(bytes: Buffer): Promise<string> {
    const hash = await crypto.subtle.digest('SHA-256', new Uint8Array(bytes));
    return Array.from(new Uint8Array(hash), (b) => b.toString(16).padStart(2, '0')).join('');
}

/** Unsigned Blossom auth (BUD-02): kind:24242, not URL-bound, so one signature
 * is reusable across every server in your list. */
export function blossomAuthTemplate(me: string, hash: string): UnsignedEvent {
    return {
        kind: 24242, created_at: now(), pubkey: me, content: 'Upload blob',
        tags: [['t', 'upload'], ['x', hash], ['expiration', String(now() + 3600)]],
    };
}

const authHeader = (signed: NostrEvent): string => `Nostr ${btoa(JSON.stringify(signed))}`;

/** Upload one blob to a single Blossom server (PUT /upload) with a signed auth. */
async function blossomPut(server: string, bytes: Buffer, contentType: string, signed: NostrEvent): Promise<{ url: string; sha256: string; type?: string }> {
    // torRequest, not raw fetch: an upload is outbound traffic and must honor Privacy Mode
    // (Strict fails closed via Tor, never leaking the clearnet IP to the Blossom server).
    const res = await torRequest(`${trim(server)}/upload`, {
        method: 'PUT',
        headers: {
            Authorization: authHeader(signed),
            'Content-Type': contentType || 'application/octet-stream',
            'Content-Length': String(bytes.length),
        },
        body: bytes,
    });
    if (res.status < 200 || res.status >= 300) throw new Error(`blossom upload failed (${res.status})`);
    let j: { url?: string; sha256?: string; type?: string } | null = null;
    try { j = JSON.parse(res.body.toString('utf8')); } catch { j = null; }
    if (!j?.url) throw new Error('blossom returned no url');
    return { url: j.url, sha256: j.sha256 ?? '', type: j.type };
}

/** Pixel dimensions from an image byte buffer (PNG / GIF / JPEG / WebP) as a NIP-92 `dim` value "WxH", or
 * null if unrecognized. Header-only, bounded, never throws - no image library (thin-deps). Enables NIP-68
 * picture `dim` + (later) auto landscape-vs-short video. Videos return null (their headers aren't parsed). */
export function imageDim(b: Buffer): string | null {
    try {
        // PNG: 8-byte signature, then IHDR with width@16, height@20 (big-endian uint32).
        if (b.length >= 24 && b[0] === 0x89 && b[1] === 0x50 && b[2] === 0x4e && b[3] === 0x47) return `${b.readUInt32BE(16)}x${b.readUInt32BE(20)}`;
        // GIF: 'GIF8', width@6, height@8 (little-endian uint16).
        if (b.length >= 10 && b[0] === 0x47 && b[1] === 0x49 && b[2] === 0x46 && b[3] === 0x38) return `${b.readUInt16LE(6)}x${b.readUInt16LE(8)}`;
        // JPEG: FFD8, then walk segments to a Start-Of-Frame marker; height then width (BE uint16) follow.
        if (b.length >= 4 && b[0] === 0xff && b[1] === 0xd8) {
            let i = 2;
            while (i + 9 < b.length) {
                if (b[i] !== 0xff) { i++; continue; }
                const marker = b[i + 1]!;
                if ((marker >= 0xc0 && marker <= 0xc3) || (marker >= 0xc5 && marker <= 0xc7) || (marker >= 0xc9 && marker <= 0xcb) || (marker >= 0xcd && marker <= 0xcf)) return `${b.readUInt16BE(i + 7)}x${b.readUInt16BE(i + 5)}`;
                if (marker === 0xd8 || marker === 0xd9 || (marker >= 0xd0 && marker <= 0xd7)) { i += 2; continue; } // markers with no length
                const len = b.readUInt16BE(i + 2);
                if (len < 2) return null;
                i += 2 + len;
            }
            return null;
        }
        // WebP (extended VP8X): 'RIFF'....'WEBP','VP8X', then (width-1),(height-1) as 24-bit little-endian.
        if (b.length >= 30 && b.toString('ascii', 0, 4) === 'RIFF' && b.toString('ascii', 8, 12) === 'WEBP' && b.toString('ascii', 12, 16) === 'VP8X') {
            return `${1 + (b[24]! | (b[25]! << 8) | (b[26]! << 16))}x${1 + (b[27]! | (b[28]! << 8) | (b[29]! << 16))}`;
        }
        return null;
    } catch { return null; }
}

/** Upload to all Blossom servers (first success wins, like Satori). */
export async function blossomUploadAll(servers: string[], bytes: Buffer, contentType: string, hash: string, signed: NostrEvent): Promise<Upload> {
    // SSRF symmetry with torFetch: the upload PUT is outbound traffic to a user-listed host,
    // so drop any server that resolves to a private/loopback target before we connect.
    const safe = servers.filter((s) => isPublicHttpUrl(`${trim(s)}/upload`));
    const results = await Promise.allSettled(safe.map((s) => blossomPut(s, bytes, contentType, signed)));
    const ok = results.find((r) => r.status === 'fulfilled');
    if (!ok || ok.status !== 'fulfilled') throw new Error('no Blossom server accepted the upload');
    const blob = ok.value;
    const dim = imageDim(bytes);
    const imeta = ['imeta', `url ${blob.url}`, ...(blob.type || contentType ? [`m ${blob.type || contentType}`] : []), ...(dim ? [`dim ${dim}`] : []), `x ${blob.sha256 || hash}`];
    return { url: blob.url, imeta };
}
