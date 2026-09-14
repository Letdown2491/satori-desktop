// A small, owned Markdown parser → block + inline ASTs. Pure (no DOM); the UI
// turns the AST into safe nodes via h(), and runs plain-text runs through the
// note tokenizer so URLs / nostr entities still resolve. A pragmatic CommonMark
// subset - headings, paragraphs, lists, blockquotes, fenced code, rules, images,
// and inline bold/italic/code/links. Unknown syntax degrades to plain text.

export type Block =
    | { t: 'heading'; level: number; text: string }
    | { t: 'paragraph'; text: string }
    | { t: 'list'; ordered: boolean; items: string[] }
    | { t: 'quote'; text: string }
    | { t: 'code'; lang: string; text: string }
    | { t: 'hr' }
    | { t: 'break' } // a `<br>` alone on its own line (authors use it for extra spacing) → a bare <br>
    | { t: 'image'; url: string; alt: string };

export type Inline =
    | { t: 'text'; v: string }
    | { t: 'strong'; v: string }
    | { t: 'em'; v: string }
    | { t: 'code'; v: string }
    | { t: 'link'; text: string; href: string }
    | { t: 'image'; url: string; alt: string }
    | { t: 'break' }; // a literal <br> the author wrote for a line break (the ONLY raw HTML we honor)

// Image/link URLs may carry an optional Markdown title: `(url "title")`. Per CommonMark the destination
// may be wrapped in optional whitespace - `]( url )` is valid - so allow `\s*` around it (sloppy authors
// write `]( https://…)` with a leading space, which a spec-compliant renderer still shows).
const IMG_ONLY = /^!\[([^\]]*)\]\(\s*([^)\s]+)(?:\s+"[^"]*")?\s*\)$/;
const HR = /^\s*(-{3,}|\*{3,}|_{3,})\s*$/;
const BR_ONLY = /^\s*<[bB][rR]\s*\/?>\s*$/; // a line that is only a bare <br> (no attributes)
const HEADING = /^(#{1,6})\s+(.*)$/;
const LIST = /^\s*([-*+]|\d+\.)\s+/;

export function parseBlocks(md: string): Block[] {
    const lines = md.replace(/\r\n/g, '\n').split('\n');
    const blocks: Block[] = [];
    let i = 0;
    while (i < lines.length) {
        const line = lines[i]!;
        if (line.trim() === '') { i++; continue; }

        const fence = /^```(.*)$/.exec(line);
        if (fence) {
            const buf: string[] = [];
            i++;
            while (i < lines.length && !/^```/.test(lines[i]!)) { buf.push(lines[i]!); i++; }
            i++; // closing fence
            blocks.push({ t: 'code', lang: fence[1]!.trim(), text: buf.join('\n') });
            continue;
        }
        const h = HEADING.exec(line);
        if (h) { blocks.push({ t: 'heading', level: h[1]!.length, text: h[2]!.trim() }); i++; continue; }
        if (HR.test(line)) { blocks.push({ t: 'hr' }); i++; continue; }
        if (BR_ONLY.test(line)) { blocks.push({ t: 'break' }); i++; continue; } // a lone `<br>` line → a bare <br>, not a <p><br></p>
        const im = IMG_ONLY.exec(line.trim());
        if (im) { blocks.push({ t: 'image', alt: im[1]!, url: im[2]! }); i++; continue; }
        if (/^>\s?/.test(line)) {
            const buf: string[] = [];
            while (i < lines.length && /^>\s?/.test(lines[i]!)) { buf.push(lines[i]!.replace(/^>\s?/, '')); i++; }
            blocks.push({ t: 'quote', text: buf.join('\n') });
            continue;
        }
        if (LIST.test(line)) {
            const ordered = /^\s*\d+\.\s+/.test(line);
            const items: string[] = [];
            while (i < lines.length && LIST.test(lines[i]!)) { items.push(lines[i]!.replace(LIST, '')); i++; }
            blocks.push({ t: 'list', ordered, items });
            continue;
        }
        // Setext heading: a text line underlined by only `=` (H1) or `-` (H2). Checked here - after
        // the fence/ATX/HR/quote/list starters and before the paragraph fallback - so `line` is a plain
        // text line. `----` directly under text is a Setext H2 (CommonMark); a standalone `----` after a
        // blank line never reaches here (it's the current line, caught by HR above), so no ambiguity.
        const under = lines[i + 1];
        if (under !== undefined && /^\s*(=+|-+)\s*$/.test(under)) {
            blocks.push({ t: 'heading', level: under.trim()[0] === '=' ? 1 : 2, text: line.trim() });
            i += 2; continue;
        }

        // paragraph: gather until a blank line or the next block-starter
        const buf: string[] = [];
        while (i < lines.length) {
            const l = lines[i]!;
            if (l.trim() === '' || /^```/.test(l) || HEADING.test(l) || /^>\s?/.test(l) || LIST.test(l) || HR.test(l)) break;
            buf.push(l); i++;
        }
        blocks.push({ t: 'paragraph', text: buf.join('\n') });
    }
    return blocks;
}

// Groups: 1 code · 2 image · 3 link · 4 angle-bracket autolink · 5/6 strong · 7/8 em · 9 <br>.
// The <br> alternative matches ONLY a bare, attribute-less break (`<br>` / `<br/>` / `<br />`); anything
// with attributes stays escaped as text. It's the one raw-HTML tag we honor (void element, no injection).
const INLINE = /(`[^`]+`)|(!\[[^\]]*\]\(\s*[^)\s]+(?:\s+"[^"]*")?\s*\))|(\[[^\]]+\]\(\s*[^)\s]+(?:\s+"[^"]*")?\s*\))|(<https?:\/\/[^>\s]+>)|(\*\*[^*]+\*\*)|(__[^_]+__)|(\*[^*]+\*)|(_[^_]+_)|(<[bB][rR]\s*\/?>)/g;
const LINK_PARTS = /^\[([^\]]+)\]\(\s*([^)\s]+)(?:\s+"[^"]*")?\s*\)$/;

export function parseInline(text: string): Inline[] {
    const out: Inline[] = [];
    let last = 0;
    for (const m of text.matchAll(INLINE)) {
        const i = m.index ?? 0;
        if (i > last) out.push({ t: 'text', v: text.slice(last, i) });
        const tok = m[0];
        if (m[1]) out.push({ t: 'code', v: tok.slice(1, -1) });
        else if (m[2]) { const mm = IMG_ONLY.exec(tok)!; out.push({ t: 'image', alt: mm[1]!, url: mm[2]! }); }
        else if (m[3]) { const mm = LINK_PARTS.exec(tok)!; out.push({ t: 'link', text: mm[1]!, href: mm[2]! }); }
        else if (m[4]) { const url = tok.slice(1, -1); out.push({ t: 'link', text: url, href: url }); } // <url> autolink
        else if (m[5] || m[6]) out.push({ t: 'strong', v: tok.slice(2, -2) });
        else if (m[9]) out.push({ t: 'break' });
        else out.push({ t: 'em', v: tok.slice(1, -1) });
        last = i + tok.length;
    }
    if (last < text.length) out.push({ t: 'text', v: text.slice(last) });
    return out;
}
