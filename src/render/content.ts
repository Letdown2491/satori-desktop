// Server-side content rendering - the string-emitting port of Satori's
// ui/components.ts Content() and renderMarkdown(). Runs over the SAME pure
// tokenizer (nostr/content.ts) and markdown parser (nostr/markdown.ts), but emits
// escaped HTML via the `html` helper instead of DOM nodes. Every text run is
// escaped by construction; nostr entities become in-app links (mentions → /u/,
// quotes → /t/, articles → /a/). Media and links are scheme-checked.

import { tokenize, type ContentToken } from '../nostr/content.ts';
import { parseBlocks, parseInline, type Inline, type Block } from '../nostr/markdown.ts';
import { parseAdocBlocks, parseAdocInline, normalizeWikiTopic, type WikiLink } from '../nostr/asciidoc.ts';
import { naddrEncode } from 'nostr-tools/nip19';
import { KIND_WIKI } from '../nostr/nip54.ts';
import { html, raw, join, safeUrl, type SafeHtml } from '../html.ts';
import { npub, displayName, shortHash, type ProfileMap } from './util.ts';
import { icon } from './svg.ts';
import { parseYouTube, youtubeWatchUrl, youtubePlaylistUrl } from '../data/youtube.ts';
import { torStrict } from '../privacy.ts';
import type { MediaMeta, ImetaMap } from '../nostr/imeta.ts';
import type { EmojiMap } from '../nostr/nip30.ts';
import { refFor } from '../manifest/registry.ts';
import { isHex64 } from '../nostr/tags.ts';

const SHORTCODE = /:([a-zA-Z0-9_-]+):/g;

/** Render a plain-text run, turning `:shortcode:` into NIP-30 custom-emoji images
 * (from `emoji`, proxied via /media); unknown shortcodes stay literal text. All
 * non-emoji text is escaped by construction. Used for note/comment bodies and for
 * profile names/about. */
export function withEmoji(text: string, emoji?: EmojiMap): SafeHtml {
    if (!emoji) return html`${text}`;
    const out: SafeHtml[] = [];
    let last = 0;
    let m: RegExpExecArray | null;
    SHORTCODE.lastIndex = 0;
    while ((m = SHORTCODE.exec(text))) {
        const url = emoji[m[1]!];
        if (!url) continue; // not a declared emoji → leave the literal :code: in place
        if (m.index > last) out.push(html`${text.slice(last, m.index)}`);
        out.push(html`<img class="emoji" src="${imgSrc(url)}" alt="${m[0]}" title="${m[0]}" loading="lazy">`);
        last = m.index + m[0].length;
    }
    if (last === 0) return html`${text}`;
    if (last < text.length) out.push(html`${text.slice(last)}`);
    return join(out);
}

export function extLink(url: string, label: string): SafeHtml {
    // Privacy: strip tracking params from BOTH the click target and the shown text. When the
    // label IS the url (the common case - a bare link), it gets cleaned too; a distinct label
    // (rare) is left as-is. YouTube links are already canonicalized upstream, so this is for
    // every other host's utm_*/fbclid/gclid cruft.
    const clean = rewriteToNitter(cleanTrackingParams(url));
    const href = safeUrl(clean);
    const text = label === url ? clean : label;
    if (href === '#') return html`${text}`; // unsafe scheme → inert text
    return html`<a href="${href}" target="_blank" rel="noopener noreferrer">${text}</a>`;
}

/** A URL's bare hostname (no `www.`) for a compact link LABEL - the full url stays the href. Falls back
 * to the raw value if it won't parse as a url, so a malformed source still shows something. */
export function prettyHost(url: string): string {
    try { return new URL(url).hostname.replace(/^www\./, ''); } catch { return url; }
}

/** An in-app reference chip: the `.mention`-styled link used for nostr references (quoted notes, article/
 * listing/etc. refs, author credits) - one place for the markup so the inline tokenizer, the highlight
 * card, and any future ref site stay in sync. `label` is escaped when a string. */
export function mentionChip(href: string, label: SafeHtml | string): SafeHtml {
    return html`<a class="mention" href="${href}" h-scroll="top instant">${label}</a>`;
}

// Tracking/analytics params we strip from displayed + clicked links. A conservative, well-known
// set (ad/analytics click-ids + the utm_* family) that never changes page content, so removing
// them can't break a link. Matches Satori's privacy stance: your clicks don't carry their tags.
const TRACKING_PARAMS = new Set([
    'fbclid', 'gclid', 'gbraid', 'wbraid', 'dclid', 'msclkid', 'yclid', 'twclid', 'igshid', 'ttclid',
    'mc_eid', 'mc_cid', 'mkt_tok', 'oly_anon_id', 'oly_enc_id', 'vero_id', '_hsenc', '_hsmi',
    'mibextid', 'fb_action_ids', 'fb_action_types', 's_cid', 'rb_clickid',
]);
const UTM_PREFIX = /^utm_/i;

