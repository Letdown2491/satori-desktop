// Note / profile / article view fragments, in Satori's exact markup + class
// names so the ported Sumi-e CSS applies 1:1. The string port of the relevant
// pieces of Satori's ui/components.ts. MVP-era reads; the full stateful action
// bar (like/zap/bookmark) is wired in Phase 1 - here the row is presentational
// with the navigational actions (reply/quote → thread) live.

import { neventEncode, naddrEncode } from 'nostr-tools/nip19';
import { html, join, raw, safeUrl, type SafeHtml } from '../html.ts';
import { renderContent, renderMarkdown, renderAsciiDoc, mediaLightboxes, imgSrc, withEmoji, cleanTrackingParams, imageItems, mediaTiles, type MediaPrefs } from './content.ts';
import { parseImeta, type ImetaMap } from '../nostr/imeta.ts';
import { parseEmojiTags } from '../nostr/nip30.ts';
import { tokenize } from '../nostr/content.ts';
import { npub, shortNpub, displayName, timeAgo, avatar, stripScheme, type ProfileMap } from './util.ts';
import { icon, enso } from './svg.ts';
import { quote } from './quotes.ts';
import { replyParent } from '../nostr/nip10.ts';
import { commentParent } from '../nostr/nip22.ts';
import { naddrFromCoord, neventFromRef } from '../nostr/nip19.ts';
import { formatNip05 } from '../nostr/nip05.ts';
import { parseArticle, readingMinutes } from '../nostr/nip23.ts';
import { parseCustomNip } from '../nostr/customnip.ts';
import { parseWiki } from '../nostr/nip54.ts';
import { parseRepo } from '../nostr/nip34.ts';
import { tag1, isAddressable } from '../nostr/tags.ts';
import { renderEvent, actionsFor } from '../manifest/registry.ts';
import { bookmarkButton, pinButton, followButton, muteButton, muteAct, likeButton, deleteAct } from './actions.ts';
import { isZapped } from '../zaps.ts';
import { replyFaces, type ReplyFaces } from '../replies.ts';
import { hasReplied, hasReposted, engageTarget } from '../engaged.ts';
import type { NostrEvent } from '../nostr/types.ts';
import type { Profile } from '../data/profiles.ts';
import type { Session } from '../session.ts';

function pic(pubkey: string, profiles?: ProfileMap): string | undefined {
    return profiles?.get(pubkey)?.picture;
}

/** NIP-36 content warning: the reason string, '' (tag with no reason), or null. */
export function contentWarning(ev: NostrEvent): string | null {
    const t = ev.tags.find((x) => x[0] === 'content-warning');
    return t ? (t[1] ?? '') : null;
}

/** Blur the rendered content behind a tap-to-reveal overlay (zero-JS via a hidden checkbox whose
 * label is the overlay). Both the overlay and the corner "hide" pill are labels for the SAME checkbox,
 * so revealing and re-hiding are both one tap: the overlay (shown while hidden) checks it, the hide
 * pill (shown while revealed) unchecks it. Re-hideable, unlike the old one-way reveal. */
export function cwWrap(inner: SafeHtml, reason: string, evId: string): SafeHtml {
    const cwid = `cw-${evId}`;
    return html`
      <div class="cw">
        <input type="checkbox" class="cw-toggle" id="${raw(cwid)}">
        ${inner}
        <label class="cw-overlay" for="${raw(cwid)}">
          <span class="cw-reason">${reason ? `Content warning · ${reason}` : 'Content warning'}</span>
          <span class="cw-tap">tap to reveal</span>
        </label>
        <label class="cw-hide" for="${raw(cwid)}">hide</label>
      </div>`;
}

/** Estimate whether a note's TEXT is tall enough to clamp (Satori measures pixel
 * height in JS at 400px; with no client JS we approximate from wrapped line count,
 * skipping bare media-URL lines so image/video notes stay fully visible). */
export function isTallText(content: string): boolean {
    let lines = 0;
    for (const raw of content.split('\n')) {
        const line = raw.trim();
        if (line === '') { lines += 1; continue; }
        if (/^https?:\/\/\S+$/.test(line)) continue; // a bare media/link line - rendered separately
        lines += Math.ceil(line.length / 62); // ~62 chars/line at the feed's width
    }
    return lines > 16; // ~16 lines ≈ the ~340px clamp height
}

// A single landscape image wider than this ratio renders short enough to skip the
// compact embed clamp (no dead "Show more"); anything taller/multiple/video collapses.
const EMBED_WIDE_RATIO = 1.9;

/** Whether a quoted/replied preview's media is likely to overflow the 240px embed
 * clamp (so it should collapse with Show more). Galleries and videos always do; a
 * single image does unless its imeta dim says it's clearly wide-and-short. With no
 * dim we assume tall, since a full-width single image usually is. */
function hasTallMedia(ev: NostrEvent): boolean {
    const media = tokenize(ev.content).filter((t) => t.t === 'image' || t.t === 'video');
    if (media.length === 0) return false;
    if (media.length > 1) return true; // gallery
    const only = media[0]!;
    if (only.t === 'video') return true;
    const dim = parseImeta(ev).get(only.url)?.dim;
    const m = dim ? /^(\d+(?:\.\d+)?)x(\d+(?:\.\d+)?)$/.exec(dim) : null; // tolerate decimals ("464.0x848.0")
    if (!m) return true; // unknown dims → assume tall
    return Number(m[1]) / Number(m[2]) <= EMBED_WIDE_RATIO;
}

/** Clamp tall content behind a "Show more"/"Show less" toggle (zero-JS checkbox). */
export function clampWrap(inner: SafeHtml, evId: string): SafeHtml {
    const id = `more-${evId}`;
    return html`
      <div class="clamp">
        <input type="checkbox" class="clamp-toggle" id="${raw(id)}">
        <div class="clamp-inner">${inner}</div>
        <label class="show-more" for="${raw(id)}"></label>
      </div>`;
}

/** FULLY collapse content behind a "Show more"/"Show less" toggle (zero-JS checkbox), the toggle on
 * TOP so it sits right under whatever precedes it. Unlike clampWrap (which previews ~340px then fades),
 * this hides the content entirely until asked - for secondary matter like a podcast's show-notes, where
 * a partial preview is just a wall of text under the payload (the player). */
export function collapse(inner: SafeHtml, evId: string): SafeHtml {
    const id = `det-${evId}`;
    return html`
      <div class="collapse">
        <input type="checkbox" class="collapse-toggle" id="${raw(id)}">
        <label class="show-more collapse-label" for="${raw(id)}"></label>
        <div class="collapse-inner">${inner}</div>
      </div>`;
}

// --- card-kind body helpers (shared by the manifest handlers + the declarative engine) ---------------

/** The card title line (bold, in a .content block), or null when untitled. One place to change the title
 * treatment for every kind (picture/podcast/calendar/classified/video + the engine). */
export const cardTitle = (title: string): SafeHtml | null =>
    title ? html`<div class="content"><strong>${title}</strong></div>` : null;

/** Clamp `rendered` behind Show-more when `clamp` is on and `measure` is tall text; else pass through
 * (null in → null out). `measure` is separate from `rendered` because some kinds measure the raw source
 * while rendering a transformed body. */
export const clampIfTall = (rendered: SafeHtml | null, measure: string, clamp: boolean, evId: string): SafeHtml | null =>
    rendered && clamp && isTallText(measure) ? clampWrap(rendered, evId) : rendered;

/** Wrap `visual` in the NIP-36 tap-to-reveal CW overlay when the event is content-warned, else pass it
 * through. The `!== null` test is deliberate: an empty-string reason still warns, so it must still wrap. */
