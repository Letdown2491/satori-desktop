// NIP-89 "recommended application handlers" - how an UNKNOWN kind gets handled. A kind:31990
// (handler information) event is an app announcing "I render kind X; open my web URL with this
// bech32 in it." Satori's fallback uses these to offer "open in an app that supports this" instead
// of masquerading an unknown event as a note. (kind:31989 = a user RECOMMENDING a handler - a trust
// signal we can weight by later; v1 discovers handlers directly.)

import type { NostrEvent } from './types.ts';

export const KIND_HANDLER_INFO = 31990;
export const KIND_HANDLER_RECOMMENDATION = 31989;

/** One app that can open events of a kind in the browser: a name + its web URL templates (each a
 * URL with a `<bech32>` placeholder + the entity type it serves - 'nevent'/'naddr'/'note'/''). */
export interface HandlerInfo {
    pubkey: string;
    name: string;
    webTemplates: { template: string; entity: string }[];
}

/** Parse a kind:31990. Returns null if it declares no WEB handler (we can only "open in browser").
 * `name` comes from the content (kind-0-like JSON) and may be empty - the render layer supplies a
 * short-npub fallback (keeping this module render-free / pure protocol). */
export function parseHandler(ev: NostrEvent): HandlerInfo | null {
    const webTemplates = ev.tags
        .filter((t) => t[0] === 'web' && t[1] && t[1].includes('<bech32>'))
        .map((t) => ({ template: t[1]!, entity: t[2] ?? '' }));
    if (!webTemplates.length) return null;
    let name = '';
    try { const c = JSON.parse(ev.content || '{}'); name = String(c.display_name || c.name || ''); } catch { /* no metadata */ }
    return { pubkey: ev.pubkey, name: name.trim(), webTemplates };
}

/** Build a handler's open-URL for a given bech32 entity: prefer a template whose declared entity
 * matches (e.g. 'nevent'), else a generic one, then substitute. Returned URL is UNTRUSTED (from a
 * stranger's event) - the caller must safeUrl() it. Null if the handler has no usable template. */
export function handlerUrl(h: HandlerInfo, bech: string, entity: string): string | null {
    const pick = h.webTemplates.find((w) => w.entity === entity)
        ?? h.webTemplates.find((w) => !w.entity)
        ?? h.webTemplates[0];
    return pick ? pick.template.replaceAll('<bech32>', bech) : null;
}

// NIP-89 doesn't standardize kind names, so a small map gives friendlier labels for common kinds;
// everything else reads as "kind N". Kept modest - this is a courtesy, not a registry.
const KIND_NAMES: Record<number, string> = {
    4: 'Direct message', 6: 'Repost', 7: 'Reaction', 16: 'Repost', 21: 'Video', 22: 'Short video', 1063: 'File',
    1111: 'Comment', 1311: 'Live chat', 1984: 'Report', 9735: 'Zap receipt', 9802: 'Highlight',
    10000: 'Mute list', 10002: 'Relay list', 30000: 'Follow set', 30008: 'Profile badges',
    30009: 'Badge', 30023: 'Article', 30311: 'Live event', 30315: 'Status', 30402: 'Listing', 31922: 'Calendar event',
    30617: 'Repository', 30817: 'Custom NIP', 30818: 'Wiki article',
    31923: 'Calendar event', 31924: 'Calendar', 31925: 'Calendar RSVP', 34235: 'Video', 34236: 'Short video', 34550: 'Community',
};

/** A friendly label for an event kind ("Live event", "Zap receipt", ...) or "kind N" as a fallback. */
export function kindLabel(kind: number): string {
    return KIND_NAMES[kind] ?? `kind ${kind}`;
}
