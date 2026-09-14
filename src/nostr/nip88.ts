// NIP-88 polls. A poll is a kind:1068 event (question in content; `option`
// tags = id + label, `polltype` single/multiple, `endsAt`, `relay` tags for
// where votes go). A vote is a kind:1018 event with an `e` tag → poll id and one
// `response` tag per chosen option. Pure parsing + tally - no DOM, no network.

import type { NostrEvent } from './types.ts';
import { relayTags } from './tags.ts';

export const KIND_POLL = 1068;
export const KIND_POLL_RESPONSE = 1018;

export type PollType = 'single' | 'multiple';
export interface PollOption { id: string; label: string }

const now = () => Math.floor(Date.now() / 1000);

export function parsePollOptions(ev: NostrEvent): PollOption[] {
    return ev.tags.filter((t) => t[0] === 'option' && t[1]).map((t) => ({ id: t[1]!, label: t[2] ?? '' }));
}

export function parsePollType(ev: NostrEvent): PollType {
    const v = ev.tags.find((t) => t[0] === 'polltype')?.[1]?.toLowerCase();
    return v === 'multiplechoice' ? 'multiple' : 'single';
}

export function parseEndsAt(ev: NostrEvent): number | null {
    const v = ev.tags.find((t) => t[0] === 'endsAt')?.[1];
    const n = v ? Number(v) : NaN;
    return Number.isFinite(n) ? n : null;
}

export function parsePollRelays(ev: NostrEvent): string[] {
    return relayTags(ev); // deduped + capped, matching the DM / draft relay parsers
}

export function isPollEnded(ev: NostrEvent): boolean {
    const ends = parseEndsAt(ev);
    return ends !== null && now() > ends;
}

/** Build the tags for a kind:1018 vote on a poll. */
export function buildResponseTags(pollId: string, optionIds: string[]): string[][] {
    return [['e', pollId], ...optionIds.map((id) => ['response', id])];
}

const ID_CHARS = 'abcdefghijklmnopqrstuvwxyz0123456789';
/** A random 9-char option id (spec format). */
export function generateOptionId(): string {
    let s = '';
    for (let i = 0; i < 9; i++) s += ID_CHARS[Math.floor(Math.random() * ID_CHARS.length)];
    return s;
}

/** Build the tags for a kind:1068 poll. `relays` is where votes should be sent. */
export function buildPollTags(options: PollOption[], type: PollType, endsAt: number | null, relays: string[]): string[][] {
    const tags: string[][] = options.map((o) => ['option', o.id, o.label]);
    for (const url of relays) tags.push(['relay', url]);
    tags.push(['polltype', type === 'multiple' ? 'multiplechoice' : 'singlechoice']);
    if (endsAt) tags.push(['endsAt', String(endsAt)]);
    return tags;
}

function responseOptionIds(ev: NostrEvent): string[] {
    return ev.tags.filter((t) => t[0] === 'response' && t[1]).map((t) => t[1]!);
}

export interface PollTally { counts: Record<string, number>; total: number; mine: string[] | null }

/** Tally kind:1018 responses for a poll: one vote per pubkey (latest created_at
 * wins), votes after `endsAt` and unknown option ids ignored. `total` is the
 * number of distinct voters; `mine` is `me`'s latest selection (or null). */
export function tallyResponses(responses: NostrEvent[], me: string | null, poll?: NostrEvent | null): PollTally {
    const endsAt = poll ? parseEndsAt(poll) : null;
    const valid = poll ? new Set(parsePollOptions(poll).map((o) => o.id)) : null;
    const single = poll ? parsePollType(poll) === 'single' : false;
    const latest = new Map<string, { ts: number; options: string[] }>();
    for (const r of responses) {
        if (endsAt !== null && r.created_at > endsAt) continue;
        // NIP-88: on a singlechoice poll only the FIRST response tag is the response (extras are
        // ignored, not counted); on multiplechoice the first tag per id counts (dupes collapse).
        // Without this, a crafted vote event inflates its option counts arbitrarily.
        const ids = responseOptionIds(r);
        const opts = (single ? ids.slice(0, 1) : [...new Set(ids)]).filter((id) => !valid || valid.has(id));
        if (opts.length === 0) continue;
        const prev = latest.get(r.pubkey);
        if (prev && prev.ts >= r.created_at) continue;
        latest.set(r.pubkey, { ts: r.created_at, options: opts });
    }
    const counts: Record<string, number> = {};
    for (const { options } of latest.values()) for (const o of options) counts[o] = (counts[o] ?? 0) + 1;
    const mine = me ? latest.get(me)?.options ?? null : null;
    return { counts, total: latest.size, mine };
}