export const cwIfFlagged = (ev: NostrEvent, visual: SafeHtml): SafeHtml => {
    const cw = contentWarning(ev);
    return cw !== null ? cwWrap(visual, cw, ev.id) : visual;
};

/** Rendered note content: CW reveal if flagged, else a Show-more clamp if the text
 * is long (unless `clamp` is false - the focused thread note shows in full). */
export function noteContent(ev: NostrEvent, profiles?: ProfileMap, clamp = true, media?: MediaPrefs, imeta?: ImetaMap): SafeHtml {
    const body = renderContent(ev.content, profiles, true, media, imeta, parseEmojiTags(ev), ev.pubkey);
    const cw = contentWarning(ev);
    if (cw !== null) return cwWrap(body, cw, ev.id);
    return clamp && isTallText(ev.content) ? clampWrap(body, ev.id) : body;
}

function authorName(pubkey: string, profiles?: ProfileMap): SafeHtml {
    const p = profiles?.get(pubkey);
    // The badge lives INSIDE .author (as in Satori's AuthorName) - so it sits right
    // after the name, not pushed to the timestamp by `.note-head .author { margin-right:auto }`.
    const badge = p?.nip05Verified ? html`<span class="badge" title="${formatNip05(p.nip05)} (verified)">✓</span>` : null;
    return html`<a class="author" href="/u/${npub(pubkey)}" h-get h-prefetch="hover" h-scroll="top instant">${withEmoji(displayName(pubkey, profiles), p?.emoji)}${badge}</a>`;
}

export function neventFor(ev: NostrEvent): string {
    // Stamp the kind so a reference carries WHAT it points at, not just where: the reply path reads it to
    // decide NIP-10 note vs NIP-22 comment without re-fetching, and other clients can pre-filter by kind.
    try { return neventEncode({ id: ev.id, author: ev.pubkey, kind: ev.kind }); } catch { return ev.id; }
}

/** The repeated author-avatar link: a boosted, prefetch-on-hover link to the author's profile
 * wrapping their avatar. `size` passes through to `avatar` (the `sm` byline variant). */
function authorAvatarLink(pubkey: string, profiles?: ProfileMap, size?: 'sm' | 'xs' | 'lg'): SafeHtml {
    return html`<a href="/u/${npub(pubkey)}" aria-label="author" h-get h-prefetch="hover" h-scroll="top instant">${avatar(pubkey, pic(pubkey, profiles), size)}</a>`;
}

/** The repeated "View thread" timestamp link: the `timeAgo` stamp + thread glyph linking to the
 * conversation. `href` is the target (a note's /t/ thread or an article's /a/); `label` the aria-label. */
function threadTime(href: string, ts: number, label = 'View thread'): SafeHtml {
    return html`<a class="time time-thread" href="${href}" aria-label="${label}" h-get h-prefetch="hover" h-scroll="top instant">${timeAgo(ts)}${icon('thread')}</a>`;
}

/** The "in reply to" card: a link to the parent's thread that lazily loads the
 * parent note's preview (helmjs h-trigger="intersect once" → /embed), matching Satori's
 * reply card. Zero-JS shows the label only (still links to the thread). */
export function replyContext(ev: NostrEvent): SafeHtml | null {
    // The parent: a NIP-10 reply target (kind:1), else a NIP-22 comment parent (kind:1111). Both render the
    // SAME lazy-embed card, so a comment shows its parent identically to a reply. The bech carries the relay
    // hint (and, for NIP-22, the parent author) so the link + /embed can actually resolve the parent.
    let key: string, bech: string, path = '/t/';
    const nip10 = replyParent(ev);
    if (nip10) {
        key = nip10.id;
        // Include the parent AUTHOR (best guess: the last p-tag, the usual direct-reply target) so /embed can
        // resolve the parent via its OUTBOX (write relays), matching the NIP-22 branch below. Without it, a
        // reply whose `e` tag carries no relay hint is only searched on the viewer's own relays and often
        // can't be found, so "in reply to an earlier note" never fills in. A wrong guess is harmless:
        // resolveEvent uses `author` only to pick relays and always queries by id.
        const parentAuthor = ev.tags.filter((t) => t[0] === 'p' && t[1]).at(-1)?.[1];
        // A malformed parent ref (a bech in the id slot, an `a`-coord, junk) can't encode to a resolvable
        // nevent - drop the card rather than emit a dead /t/ link + an undecodable /embed/ ("↗ link").
        const nev = neventFromRef(nip10.id, { author: parentAuthor, relays: nip10.relays.slice(0, 1) });
        if (!nev) return null;
        bech = nev;
    } else {
        const c = commentParent(ev);
        if (!c) return null;
        if (c.type === 'e') {
            key = c.value;
            const nev = neventFromRef(c.value, { author: c.pubkey, relays: c.relay ? [c.relay] : [] });
            if (!nev) return null;
            bech = nev;
        } else if (c.type === 'a') {
            const naddr = naddrFromCoord(c.value);
            if (!naddr) return null;
            key = c.value; bech = naddr; path = '/a/';
        } else return null; // external (`i`) parent: nothing to embed in-app
    }
    // `key` is a tag value from the parent ref (NIP-10 `e` id, or a NIP-22 `e`/`a` value whose `d`
    // identifier is arbitrary user content) - sanitize to a DOM-safe id before it reaches raw()/the
    // h-target attribute, so a crafted parent ref can't break out of the attribute.
    const id = `rc-${key.replace(/[^a-zA-Z0-9]/g, '').slice(0, 16)}`;
    // A sized card that lazily loads the parent on intersect (h-trigger="load" does
    // NOT fire on a <div>/<span>). The label links to the thread zero-JS; /embed
    // swaps in the full parent preview (rendered content, not a wrapping <a>).
    return html`<div id="${id}" class="quote reply-context" h-get="/embed/${bech}?as=reply" h-trigger="intersect once" h-target="#${id}" h-swap="inner" h-push-url="false"><a class="quote-label" href="${path}${bech}" aria-label="View thread">↩ in reply to an earlier note${icon('thread')}</a></div>`;
}

/** A note preview swapped into an embed card. The card is NOT a wrapping <a>
 * (nested anchors break it); the label links to the thread, the head to the
 * author, and the body uses renderContent (embeds=false → one level deep). */
export function embedPreview(ev: NostrEvent, bech: string, profiles?: ProfileMap, label = '↩ in reply to'): SafeHtml {
    const badge = profiles?.get(ev.pubkey)?.nip05Verified ? html`<span class="badge">✓</span>` : null;
    // The embed loads lazily, so the outer note's clamp never sees its height -
    // clamp the preview's own body when the quoted/replied text is long.
    const body = renderContent(ev.content, profiles, false, undefined, undefined, parseEmojiTags(ev));
    const clamped = (isTallText(ev.content) || hasTallMedia(ev)) ? clampWrap(body, ev.id) : body;
    return html`<a class="quote-label" href="/t/${bech}" aria-label="View thread" h-scroll="top instant">${label}${icon('thread')}</a
        ><div class="quote-head"><a class="quote-author-link" href="/u/${npub(ev.pubkey)}" h-scroll="top instant">${avatar(ev.pubkey, pic(ev.pubkey, profiles), 'xs')}<span class="quote-author">${withEmoji(displayName(ev.pubkey, profiles), profiles?.get(ev.pubkey)?.emoji)}${badge}</span></a></div
        ><div class="quote-body">${clamped}</div>`;
}

