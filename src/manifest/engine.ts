// The DECLARATIVE archetype ENGINE - the trusted code that renders a PURE-DATA manifest into a card.
// This is the "kinds over the wire" experiment kept LOCAL for now: the manifests live in manifests.ts as
// committed data (later they could arrive as nostr events), but the firewall holds today - a manifest is
// DATA (slot -> source bindings), this engine is the only CODE. A manifest can say "put tag:title in the
// title slot, formatted as text"; it can NEVER carry logic. Every resolved value flows through the same
// gates as everything else (escape via html``, imgSrc SSRF/Tor proxy, safeUrl), and layout is bounded to
// a fixed archetype set - so an untrusted manifest can't run code, leak the IP, or spoof the UI.
//
// SCOPE (deliberate, see the rule-of-three finding): READ-ONLY DISPLAY only. The bespoke logic real kinds
// need (podcast's Tor-aware audio, calendar's RSVP publish) is NOT expressible here and never will be -
// those stay hand-coded handlers. This engine renders the generic skeleton (title + cover + fields + body)
// for the long tail of kinds that fit it, a real card instead of the "open in app" fallback.

import { html, join, type SafeHtml } from '../html.ts';
import { imgSrc, renderContent, renderMarkdown, extLink, prettyHost } from '../render/content.ts';
import { neventFromId, naddrFromCoord } from '../nostr/nip19.ts';
import { cardShell, cardTitle, clampIfTall } from '../render/note.ts';
import { type SatoriDeps, notWired } from './deps.ts';
import type { KindHandler } from './registry.ts';
import type { ProfileMap } from '../render/util.ts';
import type { NostrEvent } from '../nostr/types.ts';

// A restricted selector into the event - NO expressions, just "where to read". `tag:NAME` is the first
// value of the first matching tag (t[1]); `tag:NAME@N` reads t[N] (so packed tags like ["price",amount,
// currency] are reachable); `imeta:FIELD` is a NIP-92 sub-field; `content` is .content.
export type Source = `tag:${string}` | `imeta:${string}` | 'content';
// The closed palette of trusted transforms a manifest may PICK (not write). This is how a wire-kind gets
// e.g. date formatting without a line of attacker logic: the engine owns the formatter, the manifest names it.
// `ref` turns an `e`-tag id or `a`-tag coordinate into an in-app link to that nostr event, reusing the
// content renderer's reference machinery (refFor) - so a manifest can point at nostr-native source content.
export type Format = 'text' | 'tokenized' | 'markdown' | 'image' | 'url' | 'datetime' | 'ref';

// A field reads EITHER one source OR a substitution template combining several (e.g. "{tag:price@1}
// {tag:price@2}" -> "100 sat"). Templates are still pure data - substitution, never logic.
export type Field = { label: string; format: Format } & ({ source: Source } | { template: string });

export interface Manifest {
    kinds: number[];
    archetype: 'card'; // the only archetype for now; add others only when a real kind forces it
    actions?: readonly string[];
    title?: Source;
    cover?: Source;
    fields?: Field[];
    body?: { source: Source; format: 'tokenized' | 'text' | 'markdown' };
}

/** Read a manifest Source out of an event. Returns '' when absent - the engine omits empty slots.
 * `tag:NAME@N` reads the Nth value of the tag (N defaults to 1, the canonical first value). */
function resolve(source: Source, ev: NostrEvent): string {
    if (source === 'content') return ev.content;
    if (source.startsWith('tag:')) {
        const spec = source.slice(4);
        const at = spec.indexOf('@');
        const name = at < 0 ? spec : spec.slice(0, at);
        const idx = at < 0 ? 1 : Math.max(1, Number(spec.slice(at + 1)) || 1);
        return ev.tags.find((t) => t[0] === name)?.[idx] ?? '';
    }
    if (source.startsWith('imeta:')) {
        const field = source.slice(6);
        for (const t of ev.tags) {
            if (t[0] !== 'imeta') continue;
            for (const part of t.slice(1)) if (part.startsWith(field + ' ')) return part.slice(field.length + 1);
        }
    }
    return '';
}

/** Substitute {selector} tokens in a template with resolved values, collapsing the whitespace a missing
 * value leaves behind (so "{a} {b}" with b empty is "a", not "a "). Pure substitution: the literal parts
 * are author text and every value is still escaped when the result renders through html``. */
function resolveTemplate(template: string, ev: NostrEvent): string {
    return template.replace(/\{([^}]+)\}/g, (_, sel) => resolve(sel.trim() as Source, ev)).replace(/\s+/g, ' ').trim();
}

