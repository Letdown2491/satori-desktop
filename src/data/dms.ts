// NIP-17 DM engine (server-side, BUNKER-ONLY in v1). The daemon holds the bunker
// connection, so it decrypts in-process - the fast path (vs nip07's browser round-trips,
// not built). Decrypt-once-then-persist to a 0600 disk cache; render from cache with zero
// signer calls steady-state. Triage on the FIRST decrypt (seal.pubkey = sender): muted →
// drop at 1 decrypt; you/follow → Inbox (2nd decrypt); stranger → Requests (defer the body).
// Incoming AND your own sent messages both arrive as kind-1059 wraps `#p=you` (you wrap to
// the peer AND to yourself), so one query covers both directions. See [[nip17-dms-plan]].

import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { debouncedFlush } from './json-store.ts';
import { INDEXER_RELAYS } from '../nostr/nip65.ts';
import { mutedPubkeys } from '../actions.ts';
import { dmUnread } from './dm-read.ts';
import { myDmReadRelays, publishWrapPair } from './dm-routing.ts';
import { recordScan } from './dm-metrics.ts';
import {
    buildRumor, buildPrivateReplyRumor, finalizeWrap, sealTemplate, rumorFromSeal, rumorRecipients,
    KIND_GIFTWRAP, KIND_DM, KIND_DM_RELAYS, KIND_PRIVATE_REPLY, type Rumor,
} from '../nostr/nip17.ts';
import { replyParent } from '../nostr/nip10.ts';
import type { NostrEvent } from '../nostr/types.ts';
import type { Filter } from 'nostr-tools';
import { verifyEvent } from 'nostr-tools/pure';
import type { Session } from '../session.ts';
import { signsOnServer } from '../session.ts';

type Signed = Session & { me: string; signer: NonNullable<Session['signer']> };

export type Bucket = 'inbox' | 'request';
export interface DmMessage { id: string; from: string; at: number; text: string; legacy?: boolean }
// `secure` = the conversation has NIP-17 (gift-wrapped) messages -> shows a shield. `legacy`
// = it also has NIP-04 (kept for internal logic; no longer surfaced as a "less private" tag).
export interface Conversation { peer: string; lastAt: number; preview: string; bucket: Bucket; legacy?: boolean; secure?: boolean; unread?: boolean }

const KIND_LEGACY = 4; // NIP-04 (read-only; we never SEND this metadata-leaky format)

// --- disk cache (decrypt once, ever) --------------------------------------
// Keyed by wrap id. Plaintext at rest on the daemon (the user's own machine); 0600 +
// cleared on logout (see clearDmCache). Entries: a decrypted message, a deferred request
// (peer known from seal.pubkey, body not yet decrypted), or junk we never reprocess.
// `owner` = the account (me) that decrypted this entry. The cache is process-global and
// keyed by wrap id, so without it a second identity (account switch without logout) would
// read account A's decrypted DMs. Every cache READ that iterates the whole map MUST filter
// by owner. `drop`/junk needs no owner (it carries no plaintext). Entries persisted before
// this field existed have no owner and are treated as belonging to nobody (re-decrypted).
type Entry =
    | { kind: 'msg'; owner: string; peer: string; from: string; at: number; text: string; legacy?: boolean }
    | { kind: 'request'; owner: string; peer: string; at: number }
    // A NIP-59-wrapped PRIVATE REPLY (inner kind:1) to a public note - NOT a peer conversation, so
    // it's keyed by the parent note id, carries the rumor id + tags (to render as a note in-thread),
    // and is EXCLUDED from the conversation-list aggregation (it has no `peer`).
    | { kind: 'reply'; owner: string; parent: string; id: string; from: string; at: number; text: string; tags: string[][] }
    | { kind: 'drop' };

/** A decrypted private reply, shaped to render as a synthetic note event in a thread. Shared by both
 * DM engines (nip07 imports it from here) and the routes/render that fold private replies into threads. */
export interface PrivateReply { id: string; parent: string; from: string; at: number; content: string; tags: string[][] }

/** A decrypted private reply as a synthetic kind:1 event (sig empty - never published) so it threads via
 * its NIP-10 tags and renders exactly like a public reply. The lock badge marks it private. One shared
 * builder for the thread render, the notifications row, and the optimistic-send append. */
export function syntheticReply(r: PrivateReply): NostrEvent {
    return { id: r.id, pubkey: r.from, created_at: r.at, kind: 1, tags: r.tags, content: r.content, sig: '' };
}

