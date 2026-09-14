// Stateful list actions (NIP-02 follow, NIP-51 mute/bookmark/pin) + the keystone
// signed-action runner. Every action is the same shape: toggle one tag in a
// replaceable list event, then sign+publish. The runner branches on signing mode:
//   bunker - the server signs via the bunker and publishes now.
//   nip07  - the server returns H-Nostr-Sign + the unsigned template; the browser
//            extension signs it and POSTs it to the continuation, which publishes.
// Either way the user's key never reaches this server.
//
// mute (10000) + bookmark (10003) are PRIVATE-by-default (NIP-44-encrypted to self
// in `content`), like Satori; follow (3) + pin (10001) stay public. Reads MERGE
// public tags + decrypted private tags. Writing private is BUNKER-only for now (the
// server decrypts/encrypts via the bunker); nip07 still writes public (its key isn't
// on this server - the chained nip44 round-trips are a follow-up). A write NEVER
// overwrites unreadable private content (no data loss).

import { decode } from 'nostr-tools/nip19';
import { INDEXER_RELAYS, writeRelaysFor } from './nostr/nip65.ts';
import { normalizeRelayUrl } from './data/relay-favorites.ts';
import { cachedEvent } from './data/feeds.ts';
import { recordDeletion } from './data/deletions.ts';
import { seenRelaysFor } from './data/seen-relays.ts';
import { HEX64 } from './nostr/tags.ts';
import type { NostrEvent, UnsignedEvent } from './nostr/types.ts';
import type { Session } from './session.ts';
import { signsOnServer } from './session.ts';

export type ActionName = 'follow' | 'mute' | 'bookmark' | 'pin';

interface ActionDef { kind: number; tag: 'p' | 'e' }
const DEFS: Record<ActionName, ActionDef> = {
    follow: { kind: 3, tag: 'p' },      // NIP-02 contact list
    mute: { kind: 10000, tag: 'p' },    // NIP-51 mute list
    bookmark: { kind: 10003, tag: 'e' },// NIP-51 bookmarks
    pin: { kind: 10001, tag: 'e' },     // NIP-51 pins
};

export function isActionName(x: string): x is ActionName {
    return x === 'follow' || x === 'mute' || x === 'bookmark' || x === 'pin';
}
export function actionKind(name: ActionName): number { return DEFS[name].kind; }

/** Lists kept private-by-default (NIP-44-encrypted), like Satori. */
export const PRIVATE_KINDS = new Set([DEFS.mute.kind, DEFS.bookmark.kind]);
export function isPrivateList(name: ActionName): boolean { return PRIVATE_KINDS.has(DEFS[name].kind); }

/** Decrypt + cache a private list's NIP-44 tags. Idempotent. Bunker decrypts via
 * the signer; nip07 can't (no key here) so it's left unattempted → reads public
 * only. `null` = present-but-unreadable (so a write won't clobber it). */
export async function ensurePrivate(s: Session, kind: number): Promise<void> {
    if (s.privateTags.has(kind)) return;
    const ev = s.lists.get(kind);
    if (!ev || !ev.content) { s.privateTags.set(kind, []); return; } // nothing private
    if (!signsOnServer(s)) return; // client-signs (e.g. nip07): no server key, needs a decrypt round-trip
    try {
        const json = JSON.parse(await s.signer.nip44Decrypt(s.me!, ev.content));
        s.privateTags.set(kind, Array.isArray(json) ? json as string[][] : []);
    } catch {
        s.privateTags.set(kind, null); // unreadable - never overwrite on write
    }
}

/** Public tags + decrypted private tags for a list kind (private merged when read). */
export function listTags(s: Session, kind: number): string[][] {
    const pub = s.lists.get(kind)?.tags ?? [];
    const priv = s.privateTags.get(kind);
    return Array.isArray(priv) ? [...pub, ...priv] : [...pub];
}

/** True when the list's private content was loaded and decrypted (or there was
 * none). False = present but couldn't be read (e.g. nip07, or a decrypt failure). */
export function privateReadable(s: Session, kind: number): boolean {
    const ev = s.lists.get(kind);
    if (!ev || !ev.content) return true; // nothing to read
    return Array.isArray(s.privateTags.get(kind));
}

/** How many items are bookmarked (NIP-51 kind:10003 `e`/`a` tags - public + decrypted private).
 * Counts tags, not resolved events, so it's cheap + matches the /bookmarks header chip live. */
