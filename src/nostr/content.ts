// Pure note-content tokenizer: text → an ordered list of tokens. No DOM, no HTML
// strings - the UI's Content component turns these into safe nodes via h().
// Replaces the old contentHtml/nostrEntityHtml string-building.

import { decodeEntity } from './nip19.ts';

export type ContentToken =
    | { t: 'text'; value: string }
    | { t: 'url'; url: string }
    | { t: 'image'; url: string }
    | { t: 'video'; url: string }
    | { t: 'mention'; pubkey: string; bech: string }
    | { t: 'quote'; id: string; relays: string[]; bech: string }
    | { t: 'address'; kind: number; pubkey: string; identifier: string; relays: string[]; bech: string }
    | { t: 'entity'; type: string; bech: string };

const IMG_RE = /\.(jpe?g|png|gif|webp|bmp|avif)(\?[^\s]*)?$/i;
const VIDEO_RE = /\.(mp4|webm|mov|m4v|ogv)(\?[^\s]*)?$/i;
// An http(s) URL (group 1), a nostr: NIP-21 entity (group 2), or a *bare* bech32
// entity (group 3) - many notes paste e.g. npub1… without the nostr: prefix. The
// bare form needs a preceding boundary so it isn't matched inside another word.
const BECH = '[023456789acdefghjklmnpqrstuvwxyz]{20,}'; // bech32 data charset (no 1/b/i/o)
const TOKEN_RE = new RegExp(`(https?:\\/\\/[^\\s<]+)|(?<![\\w/])nostr:([a-z0-9]+)|(?<![\\w/])((?:npub1|nprofile1|nevent1|note1|naddr1)${BECH})`, 'gi');

/** Decode a bech32 entity into a token, falling back to raw text (`raw`). */
function decodeToken(bech: string, raw: string): ContentToken {
    const d = decodeEntity(bech);
    if (!d) return { t: 'text', value: raw };
    if (d.kind === 'mention') return { t: 'mention', pubkey: d.pubkey, bech: d.bech };
    if (d.kind === 'quote') return { t: 'quote', id: d.id, relays: d.relays, bech: d.bech };
    if (d.kind === 'address') return { t: 'address', kind: d.addr.kind, pubkey: d.addr.pubkey, identifier: d.addr.identifier, relays: d.addr.relays, bech: d.bech };
    return { t: 'entity', type: d.type, bech: d.bech };
}

/** Normalize whitespace consistently for every note (trim, cap blank-line runs). */
function normalize(text: string): string {
    return text.replace(/\r\n/g, '\n').replace(/\n{3,}/g, '\n\n').trim();
}

// `normalize` trims + collapses blank lines - right for a whole note, but wrong
// for an inline fragment (it'd eat the spaces around markdown links/marks), so
// callers tokenizing a fragment pass normalize=false.
// A small bounded memo: one note's content is tokenized a few times per render (renderContent +
// mediaLightboxes, plus mention-hydration), all with the same input. The returned token array is
// treated as read-only by every caller, so sharing it is safe. CAP covers a page's distinct
// contents - sized for a large thread (focused note + ~100 replies + embeds), where a too-small
// cap made FIFO eviction defeat the memo mid-render; oldest-inserted is evicted.
const memo = new Map<string, ContentToken[]>();
const MEMO_CAP = 256;
export function tokenize(text: string, normalizeWhitespace = true): ContentToken[] {
    const key = (normalizeWhitespace ? '1' : '0') + text;
    const hit = memo.get(key);
    if (hit) return hit;
    const tokens = tokenizeImpl(text, normalizeWhitespace);
    memo.set(key, tokens);
    if (memo.size > MEMO_CAP) memo.delete(memo.keys().next().value as string);
    return tokens;
}

// A URL match (`[^\s<]+`) greedily includes any trailing sentence punctuation - "see https://x.com, and…"
// or "(https://x.com)" - none of which belongs to the link. Trim a trailing run of punctuation/quotes, plus
// an UNBALANCED closing bracket (so a Wikipedia-style .../Foo_(bar) keeps its own parens). The trimmed chars
// fall back to literal text via the caller shortening `consumed`.
const TRAIL_PUNCT = new Set([...'.,;:!?\'"‘’“”']);
function urlEnd(url: string): number {
    // Tally each bracket pair in ONE pass, then walk the tail decrementing as closers are trimmed -
    // the balance check stays O(1) per trimmed char. (The per-char full rescan this replaced was
    // O(n^2): a link followed by a long run of ')' - one adversarial note - could block the event
    // loop for seconds.)
    const opens: Record<string, number> = { ')': 0, ']': 0, '}': 0 };
    const closes: Record<string, number> = { ')': 0, ']': 0, '}': 0 };
    for (let k = 0; k < url.length; k++) {
        const ch = url[k]!;
        if (ch === '(') opens[')']!++;
        else if (ch === '[') opens[']']!++;
        else if (ch === '{') opens['}']!++;
        else if (ch === ')' || ch === ']' || ch === '}') closes[ch]!++;
    }
    let end = url.length;
    while (end > 0) {
        const c = url[end - 1]!;
        if (TRAIL_PUNCT.has(c)) { end--; continue; }
        if (c === ')' || c === ']' || c === '}') {
            if (closes[c]! > opens[c]!) { closes[c]!--; end--; continue; } // unbalanced closer → not part of the url
        }
        break;
    }
    return end;
}

function tokenizeImpl(text: string, normalizeWhitespace: boolean): ContentToken[] {
    const src = normalizeWhitespace ? normalize(text) : text;
    const tokens: ContentToken[] = [];
    let last = 0;
    for (const m of src.matchAll(TOKEN_RE)) {
        const i = m.index ?? 0;
        if (i > last) tokens.push({ t: 'text', value: src.slice(last, i) });

        let consumed = m[0].length;
        if (m[1]) {
            const url = m[1].slice(0, urlEnd(m[1]));
            consumed = url.length; // trailing punctuation stays as text, not part of the link
            if (IMG_RE.test(url)) tokens.push({ t: 'image', url });
            else if (VIDEO_RE.test(url)) tokens.push({ t: 'video', url });
            else tokens.push({ t: 'url', url });
        } else if (m[2]) {
            tokens.push(decodeToken(m[2], `nostr:${m[2]}`));
        } else if (m[3]) {
            tokens.push(decodeToken(m[3], m[3]));
        }
        last = i + consumed;
    }
    if (last < src.length) tokens.push({ t: 'text', value: src.slice(last) });
    return tokens;
}