const FILE = process.env.SATORI_DM_CACHE || join(process.cwd(), '.data', 'dms.json');
const cache = new Map<string, Entry>();

(function load(): void {
    try {
        const raw = JSON.parse(readFileSync(FILE, 'utf8')) as Record<string, Entry>;
        // Skip plaintext entries that predate the `owner` field - they'd be unattributable
        // (and so invisible to the owner-filtered reads); re-decrypted on the next refresh.
        for (const [id, e] of Object.entries(raw)) {
            if (!e?.kind) continue;
            if (e.kind !== 'drop' && !e.owner) continue;
            cache.set(id, e);
        }
    } catch { /* none yet */ }
})();

const flusher = debouncedFlush(() => {
    try { mkdirSync(dirname(FILE), { recursive: true }); writeFileSync(FILE, JSON.stringify(Object.fromEntries(cache)), { mode: 0o600 }); }
    catch (e) { console.warn('[dms] cache flush failed:', (e as Error)?.message ?? e); }
}, 5000);
const scheduleFlush = (): void => flusher.schedule();

/** Wipe the plaintext DM cache - called on logout so a different account never reads it. With `me`, wipes
 * ONLY that account's entries (other signed-in users keep their cache); without, wipes everything. The disk
 * file is rewritten synchronously so the departing account's plaintext doesn't linger at rest. */
export function clearDmCache(me?: string): void {
    if (me) {
        for (const [id, e] of cache) if ('owner' in e && e.owner === me) cache.delete(id);
        listCache.delete(me);
    } else {
        cache.clear();
        listCache.clear();
    }
    try { mkdirSync(dirname(FILE), { recursive: true }); writeFileSync(FILE, me ? JSON.stringify(Object.fromEntries(cache)) : '{}', { mode: 0o600 }); } catch { /* ignore */ }
}

// --- decrypt layers (through the bunker) ----------------------------------

/** A decrypt failure that is TRANSIENT - the remote signer (bunker) was unreachable or slow, so
 * the request timed out or couldn't be published. This is NOT a wrap we can't read: the bunker
 * never answered. Callers must NOT cache 'drop' on this (that would hide a real DM forever over a
 * hiccup); they leave the wrap uncached and retry it on the next refresh. A wrap genuinely not for
 * us instead comes back as the remote signer's OWN error response (a different message), or decrypts
 * to junk - both safe to drop. Markers match the signer's transient rejects (data/signer.ts). */
function isSignerUnavailable(e: unknown): boolean {
    return /timed out|timeout|Failed to publish/i.test((e as Error)?.message ?? '');
}

/** Layer 1: gift wrap (1059) → seal (13). One bunker decrypt; reveals the sender (seal.pubkey) for
 * triage before we pay for layer 2. A signer error propagates (caller classifies transient vs junk);
 * null means decrypted-but-not-a-seal = genuine junk. */
async function unwrapToSeal(s: Signed, wrap: NostrEvent): Promise<NostrEvent | null> {
    const plain = await s.signer.nip44Decrypt(wrap.pubkey, wrap.content); // throws on signer failure
    try {
        const seal = JSON.parse(plain) as NostrEvent;
        if (!seal || seal.kind !== 13 || !seal.pubkey || typeof seal.content !== 'string') return null;
        // NIP-59 MUST: verify the seal's signature - it's the only SIGNED layer, so it's what proves
        // the sender is who the rumor will claim (the rumor itself is unsigned by design).
        return verifyEvent(seal as never) ? seal : null;
    } catch { return null; }
}

/** Layer 2: seal (13) → rumor (kind 14). Validates the sender can't be spoofed. Signer error
 * propagates (caller classifies); null means decrypted-but-malformed = junk. */
async function unseal(s: Signed, seal: NostrEvent): Promise<Rumor | null> {
    const plain = await s.signer.nip44Decrypt(seal.pubkey, seal.content); // throws on signer failure
    try { return rumorFromSeal(JSON.parse(plain), seal.pubkey); }
    catch { return null; }
}

/** The conversation peer + sender for a decrypted rumor (a self-wrap's peer is the
 * recipient; an incoming wrap's peer is the sender). */
function peerOf(rumor: Rumor, me: string): string {
    return rumor.pubkey === me ? (rumorRecipients(rumor).find((p) => p !== me) ?? me) : rumor.pubkey;
}