export function bookmarkCount(s: Session): number {
    return listTags(s, DEFS.bookmark.kind).filter((t) => (t[0] === 'e' || t[0] === 'a') && !!t[1]).length;
}

/** Muted pubkeys (NIP-51 kind:10000 `p` tags - public + decrypted private). */
export function mutedPubkeys(s: Session): Set<string> {
    const set = new Set<string>();
    for (const t of listTags(s, DEFS.mute.kind)) if (t[0] === 'p' && t[1]) set.add(t[1]);
    return set;
}

/** Resolve a URL target token to the (tag, value) it toggles. follow/mute target
 * a pubkey (`p`). bookmark/pin target a note (`e`, hex id) or - for an naddr - an
 * article by address (`a`, `kind:pubkey:d`), matching Satori's article actions.
 * The shared HEX64 is case-INSENSITIVE, so canonicalize a hex `p`/`e` target to
 * lowercase here - the value is stored as a tag, and a mixed-case dupe must not
 * sit alongside the lowercase one (toggle-off would then miss it). */
export function resolveTarget(name: ActionName, target: string): { tag: string; value: string } {
    if (name === 'follow' || name === 'mute') return { tag: 'p', value: HEX64.test(target) ? target.toLowerCase() : target };
    if (target.startsWith('naddr1')) {
        try {
            const d = decode(target);
            if (d.type === 'naddr') return { tag: 'a', value: `${d.data.kind}:${d.data.pubkey}:${d.data.identifier}` };
        } catch { /* fall through */ }
    }
    return { tag: 'e', value: HEX64.test(target) ? target.toLowerCase() : target };
}

/** The tag to ADD when toggling something ON, enriched per NIP-51 so it can be found again. For a note
 * bookmark/pin (`e`) we record the author (t[3]) and a relay hint (t[2]) - `["e", id, relay, author]` -
 * so resolveListItems can route to the author's write relays via outbox even after the note leaves your
 * own relays. Bare `[tag, value]` for follows/mutes (`p`) and articles (`a`, whose author is in the
 * coordinate already), or when the note isn't in the event cache (best-effort: no worse than before).
 * Single source for every write path (public, bunker-private, nip07-private chain). */
export function addTag(name: ActionName, target: string): string[] {
    const { tag, value } = resolveTarget(name, target);
    if (tag !== 'e') return [tag, value];
    const author = cachedEvent(value)?.pubkey;
    if (!author) return [tag, value];              // note not cached → bare, can't invent an author
    return [tag, value, seenRelaysFor(author)[0] ?? '', author];
}

/** Validate a URL target for an action (pubkey hex, note id hex, or article naddr). */
export function isValidTarget(name: ActionName, target: string): boolean {
    if (name === 'follow' || name === 'mute') return HEX64.test(target);
    if (HEX64.test(target)) return true;
    if (target.startsWith('naddr1')) { try { return decode(target).type === 'naddr'; } catch { return false; } }
    return false;
}

export function writeRelays(s: Session): string[] {
    return writeRelaysFor(s.myRelays);
}

/** The relays to publish a top-level post to, from the compose relay-picker: the checked write relays (the
 * `relay` fields) plus an optional validated one-off (`customrelay`). Anti-tamper: checked relays must be in
 * your write set. Empty selection → ALL write relays (never publish to nowhere). Replies/quotes send no relay
 * fields, so they fall through to the full write set here. `p` is the form (bunker) or the query (nip07).
 * Shared by the note/picture/poll/article composers. */
export function chosenTargets(p: { getAll(n: string): string[]; get(n: string): string | null }, s: Session & { me: string }): string[] {
    const all = writeRelaysFor(s.myRelays);
    const allowed = new Set(all);
    const picked = p.getAll('relay').filter((u) => allowed.has(u));
    const custom = normalizeRelayUrl(p.get('customrelay') ?? '');
    const targets = [...new Set([...picked, ...(custom ? [custom] : [])])];
    return targets.length ? targets : all;
}

/** Serialize the relay-picker selection (the `relay[]` + `customrelay` form fields) onto a nip07 publish-
 * continuation URL - the write-side mirror of chosenTargets, so the field names stay in lockstep. The
 * continuation re-validates centrally via chosenTargets on the query; this only carries the raw selection.
 * Shared by the note/picture/poll/article composers. */