/** Strip known tracking params from a URL for display + click-through. Returns the input
 * unchanged if it isn't a parseable http(s) URL, has no query, or carried no tracking params. */
export function cleanTrackingParams(url: string): string {
    if (!url.includes('?')) return url;
    let u: URL;
    try { u = new URL(url); } catch { return url; }
    if (u.protocol !== 'http:' && u.protocol !== 'https:') return url;
    let changed = false;
    for (const key of [...u.searchParams.keys()]) {
        if (TRACKING_PARAMS.has(key.toLowerCase()) || UTM_PREFIX.test(key)) { u.searchParams.delete(key); changed = true; }
    }
    return changed ? u.toString() : url; // toString() drops the trailing '?' when the query empties
}

// Privacy frontend: send x.com / twitter.com links to xcancel.com (a stable Nitter instance) - no login
// wall, no JS, no tracking. Nitter mirrors Twitter's path structure, so the path carries over; we also
// drop Twitter's `s`/`t` share-tracking params (safe to here, since we know the host). This is a pure
// link rewrite - we render links, never server-fetched previews - so an xcancel outage only affects the
// click in the reader's browser, never our page (unlike the dropped Piped PROXY seam). Applied at the
// extLink chokepoint, so it covers link tokens (notes + articles); a URL the tokenizer classifies as media
// (by file extension) bypasses it - irrelevant for Twitter, whose links never carry a media extension.
const TWITTER_HOSTS = new Set(['x.com', 'www.x.com', 'twitter.com', 'www.twitter.com', 'mobile.twitter.com']);
export function rewriteToNitter(url: string): string {
    let u: URL;
    try { u = new URL(url); } catch { return url; }
    if (u.protocol !== 'http:' && u.protocol !== 'https:') return url;
    if (!TWITTER_HOSTS.has(u.hostname.toLowerCase())) return url;
    u.hostname = 'xcancel.com';
    u.protocol = 'https:';
    u.searchParams.delete('s'); u.searchParams.delete('t'); // Twitter share-tracking; xcancel ignores them anyway
    return u.toString();
}

function mentionLink(pubkey: string, profiles?: ProfileMap): SafeHtml {
    return html`<a class="mention" href="/u/${npub(pubkey)}" h-scroll="top instant">@${withEmoji(displayName(pubkey, profiles), profiles?.get(pubkey)?.emoji)}</a>`;
}

/** Pubkeys mentioned (nostr:npub / nprofile) inside a content string, so a feed can
 * hydrate their profiles BEFORE render - otherwise a mention degrades to @npub1…
 * (mirrors Satori hydrate.ts collecting `.mention[data-pubkey]` to ensureProfiles). */
export function mentionPubkeys(text: string): string[] {
    const out: string[] = [];
    for (const tok of tokenize(text)) if (tok.t === 'mention') out.push(tok.pubkey);
    return out;
}

/** The src for a DISPLAYED image: routed through our /media proxy so the browser never
 * hits the third-party host (the daemon's fetch honors Privacy Mode). safeUrl-invalid
 * → '#'. Video is not proxied yet (needs Range/streaming), so it stays direct. */
export function imgSrc(url: string, author?: string): string {
    if (safeUrl(url) === '#') return '#';
    // NIP-B7: pass the note author so the proxy can heal a dead Blossom-hash url from the author's
    // other servers (kind:10063). Only a valid hex pubkey rides along; absent → no healing, as before.
    const a = author && isHex64(author) ? `&author=${author}` : '';
    return `/media?u=${encodeURIComponent(url)}${a}`;
}

function image(url: string): SafeHtml {
    if (safeUrl(url) === '#') return extLink(url, url);
    return html`<img class="media" src="${imgSrc(url)}" loading="lazy" alt="">`;
}

/** NIP-92 `dim` (WxH) → `width`/`height` HTML attributes, so the browser reserves the
 * slot's aspect-ratio before the media loads (no layout shift) with NO inline CSS - the
 * responsive `.media` rule (width:100%, height:auto) scales it. Empty when no usable dim. */
/** Parse an imeta `dim` ("800x600", tolerating decimals like "464.0x848.0" from some clients). */
function parseDim(dim?: string): { w: number; h: number } | null {
    const d = dim ? /^(\d+(?:\.\d+)?)x(\d+(?:\.\d+)?)$/.exec(dim) : null;
    return d ? { w: Math.round(+d[1]!), h: Math.round(+d[2]!) } : null;
}
function dimStyle(meta?: MediaMeta): SafeHtml {
    const d = parseDim(meta?.dim);
    return d ? raw(` width="${d.w}" height="${d.h}"`) : raw('');
}
/** Orientation bucket from the dim - a CSS class (no inline style) so a poster-less video
 * facade reserves roughly the right aspect; the real <video>'s width/height correct it on play. */