/** Route a decrypted rumor to its cache entry by INNER kind: a private reply (kind 1) → a reply
 * entry keyed by the parent note (shown to anyone, since strangers can reply to your public note);
 * a DM (kind 14) → a msg for you/a follow, else a deferred request (body discarded); anything else
 * (e.g. kind 7 reactions, not handled yet) → drop. */
function triageRumor(rumor: Rumor, me: string, follows: Set<string>): Entry {
    if (rumor.kind === KIND_PRIVATE_REPLY) {
        const parent = replyParent({ tags: rumor.tags } as NostrEvent)?.id;
        if (!parent) return { kind: 'drop' }; // a kind-1 with no reply target isn't a placeable private reply
        return { kind: 'reply', owner: me, parent, id: rumor.id, from: rumor.pubkey, at: rumor.created_at, text: rumor.content, tags: rumor.tags };
    }
    if (rumor.kind === KIND_DM) {
        if (rumor.pubkey === me || follows.has(rumor.pubkey)) return { kind: 'msg', owner: me, peer: peerOf(rumor, me), from: rumor.pubkey, at: rumor.created_at, text: rumor.content };
        return { kind: 'request', owner: me, peer: rumor.pubkey, at: rumor.created_at }; // stranger DM → deferred, body discarded
    }
    return { kind: 'drop' };
}

// --- a tiny async concurrency pool ----------------------------------------
async function pool<T>(items: T[], n: number, fn: (t: T) => Promise<void>): Promise<void> {
    let i = 0;
    const worker = async (): Promise<void> => { while (i < items.length) await fn(items[i++]!); };
    await Promise.all(Array.from({ length: Math.min(n, items.length) }, worker));
}

// --- relay discovery + the two-target publish live in dm-routing.ts (shared with the nip07
// path, so both resolve DM relays through one cache and publish wraps identically).

/** Have you published a kind-10050 DM relay list? Drives the "publish so others can
 * reach you" nudge in the UI (decided: surface it). */
export async function hasDmRelayList(s: Session): Promise<boolean> {
    if (!signsOnServer(s) || !s.me) return false;
    const ev = await s.pool.get([...(s.myRelays?.read ?? []), ...INDEXER_RELAYS], { kinds: [KIND_DM_RELAYS], authors: [s.me] }).catch(() => null);
    return !!ev;
}

// --- load the conversation list -------------------------------------------

const RECENT_WRAPS = 500; // wrap timestamps are fuzzed ±2d, so over-fetch generously
const THREAD_WINDOW = 250; // wraps per thread page; older history loads on scroll-up
const LIST_TTL_MS = 30_000; // conversation-list cache: skip the relay round-trip on quick re-visits
// `full` = built by a real refresh (has the legacy flags), vs the instant cache-only aggregate
// (no legacy info). Only a `full` entry is trustworthy enough to drive the thread fast-path.
const listCache = new Map<string, { inbox: DmInbox; at: number }>();
// One in-flight list-refresh per pubkey, so prewarm + a user open (or two opens) share a single
// relay+decrypt pass instead of both running it cold. Subsumes the old `refreshing` dedupe set.
const refreshInflight = new Map<string, Promise<DmInbox>>();
function coalescedRefresh(sg: Signed): Promise<DmInbox> {
    const inf = refreshInflight.get(sg.me);
    if (inf) return inf;
    const p = refreshConversations(sg).finally(() => refreshInflight.delete(sg.me));
    refreshInflight.set(sg.me, p);
    return p;
}

export interface DmInbox { conversations: Conversation[]; requests: Conversation[] }
/** A thread page: messages oldest→newest + the next older-paging cursor (null = exhausted). */
export interface DmThread { messages: DmMessage[]; cursor: number | null }

/** Legacy (NIP-04) counterparty discovery for the conversation list - groups kind:4 by the
 * other party WITHOUT decrypting (participants are public in kind:4 anyway), keeping the
 * latest event per peer so the caller can decrypt just that one for an Inbox preview. */
async function legacyPeers(sg: Signed, relays: string[], muted: Set<string>): Promise<Map<string, { at: number; latest: NostrEvent }>> {
    const me = sg.me;
    const [recv, sent] = await Promise.all([
        sg.pool.query(relays, { kinds: [KIND_LEGACY], '#p': [me], limit: 200 }).catch(() => [] as NostrEvent[]),
        sg.pool.query(relays, { kinds: [KIND_LEGACY], authors: [me], limit: 200 }).catch(() => [] as NostrEvent[]),
    ]);
    const peers = new Map<string, { at: number; latest: NostrEvent }>();
    const bump = (p: string | undefined, ev: NostrEvent): void => {
        if (!p || p === me || muted.has(p)) return;
        const prev = peers.get(p);
        if (!prev || ev.created_at > prev.at) peers.set(p, { at: ev.created_at, latest: ev });
    };
    for (const e of recv) bump(e.pubkey, e);
    for (const e of sent) bump(e.tags.find((t) => t[0] === 'p' && t[1])?.[1], e);
    return peers;
}