export function appendRelayTargets(q: URLSearchParams, p: { getAll(n: string): string[]; get(n: string): string | null }): void {
    for (const u of p.getAll('relay')) q.append('relay', u);
    const cr = (p.get('customrelay') ?? '').trim();
    if (cr) q.set('customrelay', cr);
}

/** Parse the compose `schedule` field (a datetime-local string) into a future unix-seconds timestamp, or a
 * reason it's unusable. `empty` = missing/unparseable; `past` = not in the future. Each caller renders the
 * error its own way (back() vs a 400 fragment), so this shares only the parse+validate. */
export type ScheduleParse = { at: number } | { error: 'empty' | 'past' };
export function parseScheduleAt(raw: string): ScheduleParse {
    const t = new Date(raw.trim()).getTime();
    if (isNaN(t)) return { error: 'empty' };
    const at = Math.floor(t / 1000);
    if (at <= Math.floor(Date.now() / 1000)) return { error: 'past' };
    return { at };
}

/** Publish a signed event to `relays` (default: your write relays) and report whether at least one
 * relay accepted it - the accepted-check every sign/publish path otherwise repeats inline, in both
 * the server-signs branch and the client-signs continuation. (allSettled never rejects, so this
 * never throws on relay failure: the caller decides whether to throw or render an error.) */
export async function published(s: Session, signed: NostrEvent, relays?: string[]): Promise<boolean> {
    const results = await s.pool.publish(relays ?? writeRelays(s), signed);
    const ok = results.some((r) => r.status === 'fulfilled');
    // NIP-09: your own accepted deletion (a delete, an unlike) takes effect locally at once -
    // the tombstone suppresses the target everywhere without waiting for relays to echo it
    // back. PINNED: your own tombstones must survive any amount of relay-fed tombstone churn.
    if (ok && signed.kind === 5) recordDeletion(signed, true);
    return ok;
}

// Per-(session,kind) in-flight guard so two concurrent ensureList calls share one query
// (instead of both querying and both writing s.lists after the await).
const listInflight = new Map<string, Promise<NostrEvent | null>>();

// Truncated-empty reads leave the list UNKNOWN (never cached in s.lists). This short-TTL
// sentinel keeps renders from re-paying the full complete-read fan-out on every page for a
// user whose relay set chronically truncates - reads within the TTL just render empty, and
// listKnown stays false so writes stay refused. A toggle passes retry:true to bypass it (the
// user explicitly asked; one fresh attempt is worth the wait).
const listUnknownAt = new Map<string, number>();
const LIST_UNKNOWN_TTL = 60_000;

/** Load (and cache) the user's list event of a kind.
 * CONTRACT for read-modify-write callers: a resolved null can mean confirmed-absent OR
 * unknown - check listKnown() before building any event from the result, or a failed read
 * becomes a published one-entry list over the real one (the mute-list clobber class). */
export async function ensureList(s: Session, kind: number, opts: { retry?: boolean } = {}): Promise<NostrEvent | null> {
    if (s.lists.has(kind)) return s.lists.get(kind) ?? null;
    const key = `${s.id}:${kind}`;
    const unknownAt = listUnknownAt.get(key);
    if (!opts.retry && unknownAt !== undefined && Date.now() - unknownAt < LIST_UNKNOWN_TTL) return null;
    const inf = listInflight.get(key);
    if (inf) return inf;
    const p = (async (): Promise<NostrEvent | null> => {
        const relays = [...new Set([...(s.myRelays?.read ?? []), ...(s.myRelays?.write ?? []), ...INDEXER_RELAYS])];
        const track = { truncated: false };
        const events = await s.pool.query(relays, { kinds: [kind], authors: [s.me!] }, { complete: true, track }).catch(() => [] as NostrEvent[]);
        const newest = events.sort((a, b) => b.created_at - a.created_at)[0] ?? null;
        // Don't clobber a value that arrived while we queried (e.g. a toggle publish set a
        // newer event, or the relay copy is older/absent) - the publish is the source of truth.
        const cur = s.lists.get(kind);
        if (cur && (!newest || cur.created_at >= newest.created_at)) return cur;
        // A truncated-EMPTY read is UNKNOWN, not absent: the list may exist on a relay that
        // didn't answer (or refused - the tracked read counts a denial as truncation). Cache
        // only the short-TTL sentinel, so a toggle can't rebuild "from nothing" and wipe the
        // real list. A confirmed-absent read (every relay cleanly EOSE'd with no event) still
        // caches null, so a first toggle can create the list.
        if (!newest && track.truncated) { listUnknownAt.set(key, Date.now()); return null; }
        listUnknownAt.delete(key);
        s.lists.set(kind, newest);
        return newest;
    })();
    listInflight.set(key, p);
    try { return await p; } finally { listInflight.delete(key); }
}