function aspectClass(dim?: string, fallback = 'landscape'): string {
    const d = parseDim(dim);
    if (!d) return fallback; // no usable dim → the caller's orientation hint (NIP-71 kind), else landscape
    return d.w > d.h ? 'landscape' : d.w < d.h ? 'portrait' : 'square';
}

function video(url: string, meta?: MediaMeta, inline = false): SafeHtml {
    const href = safeUrl(url);
    if (href === '#') return extLink(url, url);
    // Strict Privacy Mode: a note video would stream browser→host (the lone CSP gap, leaks your IP). We
    // don't proxy video (Tor can't carry it well); instead suppress it like the YT player - poster + a
    // deliberate "leaves Tor" open link, nothing auto-loads. (Overrides the inline pref - Strict wins.)
    if (torStrict()) return videoSuppressed(url, meta);
    // With a poster (imeta thumb): show the frame; preload="none" so the video itself loads only on play -
    // a real frame with NO fetch, so we use it regardless of the inline pref.
    if (meta?.thumb) {
        return html`<video class="media ${aspectClass(meta.dim, meta.orient)}" src="${href}" controls preload="none" playsinline${dimStyle(meta)} poster="${imgSrc(meta.thumb)}"></video>`;
    }
    // No poster. Inline pref ON: load the video so the browser shows its first frame + inline play
    // (preload="metadata" = an on-load fetch to the host, the deliberate tradeoff). OFF (default): the
    // calm no-fetch play-facade that loads nothing until clicked.
    if (inline) {
        return html`<video class="media ${aspectClass(meta?.dim, meta?.orient)}" src="${href}" controls preload="metadata" playsinline${dimStyle(meta)}></video>`;
    }
    return videoFacade(url, meta);
}

/** Strict Privacy Mode video: a calm ink panel whose play glyph OPENS the raw file in a new tab (a
 * deliberate, user-chosen leak), with a quiet "opens outside Tor" caption. No inline <video src>, no
 * h-get-to-/video - so nothing streams browser→host under Tor. Mirrors the suppressed YouTube player. */
function videoSuppressed(url: string, meta?: MediaMeta): SafeHtml {
    const href = safeUrl(url);
    if (href === '#') return extLink(url, url);
    return html`<a class="video-facade strict ${aspectClass(meta?.dim, meta?.orient)}" href="${href}" target="_blank" rel="noreferrer noopener" aria-label="Open video (leaves Tor)" title="Open video (leaves Tor)">${icon('play', true)}<span class="video-strict-note">opens outside Tor</span></a>`;
}

/** No-poster play placeholder: a calm ink panel + play glyph (aspect-bucketed from the imeta
 * dim) that swaps in the autoplaying <video> on click. JS off: the link opens the file. */
function videoFacade(url: string, meta?: MediaMeta): SafeHtml {
    const href = safeUrl(url);
    if (href === '#') return extLink(url, url);
    const id = `vf-${mediaId(url)}`;
    const q = `u=${encodeURIComponent(url)}${meta?.dim ? `&dim=${encodeURIComponent(meta.dim)}` : ''}`;
    // The h-get lives on the <div> (helmjs reads h-get off non-anchors; on an <a> it would use
    // href and fetch the cross-origin mp4 itself → a connect-src block). The inner <a href=file>
    // is the no-JS fallback (opens the video directly); with JS the div swaps in the player.
    return html`<div class="video-facade ${aspectClass(meta?.dim, meta?.orient)}" id="${raw(id)}" h-get="/video?${q}" h-target="#${raw(id)}" h-swap="outer" h-push-url="false"><a class="video-facade-play" href="${href}" aria-label="Play video">${icon('play', true)}</a></div>`;
}

/** The autoplaying player swapped in when a facade is clicked (explicit play). */
export function videoEmbed(url: string, dim?: string): SafeHtml {
    const href = safeUrl(url);
    if (href === '#') return extLink(url, url);
    return html`<video class="media ${aspectClass(dim)}" src="${href}" controls autoplay playsinline preload="auto"${dimStyle({ dim })}></video>`;
}

/** Appearance media prefs (from the settings cookie): whether to auto-load images/videos at all, and
 * whether to load nostr videos inline (a real frame + inline play, at the cost of an on-load fetch to the
 * video host) vs the no-fetch play facade. (Video autoplay was removed by design - calm, never auto-playing.) */
export interface MediaPrefs { autoLoad: boolean; inlineVideo?: boolean }
const DEFAULT_MEDIA: MediaPrefs = { autoLoad: true, inlineVideo: false };

interface MediaItem { type: 'image' | 'video'; url: string; id: string; meta?: MediaMeta; author?: string }

/** A stable per-URL id for the lightbox slide (the tile links to `#<id>`). Two
 * notes sharing an image collide harmlessly (both overlays would open). */
const mediaId = (url: string): string => `lb-${shortHash(url)}`;

/** Gather a run of adjacent media starting at `i` (absorbing whitespace-only text
 * between items); returns the run + the index just after it. Shared by renderContent
 * (inline tiles) and mediaRuns (the hoisted overlays), so their slide ids line up. */