/** Decrypt one legacy (kind:4) message body via the bunker, caching it (so a later thread
 * open is free). NIP-04's conversation key is symmetric, so `peer` works for both directions.
 * Returns null on failure. */
async function legacyPreview(sg: Signed, peer: string, ev: NostrEvent): Promise<string | null> {
    const cached = cache.get(ev.id);
    if (cached?.kind === 'msg') return cached.text;
    try {
        const text = await sg.signer.nip04Decrypt(peer, ev.content);
        cache.set(ev.id, { kind: 'msg', owner: sg.me, peer, from: ev.pubkey, at: ev.created_at, text, legacy: true });
        return text;
    } catch { return null; }
}

/** Build the Inbox + Requests conversation list. Fetches recent wraps, decrypts only
 * the uncached ones (triaged: strangers cost 1 decrypt, not 2), aggregates per peer,
 * then folds in legacy NIP-04 conversations (labelled, not decrypted for the preview). */
export async function loadConversations(s: Session): Promise<DmInbox | null> {
    if (!signsOnServer(s) || !s.me) return null;
    const sg = s as Signed;
    const me = sg.me;
    const hit = listCache.get(me);
    if (hit && Date.now() - hit.at < LIST_TTL_MS) return hit.inbox;
    // Instant path: the decrypt cache (disk-backed, survives restarts) already holds every
    // decrypted message, so rebuild the list from it with NO relay query, and revalidate in
    // the background to catch new messages + legacy convos. Only a truly empty cache (first
    // ever run) falls through to the blocking query. This is what kills the ~4s open.
    if (ownsCached(me)) {
        const inbox = aggregateCached(me);
        listCache.set(me, { inbox, at: Date.now() });
        void coalescedRefresh(sg).catch(() => { /* background revalidate; best-effort */ });
        return inbox;
    }
    return coalescedRefresh(sg);
}

/** Rebuild the conversation list from the decrypt cache alone (no relay query). Triage was
 * baked into each entry at decrypt time (msg → Inbox, request → Requests), so bucketing
 * needs no follow set here. Misses legacy-only convos that haven't been thread-opened (their
 * kind-4 bodies aren't cached yet) - the background refresh folds those in. */
/** Does the cache hold any entry decrypted by THIS account? Gates the instant render path so
 * a freshly-switched identity with only the prior account's entries falls through to a real
 * (owner-stamping) refresh instead of rendering an empty list. */
function ownsCached(me: string): boolean {
    for (const e of cache.values()) if (e.kind !== 'drop' && e.owner === me) return true;
    return false;
}

function aggregateCached(me: string): DmInbox {
    // Single pass: build the `secure` peer set AND byPeer together. The secure flag can't be set
    // inline (a peer's securing msg may come later in iteration), so apply it once at the end -
    // an O(peers) loop, not a second full-cache scan.
    const t0 = performance.now();
    const secure = new Set<string>(); // peers with at least one NIP-17 (non-legacy) message
    const byPeer = new Map<string, Conversation>();
    for (const e of cache.values()) {
        if (e.kind === 'drop' || e.kind === 'reply' || e.owner !== me) continue; // replies aren't conversations
        if (e.kind === 'msg' && !e.legacy) secure.add(e.peer);
        const prev = byPeer.get(e.peer);
        if (e.kind === 'msg') {
            const preview = e.from === me ? `You: ${e.text}` : e.text;
            if (!prev || prev.bucket === 'request' || e.at > prev.lastAt) byPeer.set(e.peer, { peer: e.peer, lastAt: e.at, preview, bucket: 'inbox', secure: false, unread: dmUnread(me, e.peer, e.at, e.from === me) });
        } else if (!prev) {
            byPeer.set(e.peer, { peer: e.peer, lastAt: e.at, preview: 'Message request', bucket: 'request', secure: false, unread: dmUnread(me, e.peer, e.at, false) });
        }
    }
    for (const c of byPeer.values()) c.secure = secure.has(c.peer);
    const all = [...byPeer.values()].sort((a, b) => b.lastAt - a.lastAt);
    recordScan('aggregateCached', cache.size, performance.now() - t0);
    return { conversations: all.filter((c) => c.bucket === 'inbox'), requests: all.filter((c) => c.bucket === 'request') };
}

