// Access-control policy: WHO may hold a session on this instance. The single dial for self-host
// (just you) vs trusted group (an allowlist) vs public / multi-tenant (open). Gated ONLY at the
// login + resume boundary; everything downstream stays per-session (s.me), so this never couples
// app logic to one user. The model is a SET + an open flag - single-user is "a set of one", not a
// special case, and a public instance is just `open` (which short-circuits the owner claim). So
// "going multi-tenant" is flipping the policy, not a refactor. See [[multi-tenant-readiness]].
//
// Policy, highest precedence first:
//   SATORI_OPEN=1                          → open: anyone may sign in (a public instance)
//   SATORI_OWNER / SATORI_ALLOWED_PUBKEYS  → restricted to those npubs/hex (explicit; for exposed
//                                            instances this is the recommended, race-free way)
//   else                                   → restricted + trust-on-first-login: the first sign-in
//                                            claims this instance (persisted to .data/owner.json);
//                                            an existing logged-in user is adopted at boot.

import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { pubkeyFromBech } from './nostr/nip19.ts';

const OWNER_FILE = process.env.SATORI_OWNER_FILE || join(process.cwd(), '.data', 'owner.json');

/** npub / nprofile / 64-hex → hex pubkey, or null. */
const toHex = (s: string): string | null => pubkeyFromBech(s.trim());
const parseList = (env: string | undefined): string[] => (env ?? '').split(/[\s,]+/).map(toHex).filter((x): x is string => !!x);

const OPEN = /^(1|true|yes|on)$/i.test(process.env.SATORI_OPEN ?? '');
const envOwners = new Set<string>(parseList(process.env.SATORI_OWNER)); // the OWNER(s), narrower than the allowed set
const envAllowed = new Set<string>([...envOwners, ...parseList(process.env.SATORI_ALLOWED_PUBKEYS)]);

let claimed: string | null = (() => {
    try { const j = JSON.parse(readFileSync(OWNER_FILE, 'utf8')) as { owner?: string }; return typeof j.owner === 'string' ? j.owner : null; }
    catch { return null; }
})();

function claim(pubkey: string): void {
    claimed = pubkey;
    try { mkdirSync(dirname(OWNER_FILE), { recursive: true }); writeFileSync(OWNER_FILE, JSON.stringify({ owner: pubkey, at: Date.now() }), { mode: 0o600 }); }
    catch (e) { console.warn('[access] could not persist owner claim:', (e as Error)?.message ?? e); }
}

/** The allowed pubkey set (env ∪ claimed), or null when the instance is open. */
function allowedSet(): Set<string> | null {
    if (OPEN) return null;
    const set = new Set(envAllowed);
    if (claimed) set.add(claimed);
    return set;
}

/** May this pubkey hold a session? Used at LOGIN: on a restricted-but-UNCLAIMED instance the first
 * caller claims ownership (trust-on-first-login) and is allowed; otherwise it must be in the set. */
export function accessAllows(pubkey: string): boolean {
    const set = allowedSet();
    if (!set) return true;                              // open
    if (set.size === 0) { claim(pubkey); return true; } // unclaimed → first sign-in claims it
    return set.has(pubkey);
}

/** Membership only (never claims). Used at session RESUME, so a tightened policy evicts old
 * sessions but a resume can't claim ownership. */
export function accessHas(pubkey: string): boolean {
    const set = allowedSet();
    return !set || set.has(pubkey);
}

/** True only when this instance serves exactly ONE pubkey (an owner lock or a one-entry allowlist).
 * Gates SINGLE-USER-only features whose PROCESS-GLOBAL config would otherwise let one account affect
 * another - the private relay routes/redirects everyone's reads and writes, so it's active only here.
 * Open, unclaimed, or a >1 allowlist all return false. */
export function isSingleUser(): boolean {
    const set = allowedSet();
    return !!set && set.size === 1;
}

/** Boot migration: if the instance is restricted-but-unclaimed and a user is already logged in,
 * adopt the most-recent as owner - so adding the owner lock to a running self-host deploy doesn't
 * log them out. No-op when open, env-configured, already claimed, or no existing sessions. */
export function adoptOwnerIfUnclaimed(existingPubkeys: string[]): void {
    if (OPEN || envAllowed.size > 0 || claimed || existingPubkeys.length === 0) return;
    claim(existingPubkeys[0]!);
    console.log(`[access] adopted existing session ${existingPubkeys[0]!.slice(0, 12)}… as owner`);
}

/** Is this pubkey the instance OWNER (SATORI_OWNER env, or the trust-on-first-login claimant)? NARROWER than
 * the ALLOWED_PUBKEYS trusted group - gates owner-only surfaces like GET /metrics (process-wide volume
 * counters that a group member shouldn't read). On a pure ALLOWED_PUBKEYS group with no SATORI_OWNER and no
 * claim there is no owner, so those surfaces close to everyone until SATORI_OWNER is set. Default self-host
 * (trust-on-first-login) has a claimant, so the single owner passes. */
export function isOwner(pubkey: string): boolean {
    return envOwners.has(pubkey) || claimed === pubkey;
}

/** One-line description for boot logging. */
export function accessMode(): string {
    if (OPEN) return 'open (anyone may sign in - public instance)';
    const n = envAllowed.size + (claimed ? 1 : 0);
    if (n === 0) return 'restricted, UNCLAIMED (the first sign-in claims this instance; set SATORI_OWNER or sign in locally before exposing it)';
    return `restricted to ${n} pubkey${n === 1 ? '' : 's'}`;
}
