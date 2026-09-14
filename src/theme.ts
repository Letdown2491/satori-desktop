// Appearance settings that must affect SSR (so there's no flash) live server-side
// in a cookie, not localStorage. Phase 0 covers theme; more appearance
// prefs (media autoload, etc.) join later as the same kind of cookie pref.

import { setCookie, type Ctx } from './http.ts';
import { normalizeRelayUrl } from './nostr/nip65.ts';
import { SEARCH_NOTE_RELAYS, SEARCH_PROFILE_RELAYS } from './data/search.ts';

export type Theme = 'sumi-e' | 'sumi-e-dark';
export const THEMES: Theme[] = ['sumi-e', 'sumi-e-dark'];

export interface Appearance {
    theme: Theme;
    zapPresets: number[]; // sats amounts offered in the zap dialog
    newNotesThreshold: number; // new feed posts before the Notes button lights up
    autoLoadMedia: boolean;    // auto-load images & videos (Phase 8 media)
    inlineVideo: boolean;      // load nostr-uploaded videos inline (browser fetches a frame on sight + plays); ON by default - the timeline shows real video frames at the cost of an on-load fetch to each video host (no different from any media fetch); off keeps the no-fetch play facade. YouTube is unaffected (always its own facade).
    reactions: boolean;        // show the like (kind:7 reaction) button on notes/articles; OFF by default (Satori favors zaps + replies)
    reactionNotifs: boolean;   // surface received reactions in notifications; OFF by default
    undoEnabled: boolean;      // hold-before-publish undo window
    undoSeconds: number;       // how long the undo window lasts
    searchNoteRelays: string[];    // NIP-50 relays for note search (relays rot → editable)
    searchProfileRelays: string[]; // NIP-50 relays for people search
}

export const DEFAULT_ZAP_PRESETS = [21, 100, 500, 1000, 5000];
const COOKIE = 'satori-appearance';

const DEFAULT_APPEARANCE: Appearance = {
    theme: 'sumi-e', zapPresets: DEFAULT_ZAP_PRESETS,
    newNotesThreshold: 5, autoLoadMedia: true, inlineVideo: true,
    reactions: false, reactionNotifs: false,
    undoEnabled: true, undoSeconds: 5,
    searchNoteRelays: SEARCH_NOTE_RELAYS, searchProfileRelays: SEARCH_PROFILE_RELAYS,
};

/** Parse a "21, 100, 500" string into a clean preset list (positive ints, max 8). */
export function parseZapPresets(raw: string): number[] {
    const list = raw.split(',').map((x) => Math.floor(Number(x.trim()))).filter((n) => Number.isFinite(n) && n > 0).slice(0, 8);
    return list.length ? list : DEFAULT_ZAP_PRESETS;
}

/** Parse a textarea of relay URLs (newline/comma/space separated) → clean list,
 * deduped, max 8; empty → `fallback` (so clearing the field restores defaults).
 * Uses the shared NIP-65 normalizer (lowercases + assumes wss:// for a bare host),
 * so settings + routing agree on one canonical form. */
export function parseRelayList(raw: string, fallback: string[]): string[] {
    const list = raw.split(/[\s,]+/).map((u) => normalizeRelayUrl(u, { assumeWss: true })).filter((u): u is string => !!u);
    const dedup = [...new Set(list)].slice(0, 8);
    return dedup.length ? dedup : fallback;
}

export function readAppearance(ctx: Ctx): Appearance {
    const a: Appearance = { ...DEFAULT_APPEARANCE, zapPresets: [...DEFAULT_ZAP_PRESETS] };
    const raw = ctx.cookies[COOKIE];
    if (raw) {
        try {
            const p = JSON.parse(raw) as Partial<Appearance>;
            if (p.theme && THEMES.includes(p.theme)) a.theme = p.theme;
            if (Array.isArray(p.zapPresets)) {
                const list = p.zapPresets.filter((n): n is number => typeof n === 'number' && n > 0).slice(0, 8);
                if (list.length) a.zapPresets = list;
            }
            if (typeof p.newNotesThreshold === 'number' && p.newNotesThreshold >= 1) a.newNotesThreshold = Math.min(50, Math.floor(p.newNotesThreshold));
            if (typeof p.autoLoadMedia === 'boolean') a.autoLoadMedia = p.autoLoadMedia;
            if (typeof p.inlineVideo === 'boolean') a.inlineVideo = p.inlineVideo;
            if (typeof p.reactions === 'boolean') a.reactions = p.reactions;
            if (typeof p.reactionNotifs === 'boolean') a.reactionNotifs = p.reactionNotifs;
            if (typeof p.undoEnabled === 'boolean') a.undoEnabled = p.undoEnabled;
            if (typeof p.undoSeconds === 'number' && p.undoSeconds >= 1) a.undoSeconds = Math.min(30, Math.floor(p.undoSeconds));
            const relays = (v: unknown): string[] | null => {
                if (!Array.isArray(v)) return null;
                const l = v.filter((u): u is string => typeof u === 'string').slice(0, 8);
                return l.length ? l : null;
            };
            const note = relays(p.searchNoteRelays); if (note) a.searchNoteRelays = note;
            const prof = relays(p.searchProfileRelays); if (prof) a.searchProfileRelays = prof;
        } catch { /* malformed cookie → defaults */ }
    }
    return a;
}

export function writeAppearance(ctx: Ctx, a: Appearance): void {
    setCookie(ctx, COOKIE, JSON.stringify(a), { maxAge: 60 * 60 * 24 * 365 });
}