/** An article preview for an naddr embed card - title + author + summary. */
export function articleEmbedPreview(ev: NostrEvent, naddr: string, profiles?: ProfileMap): SafeHtml {
    const a = parseArticle(ev);
    return addressableEmbed(ev, naddr, '↗ article', a.title, a.summary, profiles, a.image);
}

/** The label-only fallback (parent not found) - keeps the thread link. */
/** The label-only fallback when an embed target can't be loaded (still a link). */
export const embedFallback = (href: string, label: string): SafeHtml => html`<a class="quote-label" href="${href}">${label}</a>`;

/** Live compose preview (Satori's renderPreview): a timeline-style note - your avatar
 * + name + "now" + the rendered draft - with no action row. `content` is the published
 * assembly (text + media urls); `imeta` carries NIP-92 dims/alt. Mentions/quotes/media
 * render through the same pipeline as a real note, so it's a true what-you-get view. */
/** Picture (NIP-68) media + caption as ONE card: the image flush at the top, a caption footer on a panel
 * below, sharing the card's rounded corners - so the caption reads as part of the image, not stray text.
 * `caption` is pre-rendered SafeHtml (or null); `location` an optional quiet dateline above it. Shared by the
 * kind:20 card (pictureBody) and the compose preview so the two stay WYSIWYG. */
export function pictureFigure(media: SafeHtml, caption: SafeHtml | null, location = ''): SafeHtml {
    const footer = caption || location
        ? html`<figcaption class="picture-caption">${location ? html`<span class="picture-loc">${location}</span>` : null}${caption}</figcaption>`
        : null;
    return html`<figure class="picture-figure">${media}${footer}</figure>`;
}

export function composePreview(me: string, content: string, imeta: string[][], profiles?: ProfileMap, opts: { picture?: boolean; title?: string } = {}): SafeHtml {
    // The body differs by mode; the preview shell (avatar + head + "now") is shared. Picture (NIP-68) mirrors
    // the real kind-20 card layout - title, image(s), then caption below - so the preview is WYSIWYG (the note
    // pipeline would wrongly show caption-above-image). Force autoLoad so you always see your own attached media.
    let body: SafeHtml;
    if (opts.picture) {
        const items = imageItems([...parseImeta({ tags: imeta } as NostrEvent)].map(([url, meta]) => ({ url, meta })));
        body = html`${cardTitle(opts.title ?? '')}${pictureFigure(mediaTiles(items, true), content ? renderContent(content, profiles, false) : null)}`;
    } else {
        const imetaMap = parseImeta({ tags: imeta, content } as NostrEvent);
        body = renderContent(content, profiles, true, { autoLoad: true }, imetaMap);
    }
    return html`
      <div class="preview-label">Preview</div>
      <div class="note preview-note">
        ${avatar(me, pic(me, profiles))}
        <div class="note-body">
          <div class="note-head">${authorName(me, profiles)}<span class="time">now</span></div>
          ${body}
        </div>
      </div>`;
}

/** The note action row. Reply/quote navigate to the thread (zero-JS); bookmark
 * (+ pin on your own notes) are live stateful actions; like/zap are presentational
 * until Phase 4. Order matches Satori: reply · quote · [like · zap] · bookmark · [pin]. */
/** The zap action: a link opening the zap modal when the author has a lightning
 * address (NIP-57), else a dimmed "unavailable" glyph (Satori's behavior). */
function zapAct(ev: NostrEvent, s?: Session): SafeHtml {
    const canZap = !!s && !!s.profiles.get(ev.pubkey)?.lud16;
    if (canZap) return zapButton(ev.id, ev.pubkey, isZapped(s!, ev.id));
    return html`<span class="note-act zap unavailable" title="No lightning address" aria-disabled="true">${icon('zap')}</span>`;
}

/** The zap link, keyed by note id so a successful one-tap zap can OOB-swap it to
 * the active (zapped) state. `active` fills the bolt + adds .active (warn tint);
 * `oob` marks it for an out-of-band swap from the /zap/paid continuation. */
export function zapButton(noteId: string, recipient: string, active: boolean, oob = false): SafeHtml {
    return html`<a id="zap-${noteId}" class="note-act zap${active ? ' active' : ''}"${oob ? raw(' h-oob="true"') : raw('')} href="/zap?e=${noteId}&p=${recipient}" h-target="#modal" h-swap="inner" title="${active ? 'Zapped ⚡' : 'Zap'}" aria-label="Zap">${icon('zap', active)}</a>`;
}

/** The reply glyph's presence dot + its tooltip, from the same data. `cls`: '' (none), ' convo'
 * (has replies), ' convo convo-net' (a follow is in it - accent). `label` reflects BOTH your own
 * participation and others' (they compose). `show` is false where the convo is already visible
 * (focused note / article reader) - then no dot and just the plain action label. */
function replyPresence(key: string, replied: boolean, show: boolean, verb: 'reply' | 'comment' = 'reply'): { cls: string; label: string } {
    const f = show ? replyFaces(key) : null;
    const has = !!f && f.repliers.length > 0;
    const net = has && f!.hasFollow;
    const you = verb === 'comment' ? 'You commented' : 'You replied'; // articles use NIP-22 comments
    const label = replied && net ? `${you}, and people you know are too`
        : replied && has ? `${you}, and others are too`
        : replied ? you
        : net ? 'People you know are discussing'
        : has ? 'People are discussing'
        : verb === 'comment' ? 'Comment' : 'Reply';
    return { cls: !has ? '' : net ? ' convo convo-net' : ' convo', label };
}

/** The right-aligned "who's talking" avatar stack: up to 3 repliers (follows-first) + a numberless
 * "+" when there are more. PEOPLE, not a count. Links into the conversation. Empty when no replies
 * (or not hydrated). Faces use cached profiles (follows-first → usually cached; a colored hue circle
 * otherwise). */
// The faces element's stable id (so the lazy /faces hydrate can OOB-swap it) + its conversation link.
const facesId = (key: string): string => `faces-${key}`;
const facesHref = (key: string): string => {
    if (key.startsWith('naddr')) return `/a/${key}`;
    try { return `/t/${neventEncode({ id: key })}`; } catch { return `/t/${key}`; }
};

/** The resolved avatar stack (warm). `oob` makes it an out-of-band swap onto its own id - the lazy
 * post-paint hydration path (the /faces endpoint returns these). */
function facesLink(key: string, f: ReplyFaces, s: Session, oob = false): SafeHtml {
    const label = `${f.repliers.length}${f.more ? '+' : ''} replied`;
    return html`<a id="${facesId(key)}" class="reply-faces"${oob ? raw(' h-oob="true"') : raw('')} href="${facesHref(key)}" h-get h-prefetch="hover" h-scroll="top instant" title="${label}" aria-label="${label}">${f.repliers.map((pk) => avatar(pk, pic(pk, s.profiles), 'xs'))}${f.more ? html`<span class="reply-faces-more">+</span>` : null}</a>`;
}

/** The faces slot for a card: the resolved stack when warm, else an empty placeholder carrying the stable
 * id, so the lazy hydrate (facesHydrate → /faces) can fill it after paint. Faces are best-effort, so they're
 * usually cold on first paint anywhere but a re-visited Following feed - the hydrate makes them reliable. */
function replyFacesEl(key: string, s?: Session): SafeHtml {
    if (!s) return html``;
    const f = replyFaces(key);
    return f && f.repliers.length > 0 ? facesLink(key, f, s) : html`<span class="reply-faces-slot" id="${facesId(key)}"></span>`;
}

/** The OOB faces element for the /faces endpoint: the resolved stack for a now-warm key, or null when it has
 * no replies (leave the placeholder untouched). */