function gatherRun(toks: ReturnType<typeof tokenize>, i: number, imeta?: ImetaMap, author?: string): { run: MediaItem[]; next: number } {
    const run: MediaItem[] = [];
    let j = i;
    while (j < toks.length) {
        const t = toks[j]!;
        if (t.t === 'image' || t.t === 'video') { run.push({ type: t.t, url: t.url, id: mediaId(t.url), meta: imeta?.get(t.url), author }); j++; }
        else if (t.t === 'text' && t.value.trim() === '') j++;
        else break;
    }
    return { run, next: j };
}

/** NIP-92: a media URL with no file extension (a Blossom/hash url) tokenizes as a plain `url` because the
 * tokenizer classifies by extension alone. Upgrade it to image/video when its imeta `m` (mime) says so, so
 * extensionless media still renders as media - grouping into galleries and getting a lightbox like any other
 * image/video. Returns the SAME array when nothing changed (the memoized token array stays untouched). */
function applyImetaMime(toks: ContentToken[], imeta?: ImetaMap): ContentToken[] {
    if (!imeta) return toks;
    let changed = false;
    const out = toks.map((t): ContentToken => {
        if (t.t !== 'url') return t;
        const mime = imeta.get(t.url)?.mime;
        if (mime?.startsWith('image/')) { changed = true; return { t: 'image', url: t.url }; }
        if (mime?.startsWith('video/')) { changed = true; return { t: 'video', url: t.url }; }
        return t;
    });
    return changed ? out : toks;
}

const playBadge = (): SafeHtml => html`<div class="gallery-play">${icon('play', true)}</div>`;

/** Where a media tile points: the in-app lightbox (`#id`, top-level notes) or the
 * raw file in a new tab (embed previews - their lightbox can't escape the note's
 * `content-visibility` containment, so we don't trap a broken overlay there). */
function tileHref(m: MediaItem, lightbox: boolean): { href: string; attrs: SafeHtml } {
    if (lightbox) return { href: `#${m.id}`, attrs: raw('') };
    const u = safeUrl(m.url);
    return { href: u, attrs: u === '#' ? raw('') : raw(' target="_blank" rel="noopener noreferrer"') };
}

/** A single image → opens the lightbox (its overlay is hoisted out of the note by
 * the caller via mediaLightboxes, so `position:fixed` escapes content-visibility). */
function singleImage(m: MediaItem, lightbox: boolean): SafeHtml {
    const href = safeUrl(m.url);
    if (href === '#') return extLink(m.url, m.url);
    const t = tileHref(m, lightbox);
    // aria-label gives the icon/image-only link an accessible name (WCAG H30); the img
    // stays alt="" (or its imeta alt) so it isn't announced twice.
    const label = m.meta?.alt || 'Open image';
    return html`<a class="media-link" href="${t.href}"${t.attrs} aria-label="${label}"><img class="media" src="${imgSrc(m.url, m.author)}" loading="lazy" alt="${m.meta?.alt ?? ''}"${dimStyle(m.meta)}></a>`;
}

/** 2+ adjacent media → a horizontal scroll-snap carousel; each tile opens the
 * lightbox at its index. Native swipe pages the strip; click-paging happens in the
 * (full-screen) lightbox. */
function gallery(run: MediaItem[], lightbox: boolean): SafeHtml {
    const tiles = run.map((m) => {
        // Video tiles show the proxied poster (a frame, no video fetch) when the imeta has one;
        // otherwise a neutral placeholder. Either way the tile is a link into the lightbox.
        const inner = m.type === 'video'
            ? (m.meta?.thumb
                ? html`<img class="gallery-media" src="${imgSrc(m.meta.thumb, m.author)}" loading="lazy" alt="${m.meta?.alt ?? ''}">`
                : html`<div class="gallery-media video-ph"></div>`) // ink placeholder (no fetch); tile links to lightbox
            : html`<img class="gallery-media" src="${imgSrc(m.url, m.author)}" loading="lazy" alt="${m.meta?.alt ?? ''}">`;
        const t = tileHref(m, lightbox);
        const label = m.meta?.alt || (m.type === 'video' ? 'Play video' : 'Open image');
        return html`<a class="gallery-tile" href="${t.href}"${t.attrs} aria-label="${label}">${inner}${m.type === 'video' ? playBadge() : null}</a>`;
    });
    return html`<div class="gallery-wrap"><div class="gallery">${join(tiles)}</div></div>`;
}

/** Group adjacent image/video tokens (absorbing whitespace-only separators) into
 * media runs, in document order - shared by renderContent (inline placement) and
 * mediaLightboxes (the hoisted overlays), so their slide ids line up. */
