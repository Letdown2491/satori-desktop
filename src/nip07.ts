// Server-side NIP-07 support: the bits that back the nip07-hateoas wire contract.
// Login is the same "sign-and-resubmit" primitive as any write - the server hands
// out a challenge auth event, the browser extension signs it, and the server
// verifies the signature over its own single-use nonce to learn (and trust) the
// pubkey. The private key never reaches this process.

import { randomBytes } from 'node:crypto';
import type { IncomingMessage } from 'node:http';
import { verifyEvent } from 'nostr-tools/pure';
import { readJson, sendFragment, type Ctx } from './http.ts';
import { html } from './html.ts';
import { nowSec } from './nostr/tags.ts';
import type { NostrEvent } from './nostr/types.ts';

const now = nowSec;

// --- single-use, time-bound login challenges --------------------------------

const CHALLENGE_TTL_MS = 5 * 60_000;
const challenges = new Map<string, number>(); // nonce → expiry (ms epoch)

export function issueChallenge(): string {
    // Sweep expired nonces first: consumeChallenge only removes nonces that are actually used,
    // so abandoned login attempts (cancelled / closed tab) would otherwise accumulate forever.
    const now = Date.now();
    for (const [n, exp] of challenges) if (exp <= now) challenges.delete(n);
    const nonce = randomBytes(24).toString('base64url');
    challenges.set(nonce, now + CHALLENGE_TTL_MS);
    return nonce;
}

/** Consume a nonce: true once, then it's gone (and expired ones are rejected). */
function consumeChallenge(nonce: string): boolean {
    const expiry = challenges.get(nonce);
    if (expiry === undefined) return false;
    challenges.delete(nonce);
    return expiry > Date.now();
}

/** The unsigned challenge event (NIP-98-style kind 27235). pubkey is omitted -
 * the extension fills it when signing, and we read it back from the signed event. */
export function buildChallengeEvent(nonce: string, url: string): Record<string, unknown> {
    return {
        kind: 27235,
        created_at: now(),
        tags: [['u', url], ['method', 'POST'], ['challenge', nonce]],
        content: '',
    };
}

// --- signed-event validation -----------------------------------------------

/** Coerce arbitrary JSON into a NostrEvent only if it's structurally complete. */
export function coerceEvent(x: unknown): NostrEvent | null {
    if (!x || typeof x !== 'object') return null;
    const e = x as Record<string, unknown>;
    if (typeof e.id !== 'string' || typeof e.pubkey !== 'string' || typeof e.sig !== 'string') return null;
    if (typeof e.kind !== 'number' || typeof e.created_at !== 'number' || typeof e.content !== 'string') return null;
    if (!Array.isArray(e.tags)) return null;
    return e as unknown as NostrEvent;
}

/** Verify a signed event: structurally valid + a real signature (id hash + sig). */
export function verifySigned(x: unknown): NostrEvent | null {
    const ev = coerceEvent(x);
    if (!ev) return null;
    try { return verifyEvent(ev as never) ? ev : null; } catch { return null; }
}

/** Read a request body as JSON and verify it's a real signed event (or null) - the
 * standard first step of every nip07 sign-and-resubmit continuation. */
export async function readSignedEvent(req: IncomingMessage): Promise<NostrEvent | null> {
    return verifySigned(await readJson(req).catch(() => null));
}

/** The standard nip07 continuation gate: read the resubmitted body, verify it's a real signed event
 * from `me` of the expected `kind`, or render the bespoke error fragment + return null. Collapses the
 * `readSignedEvent(...) + pubkey/kind check + error fragment` triple every simple continuation repeats. */
export async function requireSigned(ctx: Ctx, me: string, kind: number, label: string, headers: Record<string, string> = {}, status = 400): Promise<NostrEvent | null> {
    const signed = await readSignedEvent(ctx.req);
    if (!signed || signed.pubkey !== me || signed.kind !== kind) { sendFragment(ctx, html`<div class="notice error">Couldn't verify ${label}.</div>`, headers, status); return null; }
    return signed as NostrEvent;
}

/** Read the `{ result }` a nip44_encrypt/decrypt continuation POSTs back (the
 * cipher/plaintext string), or undefined. */
export async function readSignResult(req: IncomingMessage): Promise<unknown> {
    const body = await readJson(req).catch(() => null);
    return body && typeof body === 'object' ? (body as { result?: unknown }).result : undefined;
}

/** Verify a signed login challenge: real signature + a live, single-use nonce.
 * Returns the proven pubkey, or null. */
export function verifyChallenge(x: unknown): string | null {
    const ev = verifySigned(x);
    if (!ev || ev.kind !== 27235) return null;
    const nonce = ev.tags.find((t) => t[0] === 'challenge')?.[1];
    if (!nonce || !consumeChallenge(nonce)) return null;
    // Guard against a stale-but-valid signature being replayed: the auth event
    // must be freshly minted (created_at within the challenge window).
    if (Math.abs(now() - ev.created_at) > 5 * 60) return null;
    return ev.pubkey;
}