export function facesOOB(key: string, s: Session): SafeHtml | null {
    const f = replyFaces(key);
    return f && f.repliers.length > 0 ? facesLink(key, f, s, true) : null;
}

/** A one-shot lazy trigger (hidden <li>) that, after paint, fetches + OOB-swaps faces for the page's COLD
 * keys (notes by id, articles by naddr). Emitted by noteList when opts.faces is set; skipped when every key
 * is already warm (a re-visited Following feed) - no needless round-trip. */
export function facesHydrate(events: NostrEvent[], s?: Session): SafeHtml {
    if (!s) return html``;
    const keys = events.map((e) => (isAddressable(e.kind) ? naddrFor(e) : e.kind === 1 ? e.id : '')).filter(Boolean);
    const cold = [...new Set(keys)].filter((k) => replyFaces(k) === null);
    if (cold.length === 0) return html``;
    return html`<li class="faces-hydrate" h-get="/faces?keys=${cold.join(',')}" h-trigger="load" h-swap="none" h-push-url="false"></li>`;
}

/** A note's action vocabulary, in render order. Declared DATA (also exposed on the kind handler's
 * `actions`), so the row is assembled from this list - the seam where a manifest's declared actions
 * slot in. Each id maps to its button below; the list decides which appear and in what order. */
export const NOTE_ACTIONS = ['reply', 'quote', 'like', 'zap', 'bookmark', 'mute', 'pin'] as const;

export function noteActions(ev: NostrEvent, nevent: string, s?: Session, inThread?: string, faces = true, mute = false): SafeHtml {
    const mine = !!s && ev.pubkey === s.me;
    // In a thread, the reply carries the thread context so it appends back here
    // (optimistic reply) instead of landing on the feed.
    const replyHref = `/compose?reply=${nevent}${inThread ? `&inthread=${inThread}` : ''}`;
    const replied = !!s && hasReplied(s, engageTarget(ev));   // fill the glyph if you've replied
    const reposted = !!s && hasReposted(s, engageTarget(ev));  // …or reposted (kind:6 / quote)
    const { cls: convo, label } = replyPresence(ev.id, replied, faces); // dot + tooltip ("You replied" / "People are discussing" / …)
    // Each declared action → its button, or null when it doesn't apply here. Same markup + conditions
    // as before, just keyed; NOTE_ACTIONS drives the set + order. (.note-acts is flex, so the inline
    // whitespace the old template carried between buttons is irrelevant - identical layout.)
    const acts: Record<string, SafeHtml | null> = {
        reply: html`<a class="note-act reply ${replied ? 'engaged' : ''}${convo}" href="${replyHref}" h-target="#modal" h-swap="inner" h-focus="#compose-text" title="${label}" aria-label="${label}">${icon('reply', replied)}</a>`,
        quote: html`<a class="note-act quote-act ${reposted ? 'engaged' : ''}" href="/compose?quote=${nevent}" h-target="#modal" h-swap="inner" h-focus="#compose-text" title="Quote" aria-label="Quote">${icon('quote', reposted)}</a>`,
        like: !mine && s?.reactions ? likeButton(s, ev.id, ev.pubkey, ev.kind) : null,
        zap: !mine ? zapAct(ev, s) : null,
        bookmark: s ? bookmarkButton(s, ev.id) : null,
        mute: mute && s && !mine ? muteAct(s, ev.pubkey, ev.id) : null,
        pin: mine && s ? pinButton(s, ev.id) : null,
    };
    // The action set + order come from the kind's manifest declaration (actionsFor), so a kind
    // affords exactly what its handler declares; NOTE_ACTIONS is the fallback for an unregistered kind.
    const ids = actionsFor(ev.kind) ?? NOTE_ACTIONS;
    // Delete rides OUTSIDE the vocabulary: it's an ownership affordance (NIP-09), not a
    // per-kind one, so every own event affords it without each manifest declaring it.
    return html`
      <div class="note-actions">
        <div class="note-acts">${ids.map((id) => acts[id]).filter((x): x is SafeHtml => x !== null)}${mine && s ? deleteAct(ev) : null}</div>
        ${faces ? replyFacesEl(ev.id, s) : null}
      </div>`;
}

/** The article zap action: opens the zap modal keyed by the article's naddr (the
 * zap request carries an `a`-tag), or a dimmed glyph when the author has no lud16. */
function articleZapAct(ev: NostrEvent, naddr: string, s?: Session): SafeHtml {
    const canZap = !!s && !!s.profiles.get(ev.pubkey)?.lud16;
    if (canZap) return articleZapButton(naddr, ev.pubkey, isZapped(s!, naddr));
    return html`<span class="note-act zap unavailable" title="No lightning address" aria-disabled="true">${icon('zap')}</span>`;
}

/** The article zap link, keyed by the naddr so a one-tap zap can OOB-swap it to the
 * zapped state. Mirrors zapButton but routes through /zap?a= (addressable target). */
export function articleZapButton(naddr: string, recipient: string, active: boolean, oob = false): SafeHtml {
    return html`<a id="zap-${naddr}" class="note-act zap${active ? ' active' : ''}"${oob ? raw(' h-oob="true"') : raw('')} href="/zap?a=${naddr}&p=${recipient}" h-target="#modal" h-swap="inner" title="${active ? 'Zapped ⚡' : 'Zap'}" aria-label="Zap">${icon('zap', active)}</a>`;
}

/** The article action row (Satori's articleActions): reply · quote · [like · zap]
 * · bookmark · [pin], keyed by the article's naddr (like/zap/bookmark/pin → `a`-tag).
 * `onPage` (the full reader) jumps Reply to the comment box; a feed row opens the article. */
/** An article's action vocabulary, in render order (no mute - articles aren't muted from the row). */
export const ARTICLE_ACTIONS = ['reply', 'quote', 'like', 'zap', 'bookmark', 'pin'] as const;

export function articleActions(ev: NostrEvent, naddr: string, s?: Session, onPage = false): SafeHtml {
    const mine = !!s && ev.pubkey === s.me;
    const replied = !!s && hasReplied(s, engageTarget(ev));
    const reposted = !!s && hasReposted(s, engageTarget(ev));
    // Reply = a NIP-22 comment. On the article page the comment box is right here, so jump to it;
    // in a feed row there's no box, so open the article (where the box lives).
    const { cls: convo, label } = replyPresence(naddr, replied, !onPage, 'comment'); // articles → "comment" wording; no dot on the reader
    const acts: Record<string, SafeHtml | null> = {
        reply: onPage
            ? html`<a class="note-act reply ${replied ? 'engaged' : ''}" href="#comment-form" h-boost="false" title="${label}" aria-label="${label}">${icon('reply', replied)}</a>`
            : html`<a class="note-act reply ${replied ? 'engaged' : ''}${convo}" href="/a/${naddr}" h-scroll="top instant" title="${label}" aria-label="${label}">${icon('reply', replied)}</a>`,
        quote: html`<a class="note-act quote-act ${reposted ? 'engaged' : ''}" href="/compose?quote=${naddr}" h-target="#modal" h-swap="inner" h-focus="#compose-text" title="Quote" aria-label="Quote">${icon('quote', reposted)}</a>`,
        like: !mine && s?.reactions ? likeButton(s, naddr, ev.pubkey, ev.kind, ev.id) : null,
        zap: !mine ? articleZapAct(ev, naddr, s) : null,
        bookmark: s && naddr ? bookmarkButton(s, naddr) : null,
        pin: mine && s && naddr ? pinButton(s, naddr) : null,
    };
    const ids = actionsFor(ev.kind) ?? ARTICLE_ACTIONS; // article's declared vocabulary (manifest-driven)
    // Delete outside the vocabulary - ownership affordance, same as noteActions.
    return html`
      <div class="note-actions article-actions">
        <div class="note-acts">${ids.map((id) => acts[id]).filter((x): x is SafeHtml => x !== null)}${mine && s ? deleteAct(ev) : null}</div>
        ${!onPage ? replyFacesEl(naddr, s) : null}
      </div>`;
}