/** True when a list's current state is KNOWN after ensureList: loaded, or confirmed absent
 * by a complete read. False = the read timed out with nothing, so the real list may exist
 * on a relay that didn't answer - refuse read-modify-write until a re-read succeeds. */
export function listKnown(s: Session, kind: number): boolean {
    return s.lists.has(kind);
}

/** Ensure several lists are loaded (before rendering buttons that read them), and
 * decrypt the private ones (mute/bookmark) so reads merge public + private. */
export async function ensureLists(s: Session, names: ActionName[]): Promise<void> {
    const kinds = [...new Set(names.map((n) => DEFS[n].kind))];
    await Promise.all(kinds.map((k) => ensureList(s, k)));
    await Promise.all(kinds.filter((k) => PRIVATE_KINDS.has(k)).map((k) => ensurePrivate(s, k)));
}

/** Is `target` currently in the list for `name` (public + decrypted private)? */
export function isOn(s: Session, name: ActionName, target: string): boolean {
    const { tag, value } = resolveTarget(name, target);
    return listTags(s, actionKind(name)).some((t) => t[0] === tag && t[1] === value);
}

/** Build the toggled list event: drop the target tag, add it back iff `on`.
 * Other tags + the content blob are preserved (no data loss). `prev` MUST come from an
 * ensureList whose listKnown() is true - building from an UNKNOWN read publishes a one-entry
 * list over the real one (see the ensureList contract). */
export function buildToggle(name: ActionName, prev: NostrEvent | null, target: string, on: boolean, me: string): UnsignedEvent {
    const { tag, value } = resolveTarget(name, target);
    const tags = (prev?.tags ?? []).filter((t) => !(t[0] === tag && t[1] === value));
    if (on) tags.push(addTag(name, target));
    return { kind: actionKind(name), created_at: Math.floor(Date.now() / 1000), tags, content: prev?.content ?? '', pubkey: me };
}

/** Record a freshly-published list event as the new source of truth. */
export function applyPublished(s: Session, signed: NostrEvent): void {
    s.lists.set(signed.kind, signed);
}

/** Build the PRIVATE-toggled list event (bunker): decrypt current private tags,
 * toggle the target in the PRIVATE set, re-encrypt to `content`, keep the public
 * tags untouched. Throws if the existing private content is unreadable (so we never
 * overwrite - i.e. lose - private items we couldn't read). Bunker only. */
export async function buildPrivateToggle(s: Session, name: ActionName, target: string, on: boolean): Promise<UnsignedEvent> {
    const kind = actionKind(name);
    await ensurePrivate(s, kind);
    const priv = s.privateTags.get(kind);
    if (priv === null) throw new Error('couldn’t read your private list, so it wasn’t overwritten');
    const { tag, value } = resolveTarget(name, target);
    const next = (priv ?? []).filter((t) => !(t[0] === tag && t[1] === value));
    if (on) next.push(addTag(name, target));
    const content = await s.signer!.nip44Encrypt(s.me!, JSON.stringify(next));
    // Drop the target from PUBLIC tags too, so toggling off an item saved publicly
    // (by an older client / our old public toggle) actually removes it; new items
    // live only in the private set.
    const publicTags = (s.lists.get(kind)?.tags ?? []).filter((t) => !(t[0] === tag && t[1] === value));
    return { kind, created_at: Math.floor(Date.now() / 1000), tags: publicTags, content, pubkey: s.me! };
}

/** Record a published private toggle: update the list event + the private-tag cache. */
export function applyPrivatePublished(s: Session, signed: NostrEvent, name: ActionName, target: string, on: boolean): void {
    s.lists.set(signed.kind, signed);
    const { tag, value } = resolveTarget(name, target);
    const next = (s.privateTags.get(signed.kind) ?? []).filter((t) => !(t[0] === tag && t[1] === value));
    if (on) next.push(addTag(name, target));
    s.privateTags.set(signed.kind, next);
}
