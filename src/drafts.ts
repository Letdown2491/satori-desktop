// Server-side compose drafts, keyed by your pubkey. Satori autosaves drafts to
// localStorage; with no app JS we can't, so drafts live on disk here and you
// "Save draft" explicitly. Per-account, written 0600 (local single-user daemon).
//
// Three draft kinds share one store, discriminated by `type` + keyed by `id`:
//   - article: `id` IS the NIP-23 `d` slug, so saving + later publishing reuse it
//     (edit-as-update), and publishing clears the draft.
//   - note / poll: `id` is a random token (kind-1 / kind-1068 have no addressable slug).

import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { randomBytes } from 'node:crypto';

export interface ArticleDraft {
    type: 'article';
    id: string;          // === identifier (the NIP-23 `d` slug)
    identifier: string;
    title: string;
    summary: string;
    image: string;
    topics: string;
    body: string;
    savedAt: number;
    synced?: boolean;   // published as an encrypted NIP-37 wrap to your draft relays (cross-device)
    syncedAt?: number;  // local savedAt at the last successful relay push; savedAt > this = edited-since-sync
}

export interface NoteDraft {
    type: 'note';
    id: string;
    content: string;       // raw textarea text (media urls are derived from imeta, not stored here)
    imeta: string[][];     // NIP-92 imeta tags for attached media
    cw: boolean;
    cwReason: string;
    savedAt: number;
    synced?: boolean;   // published as an encrypted NIP-37 wrap to your draft relays (cross-device)
    syncedAt?: number;  // local savedAt at the last successful relay push; savedAt > this = edited-since-sync
}

export interface PollDraft {
    type: 'poll';
    id: string;
    question: string;
    options: string[];
    multi: boolean;        // single- vs multiple-choice
    duration: number;      // index into the composer's DURATIONS select (0 = no end date)
    savedAt: number;
    synced?: boolean;   // published as an encrypted NIP-37 wrap to your draft relays (cross-device)
    syncedAt?: number;  // local savedAt at the last successful relay push; savedAt > this = edited-since-sync
}

export type Draft = ArticleDraft | NoteDraft | PollDraft;

export function newDraftId(): string { return randomBytes(8).toString('hex'); }

const FILE = process.env.SATORI_DRAFTS_FILE || join(process.cwd(), '.data', 'drafts.json');
type Store = Record<string, Record<string, Draft>>; // pubkey → id → draft

/** Normalize a stored entry: pre-`type` drafts were articles keyed by `identifier`. */
function normalize(raw: Record<string, unknown>): Record<string, Draft> {
    const out: Record<string, Draft> = {};
    for (const [key, v] of Object.entries(raw)) {
        if (!v || typeof v !== 'object') continue;
        const d = v as Partial<Draft> & { identifier?: string };
        if (!d.type) { // legacy article draft (no type, keyed by identifier)
            out[key] = { title: '', summary: '', image: '', topics: '', body: '', savedAt: 0, ...(d as object), type: 'article', id: key, identifier: d.identifier ?? key } as ArticleDraft;
        } else {
            out[key] = d as Draft;
        }
    }
    return out;
}

function readAll(): Store {
    try {
        const raw = JSON.parse(readFileSync(FILE, 'utf8')) as Record<string, Record<string, unknown>>;
        const store: Store = {};
        for (const [pk, drafts] of Object.entries(raw)) store[pk] = normalize(drafts);
        return store;
    } catch { return {}; }
}

function writeAll(all: Store): void {
    try {
        mkdirSync(dirname(FILE), { recursive: true });
        writeFileSync(FILE, JSON.stringify(all), { mode: 0o600 });
    } catch (e) {
        console.warn('[drafts] could not persist:', (e as Error)?.message ?? e);
    }
}

// Per-author cap: drafts autosave constantly and are ephemeral, so when a NEW draft would exceed the cap
// we evict the author's OLDEST (LRU by savedAt) rather than reject - you never lose recent work, and one
// account can't grow the shared store without bound on a multi-user instance.
const MAX_PER_USER = 50;
export function saveDraft(me: string, d: Draft): void {
    const all = readAll();
    const mine = (all[me] ??= {});
    if (!(d.id in mine) && Object.keys(mine).length >= MAX_PER_USER) {
        const oldest = Object.values(mine).sort((a, b) => a.savedAt - b.savedAt)[0];
        if (oldest) delete mine[oldest.id];
    }
    mine[d.id] = d;
    writeAll(all);
}

export function listDrafts(me: string): Draft[] {
    return Object.values(readAll()[me] ?? {}).sort((a, b) => b.savedAt - a.savedAt);
}

export function getDraft(me: string, id: string): Draft | null {
    return readAll()[me]?.[id] ?? null;
}

export function deleteDraft(me: string, id: string): void {
    const all = readAll();
    if (all[me]?.[id]) { delete all[me][id]; writeAll(all); }
}