function mediaRuns(toks: ReturnType<typeof tokenize>, imeta?: ImetaMap): MediaItem[][] {
    const runs: MediaItem[][] = [];
    let i = 0;
    while (i < toks.length) {
        if (toks[i]!.t === 'image' || toks[i]!.t === 'video') {
            const { run, next } = gatherRun(toks, i, imeta);
            runs.push(run); i = next;
        } else i++;
    }
    return runs;
}

/** The full-screen lightbox overlays for a note's content. Rendered by noteCard
 * OUTSIDE the `<li>` (the `.note` has content-visibility → a containing block that
 * would trap a `position:fixed` overlay; Satori sidesteps this by mounting on
 * <body>). Single videos play inline, so they get no overlay. */
export function mediaLightboxes(text: string, autoLoad = true, imeta?: ImetaMap): SafeHtml {
    if (!autoLoad) return html``; // media shown as links → no lightbox overlays
    const overlays = mediaRuns(applyImetaMime(tokenize(text), imeta), imeta)
        .filter((run) => !(run.length === 1 && run[0]!.type === 'video'))
        .map((run) => lightbox(run));
    if (!overlays.length) return html``;
    // Wrap in an <li> (display:contents, see CSS) so these hoisted overlays are VALID
    // children of the feed <ul> - a bare <div> under <ul> is invalid - while staying out
    // of the note's paint-containment (content-visibility), which would trap the fixed overlay.
    return html`<li class="lightbox-host">${join(overlays)}</li>`;
}

/** The full-screen viewer for a media run (one overlay, one slide per item).
 * Opened purely by `:target` (see CSS): the strip scroll-snaps to the targeted
 * slide; ‹ › are links to neighbour slides; the backdrop / ✕ are links to `#_`. */
function lightbox(run: MediaItem[]): SafeHtml {
    const slides = run.map((m, idx) => {
        const href = safeUrl(m.url);
        const media = m.type === 'video'
            ? html`<video class="lightbox-media" src="${href}" controls playsinline preload="none"${m.meta?.thumb ? html` poster="${imgSrc(m.meta.thumb)}"` : html``}></video>`
            : html`<img class="lightbox-media" src="${imgSrc(m.url)}" alt="${m.meta?.alt ?? ''}">`;
        const prev = idx > 0 ? html`<a class="lightbox-nav prev" href="#${raw(run[idx - 1]!.id)}" aria-label="Previous">‹</a>` : null;
        const next = idx < run.length - 1 ? html`<a class="lightbox-nav next" href="#${raw(run[idx + 1]!.id)}" aria-label="Next">›</a>` : null;
        return html`<div class="lightbox-slide" id="${raw(m.id)}"><a class="lightbox-backdrop" href="#_" aria-label="Close"></a>${media}${prev}${next}</div>`;
    });
    return html`<div class="lightbox"><div class="lightbox-strip">${join(slides)}</div><a class="lightbox-close" href="#_" aria-label="Close">✕</a></div>`;
}

// --- reusable media for tag-sourced images (cardShell kinds) ---------------
// Kinds whose images live in TAGS (NIP-99 `image`, NIP-68 imeta) rather than tokenized content can reuse
// the SAME gallery/lightbox a note uses, instead of re-rolling it. mediaTiles places the inline tiles;
// mediaOverlays returns the hoisted lightbox <li> a cardShell kind emits outside its own containment.

/** Build MediaItem[] for tag-sourced images (images only). Entries are bare urls (NIP-99 `image` tags)
 * or {url, meta} (NIP-92 imeta - so alt/dim ride along and dimStyle still reserves the slot). */
export function imageItems(entries: Array<string | { url: string; meta?: MediaMeta }>): MediaItem[] {
    return entries.map((e) => typeof e === 'string'
        ? { type: 'image', url: e, id: mediaId(e) }
        : { type: 'image', url: e.url, id: mediaId(e.url), meta: e.meta });
}

/** Build VIDEO MediaItem[] for tag-sourced videos (NIP-71 imeta). meta.thumb is the poster, meta.dim the
 * aspect - both flow into the same privacy-aware player notes use. */
export function videoItems(entries: Array<{ url: string; meta?: MediaMeta }>): MediaItem[] {
    return entries.map((e) => ({ type: 'video', url: e.url, id: mediaId(e.url), meta: e.meta }));
}

/** Inline tiles for a set of items - exactly as a note renders them: a single video plays inline (the
 * privacy-aware player), a single image is one tile, 2+ become the scroll-snap gallery. `lightbox` true
 * links image/gallery tiles to the in-page overlay; false opens the raw file (embeds). */
export function mediaTiles(items: MediaItem[], lightbox = true, inlineVideo = false): SafeHtml {
    if (items.length === 0) return html``;
    if (items.length >= 2) return gallery(items, lightbox);
    return items[0]!.type === 'video' ? video(items[0]!.url, items[0]!.meta, inlineVideo) : singleImage(items[0]!, lightbox);
}

/** The hoisted full-screen lightbox overlays for a set of items, in the lightbox-host <li> so a cardShell
 * kind can emit them OUTSIDE its content-visibility container (the same hoist mediaLightboxes does). A
 * single video plays inline, so it gets no overlay (mirrors mediaLightboxes). */
