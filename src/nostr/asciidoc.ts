// A small, owned AsciiDoc parser → the SHARED Block/Inline AST (from markdown.ts), so the wiki reader
// reuses the exact article-body renderer + typography. A pragmatic subset of AsciiDoc as used by Nostr
// wiki (NIP-54) and Alexandria publications: section headings (= .. ======), paragraphs, bulleted (* / -)
// and numbered (. / 1.) lists, listing/literal/passthrough code blocks (---- / .... / ++++), quote blocks
// (____), admonitions (NOTE:/TIP:/...), block images (image::url[alt]), horizontal rules ('''), and inline
// bold (*x*), italic (_x_), monospace (`x`), and links (link:url[text] / url[text]). Bare URLs + nostr:
// entities are left in text runs so the note tokenizer resolves them, exactly like the Markdown path.
// Unknown syntax degrades to plain text. Pure (no DOM); the UI turns the AST into safe nodes.

import type { Block, Inline } from './markdown.ts';

const HEADING = /^(={1,6})\s+(.*)$/;
const HR = /^'{3,}$/;
const ULIST = /^\s*[*-]\s+/;
const OLIST = /^\s*(?:\.+|\d+\.)\s+/;
const IMAGE = /^image::([^[]+)\[([^\]]*)\]\s*$/;
const DELIM = /^(-{4,}|\.{4,}|_{4,}|={4,}|\*{4,}|\+{4,})\s*$/; // listing/literal/quote/example/sidebar/passthrough
const ADMON = /^(NOTE|TIP|IMPORTANT|WARNING|CAUTION):\s+(.*)$/;