export interface NoteOpts {
    focused?: boolean;
    hideParent?: boolean;
    depth?: number;
    inThread?: string;                          // the thread's nevent → reply appends here
    pending?: { token: string; seconds: number }; // an optimistic (not-yet-published) reply
    mute?: boolean;                             // stranger-facing rows (notifications, relay timelines): add a mute glyph that dismisses the card
    isPrivate?: boolean;                         // a gift-wrapped private reply (NIP-59): badge it with a lock
    faces?: boolean;                            // LIST-level: append the lazy reply-faces hydrate trigger for this page (feeds/profile, not search)
}

/** The footer of a pending optimistic note: countdown + Undo (the whole note removes
 * itself on Undo). Replaces the action row until the note is confirmed/published. */
function pendingFooter(token: string, seconds: number): SafeHtml {
    const t = encodeURIComponent(token);
    return html`<div class="post-pending"><span>Posting in ${String(seconds)}s…</span> <button type="button" class="toast-action" h-post="/note/undo?token=${t}" h-target="#opt-${raw(token)}" h-swap="outer">Undo</button></div>`;
}

/** Timeline entry point for ANY event: dispatch to the kind's handler via the manifest registry
 * (article → articleRow, every other kind → noteRow). Byte-identical to the old `if kind===ARTICLE`
 * branch, now manifest-driven so adding a kind is a registration, not an edit here. */
export function noteCard(ev: NostrEvent, profiles?: ProfileMap, s?: Session, opts: NoteOpts = {}): SafeHtml {
    return renderEvent(ev, 'timeline', { profiles, s, opts });
}

/** The shared SIMPLE-kind card: the `.note` shell (avatar + head + the kind's action row) around a
 * kind-specific BODY. A new simple kind supplies only its body + field extraction instead of re-rolling
 * the shell; bespoke kinds (noteRow/articleRow) stay hand-written. `compact` (embed) drops the actions.
 * The action row is kind-aware (noteActions reads the kind's declared `actions`), so each kind shows its
 * own affordances. */
export function cardShell(ev: NostrEvent, profiles: ProfileMap | undefined, s: Session | undefined, body: SafeHtml, opts: { compact?: boolean; lightboxes?: SafeHtml; depth?: number } = {}): SafeHtml {
    const nevent = neventFor(ev);
    // `depth` indents a card nested in a thread (the reply line), matching noteRow's nesting classes - so a
    // comment/highlight card threads visually like a note reply, not flat.
    const depthCls = opts.depth ? ` reply-nested depth-${Math.min(opts.depth, 4)}` : '';
    // `lightboxes` (the hoisted media overlays, built via mediaOverlays) ride OUTSIDE the `.note` <li> -
    // its content-visibility would otherwise trap the position:fixed overlay (same hoist noteCard does).
    return html`
      <li class="note${depthCls}">
        ${authorAvatarLink(ev.pubkey, profiles)}
        <div class="note-body">
          <div class="note-head">
            ${authorName(ev.pubkey, profiles)}
            ${threadTime(`/t/${nevent}`, ev.created_at)}
          </div>
          ${body}
          ${opts.compact ? null : noteActions(ev, nevent, s)}
        </div>
      </li>${opts.lightboxes ?? null}`;
}

/** One note as a feed/thread <li>, in Satori's flex `.note` layout. `depth` indents nested replies
 * (with the thread line), matching the thread view. A `pending` note self-polls /note/tick (countdown
 * → confirm in place / undo). The note/poll timeline render - the registry's fallback handler. */
export function noteRow(ev: NostrEvent, profiles?: ProfileMap, s?: Session, opts: NoteOpts = {}, extra?: SafeHtml): SafeHtml {
    const nevent = neventFor(ev);
    const im = parseImeta(ev); // NIP-92 alt + dim for the note's media
    const p = opts.pending;
    const parent = (opts.hideParent || p) ? null : replyContext(ev);
    const depth = opts.depth ?? 0;
    const cls = `note${p ? ' pending' : ''}${depth > 0 ? ` reply-nested depth-${Math.min(depth, 4)}` : ''}`;
    const liExtra = p
        ? raw(` id="opt-${p.token}" h-get="/note/tick?token=${encodeURIComponent(p.token)}" h-trigger="every 1s" h-target="#opt-${p.token}" h-swap="outer" h-push-url="false"`)
        : opts.mute ? raw(` id="card-${ev.id}"`) : raw(''); // mute glyph dismisses this card by id (ev.id is verified 64-hex via SimplePool verifyEvent → raw()-safe)
    return html`
      <li class="${cls}"${liExtra}>
        ${authorAvatarLink(ev.pubkey, profiles)}
        <div class="note-body">
          <div class="note-head">
            ${authorName(ev.pubkey, profiles)}
            ${opts.isPrivate ? html`<span class="private-mark" title="Private reply - only you and the author can see it">${icon('lock')}<span class="sr-only">Private reply</span></span>` : null}
            ${p || opts.isPrivate ? html`<span class="time">${p ? html`now` : timeAgo(ev.created_at)}</span>` : threadTime(`/t/${nevent}`, ev.created_at)}
          </div>
          ${parent}
          ${noteContent(ev, profiles, true, s?.media, im)}
          ${!p && extra ? extra : null}
          ${p ? pendingFooter(p.token, p.seconds) : noteActions(ev, nevent, s, opts.inThread, true, opts.mute)}
        </div>
      </li>${p ? html`` : mediaLightboxes(ev.content, s?.media?.autoLoad ?? true, im)}`;
}

/** An article preview card - the string port of Satori's fillArticleCard: a cover
 * (image or quiet ensō placeholder) atop a body of kicker · title · summary · meta.
 * `hideAuthor` drops the author from the meta (shown elsewhere), leaving the
 * reading time. The clickable card is an <a> here (Satori used a JS-onClick div). */
function articleCard(ev: NostrEvent, profiles: ProfileMap | undefined, hideAuthor: boolean, a = parseArticle(ev)): SafeHtml {
    const naddr = naddrFor(ev);
    const href = naddr ? `/a/${naddr}` : '#';
    const mins = `${readingMinutes(a.content)} min read`;
    const cover = a.image && safeUrl(a.image) !== '#'
        ? html`<img class="article-card-cover" src="${imgSrc(a.image)}" loading="lazy" alt="">`
        : html`<div class="article-card-cover cover-missing">${enso(30, true)}</div>`;
    return html`
      <a class="article-card" href="${href}">
        ${cover}
        <div class="article-card-body">
          <div class="article-card-kicker">↗ Article</div>
          <div class="article-card-title">${a.title}</div>
          ${a.summary ? html`<div class="article-card-summary">${a.summary}</div>` : null}
          <div class="article-card-meta">
            ${hideAuthor ? null : authorName(ev.pubkey, profiles)}
            <span>${hideAuthor ? mins : ` · ${mins}`}</span>
          </div>
        </div>
      </a>`;
}

/** An addressable event's CANONICAL naddr (relays:[]) - the shared key for its like/zap state across the
 * render (action buttons), the engagement-cache like sync, and the zap-receipt hydration. Encodes with the
 * event's own kind, so it serves articles, custom NIPs, and any other addressable kind identically. */