export function mediaOverlays(items: MediaItem[]): SafeHtml {
    if (items.length === 0) return html``;
    if (items.length === 1 && items[0]!.type === 'video') return html``;
    return html`<li class="lightbox-host">${lightbox(items)}</li>`;
}

/** A lazily-loaded embed card (helmjs intersect → /embed): the quoted note /
 * article renders inline. The chip inside is the zero-JS + loading fallback. */
function embedCard(bech: string, as: string, label: string, href: string): SafeHtml {
    const id = `emb-${bech.slice(-14)}`;
    return html`<div class="quote embed-card" id="${id}" h-get="/embed/${bech}?as=${as}" h-trigger="intersect once" h-target="#${raw(id)}" h-swap="inner" h-push-url="false"><a class="quote-label" href="${href}">${label}</a></div>`;
}

/** A YouTube privacy-facade card (lazy, like embedCard). Hydrates into the proxied
 * thumbnail + title + a click-to-load nocookie player via /yt/card. Before that (and
 * with JS off) it's just a clean, tracking-free external link - so nothing contacts
 * Google until the card loads or the user clicks through. */
function youtubeCard(id: string, start?: number): SafeHtml {
    const t = start ? `?t=${start}` : '';
    return html`<div class="yt-card" id="yt-${id}" h-get="/yt/card/${id}${t}" h-trigger="intersect once" h-target="#yt-${id}" h-swap="inner" h-push-url="false"><a class="yt-fallback" href="${youtubeWatchUrl(id, start)}" target="_blank" rel="noopener noreferrer">▶ Watch on YouTube</a></div>`;
}

/** A YouTube PLAYLIST facade card - same lazy /yt path as the video card, keyed by list id; the
 * no-JS / pre-hydration state is a clean playlist link. */
function youtubePlaylistCard(list: string): SafeHtml {
    return html`<div class="yt-card" id="yt-pl-${list}" h-get="/yt/playlist/${list}" h-trigger="intersect once" h-target="#yt-pl-${list}" h-swap="inner" h-push-url="false"><a class="yt-fallback" href="${youtubePlaylistUrl(list)}" target="_blank" rel="noopener noreferrer">▶ Open playlist on YouTube</a></div>`;
}

/** Render note content to safe HTML. nostr entities resolve to in-app links;
 * with `embeds` (default) a quoted note / article becomes a lazy inline card.
 * Pass `embeds=false` inside an embed preview to keep one level deep (chips). */