/** The authoritative (slow) build: query relays for recent wraps, decrypt the uncached ones
 * (triaged), aggregate, fold in legacy NIP-04, and refresh the list cache. Runs on the cold
 * first load and as the background revalidation behind the instant cache path. */
async function refreshConversations(sg: Signed): Promise<DmInbox> {
    const me = sg.me;
    const muted = mutedPubkeys(sg);
    const follows = new Set(sg.followsRoute?.authors ?? []);
    const relays = await myDmReadRelays(sg);
    const wraps = await sg.pool.query(relays, { kinds: [KIND_GIFTWRAP], '#p': [me], limit: RECENT_WRAPS }).catch(() => [] as NostrEvent[]);

    // `down` short-circuits the rest of the batch once the bunker is detected unreachable: no point
    // paying another 90s timeout per wrap, and those wraps stay UNCACHED so they retry (not poisoned).
    let down = false;
    await pool(wraps.filter((w) => !cache.has(w.id)), 6, async (wrap) => {
        if (down) return;
        let seal: NostrEvent | null;
        try { seal = await unwrapToSeal(sg, wrap); }
        catch (e) { if (isSignerUnavailable(e)) { down = true; return; } cache.set(wrap.id, { kind: 'drop' }); return; }
        if (!seal) { cache.set(wrap.id, { kind: 'drop' }); return; }
        const sender = seal.pubkey;
        if (sender !== me && muted.has(sender)) { cache.set(wrap.id, { kind: 'drop' }); return; } // muted → drop at 1 decrypt
        // 2nd decrypt for EVERYONE (not just follows): a private reply is indistinguishable from a DM
        // until unsealed, and strangers can reply privately to your public note - so we must read the
        // inner kind to catch those. triageRumor then routes by kind (DM vs private reply).
        let rumor: Rumor | null;
        try { rumor = await unseal(sg, seal); }
        catch (e) { if (isSignerUnavailable(e)) { down = true; return; } cache.set(wrap.id, { kind: 'drop' }); return; }
        if (!rumor) { cache.set(wrap.id, { kind: 'drop' }); return; }
        cache.set(wrap.id, triageRumor(rumor, me, follows));
    });
    scheduleFlush();

    // Aggregate this user's NIP-17 entries into one conversation per peer. (legacy folded below)
    const byPeer = new Map<string, Conversation>();
    const fold = (e: Entry | undefined): void => {
        if (!e || e.kind === 'drop' || e.kind === 'reply' || e.owner !== me) return; // replies aren't conversations
        const peer = e.peer;
        const prev = byPeer.get(peer);
        if (e.kind === 'msg') {
            if (e.legacy) return; // NIP-04 folded separately below (legacyPeers)
            const preview = e.from === me ? `You: ${e.text}` : e.text;
            if (!prev || prev.bucket === 'request' || e.at > prev.lastAt) byPeer.set(peer, { peer, lastAt: e.at, preview, bucket: 'inbox', secure: true, unread: dmUnread(me, peer, e.at, e.from === me) });
        } else if (!prev) { // NIP-17 stranger request
            byPeer.set(peer, { peer, lastAt: e.at, preview: 'Message request', bucket: 'request', secure: true, unread: dmUnread(me, peer, e.at, false) });
        }
    };
    for (const w of wraps) fold(cache.get(w.id)); // relay wraps: fresh + authoritative
    // Then fold in any owned NIP-17 message the relays haven't returned yet (e.g. a DM you just
    // sent, not yet propagated) so the background refresh can't transiently drop it. Idempotent
    // for messages already folded above. Requests come only from wraps / legacyPeers, so we
    // restrict this pass to msgs (avoids mis-bucketing a legacy stranger request as secure).
    for (const e of cache.values()) if (e.kind === 'msg') fold(e);
    // Fold in legacy NIP-04 conversations: flag peers we already have, add legacy-only ones.
    // In-network (Inbox) legacy convos get their last message decrypted for the preview, just
    // like NIP-17 inbox convos; strangers (Requests) stay deferred. The "less private" tag is
    // the security signal, not a hidden preview.
    for (const [peer, info] of await legacyPeers(sg, relays, muted)) {
        const existing = byPeer.get(peer);
        if (existing) { existing.legacy = true; continue; }
        const inInbox = follows.has(peer);
        let preview = 'Message request';
        if (inInbox) {
            const text = await legacyPreview(sg, peer, info.latest);
            preview = text == null ? 'Encrypted message' : info.latest.pubkey === me ? `You: ${text}` : text;
        } else {
            // Cache the stranger discovery (body deferred, like a NIP-17 request) so the instant
            // cache-only render includes legacy requests too - else they vanish off the fast path.
            if (!cache.has(info.latest.id)) cache.set(info.latest.id, { kind: 'request', owner: me, peer, at: info.at });
        }
        byPeer.set(peer, { peer, lastAt: info.at, preview, bucket: inInbox ? 'inbox' : 'request', legacy: true, unread: dmUnread(me, peer, info.at, inInbox && info.latest.pubkey === me) });
    }
    scheduleFlush(); // persist the legacy preview/discovery writes so they survive a restart

    const all = [...byPeer.values()].sort((a, b) => b.lastAt - a.lastAt);
    const inbox: DmInbox = { conversations: all.filter((c) => c.bucket === 'inbox'), requests: all.filter((c) => c.bucket === 'request') };
    listCache.set(me, { inbox, at: Date.now() });
    return inbox;
}