export function parseAdocBlocks(src: string): Block[] {
    const lines = src.replace(/\r\n/g, '\n').split('\n');
    const blocks: Block[] = [];
    let i = 0;
    let sourceLang = ''; // captured from a preceding [source,lang] attribute line
    while (i < lines.length) {
        const line = lines[i]!;
        if (line.trim() === '') { i++; continue; }
        // Block comment (//// ... ////) then line comment (//)
        if (/^\/{4,}\s*$/.test(line)) { i++; while (i < lines.length && !/^\/{4,}\s*$/.test(lines[i]!)) i++; i++; continue; }
        if (/^\/\//.test(line)) { i++; continue; }
        // Attribute line: capture [source,lang] for the NEXT delimited block; drop other [..] / anchors.
        const attr = /^\[(.+)\]\s*$/.exec(line);
        if (attr) { const m = /source\s*,\s*([\w-]+)/i.exec(attr[1]!); sourceLang = m ? m[1]! : ''; i++; continue; }
        if (/^\.\S/.test(line)) { i++; continue; } // block title (.Title, no space) - dropped in the subset

        const d = DELIM.exec(line);
        if (d) {
            const delim = d[1]![0]!; // one of - . _ = * +
            const buf: string[] = []; i++;
            while (i < lines.length && !DELIM.test(lines[i]!)) { buf.push(lines[i]!); i++; }
            i++; // closing delimiter (or EOF)
            if (delim === '-' || delim === '.' || delim === '+') blocks.push({ t: 'code', lang: sourceLang, text: buf.join('\n') });
            else if (delim === '_') blocks.push({ t: 'quote', text: buf.join('\n') });
            else for (const b of parseAdocBlocks(buf.join('\n'))) blocks.push(b); // example/sidebar: render contents inline
            sourceLang = '';
            continue;
        }
        sourceLang = '';

        const h = HEADING.exec(line);
        if (h) { blocks.push({ t: 'heading', level: h[1]!.length, text: h[2]!.trim() }); i++; continue; }
        if (HR.test(line)) { blocks.push({ t: 'hr' }); i++; continue; }
        const im = IMAGE.exec(line);
        if (im) { blocks.push({ t: 'image', url: im[1]!.trim(), alt: im[2]! }); i++; continue; }
        const adm = ADMON.exec(line);
        if (adm) { blocks.push({ t: 'quote', text: `*${adm[1]}:* ${adm[2]}` }); i++; continue; }

        if (ULIST.test(line) || OLIST.test(line)) {
            const ordered = !ULIST.test(line); // '. ' / '1. ' are ordered; '* ' / '- ' unordered
            const re = ordered ? OLIST : ULIST;
            const items: string[] = [];
            while (i < lines.length && re.test(lines[i]!)) { items.push(lines[i]!.replace(re, '')); i++; }
            blocks.push({ t: 'list', ordered, items });
            continue;
        }
        // paragraph: gather until a blank line or the next block-starter
        const buf: string[] = [];
        while (i < lines.length) {
            const l = lines[i]!;
            if (l.trim() === '' || HEADING.test(l) || HR.test(l) || DELIM.test(l) || ULIST.test(l) || OLIST.test(l) || IMAGE.test(l) || /^\/\//.test(l)) break;
            buf.push(l); i++;
        }
        blocks.push({ t: 'paragraph', text: buf.join('\n') });
    }
    return blocks;
}

/** A NIP-54 wikilink `[[Topic]]` / `[[Topic#Section|display]]` - a reference to another wiki article by
 * its topic (the render layer resolves the topic to a `d` slug + naddr; kept DOM-free here). */
export type WikiLink = { t: 'wikilink'; target: string; display: string };

/** NIP-54 topic → `d` slug, per the spec's normalization rules: lowercase (MUST), whitespace
 * to `-` (MUST), punctuation/symbols removed (SHOULD - so "what's up" is "whats-up", not
 * "what-s-up"), dash runs collapsed + trimmed (SHOULD). Non-ASCII letters and numbers MUST
 * survive as UTF-8 - a Japanese or Cyrillic topic keeps its characters. `-` itself is kept as
 * the separator (it's what whitespace becomes), so dashed topics round-trip. */
export function normalizeWikiTopic(s: string): string {
    return s.toLowerCase()
        .replace(/\s+/gu, '-')
        .replace(/(?!-)[\p{P}\p{S}]/gu, '')
        .replace(/-{2,}/g, '-')
        .replace(/^-+|-+$/g, '');
}

// Groups: 1 wikilink · 2 monospace · 3 inline image · 4 link (optional `link:` prefix, http(s) url, [text]) · 5 bold · 6 italic.
// A bare URL / nostr: entity is intentionally NOT matched here - it stays in a text run so inlineEntities resolves it.
const INLINE = /(\[\[[^\]]+\]\])|(`[^`]+`)|(image:[^[\s]+\[[^\]]*\])|((?:link:)?https?:\/\/[^\s[]+\[[^\]]*\])|(\*[^*\n]+\*)|(_[^_\n]+_)/g;

export function parseAdocInline(text: string): (Inline | WikiLink)[] {
    const out: (Inline | WikiLink)[] = [];
    let last = 0;
    for (const m of text.matchAll(INLINE)) {
        const i = m.index ?? 0;
        if (i > last) out.push({ t: 'text', v: text.slice(last, i) });
        const tok = m[0];
        if (m[1]) { // [[Topic#Section|display]] - target is the topic (drop the #section anchor for the slug)
            const inner = tok.slice(2, -2);
            const bar = inner.indexOf('|');
            const targetPart = (bar >= 0 ? inner.slice(0, bar) : inner).trim();
            const display = (bar >= 0 ? inner.slice(bar + 1) : targetPart).trim();
            out.push({ t: 'wikilink', target: targetPart.split('#')[0]!.trim(), display: display || targetPart });
        }
        else if (m[2]) out.push({ t: 'code', v: tok.slice(1, -1) });
        else if (m[3]) { const mm = /^image:([^[]+)\[([^\]]*)\]$/.exec(tok)!; out.push({ t: 'image', url: mm[1]!, alt: mm[2]! }); }
        else if (m[4]) { const mm = /^(?:link:)?(\S+?)\[([^\]]*)\]$/.exec(tok)!; out.push({ t: 'link', href: mm[1]!, text: mm[2]! || mm[1]! }); }
        else if (m[5]) out.push({ t: 'strong', v: tok.slice(1, -1) });
        else out.push({ t: 'em', v: tok.slice(1, -1) });
        last = i + tok.length;
    }
    if (last < text.length) out.push({ t: 'text', v: text.slice(last) });
    return out;
}