export function renderContent(text: string, profiles?: ProfileMap, embeds = true, media: MediaPrefs = DEFAULT_MEDIA, imeta?: ImetaMap, emoji?: EmojiMap, author?: string): SafeHtml {
    const parts: SafeHtml[] = [];
    const toks = applyImetaMime(tokenize(text), imeta);
    let i = 0;
    // A token that renders as a BLOCK-level card (quoted note / article / media / YT facade).
    // The author's blank lines around such a card would otherwise render (.content is pre-wrap)
    // as a tall gap ON TOP of the card's own margin, so we trim newlines in the adjacent text.
    const isBlockEmbed = (t: typeof toks[number] | undefined): boolean => {
        if (!t) return false;
        if (t.t === 'image' || t.t === 'video') return true;
        if (t.t === 'quote') return embeds;
        if (t.t === 'address') return embeds && !!refFor(t.kind); // a kind with an in-app reference embeds as a block
        if (t.t === 'url') return embeds && media.autoLoad && !!parseYouTube(t.url);
        return false;
    };
    while (i < toks.length) {
        const tok = toks[i]!;
        // Media off (autoLoad=false) → each image/video is just a link, no fetch.
        if ((tok.t === 'image' || tok.t === 'video') && !media.autoLoad) {
            parts.push(extLink(tok.url, tok.url));
            i++;
            continue;
        }
        // Group a run of adjacent media (absorbing whitespace-only separators) →
        // a gallery (2+) or a single image / inline video. The lightbox OVERLAYS
        // are emitted separately (mediaLightboxes) so they can escape the note's
        // content-visibility containment; here we only place the tiles. In an embed
        // preview (embeds=false) tiles open the raw file (no in-note lightbox).
        if (tok.t === 'image' || tok.t === 'video') {
            const { run, next } = gatherRun(toks, i, imeta, author);
            if (run.length >= 2) parts.push(gallery(run, embeds));
            else if (run[0]!.type === 'video') parts.push(video(run[0]!.url, run[0]!.meta, media.inlineVideo)); // single video: facade, or inline frame when the pref is on
            else parts.push(singleImage(run[0]!, embeds));
            i = next;
            continue;
        }
        if (tok.t === 'text') {
            let v = tok.value;
            if (isBlockEmbed(toks[i - 1])) v = v.replace(/^[ \t]*\n[ \t\n]*/, ''); // drop blank line(s) after a card
            if (isBlockEmbed(toks[i + 1])) v = v.replace(/[ \t\n]*\n[ \t]*$/, ''); // drop blank line(s) before a card
            parts.push(withEmoji(v, emoji));
        }
        else if (tok.t === 'url') {
            // YouTube → a privacy facade card (top-level + media-on); otherwise (media
            // off, or inside an embed preview) still emit a CLEANED link (no si/utm).
            const yt = parseYouTube(tok.url);
            if (yt && embeds && media.autoLoad) parts.push(yt.kind === 'playlist' ? youtubePlaylistCard(yt.list) : youtubeCard(yt.id, yt.start));
            else if (yt) { const w = yt.kind === 'playlist' ? youtubePlaylistUrl(yt.list) : youtubeWatchUrl(yt.id, yt.start); parts.push(extLink(w, w)); }
            else parts.push(extLink(tok.url, tok.url));
        }
        else if (tok.t === 'mention') parts.push(mentionLink(tok.pubkey, profiles));
        else if (tok.t === 'quote') {
            parts.push(embeds ? embedCard(tok.bech, 'quote', '↗ quoted note', `/t/${tok.bech}`)
                : mentionChip(`/t/${tok.bech}`, '↗ quoted note'));
        } else if (tok.t === 'address') {
            const r = refFor(tok.kind); // the manifest decides how a reference to this addressable kind renders
            if (r) parts.push(embeds ? embedCard(tok.bech, r.as, r.label, r.path(tok.bech))
                : mentionChip(r.path(tok.bech), r.label));
            else parts.push(html`<a class="mention" href="https://njump.me/${tok.bech}" target="_blank" rel="noopener noreferrer">↗ event</a>`);
        } else {
            // NIP-21 only defines npub/nprofile/note/nevent/naddr as linkable references. Anything else - an
            // nsec/ncryptsec (SECRET key material) or an unknown future type - must NOT become a njump link:
            // that would put the raw bech (a PRIVATE KEY, for nsec) into an href and leak it to a third party
            // on click. Render inert, and never echo the bech for secret types.
            const secret = tok.type === 'nsec' || tok.type === 'ncryptsec';
            parts.push(html`<span class="mention-inert">${secret ? `[${tok.type} redacted]` : tok.bech}</span>`);
        }
        i++;
    }
    return html`<div class="content">${join(parts)}</div>`;
}

// --- Markdown (NIP-23 article bodies) → safe HTML --------------------------

/** A plain-text run inside markdown: newlines → <br>, resolve urls/nostr entities. */
function inlineText(s: string, profiles?: ProfileMap): SafeHtml {
    const lines = s.split('\n');
    const out: SafeHtml[] = [];
    lines.forEach((line, i) => {
        if (i) out.push(raw('<br>'));
        if (line) out.push(inlineEntities(line, profiles));
    });
    return join(out);
}

/** Resolve urls / nostr entities in a markdown text fragment (no whitespace normalize). */
export function inlineEntities(text: string, profiles?: ProfileMap): SafeHtml {
    const parts: SafeHtml[] = [];
    for (const tok of tokenize(text, false)) {
        if (tok.t === 'text') parts.push(html`${tok.value}`);
        else if (tok.t === 'url') { const yt = parseYouTube(tok.url); const w = yt ? (yt.kind === 'playlist' ? youtubePlaylistUrl(yt.list) : youtubeWatchUrl(yt.id, yt.start)) : tok.url; parts.push(extLink(w, w)); } // article body: clean the link (no card inline)
        else if (tok.t === 'image' || tok.t === 'video') parts.push(extLink(tok.url, tok.url));
        else if (tok.t === 'mention') parts.push(mentionLink(tok.pubkey, profiles));
        else if (tok.t === 'quote') parts.push(mentionChip(`/t/${tok.bech}`, '↗ note'));
        else if (tok.t === 'address') { const r = refFor(tok.kind); parts.push(r
            ? html`<a class="mention" href="${r.path(tok.bech)}" h-scroll="top instant">${r.label}</a>`
            : html`<a class="mention" href="https://njump.me/${tok.bech}" target="_blank" rel="noopener noreferrer">↗ event</a>`); }
        else parts.push(html`<a class="mention" href="https://njump.me/${tok.bech}" target="_blank" rel="noopener noreferrer">↗ ${tok.type}</a>`);
    }
    return join(parts);
}

/** Render pre-parsed inline tokens to safe HTML - shared by the Markdown and AsciiDoc bodies (same token
 * shape, same escaped leaf renderers; only the tokenizer differs). Text runs flow through inlineEntities
 * so URLs + nostr: entities resolve identically in both. */
