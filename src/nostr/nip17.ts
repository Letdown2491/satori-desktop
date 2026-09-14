// NIP-17 private direct messages (gift-wrapped, NIP-59 + NIP-44). The PURE pieces:
// build the unsigned kind-14 rumor, finalize the ephemeral kind-1059 gift wrap, parse
// the kind-10050 DM relay list, and validate an unwrapped rumor. The signer-bound
// layers (seal encrypt+sign, and the two decrypts on receive) live in data/dms.ts -
// because the SEAL is encrypted/signed with the USER's key, which here lives in the
// bunker, not in this process. The wrap layer uses a throwaway ephemeral key (local),
// so it stays pure crypto. Structure mirrors nostr-tools' nip59 exactly.

import { generateSecretKey, getEventHash, finalizeEvent } from 'nostr-tools/pure';
import { getConversationKey, encrypt as nip44encrypt } from 'nostr-tools/nip44';
import type { NostrEvent, UnsignedEvent } from './types.ts';
import { relayTags } from './tags.ts';

export const KIND_DM = 14;          // the chat message (rumor, never signed/published as-is)
export const KIND_SEAL = 13;        // sender-signed, NIP-44 to recipient
export const KIND_GIFTWRAP = 1059;  // ephemeral-signed, NIP-44 to recipient
export const KIND_DM_RELAYS = 10050; // NIP-17 inbox relay list

const TWO_DAYS = 2 * 24 * 60 * 60;
const now = () => Math.floor(Date.now() / 1000);
/** A timestamp randomized up to 2 days in the PAST (NIP-59) to blur when a wrap/seal
 * was really created - the true time lives in the kind-14 rumor inside. */
export const fuzzedTime = (): number => Math.round(now() - Math.random() * TWO_DAYS);

export const KIND_PRIVATE_REPLY = 1; // a NIP-59-wrapped kind:1 reply to a PUBLIC note (not a DM)

/** A decrypted gift-wrapped rumor (unsigned, but id-bearing). kind 14 = DM (NIP-17); kind 1 = a
 * private reply to a public note (same wrap machinery, inner kind differs); kind 7 = a private
 * reaction (later). Callers branch on `kind`. */
export interface Rumor {
    id: string;
    pubkey: string;     // the sender
    created_at: number; // the REAL send time (not fuzzed)
    kind: number;       // 14 (DM) | 1 (private reply) | 7 (private reaction)
    tags: string[][];
    content: string;
}

/** Build the unsigned kind-14 rumor (the message itself). `p` tags name the
 * recipients; an optional subject becomes the conversation title; replyTo links a
 * prior message. Id is the standard event hash; it is never signed. */
export function buildRumor(sender: string, recipients: string[], text: string, opts: { subject?: string; replyTo?: string } = {}): Rumor {
    const tags: string[][] = recipients.map((p) => ['p', p]);
    if (opts.subject) tags.push(['subject', opts.subject]);
    if (opts.replyTo) tags.push(['e', opts.replyTo]);
    const base = { pubkey: sender, created_at: now(), kind: KIND_DM, tags, content: text };
    return { id: getEventHash(base), ...base };
}

/** Wrap an already-built (sender-signed) kind-13 seal into a kind-1059 gift wrap,
 * NIP-44-encrypted to `recipient` under a fresh THROWAWAY key and signed by it - so
 * the wrap leaks no sender identity. Pure: the ephemeral key never leaves here. */
export function finalizeWrap(seal: NostrEvent, recipient: string): NostrEvent {
    const sk = generateSecretKey();
    const content = nip44encrypt(JSON.stringify(seal), getConversationKey(sk, recipient));
    return finalizeEvent({ kind: KIND_GIFTWRAP, created_at: fuzzedTime(), tags: [['p', recipient]], content }, sk) as NostrEvent;
}

/** The unsigned kind-13 seal TEMPLATE the signer must sign (its `content` is the
 * rumor already NIP-44-encrypted to the recipient by the caller, via the signer). */
export function sealTemplate(sender: string, encryptedRumor: string): UnsignedEvent {
    return { kind: KIND_SEAL, pubkey: sender, created_at: fuzzedTime(), tags: [], content: encryptedRumor };
}

/** Validate an unwrapped rumor: it must actually come from the seal's signer (the sender can't be
 * spoofed) - NIP-59's core integrity check. Returns the rumor with its REAL kind (14/1/7); the
 * caller branches (DM vs private reply vs reaction). Kind is NOT gated here so private replies aren't
 * silently dropped as "not a DM". The id is RE-DERIVED via getEventHash, never taken from the sender:
 * a rumor is unsigned, so a forged `id` would otherwise flow untrusted into id-keyed sinks (renders,
 * cache keys) - deriving it guarantees a canonical 64-hex id and the standard event-integrity binding. */
export function rumorFromSeal(rumor: unknown, sealPubkey: string): Rumor | null {
    if (!rumor || typeof rumor !== 'object') return null;
    const r = rumor as Partial<Rumor>;
    if (typeof r.kind !== 'number' || typeof r.content !== 'string' || typeof r.created_at !== 'number') return null;
    if (r.pubkey !== sealPubkey) return null; // sender spoofing guard
    const base = { pubkey: r.pubkey, created_at: r.created_at, kind: r.kind, tags: Array.isArray(r.tags) ? r.tags : [], content: r.content };
    return { id: getEventHash(base), ...base };
}

/** Build the unsigned kind:1 reply RUMOR for a private reply to a public note: a normal NIP-10
 * reply (e root/reply + p tags), gift-wrapped instead of published. `baseTags` is the public-reply
 * tag set (built the same way a public reply is), so the recipient renders it as an ordinary reply. */
export function buildPrivateReplyRumor(sender: string, baseTags: string[][], text: string): Rumor {
    const base = { pubkey: sender, created_at: now(), kind: KIND_PRIVATE_REPLY, tags: baseTags, content: text };
    return { id: getEventHash(base), ...base };
}

/** The recipients a rumor was addressed to (its `p` tags). */
export const rumorRecipients = (r: Rumor): string[] => r.tags.filter((t) => t[0] === 'p' && t[1]).map((t) => t[1]!);

/** Parse a kind-10050 DM-relay-list event into inbox relay urls (`relay` tags). */
export function parseDmRelays(ev: NostrEvent): string[] {
    return relayTags(ev);
}