/** A thread's messages straight from the warm decrypt cache (no relay query, no spinner) -
 * but ONLY when it's safe: the inbox is warm and this peer is a non-legacy Inbox conversation
 * (its NIP-17 bodies were fully decrypted during the inbox load). Returns null for strangers
 * (bodies deferred) and legacy/NIP-04 peers (kind-4 bodies aren't cached until a thread open),
 * so the caller falls back to the full loadThread, which never silently drops messages. */
export function cachedThread(s: Session, peer: string): DmMessage[] | null {
    if (!signsOnServer(s) || !s.me) return null;
    const me = s.me;
    // Render straight from the disk-backed decrypt cache (survives restarts, no TTL - so a
    // refresh stays warm, unlike the old in-memory listCache gate). We only do so when there's
    // an INCOMING decrypted message: that means the peer's side was fully decrypted (a follow,
    // or a thread opened before - which also caches its legacy bodies). Otherwise return null so
    // loadThread decrypts the deferred-stranger / legacy parts and caches them (warm next time).
    const t0 = performance.now();
    const out: DmMessage[] = [];
    let incoming = false;
    for (const [id, e] of cache) {
        if (e.kind === 'msg' && e.owner === me && e.peer === peer) {
            out.push({ id, from: e.from, at: e.at, text: e.text, legacy: e.legacy });
            if (e.from !== me) incoming = true;
        }
    }
    recordScan('cachedThread', cache.size, performance.now() - t0);
    return incoming ? out.sort((a, b) => a.at - b.at) : null;
}

/** All decrypted private replies to `noteId` (from the disk cache), oldest-first - shown inline in
 * the note's thread, badged private. Covers both replies others sent to your note AND self-copies of
 * private replies you sent to any note. */
export function privateRepliesFor(s: Session, noteId: string): PrivateReply[] {
    if (!signsOnServer(s) || !s.me) return [];
    const me = s.me;
    const t0 = performance.now();
    const out: PrivateReply[] = [];
    for (const e of cache.values()) {
        if (e.kind === 'reply' && e.owner === me && e.parent === noteId) out.push({ id: e.id, parent: e.parent, from: e.from, at: e.at, content: e.text, tags: e.tags });
    }
    recordScan('privateRepliesFor', cache.size, performance.now() - t0);
    return out.sort((a, b) => a.at - b.at);
}

/** Every private reply OTHERS sent you (from the disk-backed cache), newest-first - the notifications
 * source. Self-copies of replies you sent (from === me) are excluded, mirroring how notifications skip
 * your own actions. */
export function allPrivateReplies(s: Session): PrivateReply[] {
    if (!signsOnServer(s) || !s.me) return [];
    const me = s.me;
    const t0 = performance.now();
    const out: PrivateReply[] = [];
    for (const e of cache.values()) {
        if (e.kind === 'reply' && e.owner === me && e.from !== me) out.push({ id: e.id, parent: e.parent, from: e.from, at: e.at, content: e.text, tags: e.tags });
    }
    recordScan('allPrivateReplies', cache.size, performance.now() - t0);
    return out.sort((a, b) => b.at - a.at);
}

/** Fire-and-forget background pre-warm (bunker only): decrypt recent wraps into the cache
 * right after login, so opening Messages is instant instead of a cold decrypt. The daemon
 * holds the signer, so this needs no browser involvement (nip07 can't be warmed this way -
 * its key is in the extension). Errors are swallowed; it's best-effort. */
