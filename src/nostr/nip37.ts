// NIP-37 draft events: a kind-agnostic, ENCRYPTED wrapper for an unsigned draft event so
// drafts can sync across your own devices privately. The wrap is kind:31234; its `.content`
// is nip44Encrypt-to-self(JSON.stringify(innerDraftEvent)); the `k` tag records the wrapped
// kind and `d` is the draft identifier (shared with the LOCAL draft id so local + synced
// dedupe). The user-key crypto (encrypt/decrypt) is routed through the Signer (bunker, server
// side) or the nip07 batch chain - this module stays PURE (template build/parse only), like
// nip17.ts. Deletion (NIP-37) = re-publish the wrap with an empty `.content`.

import type { NostrEvent, UnsignedEvent } from './types.ts';
import { relayTags } from './tags.ts';

export const KIND_DRAFT = 31234;          // the encrypted draft wrap (parameterized replaceable)
export const KIND_DRAFT_RELAYS = 10013;   // NIP-51 "draft relays" list (where to publish/read wraps)

/** The unsigned kind:31234 wrap, given already-encrypted content. `kind` = the wrapped draft's
 * kind (the `k` tag). Pass an empty `encrypted` (and a fresh createdAt) to DELETE the draft. */
export function draftWrapTemplate(me: string, identifier: string, kind: number, encrypted: string, createdAt?: number): UnsignedEvent {
    const created = createdAt ?? Math.floor(Date.now() / 1000);
    // NIP-37 recommends a NIP-40 expiration so relays can auto-purge stale synced drafts (the LOCAL
    // draft is unaffected - it lives on disk regardless). 90 days, refreshed on every save.
    const expiration = String(created + 90 * 24 * 60 * 60);
    return {
        kind: KIND_DRAFT,
        created_at: created,
        tags: [['d', identifier], ['k', String(kind)], ['expiration', expiration]],
        content: encrypted,
        pubkey: me,
    };
}

/** The plaintext to encrypt: the inner draft event template, JSON-serialized. */
export const serializeDraft = (inner: UnsignedEvent): string => JSON.stringify(inner);

/** Parse a decrypted wrap's content back into the inner unsigned draft event (or null). */
export function parseDraft(decrypted: string): UnsignedEvent | null {
    try {
        const ev = JSON.parse(decrypted) as Partial<UnsignedEvent>;
        if (typeof ev?.kind !== 'number' || typeof ev.content !== 'string') return null;
        // Require tags to be an array OF arrays: downstream eventToDraft() indexes t[0]/t[1]/t[2]
        // on every element, so a null/non-array element would throw. (Content is to-self ciphertext,
        // i.e. our own data, but stay crash-proof against a corrupted wrap.)
        if (!Array.isArray(ev.tags) || !ev.tags.every((t) => Array.isArray(t))) return null;
        const created_at = typeof ev.created_at === 'number' ? ev.created_at : 0; // never let a string through (→ NaN savedAt)
        return { kind: ev.kind, created_at, tags: ev.tags as string[][], content: ev.content, pubkey: ev.pubkey ?? '' };
    } catch { return null; }
}

/** Read a wrap's identifier / wrapped-kind / deleted-state WITHOUT decrypting (cheap listing). */
export const draftId = (ev: NostrEvent): string => ev.tags.find((t) => t[0] === 'd')?.[1] ?? '';
export const isDeletedDraft = (ev: NostrEvent): boolean => ev.content.trim() === '';

/** Parse a kind:10013 draft-relay list into relay urls, capped. Per NIP-37 the list is
 * PRIVATE: `.content` = nip44-to-self of JSON.stringify([['relay', url], ...]) with empty tags
 * (your draft-relay set shouldn't itself be public). Pass the decrypted content when available;
 * plaintext `relay` tags are kept as a lenient fallback for non-spec publishers. */
export function parseDraftRelays(ev: NostrEvent, decrypted?: string | null): string[] {
    if (decrypted) {
        try {
            const arr = JSON.parse(decrypted) as unknown;
            if (Array.isArray(arr)) {
                const urls = [...new Set(arr
                    .filter((t): t is [string, string] => Array.isArray(t) && t[0] === 'relay' && typeof t[1] === 'string' && !!t[1])
                    .map((t) => t[1]))].slice(0, 8); // same cap as relayTags
                if (urls.length) return urls;
            }
        } catch { /* malformed plaintext → fall through to tags */ }
    }
    return relayTags(ev);
}
