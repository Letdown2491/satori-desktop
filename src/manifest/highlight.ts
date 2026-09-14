// NIP-84 highlight (kind 9802), GRADUATED from the declarative engine to a hand-coded handler. The generic
// `card` archetype rendered title -> fields -> body, which forced the highlighted passage (the whole point)
// to the BOTTOM, under utilitarian "Source"/"Context" labels. A highlight is a QUOTATION, so it wants the
// inverse: lead with the passage as a blockquote, then a small source citation. That hierarchy isn't
// expressible in the engine's fixed archetype, which is exactly the graduation trigger classifieds proved.

import type { KindHandler } from './registry.ts';
import { refFor } from './registry.ts';
import { type SatoriDeps, notWired } from './deps.ts';
import { html, join, type SafeHtml } from '../html.ts';
import { extLink, prettyHost, mentionChip, inlineEntities } from '../render/content.ts';
import { cardShell, clampIfTall } from '../render/note.ts';
import { npub, displayName, type ProfileMap } from '../render/util.ts';
import { ensureProfiles } from '../routes/common.ts';
import { neventFromId, naddrFromCoord } from '../nostr/nip19.ts';
import { tag1, coordParts } from '../nostr/tags.ts';
import { KIND_HIGHLIGHT } from '../nostr/nip84.ts';
import type { NostrEvent } from '../nostr/types.ts';

/** The ORIGINAL author(s) of the highlighted content (NIP-84 `p` tags), for "by X" attribution. A roleless
 * `["p", pubkey]` or an explicit `["p", pubkey, relay, "author"]` counts; a `"mention"` role (a mention in
 * the highlighter's comment, not the author) is excluded, as is `"editor"`. Deduped, preserves order. */
function originalAuthors(ev: NostrEvent): string[] {
    const out: string[] = [];
    for (const t of ev.tags) {
        if (t[0] !== 'p' || !t[1]) continue;
        const role = t[3];
        if (role === 'mention' || role === 'editor') continue; // a comment mention / editor, not the author
        if (!out.includes(t[1])) out.push(t[1]);
    }
    return out;
}

/** The source link: a web page (`r`) → a clean domain link; a nostr addressable (`a`) or event (`e`) → an
 * in-app reference link (reusing the registry's refFor, same as inline content references). First present
 * source wins; null when a highlight carries none. */
function sourceLink(ev: NostrEvent): SafeHtml | null {
    // NIP-84: the highlighted page's url carries the `source` marker; `mention`-marked r tags are
    // urls from the highlighter's comment, NOT the source. Prefer the marked source, tolerate a
    // bare (unmarked, legacy) r, never cite a mention as the source.
    const r = (ev.tags.find((t) => t[0] === 'r' && t[1] && t[2] === 'source')
        ?? ev.tags.find((t) => t[0] === 'r' && t[1] && t[2] !== 'mention'))?.[1];
    if (r) return extLink(r, prettyHost(r));
    const a = tag1(ev, 'a');
    if (a) {
        const bech = naddrFromCoord(a);
        if (bech) {
            const ref = refFor(coordParts(a)?.kind ?? 0);
            return ref
                ? mentionChip(ref.path(bech), ref.label)
                : html`<a class="mention" href="https://njump.me/${bech}" target="_blank" rel="noopener noreferrer">↗ source</a>`;
        }
    }
    const e = tag1(ev, 'e');
    const bech = e ? neventFromId(e) : null;
    if (bech) return mentionChip(`/t/${bech}`, '↗ quoted note');
    return null;
}

/** The citation beneath the quote: "by <original author>" (NIP-84 attribution) and/or the source link.
 * Either may be absent; null when both are. Author names resolve from the profiles the `prepare` hook warms,
 * degrading to a short npub link when a profile hasn't loaded. */
function citation(ev: NostrEvent, profiles: ProfileMap | undefined): SafeHtml | null {
    const src = sourceLink(ev);
    const authors = originalAuthors(ev);
    const by = authors.length
        ? html`by ${join(authors.map((pk) => mentionChip(`/u/${npub(pk)}`, displayName(pk, profiles))), ', ')}`
        : null;
    if (!src && !by) return null;
    return html`<div class="hl-cite">${by}${by && src ? html` · ` : null}${src}</div>`;
}

/** The highlight body: the passage as a blockquote, with attribution + source cited beneath. When a `context`
 * tag genuinely surrounds the passage, render the context and emphasize the highlighted span inside it (the
 * passage in situ); otherwise just the passage. Clamps on non-focused surfaces, like every other card. */
function highlightBody(ev: NostrEvent, profiles: ProfileMap | undefined, clamp: boolean): SafeHtml {
    const passage = ev.content.trim();
    const context = tag1(ev, 'context').trim();
    let inner: SafeHtml;
    let rawText: string;
    if (passage && context.includes(passage) && context !== passage) {
        const i = context.indexOf(passage);
        inner = html`${context.slice(0, i)}<mark class="hl-mark">${passage}</mark>${context.slice(i + passage.length)}`;
        rawText = context;
    } else {
        rawText = passage || context;
        inner = html`${rawText}`;
    }
    const quote = rawText ? html`<blockquote class="hl-quote">${inner}</blockquote>` : null;
    // NIP-84: a `comment` tag MUST render like a quote repost - the highlighter's words as the
    // post, the highlighted passage as the quoted block beneath them.
    const comment = tag1(ev, 'comment').trim();
    const commentEl = comment ? html`<div class="hl-comment">${inlineEntities(comment, profiles)}</div>` : null;
    return html`${commentEl}${clampIfTall(quote, rawText, clamp, ev.id)}${citation(ev, profiles)}`;
}

export const highlightHandler: KindHandler<SatoriDeps> = {
    kinds: [KIND_HIGHLIGHT],
    actions: ['reply', 'quote', 'like', 'zap', 'bookmark'],
    // Warm the ORIGINAL authors' profiles so the "by X" attribution shows names, not short npubs. They're
    // tag pubkeys (not the event author, not in-content mentions), so the route's own hydration misses them.
    async prepare(events, s) {
        const authors = [...new Set(events.flatMap(originalAuthors))];
        if (authors.length) await ensureProfiles(s, authors).catch(() => {});
    },
    render(ev, surface, d) {
        if (surface === 'reader') return notWired(surface); // the card carries the highlight; no separate reader
        return cardShell(ev, d.profiles, d.s, highlightBody(ev, d.profiles, surface !== 'focused'), { compact: surface === 'embed' });
    },
};