export function prewarmDms(s: Session): void {
    if (!signsOnServer(s) || !s.me) return;
    void loadConversations(s).catch(() => { /* best-effort; the next open will retry */ });
}

/** Cheap unread signal for the quiet dot: are there recent wraps `#p=me` we haven't
 * processed yet? Query only, NO decrypt - so it's light enough to poll. Opening Messages
 * decrypts+caches them, which clears the dot naturally. */
export async function hasUnprocessedWraps(s: Session): Promise<boolean> {
    if (!signsOnServer(s) || !s.me) return false;
    const sg = s as Signed;
    const relays = await myDmReadRelays(sg);
    const wraps = await sg.pool.query(relays, { kinds: [KIND_GIFTWRAP], '#p': [sg.me], limit: 60 }).catch(() => [] as NostrEvent[]);
    return wraps.some((w) => !cache.has(w.id));
}

/** Legacy NIP-04 (kind:4) messages between you and `peer`, decrypted via the bunker.
 * Read-only; we never send this format. Real timestamps (not fuzzed). */
async function legacyThread(sg: Signed, peer: string, relays: string[]): Promise<DmMessage[]> {
    const me = sg.me;
    const evs = await sg.pool.query(relays, { kinds: [KIND_LEGACY], authors: [peer, me], '#p': [peer, me], limit: 400 }).catch(() => [] as NostrEvent[]);
    const pair = evs.filter((e) =>
        (e.pubkey === peer && e.tags.some((t) => t[0] === 'p' && t[1] === me))
        || (e.pubkey === me && e.tags.some((t) => t[0] === 'p' && t[1] === peer)));
    const out: DmMessage[] = [];
    await pool(pair, 6, async (ev) => {
        const cached = cache.get(ev.id);
        if (cached?.kind === 'msg') { out.push({ id: ev.id, from: cached.from, at: cached.at, text: cached.text, legacy: true }); return; }
        if (cached?.kind === 'drop') return;
        try {
            const text = await sg.signer.nip04Decrypt(peer, ev.content);
            cache.set(ev.id, { kind: 'msg', owner: me, peer, from: ev.pubkey, at: ev.created_at, text, legacy: true });
            out.push({ id: ev.id, from: ev.pubkey, at: ev.created_at, text, legacy: true });
        } catch { cache.set(ev.id, { kind: 'drop' }); }
    });
    return out;
}

// --- one conversation's full history --------------------------------------

/** All decrypted messages with `peer`, oldest-first, plus a `cursor` (the next `until`
 * for older paging, or null once the mailbox is exhausted). Unseals any deferred request
 * bodies for this peer (and upgrades their cache entries). `until` pages older: each call
 * fetches one window of wraps `created_at <= until`. Legacy NIP-04 history is folded in
 * on the initial load only (it's a read-only nicety; gift-wrap paging is the main path). */
export async function loadThread(s: Session, peer: string, until?: number): Promise<DmThread | null> {
    if (!signsOnServer(s) || !s.me) return null;
    const sg = s as Signed;
    const me = sg.me;
    const relays = await myDmReadRelays(sg);
    const filter: Filter = { kinds: [KIND_GIFTWRAP], '#p': [me], limit: THREAD_WINDOW };
    if (until != null) filter.until = until;
    const wraps = await sg.pool.query(relays, filter).catch(() => [] as NostrEvent[]);
    const msgs: DmMessage[] = [];

    let down = false; // bunker unreachable → stop paying timeouts; leave the rest uncached to retry
    await pool(wraps, 6, async (wrap) => {
        if (down) return;
        const cached = cache.get(wrap.id);
        if (cached?.kind === 'msg') { if (cached.peer === peer) msgs.push({ id: wrap.id, from: cached.from, at: cached.at, text: cached.text }); return; }
        if (cached?.kind === 'drop') return;
        // request (this peer) or uncached → decrypt fully now
        let seal: NostrEvent | null;
        try { seal = await unwrapToSeal(sg, wrap); }
        catch (e) { if (isSignerUnavailable(e)) { down = true; return; } cache.set(wrap.id, { kind: 'drop' }); return; }
        if (!seal) { cache.set(wrap.id, { kind: 'drop' }); return; }
        if (seal.pubkey !== me && seal.pubkey !== peer) return; // a different conversation; leave its cache alone
        let rumor: Rumor | null;
        try { rumor = await unseal(sg, seal); }
        catch (e) { if (isSignerUnavailable(e)) { down = true; return; } cache.set(wrap.id, { kind: 'drop' }); return; }
        if (!rumor) { cache.set(wrap.id, { kind: 'drop' }); return; }
        if (rumor.kind !== KIND_DM) { cache.set(wrap.id, triageRumor(rumor, me, new Set())); return; } // private reply etc. → not a DM message
        const p = peerOf(rumor, me);
        cache.set(wrap.id, { kind: 'msg', owner: me, peer: p, from: rumor.pubkey, at: rumor.created_at, text: rumor.content });
        if (p === peer) msgs.push({ id: wrap.id, from: rumor.pubkey, at: rumor.created_at, text: rumor.content });
    });
    // Cursor: only offer older paging when the relay returned a FULL window (more likely
    // exists). A short window means we've seen everything - no sentinel, no wasted fetch.
    // Step one second below the oldest to exclude the boundary (wrap timestamps are fuzzed).
    const cursor = wraps.length >= THREAD_WINDOW ? Math.min(...wraps.map((w) => w.created_at)) - 1 : null;
    const legacy = until == null ? await legacyThread(sg, peer, relays) : []; // initial load only
    scheduleFlush();
    return { messages: [...msgs, ...legacy].sort((a, b) => a.at - b.at), cursor };
}