export function naddrFor(ev: NostrEvent): string {
    try { return naddrEncode({ kind: ev.kind, pubkey: ev.pubkey, identifier: tag1(ev, 'd'), relays: [] }); } catch { return ''; }
}

// Shared shells for the article-like addressable kinds (article / custom NIP / wiki), which render the SAME
// row/byline/embed and differ only in their card body + label. The card/body is passed in so each kind keeps
// its own shape; these hold the identical scaffolding (rule-of-three, met once wiki landed).

/** Feed-row skeleton: author head + a `time-thread` link into /a/ + the kind's card + the action row. */
function addressableRow(ev: NostrEvent, card: SafeHtml, publishedAt: number, ariaLabel: string, profiles?: ProfileMap, s?: Session): SafeHtml {
    const naddr = naddrFor(ev);
    return html`
      <li class="note article-row">
        ${authorAvatarLink(ev.pubkey, profiles)}
        <div class="note-body">
          <div class="note-head">
            ${authorName(ev.pubkey, profiles)}
            <a class="time time-thread" href="/a/${naddr}" aria-label="${ariaLabel}" h-scroll="top instant">${timeAgo(publishedAt)}${icon('thread')}</a>
          </div>
          ${card}
          ${articleActions(ev, naddr, s)}
        </div>
      </li>`;
}

/** Reader byline: author avatar + name + "· time · N min read". */
function addressableByline(ev: NostrEvent, publishedAt: number, content: string, profiles?: ProfileMap): SafeHtml {
    return html`<div class="article-byline">
          ${authorAvatarLink(ev.pubkey, profiles, 'sm')}
          ${authorName(ev.pubkey, profiles)}
          <span class="article-byline-meta">· ${timeAgo(publishedAt)} · ${readingMinutes(content)} min read</span>
        </div>`;
}

/** Inline embed card: label · optional cover · title · author · summary. */
function addressableEmbed(ev: NostrEvent, naddr: string, label: string, title: string, summary: string, profiles?: ProfileMap, coverUrl?: string): SafeHtml {
    return html`<a class="article-embed" href="/a/${naddr}" h-scroll="top instant">
        <span class="quote-label">${label}</span>
        ${coverUrl && safeUrl(coverUrl) !== '#' ? html`<img class="article-embed-cover" src="${imgSrc(coverUrl)}" alt="" loading="lazy">` : null}
        <span class="article-embed-title">${title}</span>
        <span class="article-embed-by">${displayName(ev.pubkey, profiles)}</span>
        ${summary ? html`<span class="article-embed-summary">${summary}</span>` : null}
      </a>`;
}

/** An article as a feed row (Satori's ArticleRow): author head + card + actions. */
export function articleRow(ev: NostrEvent, profiles?: ProfileMap, s?: Session): SafeHtml {
    const a = parseArticle(ev); // parsed once, shared with the card
    return addressableRow(ev, articleCard(ev, profiles, true, a), a.publishedAt, 'Open article', profiles, s);
}

/** The focused note in a thread - an <li class="note focused"> in the feed list
 * (matching Satori), full content + the action row. */
export function focusedNote(ev: NostrEvent, profiles?: ProfileMap, s?: Session, inThread?: string, extra?: SafeHtml): SafeHtml {
    const parent = replyContext(ev);
    const im = parseImeta(ev);
    return html`
      <li class="note focused">
        ${authorAvatarLink(ev.pubkey, profiles)}
        <div class="note-body">
          <div class="note-head">
            ${authorName(ev.pubkey, profiles)}
            <span class="time">${timeAgo(ev.created_at)}</span>
          </div>
          ${parent}
          ${noteContent(ev, profiles, false, s?.media, im)}
          ${extra ?? null}
          ${noteActions(ev, neventFor(ev), s, inThread, false)}
        </div>
      </li>${mediaLightboxes(ev.content, s?.media?.autoLoad ?? true, im)}`;
}

/** The "Pinned" strip atop a profile (NIP-51 kind:10001 - featured notes then
 * articles, in pin order). noteCard renders an article event as its article row. */
export function pinnedStrip(notes: NostrEvent[], articles: NostrEvent[], profiles?: ProfileMap, s?: Session): SafeHtml {
    if (notes.length === 0 && articles.length === 0) return html``;
    const items = [...notes, ...articles].map((ev) => noteCard(ev, profiles, s));
    return html`
      <div class="pinned-wrap">
        <div class="strip-label">Pinned</div>
        <ul class="feed">${join(items)}</ul>
      </div>`;
}

/** The horizontal "Articles" strip - this author's long-form (kind:30023). A
 * native scroll-snap row of compact cards (Satori's JS prev/next arrows dropped). */
export function articlesStrip(articles: NostrEvent[], profiles?: ProfileMap): SafeHtml {
    if (articles.length === 0) return html``;
    const cards = articles.map((ev) => articleCard(ev, profiles, true));
    return html`
      <div class="article-strip-wrap">
        <div class="strip-label">Articles</div>
        <div class="article-strip-scroll"><div class="article-strip">${join(cards)}</div></div>
      </div>`;
}

/** A profile header in Satori's exact structure: avatar + actions, name (with the
 * verified badge inside .author), nip05 + copy-npub meta, about. The action
 * buttons are presentational in Phase 0 - wired (follow/mute/edit) in Phase 1. */
export function profileHeader(pubkey: string, profile: Profile | undefined, profiles: ProfileMap | undefined, s: Session, isMe = false): SafeHtml {
    const map = profiles ?? (profile ? new Map([[pubkey, profile]]) : undefined);
    // Edit your own profile: a real link to /profile/edit (zero-JS full-page editor);
    // helmjs opens it as a modal (#modal). The kind:0 save refreshes this header.
    const actions = isMe
        ? html`<a class="edit-profile-btn" href="/profile/edit" h-get="/profile/edit" h-target="#modal" h-swap="inner">Edit profile</a>`
        : html`${followButton(s, pubkey)}${muteButton(s, pubkey)}`;
    // NIP-24: wide background banner (proxied via /media like all images) and a website
    // link (scheme-checked; bare domains get https). (No "bot" badge: a bot won't self-declare,
    // so the badge was theatre - removed.)
    const banner = profile?.banner && imgSrc(profile.banner) !== '#'
        ? html`<img class="profile-banner" src="${imgSrc(profile.banner)}" alt="" loading="lazy">` : null;
    const website = websiteLink(profile?.website);
    return html`
      <div class="profile-header${banner ? ' has-banner' : ''}" id="profile-header">
        ${banner}
        <div class="profile-top">
          ${avatar(pubkey, profile?.picture, 'lg')}
          <div class="profile-actions">${actions}</div>
        </div>
        <div class="profile-name">${authorName(pubkey, map)}</div>
        <div class="profile-meta">
          <div class="profile-nip05">${profile?.nip05Verified ? formatNip05(profile.nip05) : ''}</div>
          <span class="copy-npub">${shortNpub(pubkey)} ⧉</span>
        </div>
        ${website}
        ${profile?.about ? html`<div class="profile-about">${withEmoji(profile.about, profile.emoji)}</div>` : null}
      </div>`;
}

/** A profile's NIP-24 website as a safe external link, or null. Bare domains get
 * https://; the visible text drops the scheme + trailing slash for a clean look. */
