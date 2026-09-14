// Owned HTML templating. A tagged-template `html` that HTML-escapes EVERY
// interpolated value by default - the server-side mirror of Satori's h(), where
// text is safe by construction. The only way to interpolate trusted markup is to
// wrap it in `raw()` or to nest another `html` template (both are SafeHtml), so
// unescaped user/relay content can never reach the page by accident.

const SAFE = Symbol('SafeHtml');

export interface SafeHtml {
    [SAFE]: true;
    value: string;
}

export function isSafe(x: unknown): x is SafeHtml {
    return typeof x === 'object' && x !== null && (x as Record<symbol, unknown>)[SAFE] === true;
}

/** Wrap already-trusted markup (never user content) so `html` won't escape it. */
export function raw(value: string): SafeHtml {
    return { [SAFE]: true, value };
}

/** HTML-escape a string for text/attribute contexts. Runs on every interpolated value (thousands
 * of times per page), and most inputs contain nothing escapable - one combined test skips the five
 * per-character replace passes for those. */
const NEEDS_ESCAPE = /[&<>"']/;
export function escape(s: string): string {
    if (!NEEDS_ESCAPE.test(s)) return s;
    return s
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;')
        .replace(/'/g, '&#39;');
}

type Value = SafeHtml | string | number | boolean | null | undefined | Value[];

function render(v: Value): string {
    if (v === null || v === undefined || v === false || v === true) return '';
    if (Array.isArray(v)) return v.map(render).join('');
    if (isSafe(v)) return v.value;
    return escape(String(v)); // strings & numbers are escaped - the safe default
}

/** Tagged template producing SafeHtml. Interpolations are escaped unless SafeHtml. */
export function html(strings: TemplateStringsArray, ...values: Value[]): SafeHtml {
    let out = strings[0] ?? '';
    for (let i = 0; i < values.length; i++) out += render(values[i]) + (strings[i + 1] ?? '');
    return raw(out);
}

/** Join an array of SafeHtml fragments (e.g. a list of notes). */
export function join(parts: SafeHtml[], sep: SafeHtml | string = ''): SafeHtml {
    const s = isSafe(sep) ? sep.value : escape(sep);
    return raw(parts.map((p) => p.value).join(s));
}

/** Serialize SafeHtml to a string for the HTTP response body. */
export function renderToString(s: SafeHtml): string {
    return s.value;
}

/** Build a safe `href`/`src` URL attribute value: only http(s), mailto, nostr,
 * and local (/…) URLs become links; anything else (javascript:, data:) is dropped
 * to '#'. Markdown article bodies are attacker-controlled, mirroring Satori's
 * SAFE_LINK_SCHEME guard in components.ts. */
const SAFE_URL = /^(?:https?:|mailto:|nostr:|\/|#|\.{0,2}\/)/i;
export function safeUrl(url: string): string {
    const trimmed = url.trim();
    return SAFE_URL.test(trimmed) ? trimmed : '#';
}