// --- send -----------------------------------------------------------------

/** Send a NIP-17 DM: build the kind-14 rumor, seal+wrap it to the peer AND to yourself
 * (so you keep your own copy), publish each to the right inbox relays. ~4 bunker calls. */
export async function sendDm(s: Session, peer: string, text: string): Promise<boolean> {
    if (!signsOnServer(s) || !s.me) return false;
    const sg = s as Signed;
    const me = sg.me;
    const body = text.trim();
    if (!body) return false;
    const rumor = buildRumor(me, [peer], body);

    const wrapFor = async (target: string): Promise<NostrEvent> => {
        const encrypted = await sg.signer.nip44Encrypt(target, JSON.stringify(rumor));
        const seal = await sg.signer.signEvent(sealTemplate(me, encrypted));
        return finalizeWrap(seal, target);
    };

    try {
        const [toPeer, toSelf] = await Promise.all([wrapFor(peer), wrapFor(me)]);
        await publishWrapPair(sg, peer, toPeer, toSelf);
        // Optimistically cache our own message so it shows immediately.
        cache.set(toSelf.id, { kind: 'msg', owner: me, peer, from: me, at: rumor.created_at, text: body });
        listCache.delete(me); // the new message changes the conversation list
        scheduleFlush();
        return true;
    } catch (e) { console.warn('[dms] send failed:', (e as Error)?.message ?? e); return false; }
}

/** Send a PRIVATE reply (bunker): a kind:1 reply rumor (NIP-10 `baseTags` built exactly like a public
 * reply) gift-wrapped to the note author AND yourself, delivered to their DM relays. Never published as
 * a public note. Returns the cached PrivateReply (same shape as the nip07 path) so the caller can render
 * it optimistically in-thread. Null on failure. */
export async function sendPrivateReply(s: Session, author: string, parentId: string, baseTags: string[][], content: string): Promise<PrivateReply | null> {
    if (!signsOnServer(s) || !s.me) return null;
    const sg = s as Signed;
    const me = sg.me;
    const body = content.trim();
    if (!body) return null;
    const rumor = buildPrivateReplyRumor(me, baseTags, body);

    const wrapFor = async (target: string): Promise<NostrEvent> => {
        const encrypted = await sg.signer.nip44Encrypt(target, JSON.stringify(rumor));
        const seal = await sg.signer.signEvent(sealTemplate(me, encrypted));
        return finalizeWrap(seal, target);
    };

    try {
        const [toPeer, toSelf] = await Promise.all([wrapFor(author), wrapFor(me)]);
        await publishWrapPair(sg, author, toPeer, toSelf);
        // Cache our own copy keyed by the parent note, so it shows in-thread immediately.
        cache.set(toSelf.id, { kind: 'reply', owner: me, parent: parentId, id: rumor.id, from: me, at: rumor.created_at, text: body, tags: rumor.tags });
        scheduleFlush();
        return { id: rumor.id, parent: parentId, from: me, at: rumor.created_at, content: body, tags: rumor.tags };
    } catch (e) { console.warn('[dms] private reply failed:', (e as Error)?.message ?? e); return null; }
}
