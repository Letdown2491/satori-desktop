// NIP-19 / NIP-21: decode a bare bech32 entity (the part after `nostr:`) into a
// shape the content renderer can use. Thin wrapper over nostr-tools' decode.

import { decode, neventEncode, naddrEncode } from 'nostr-tools/nip19';

export interface AddrPointer { kind: number; pubkey: string; identifier: string; relays: string[] }

export type DecodedEntity =
    | { kind: 'mention'; pubkey: string; bech: string }
    | { kind: 'quote'; id: string; relays: string[]; bech: string }
    | { kind: 'address'; addr: AddrPointer; bech: string }
    | { kind: 'other'; type: string; bech: string };

export function decodeEntity(bech: string): DecodedEntity | null {
    let decoded;
    try { decoded = decode(bech); } catch { return null; }
    const { type, data } = decoded;
    if (type === 'npub') return { kind: 'mention', pubkey: data as string, bech };
    if (type === 'nprofile') return { kind: 'mention', pubkey: (data as { pubkey: string }).pubkey, bech };
    if (type === 'note') return { kind: 'quote', id: data as string, relays: [], bech };
    if (type === 'nevent') {
        const d = data as { id: string; relays?: string[] };
        return { kind: 'quote', id: d.id, relays: d.relays ?? [], bech };
    }
    if (type === 'naddr') {
        const d = data as { kind: number; pubkey: string; identifier: string; relays?: string[] };
        return { kind: 'address', addr: { kind: d.kind, pubkey: d.pubkey, identifier: d.identifier, relays: d.relays ?? [] }, bech };
    }
    return { kind: 'other', type, bech };
}

const HEX64_RE = /^[0-9a-f]{64}$/i;

/** A bare 64-hex / npub / nprofile → hex pubkey, or null. One decoder for every route that takes a
 * "who" param (closes the npub-only sites that silently dropped nprofile). */
export function pubkeyFromBech(s: string): string | null {
    if (HEX64_RE.test(s)) return s.toLowerCase();
    try {
        const d = decode(s);
        if (d.type === 'npub') return d.data as string;
        if (d.type === 'nprofile') return (d.data as { pubkey: string }).pubkey;
    } catch { /* fall through */ }
    return null;
}

const COORD_RE = /^(\d+):([0-9a-f]{64}):(.*)$/i;

/** Encode an `e`-tag event id (hex) as an `nevent`, or null if it isn't a 64-hex id. The inverse of the
 * `quote` decode above - lets a raw tag value become a `nostr:` reference the content renderer can link. */
export function neventFromId(id: string): string | null {
    return HEX64_RE.test(id) ? neventEncode({ id: id.toLowerCase() }) : null;
}

/** Canonicalize a reply/comment parent reference (a raw `e`-tag value) into an `nevent`, or null if it isn't
 * one. A clean 64-hex id is encoded (carrying any author/relay hints); a value that is ALREADY a note/nevent
 * bech is kept as-is; anything else (an `a`-coordinate, junk, or a short/padded id that `neventEncode` would
 * reject) yields null - so callers drop a would-be-broken embed rather than emit a dead `/t/` link + an
 * undecodable `/embed/` (which the reader surfaced as a bare "↗ link"). */
export function neventFromRef(value: string, opts: { author?: string; relays?: string[] } = {}): string | null {
    if (HEX64_RE.test(value)) {
        const relays = (opts.relays ?? []).filter((r) => typeof r === 'string' && r.length > 0);
        // `neventEncode` hex-decodes the author too, so a malformed parent pubkey (NIP-22) would throw -
        // gate it like the id, and keep a try/catch as a backstop so any bad input yields null, never a 500.
        const author = opts.author && HEX64_RE.test(opts.author) ? opts.author.toLowerCase() : undefined;
        try { return neventEncode({ id: value.toLowerCase(), ...(author ? { author } : {}), ...(relays.length ? { relays } : {}) }); }
        catch { return null; }
    }
    try { const d = decode(value); if (d.type === 'note' || d.type === 'nevent') return value; } catch { /* not a bech either */ }
    return null;
}

/** Encode an `a`-tag coordinate (`kind:pubkey:identifier`) as an `naddr`, or null if malformed. */
export function naddrFromCoord(coord: string): string | null {
    const m = COORD_RE.exec(coord);
    return m ? naddrEncode({ kind: Number(m[1]), pubkey: m[2]!.toLowerCase(), identifier: m[3]! }) : null;
}

/** Decode an naddr into its address pointer + the `kind:pubkey:identifier` coordinate, or null. */
export function decodeNaddr(bech: string): (AddrPointer & { coord: string }) | null {
    try {
        const d = decode(bech);
        if (d.type !== 'naddr') return null;
        const a = d.data as { kind: number; pubkey: string; identifier: string; relays?: string[] };
        return { kind: a.kind, pubkey: a.pubkey, identifier: a.identifier, relays: a.relays ?? [], coord: `${a.kind}:${a.pubkey}:${a.identifier}` };
    } catch { return null; }
}