/** A field's resolved string - one source, or a substitution template. */
function fieldValue(f: Field, ev: NostrEvent): string {
    return 'template' in f ? resolveTemplate(f.template, ev) : resolve(f.source, ev);
}

/** Generic datetime: a bare unix-seconds string → local date+time; a "YYYY-MM-DD" → that date (UTC, no
 * tz drift); anything else passes through. The bounded "logic" a manifest gets by NAMING this formatter. */
function formatDateTime(value: string): string {
    if (/^\d{9,}$/.test(value)) return new Date(Number(value) * 1000).toLocaleString('en-US', { dateStyle: 'medium', timeStyle: 'short' });
    const m = /^(\d{4})-(\d{2})-(\d{2})/.exec(value);
    if (m) return new Date(Date.UTC(+m[1]!, +m[2]! - 1, +m[3]!)).toLocaleDateString('en-US', { dateStyle: 'medium', timeZone: 'UTC' });
    return value;
}

/** Apply a Format to a resolved string. Every branch goes through an existing safe primitive. */
function formatValue(value: string, format: Format, profiles?: ProfileMap): SafeHtml {
    switch (format) {
        case 'tokenized': return renderContent(value, profiles, false);
        case 'markdown': return renderMarkdown(value, profiles);
        case 'image': return html`<img class="media" src="${imgSrc(value)}" alt="" loading="lazy">`;
        case 'url': return extLink(value, prettyHost(value)); // show the bare domain, link the full url
        case 'datetime': return html`${formatDateTime(value)}`;
        case 'ref': {
            // An `a` coordinate (kind:pubkey:dtag) → naddr, else a bare event id → nevent; render the
            // `nostr:` reference through the content tokenizer (embeds=false → an in-app chip link via
            // refFor). Not a recognizable ref → fall through to escaped text.
            const bech = naddrFromCoord(value) ?? neventFromId(value);
            return bech ? renderContent(`nostr:${bech}`, profiles, false) : html`${value}`;
        }
        default: return html`${value}`; // 'text' - escaped by html``
    }
}

/** Render the `card` archetype: title -> labeled fields -> cover -> body. The body clamps on non-focused
 * surfaces (a long generic write-up shouldn't wall a list), reusing the same clamp the coded kinds use. */
function renderCard(m: Manifest, ev: NostrEvent, profiles: ProfileMap | undefined, clamp: boolean): SafeHtml {
    const title = m.title ? resolve(m.title, ev) : '';
    const cover = m.cover ? resolve(m.cover, ev) : '';
    const bodyVal = m.body ? resolve(m.body.source, ev) : '';
    // Drop a field that just repeats the body verbatim (e.g. a highlight whose `context` tag is the
    // highlighted passage again) so the same text never renders twice. Whitespace-normalized exact match
    // only - a genuinely broader context (one that merely CONTAINS the passage) still shows.
    const norm = (s: string) => s.replace(/\s+/g, ' ').trim();
    const bodyNorm = norm(bodyVal);
    const fields = (m.fields ?? [])
        .map((f) => ({ label: f.label, value: fieldValue(f, ev), format: f.format }))
        .filter((f) => f.value && norm(f.value) !== bodyNorm);
    const bodyHtml = m.body && bodyVal ? formatValue(bodyVal, m.body.format, profiles) : null;
    return html`
      ${cardTitle(title)}
      ${fields.length ? html`<div class="field-list">${join(fields.map((f) =>
        html`<div class="field"><span class="field-label">${f.label}</span><span class="field-value">${formatValue(f.value, f.format, profiles)}</span></div>`))}</div>` : null}
      ${cover ? html`<img class="media" src="${imgSrc(cover)}" alt="${title}" loading="lazy">` : null}
      ${clampIfTall(bodyHtml, bodyVal, clamp, ev.id)}`;
}

/** Synthesize a manifest into a KindHandler. The registry can't tell this from a hand-coded handler -
 * that's the point: a kind can be data OR code transparently, dispatched the same way. */
export function fromManifest(m: Manifest): KindHandler<SatoriDeps> {
    return {
        kinds: m.kinds,
        actions: m.actions ?? ['reply', 'quote', 'like', 'zap', 'bookmark'],
        render(ev, surface, d) {
            if (surface === 'reader') return notWired(surface);
            return cardShell(ev, d.profiles, d.s, renderCard(m, ev, d.profiles, surface !== 'focused'), { compact: surface === 'embed' });
        },
    };
}