function websiteLink(raw0?: string): SafeHtml | null {
    if (!raw0) return null;
    // Clean tracking params off the profile website too (same privacy stance as content links).
    const url = cleanTrackingParams(/^https?:\/\//i.test(raw0) ? raw0 : `https://${raw0}`);
    const href = safeUrl(url);
    if (href === '#') return null;
    return html`<a class="profile-website" href="${href}" target="_blank" rel="noopener noreferrer">${icon('globe')}${stripScheme(url)}</a>`;
}

/** The article reader (NIP-23): cover, title, byline, rendered markdown body,
 * then the action bar - mirroring Satori's showArticle. */
export function articleReader(ev: NostrEvent, profiles?: ProfileMap, s?: Session): SafeHtml {
    const a = parseArticle(ev);
    // Only treat it as a cover (and dedupe a matching leading body image) when it'll
    // actually render - an unsafe URL falls through to safeUrl '#'.
    const coverUrl = a.image && safeUrl(a.image) !== '#' ? a.image : undefined;
    const cover = coverUrl ? html`<img class="article-cover" src="${imgSrc(coverUrl)}" alt="">` : null;
    return html`
      <article class="article">
        ${cover}
        <h1 class="article-title">${a.title}</h1>
        ${addressableByline(ev, a.publishedAt, a.content, profiles)}
        ${a.topics.length ? html`<div class="article-topics">${join(a.topics.slice(0, 12).map((t) => html`<a class="nip-kind-chip" href="/search?q=${encodeURIComponent('#' + t)}" h-scroll="top instant">#${t}</a>`))}</div>` : null}
        ${renderMarkdown(a.content, profiles, coverUrl)}
        ${articleActions(ev, naddrFor(ev), s, true)}
      </article>`;
}

// --- Custom NIP (kind:30817) -----------------------------------------------
// A NUD (community-authored NIP): markdown body like an article, plus a `title` and zero+ `k` tags naming
// the kinds it defines. Rendered like an article (reader + rows + embed) with those defined kinds surfaced
// as chips, per the NUD's "display custom NIPs distinctly" guidance. Reuses the article DOM + CSS.

/** The "kind N · Name" chips from a custom NIP's `k` tags, or null when it defines none. */
function definedKindChips(kinds: { num: string; name: string }[]): SafeHtml | null {
    if (!kinds.length) return null;
    return html`<div class="nip-kinds">${join(kinds.map((k) => html`<span class="nip-kind-chip">kind ${k.num}${k.name ? html` · ${k.name}` : null}</span>`))}</div>`;
}

/** A custom NIP preview card (the article-card layout, coverless): kicker · title · summary · defined
 * kinds · reading time. The whole card links to its reader at /a/<naddr>. */
function customNipCard(ev: NostrEvent, c = parseCustomNip(ev)): SafeHtml {
    const naddr = naddrFor(ev);
    const href = naddr ? `/a/${naddr}` : '#';
    return html`
      <a class="article-card nip-card" href="${href}">
        <div class="article-card-cover cover-missing">${enso(30, true)}</div>
        <div class="article-card-body">
          <div class="article-card-kicker">↗ Custom NIP</div>
          <div class="article-card-title">${c.title}</div>
          ${c.summary ? html`<div class="article-card-summary">${c.summary}</div>` : null}
          ${definedKindChips(c.kinds)}
          <div class="article-card-meta"><span>${readingMinutes(c.content)} min read</span></div>
        </div>
      </a>`;
}

/** A custom NIP as a feed row: author head + card + the addressable action row (like articleRow). */
export function customNipRow(ev: NostrEvent, profiles?: ProfileMap, s?: Session): SafeHtml {
    const c = parseCustomNip(ev); // parsed once, shared with the card
    return addressableRow(ev, customNipCard(ev, c), c.publishedAt, 'Open custom NIP', profiles, s);
}

/** The custom NIP reader (like articleReader, coverless): title, byline, defined-kind chips, the rendered
 * markdown body, then the action bar. */
export function customNipReader(ev: NostrEvent, profiles?: ProfileMap, s?: Session): SafeHtml {
    const c = parseCustomNip(ev);
    return html`
      <article class="article nip-article">
        <h1 class="article-title">${c.title}</h1>
        ${addressableByline(ev, c.publishedAt, c.content, profiles)}
        ${definedKindChips(c.kinds)}
        ${renderMarkdown(c.content, profiles)}
        ${articleActions(ev, naddrFor(ev), s, true)}
      </article>`;
}

/** A custom NIP as an inline embed (the article-embed card): kicker · title · author · summary. */
export function customNipEmbedPreview(ev: NostrEvent, naddr: string, profiles?: ProfileMap): SafeHtml {
    const c = parseCustomNip(ev);
    return addressableEmbed(ev, naddr, '↗ custom NIP', c.title, c.summary, profiles);
}

// --- Wiki article (kind:30818, NIP-54) -------------------------------------
// A collaborative wiki article: an AsciiDoc body, `d` = the normalized topic slug, optional `title`.
// Rendered like an article (reader + rows + embed) but through renderAsciiDoc. Reuses the article DOM + CSS.

/** A wiki preview card (the article-card layout, coverless): kicker · title · summary · reading time. */
function wikiCard(ev: NostrEvent, w = parseWiki(ev)): SafeHtml {
    const naddr = naddrFor(ev);
    const href = naddr ? `/a/${naddr}` : '#';
    return html`
      <a class="article-card wiki-card" href="${href}">
        <div class="article-card-cover cover-missing">${enso(30, true)}</div>
        <div class="article-card-body">
          <div class="article-card-kicker">↗ Wiki</div>
          <div class="article-card-title">${w.title}</div>
          ${w.summary ? html`<div class="article-card-summary">${w.summary}</div>` : null}
          <div class="article-card-meta"><span>${readingMinutes(w.content)} min read</span></div>
        </div>
      </a>`;
}

/** A wiki article as a feed row: author head + card + the addressable action row (like articleRow). */
export function wikiRow(ev: NostrEvent, profiles?: ProfileMap, s?: Session): SafeHtml {
    const w = parseWiki(ev); // parsed once, shared with the card
    return addressableRow(ev, wikiCard(ev, w), w.publishedAt, 'Open wiki article', profiles, s);
}

/** The wiki reader (like articleReader, coverless): title, byline, the rendered AsciiDoc body, action bar. */
export function wikiReader(ev: NostrEvent, profiles?: ProfileMap, s?: Session): SafeHtml {
    const w = parseWiki(ev);
    return html`
      <article class="article wiki-article">
        <h1 class="article-title">${w.title}</h1>
        ${addressableByline(ev, w.publishedAt, w.content, profiles)}
        ${renderAsciiDoc(w.content, profiles, ev.pubkey)}
        ${articleActions(ev, naddrFor(ev), s, true)}
      </article>`;
}

/** A wiki article as an inline embed (the article-embed card): kicker · title · author · summary. */
export function wikiEmbedPreview(ev: NostrEvent, naddr: string, profiles?: ProfileMap): SafeHtml {
    const w = parseWiki(ev);
    return addressableEmbed(ev, naddr, '↗ wiki article', w.title, w.summary, profiles);
}

// --- Git repository (kind:30617, NIP-34) -----------------------------------
// A repository announcement: name + description + clone/web urls + maintainers, addressable by naddr. Its
// OWN card shape (not the article title/summary/reading-time one): a git glyph + the repo name + clone
// affordances. Read-only (browse / reference); patches and issues are a later phase.

/** A safe external repo web link, scheme dropped from the visible text (like the profile-website link). */
function repoWebLink(u: string): SafeHtml | null {
    const href = safeUrl(u);
    if (href === '#') return null;
    return html`<a href="${href}" target="_blank" rel="noopener noreferrer">${stripScheme(u)}</a>`;
}

/** A generic labeled metadata row: an uppercase label + inline items, or null when empty. Shared by the repo
 * web/maintainer/relay rows and reusable by future addressable kinds (issue/patch status, labels, ...). */
function labeledRow(label: string, items: SafeHtml[]): SafeHtml | null {
    return items.length ? html`<div class="meta-row"><span class="meta-label">${label}</span>${join(items, ' ')}</div>` : null;
}

/** A repo preview card (coverless, article-card layout): git glyph · name · description · repo id. */
function repoCard(ev: NostrEvent): SafeHtml {
    const r = parseRepo(ev);
    const naddr = naddrFor(ev);
    return html`
      <a class="article-card repo-card" href="${naddr ? `/a/${naddr}` : '#'}">
        <div class="article-card-cover cover-missing">${icon('git')}</div>
        <div class="article-card-body">
          <div class="article-card-kicker">↗ Repository</div>
          <div class="article-card-title">${r.name}</div>
          ${r.description ? html`<div class="article-card-summary">${r.description}</div>` : null}
          <div class="article-card-meta"><span>${r.identifier}</span></div>
        </div>
      </a>`;
}

/** A repo as a feed/profile row: the shared addressable row wrapper + the repo card. */
export function repoRow(ev: NostrEvent, profiles?: ProfileMap, s?: Session): SafeHtml {
    return addressableRow(ev, repoCard(ev), ev.created_at, 'Open repository', profiles, s);
}

/** A repo as an inline embed - reuses the article-embed card (name → title, description → summary). */
export function repoEmbed(ev: NostrEvent, naddr: string, profiles?: ProfileMap): SafeHtml {
    const r = parseRepo(ev);
    return addressableEmbed(ev, naddr, '↗ repository', r.name, r.description, profiles);
}

/** Clone urls: a label above a vertical stack of single-line (scrollable) copyable monospace urls - git/ssh
 * urls aren't clickable, so they're select-all text, one per line rather than a wrapping wall of panels. */
function repoCloneBlock(urls: string[]): SafeHtml | null {
    return urls.length ? html`<div class="repo-clones"><span class="meta-label">Clone</span>${join(urls.map((u) => html`<code class="repo-clone">${u}</code>`))}</div>` : null;
}

/** Extra web urls (beyond the primary Browse button) as a labeled row of safe links. */
function repoWebRow(urls: string[]): SafeHtml | null {
    return labeledRow('Web', urls.map(repoWebLink).filter((x): x is SafeHtml => x !== null));
}

/** The repo detail page (/a/): name (git glyph), byline, description, a primary Browse action, clone urls,
 * any extra web urls, topics, maintainers (the announcer is dropped - they're already the byline), actions. */
export function repoReader(ev: NostrEvent, profiles?: ProfileMap, s?: Session): SafeHtml {
    const r = parseRepo(ev);
    // the announcer is already the byline, so drop them from the maintainers row
    const maintainers = labeledRow('Maintainers', r.maintainers.filter((pk) => pk !== ev.pubkey).map((pk) => html`<a href="/u/${npub(pk)}" h-scroll="top instant">${displayName(pk, profiles)}</a>`));
    const relays = labeledRow('Relays', r.relays.map((u) => html`<span class="repo-relay">${u}</span>`));
    const topics = r.topics.length ? html`<div class="repo-topics">${join(r.topics.map((t) => html`<span class="nip-kind-chip">#${t}</span>`))}</div>` : null;
    const browseUrl = r.web.find((u) => safeUrl(u) !== '#'); // the primary "go see the code" action
    const browse = browseUrl ? html`<a class="repo-browse" href="${safeUrl(browseUrl)}" target="_blank" rel="noopener noreferrer">${icon('globe')} Browse ↗</a>` : null;
    return html`
      <article class="article repo-article">
        <h1 class="article-title repo-title">${icon('git')}${r.name}</h1>
        <div class="article-byline">
          ${authorAvatarLink(ev.pubkey, profiles, 'sm')}
          ${authorName(ev.pubkey, profiles)}
          <span class="article-byline-meta">· ${timeAgo(ev.created_at)}</span>
        </div>
        ${r.description ? html`<p class="repo-desc">${r.description}</p>` : null}
        ${browse}
        ${repoCloneBlock(r.clone)}
        ${repoWebRow(r.web.filter((u) => u !== browseUrl))}
        ${topics}
        ${maintainers}
        ${relays}
        ${articleActions(ev, naddrFor(ev), s, true)}
      </article>`;
}

/** A list of notes (feed / profile / thread replies). `opts.faces` appends the lazy reply-faces hydrate
 * trigger for the page (feeds + profile; search omits it - faces aren't shown there). */
export function noteList(events: NostrEvent[], profiles?: ProfileMap, s?: Session, opts: NoteOpts = {}): SafeHtml {
    const rows = join(events.map((ev) => noteCard(ev, profiles, s, opts)));
    return opts.faces ? html`${rows}${facesHydrate(events, s)}` : rows;
}

/** Infinite-scroll sentinel: a real "older →" link (zero-JS navigation) that
 * helmjs upgrades to intersect-once load, replacing itself (outer) with the next
 * page + a fresh sentinel. Shared by the feed and the profile timeline. */
export function pagerSentinel(href: string): SafeHtml {
    return html`
      <li class="pager" id="more">
        <a href="${href}" h-get h-target="#more" h-swap="outer" h-trigger="intersect once, click" h-indicator="#more" h-push-url="false"><span class="pager-label">older →</span><span class="pager-loading">settling…</span></a>
      </li>`;
}

/** The Following feed's "caught up" clearing (ported from the original Satori): a still ensō + line that
 * ENDS a reading batch - a feed with real ends, not an endless scroll. The new-since-last-visit set is
 * capped to one window, then this; "View older posts" loads exactly one more batch ending in another
 * clearing (a deliberate tap, never auto-scroll), so you choose to keep going - it never runs away.
 *  - `caughtUp`: kept for the caller's mark-seen bookkeeping; the clearing itself reads the same either way.
 *  - `markTs` (newest shown): arms an intersect that advances your last-visit high-water when this
 *    clearing scrolls into view - so reaching the end marks you caught up, not merely loading.
 *  - `more` (a `until` cursor): renders the click-only "View older posts" that swaps in the next batch. */
/** The shared "caught up" clearing (the calm "design for exit": a contemplative quote over the ensō seal).
 * Used by the feed AND notifications - same shape, so one helper keeps the two parity surfaces from drifting.
 * `older` (a `until` cursor) renders the click-only "View older …" link above a smaller seal; with nothing
 * older, the seal (落款) stands alone. `mark` is an optional intersect attr the feed uses to advance its
 * last-visit high-water on scroll-into-view. The id/cls are static literals (no injection surface). */
export function caughtUpClearing(o: { id: string; cls: string; href: string; label: string; older?: number; mark?: SafeHtml }): SafeHtml {
    const tail = o.older !== undefined
        ? html`<a class="see-earlier view-older" href="${o.href}" h-get h-target="#${raw(o.id)}" h-swap="outer" h-push-url="false">${o.label}</a>${enso(30, true)}`
        : enso(40, true);
    return html`<li class="empty ${o.cls}" id="${raw(o.id)}"${o.mark ?? raw('')}><span>“${quote('caughtUp')}”</span>${tail}</li>`;
}

export function feedClearing(opts: { caughtUp: boolean; markTs?: number; more?: number }): SafeHtml {
    const mark = opts.markTs ? raw(` h-get="/feed/seen?ts=${String(opts.markTs)}" h-trigger="intersect once" h-swap="none" h-push-url="false"`) : undefined;
    return caughtUpClearing({
        id: 'feed-clearing', cls: 'caught-up', older: opts.more,
        href: `/?b=1&until=${String(opts.more)}`, label: 'View older posts →', mark,
    });
}
