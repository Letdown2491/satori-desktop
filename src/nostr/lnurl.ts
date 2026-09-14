// LUD-06: decode a bech32 `lnurl…` string to its https URL. Owned (bech32 is
// small + stable) so nostr-tools stays our only runtime dependency.

const CHARSET = 'qpzry9x8gf2tvdw0s3jn54khce6mua7l';

/** Decode an LNURL (bech32, no length cap) to its URL, or null if malformed.
 * The checksum isn't verified - a bad decode yields a bad URL that simply fails
 * to fetch, which the zap flow already handles gracefully. */
export function decodeLnurl(lnurl: string): string | null {
    const s = lnurl.trim().toLowerCase();
    const sep = s.lastIndexOf('1');
    if (sep < 1 || s.slice(0, sep) !== 'lnurl') return null;
    const words: number[] = [];
    for (const ch of s.slice(sep + 1)) {
        const v = CHARSET.indexOf(ch);
        if (v === -1) return null;
        words.push(v);
    }
    if (words.length < 6) return null;
    // 5-bit words → 8-bit bytes (dropping the 6-word checksum at the end).
    let acc = 0, bits = 0;
    const bytes: number[] = [];
    for (const w of words.slice(0, -6)) {
        acc = (acc << 5) | w;
        bits += 5;
        if (bits >= 8) { bits -= 8; bytes.push((acc >> bits) & 0xff); }
    }
    try { return new TextDecoder().decode(Uint8Array.from(bytes)) || null; }
    catch { return null; }
}

// bech32 checksum (LUD-06 encode side), owned like the decoder above.
const GEN = [0x3b6a57b2, 0x26508e6d, 0x1ea119fa, 0x3d4233dd, 0x2a1462b3];
function polymod(values: number[]): number {
    let chk = 1;
    for (const v of values) {
        const top = chk >>> 25;
        chk = (((chk & 0x1ffffff) << 5) ^ v) >>> 0;
        for (let i = 0; i < 5; i++) if ((top >>> i) & 1) chk = (chk ^ GEN[i]!) >>> 0;
    }
    return chk >>> 0;
}
function hrpExpand(hrp: string): number[] {
    const out: number[] = [];
    for (let i = 0; i < hrp.length; i++) out.push(hrp.charCodeAt(i) >>> 5);
    out.push(0);
    for (let i = 0; i < hrp.length; i++) out.push(hrp.charCodeAt(i) & 31);
    return out;
}

/** Encode an https URL as an `lnurl…` bech32 string (LUD-06), for the NIP-57 zap-request `lnurl` tag.
 * No length cap (URLs exceed bech32's usual 90-char limit). Mirrors decodeLnurl - owned, no new dep. */
export function encodeLnurl(url: string): string {
    const bytes = new TextEncoder().encode(url);
    const words: number[] = [];
    let acc = 0, bits = 0;
    for (const b of bytes) {
        acc = ((acc << 8) | b) >>> 0;
        bits += 8;
        while (bits >= 5) { bits -= 5; words.push((acc >>> bits) & 31); }
        acc = acc & ((1 << bits) - 1); // keep only the <5 leftover low bits
    }
    if (bits > 0) words.push((acc << (5 - bits)) & 31);
    const mod = polymod([...hrpExpand('lnurl'), ...words, 0, 0, 0, 0, 0, 0]) ^ 1;
    for (let i = 0; i < 6; i++) words.push((mod >>> (5 * (5 - i))) & 31);
    let out = 'lnurl1';
    for (const w of words) out += CHARSET[w];
    return out;
}