function renderInlineToken(tok: Inline, profiles?: ProfileMap): SafeHtml {
    if (tok.t === 'text') return inlineText(tok.v, profiles);
    if (tok.t === 'strong') return html`<strong>${inlineEntities(tok.v, profiles)}</strong>`;
    if (tok.t === 'em') return html`<em>${inlineEntities(tok.v, profiles)}</em>`;
    if (tok.t === 'code') return html`<code class="md-code">${tok.v}</code>`;
    if (tok.t === 'link') return extLink(tok.href, tok.text);
    if (tok.t === 'break') return raw('<br>');
    return image(tok.url);
}

function renderInlineTokens(tokens: Inline[], profiles?: ProfileMap): SafeHtml {
    return join(tokens.map((t) => renderInlineToken(t, profiles)));
}

function renderInline(text: string, profiles?: ProfileMap): SafeHtml {
    return renderInlineTokens(parseInline(text), profiles);
}

/** A NIP-54 wikilink → an in-app link to the SAME author's wiki article on that topic (kind:30818,
 * d = normalized topic); the article's own reader + seen-relays resolve it. Without an author to key
 * against (or if the topic won't encode), it degrades to clean styled text - never the raw `[[ ]]`. */
function wikiLink(w: WikiLink, author?: string, cache?: Map<string, string>): SafeHtml {
    const slug = normalizeWikiTopic(w.target);
    if (author && slug) {
        let naddr = cache?.get(slug);
        if (naddr === undefined) { // encode once per topic per render (dense wikis repeat topics heavily)
            try { naddr = naddrEncode({ kind: KIND_WIKI, pubkey: author, identifier: slug, relays: [] }); } catch { naddr = ''; }
            cache?.set(slug, naddr);
        }
        if (naddr) return html`<a class="wikilink" href="/a/${naddr}" h-scroll="top instant">${w.display}</a>`;
    }
    return html`<span class="wikilink">${w.display}</span>`;
}

/** The shared block-render loop for the article-body reader. Markdown and AsciiDoc share the same Block AST
 * and the escaped `.article-body` typography; they differ only in the parser, the inline renderer, and the
 * heading-level shift (Markdown's `#` → h2, so shift 1; AsciiDoc's `==` → h2, so shift 0). */
function renderBlocks(blocks: Block[], inl: (t: string) => SafeHtml, headingShift: number): SafeHtml[] {
    const out: SafeHtml[] = [];
    for (const b of blocks) {
        if (b.t === 'heading') { const level = Math.min(b.level + headingShift, 6); out.push(raw(`<h${level}>`)); out.push(inl(b.text)); out.push(raw(`</h${level}>`)); }
        else if (b.t === 'paragraph') out.push(html`<p>${inl(b.text)}</p>`);
        else if (b.t === 'list') { const items = join(b.items.map((it) => html`<li>${inl(it)}</li>`)); out.push(b.ordered ? html`<ol>${items}</ol>` : html`<ul>${items}</ul>`); }
        else if (b.t === 'quote') out.push(html`<blockquote>${inl(b.text)}</blockquote>`);
        else if (b.t === 'code') out.push(html`<pre class="md-pre"><code>${b.text}</code></pre>`);
        else if (b.t === 'hr') out.push(raw('<hr>'));
        else if (b.t === 'break') out.push(raw('<br>'));
        else out.push(image(b.url));
    }
    return out;
}

/** Render a Markdown article body to safe HTML (no innerHTML; everything escaped). */
export function renderMarkdown(md: string, profiles?: ProfileMap, coverUrl?: string): SafeHtml {
    const blocks = parseBlocks(md);
    // Drop a leading body image that duplicates the article's cover (the NIP-23 `image` tag) - many authors
    // put the hero in both, which would render it twice. FIRST block, exact-URL match → near-zero false positives.
    const start = coverUrl && blocks[0]?.t === 'image' && blocks[0].url === coverUrl ? 1 : 0;
    return html`<div class="article-body">${join(renderBlocks(blocks.slice(start), (t) => renderInline(t, profiles), 1))}</div>`;
}

/** Render an AsciiDoc body (NIP-54 wiki, Alexandria publications) to safe HTML - the SAME escaped,
 * no-innerHTML pipeline + `.article-body` shell as renderMarkdown, so the wiki reader reuses the article
 * typography. Only the parser + inline (wikilink-aware) differ. A leading level-1 heading is dropped:
 * AsciiDoc's doc title is `= Title`, which the reader already shows as the page <h1>. */
export function renderAsciiDoc(src: string, profiles?: ProfileMap, author?: string): SafeHtml {
    const naddrCache = new Map<string, string>(); // memoize per render: a link-dense wiki repeats topics
    const inl = (t: string): SafeHtml => join(parseAdocInline(t).map((tok) =>
        tok.t === 'wikilink' ? wikiLink(tok, author, naddrCache) : renderInlineToken(tok, profiles)));
    const blocks = parseAdocBlocks(src);
    const start = blocks[0]?.t === 'heading' && blocks[0].level === 1 ? 1 : 0;
    return html`<div class="article-body">${join(renderBlocks(blocks.slice(start), inl, 0))}</div>`;
}
