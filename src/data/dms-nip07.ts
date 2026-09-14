// NIP-17 DM engine for NIP-07 (browser-extension) logins - the SLOW path inverted.
// The user's key lives in the extension, not here, so every NIP-44 decrypt/encrypt
// and seal-sign is a round-trip driven by the browser via the nip07-hateoas batch
// chain (H-Nostr-Sign + *_batch methods). Batching collapses a full mailbox sync to
// 2 decrypt round-trips (layer-1 wrap->seal for all wraps, layer-2 seal->rumor for
// known senders), and a send to encrypt-batch -> sign-batch -> local wrap.
//
// Plaintext at rest: NONE. An extension user reasonably expects nothing of theirs to
// persist on the daemon, so the decrypt cache is IN-MEMORY ONLY (no .data/dms.json),
// cleared on restart and logout. A restart costs a re-decrypt (cheap: 2 round-trips).
// Mid-chain correlation (which result belongs to which wrap) is too big for a URL, so
// we hold a short-lived `chains` map keyed by a chainId in the continuation URL - the
// one bit of server state, TTL'd and dropped on completion. See [[nip17-dms-plan]].

import { randomUUID } from 'node:crypto';
import { mutedPubkeys } from '../actions.ts';
import { dmUnread } from './dm-read.ts';
import { myDmReadRelays, publishWrapPair } from './dm-routing.ts';
import { recordScan } from './dm-metrics.ts';
import { coerceEvent } from '../nip07.ts';
import {
    buildRumor, buildPrivateReplyRumor, finalizeWrap, sealTemplate, rumorFromSeal, rumorRecipients,
    KIND_GIFTWRAP, KIND_SEAL, KIND_DM, KIND_PRIVATE_REPLY, type Rumor,
} from '../nostr/nip17.ts';
import { replyParent } from '../nostr/nip10.ts';
import type { NostrEvent, UnsignedEvent } from '../nostr/types.ts';
import type { Filter } from 'nostr-tools';
import { verifyEvent } from 'nostr-tools/pure';
import type { BatchResult } from '../wire.ts';
import type { Session } from '../session.ts';
import { signsOnClient } from '../session.ts';
import { HEX64 } from '../nostr/tags.ts';
import { trimOldest } from './json-store.ts';
import type { Conversation, DmMessage, DmInbox, PrivateReply } from './dms.ts';

const RECENT_WRAPS = 500; // fits in one batch (maxBatchItems 1024); timestamps fuzzed ±2d
const THREAD_WINDOW = 250; // wraps per thread page; older history loads on scroll-up
const KIND_LEGACY = 4; // NIP-04 (read-only; we never SEND this metadata-leaky format)

// --- in-memory cache (decrypt once per process lifetime; never touches disk) ----
// `legacy` marks a NIP-04 (kind-4) message/request -> no shield, decrypted via nip04 not nip44.
// `owner` = the account (me) that decrypted this entry. The cache is process-global (multiple npubs may
// use one daemon), so EVERY read that iterates it MUST filter owner === me, else one account sees another's
// decrypted DMs. Mirrors the bunker path's owner field in data/dms.ts. `drop` carries no plaintext, so it
// needs no owner. Keep these guards in lockstep with the stamps at each memSet below.
type Entry =
    | { kind: 'msg'; owner: string; peer: string; from: string; at: number; text: string; legacy?: boolean }
    | { kind: 'request'; owner: string; peer: string; at: number; legacy?: boolean }
    // A NIP-59-wrapped private reply (inner kind:1) to a public note - keyed by the parent note id,
    // carries the rumor id + tags (to render in-thread), excluded from conversation aggregation.
    | { kind: 'reply'; owner: string; parentId: string; id: string; from: string; at: number; text: string; tags: string[][] }
    | { kind: 'drop' };

/** A legacy (kind-4) message queued for nip04 decryption through the browser. */
interface LegacyItem { id: string; peer: string; from: string; at: number; ciphertext: string }

const mem = new Map<string, Entry>();
// Bound it: keys are gift-wrap ids from relays (external, unbounded over a long DM history) and
// this cache never touches disk, so without a cap it grows for the whole process lifetime. An
// evicted entry just costs one cheap re-decrypt next time it's viewed (the data is reconstructible).
const MEM_CAP = 8000;
function memSet(id: string, e: Entry): void {
    mem.set(id, e);
    trimOldest(mem, MEM_CAP);
}

// Conversation-list cache: lets the route skip the whole decrypt-chain (and its relay
// round-trip) on quick re-visits to Messages/Requests. TTL'd; cleared on send + logout.
const LIST_TTL_MS = 30_000;
const listMem = new Map<string, { inbox: DmInbox; at: number }>();

/** The cached conversation list for `me`. Fresh listMem within the TTL is returned as-is; past
 * the TTL we rebuild from the in-memory `mem` decrypt cache (no relay round-trip, no spinner) -
 * the analog of bunker's disk-backed aggregateCached, so a refresh stays warm for the whole
 * process lifetime, not just 30s. Null only when truly cold (mem empty, e.g. after a restart),
 * where the route falls back to the decrypting shell. */
export function cachedInboxNip07(me: string): DmInbox | null {
    const hit = listMem.get(me);
    if (hit && Date.now() - hit.at < LIST_TTL_MS) return hit.inbox;
    if (mem.size === 0) return null;
    const t0 = performance.now();
    const inbox = aggregate(mem.keys(), me); // iterate the keys directly - no full-array allocation
    recordScan('cachedInboxNip07', mem.size, performance.now() - t0);
    listMem.set(me, { inbox, at: Date.now() });
    return inbox;
}

/** A thread's messages straight from the warm decrypt cache (no relay query, no decrypt
 * round-trip, no spinner) - usable only when the inbox is warm AND we already decrypted
 * messages for this peer. Null otherwise (cold, or a stranger whose body wasn't decrypted
 * during inbox triage), so the caller falls back to the decrypt-chain shell. */
export function cachedThreadNip07(me: string, peer: string): DmMessage[] | null {
    if (mem.size === 0) return null; // truly cold -> decrypt-chain shell
    const t0 = performance.now();
    const msgs = threadMessages(mem.keys(), peer, me); // iterate the keys directly - no full-array allocation
    recordScan('cachedThreadNip07', mem.size, performance.now() - t0);
    // Only serve from cache when an incoming message is present (the peer's side was decrypted);
    // a your-side-only set means a stranger thread not yet fully decrypted -> let the chain run.
    return msgs.some((m) => m.from !== me) ? msgs : null;
}

/** Wipe the in-memory nip07 DM state (logout, or account switch). */
/** With `me`, drop only that account's decrypted entries (other signed-in users keep theirs); without, wipe
 * all. `chains` are ephemeral per-send correlation keyed by a random id and TTL-pruned, so the scoped path
 * leaves them (untagged by owner, harmless, self-expiring). */
export function clearNip07DmCache(me?: string): void {
    if (me) {
        for (const [id, e] of mem) if ('owner' in e && e.owner === me) mem.delete(id);
        listMem.delete(me);
        return;
    }
    mem.clear(); chains.clear(); listMem.clear();
}

// --- ephemeral chain state (the wrap<->result correlation across round-trips) ---
const CHAIN_TTL_MS = 2 * 60_000;

interface SyncChain {
    kind: 'sync';
    view: 'inbox' | 'requests' | 'thread';
    warm?: boolean;                         // background prewarm: warm the cache, render NOTHING (no convList/gate/error leak)
    peer?: string;                          // thread / opened-request target
    older: boolean;                         // a scroll-up older-page (prepend) vs initial load
    cursor: number | null;                  // next older-paging `until` (thread only; null = exhausted)
    allIds: string[];                       // this window's wrap ids - terminal aggregation
    l1: { id: string; at: number }[];       // uncached wraps, aligned to decrypt batch #1
    l2: { id: string; sealPubkey: string }[]; // known-sender wraps, aligned to decrypt batch #2
    legacy: LegacyItem[];                    // kind-4 to decrypt via nip04 (a 3rd batch step)
    expires: number;
}
interface SendChain {
    kind: 'send';
    peer: string;
    rumor: Rumor;
    targets: string[]; // [peer, me] - aligned to the encrypt + sign batches
    text: string;
    replyParent?: string; // set => this is a private REPLY to a public note (cache as 'reply', not 'msg')
    expires: number;
}
type Chain = SyncChain | SendChain;

const chains = new Map<string, Chain>();

function putChain(c: Chain): string {
    const now = Date.now();
    for (const [k, v] of chains) if (v.expires < now) chains.delete(k); // sweep
    const id = randomUUID();
    chains.set(id, c);
    return id;
}
function takeSync(id: string): SyncChain | null {
    const c = chains.get(id);
    if (!c || c.kind !== 'sync' || c.expires < Date.now()) { chains.delete(id); return null; }
    return c;
}
/** The view a sync chain is rendering ('thread' vs inbox/requests), so a chain error/timeout
 * can be placed at the right DOM target. Returns null if the chain is gone/expired. */
export function chainView(id: string): SyncChain['view'] | null {
    const c = chains.get(id);
    return c && c.kind === 'sync' && c.expires >= Date.now() ? c.view : null;
}
/** Whether a sync chain is a background prewarm (so its terminal/error renders nothing - it only fills
 * the cache). Read alongside chainView, BEFORE finalizeSync consumes the chain. */
export function chainWarm(id: string): boolean {
    const c = chains.get(id);
    return !!(c && c.kind === 'sync' && c.expires >= Date.now() && c.warm);
}
function takeSend(id: string): SendChain | null {
    const c = chains.get(id);
    if (!c || c.kind !== 'send' || c.expires < Date.now()) { chains.delete(id); return null; }
    return c;
}
function dropChain(id: string): void { chains.delete(id); }

// --- relay discovery + the two-target publish live in dm-routing.ts (shared with the bunker
// path, so both resolve DM relays through one cache and publish wraps identically).

// --- aggregation (reads the cache; no legacy NIP-04 - the lib has no nip04) -------
function peerOf(rumor: Rumor, me: string): string {
    return rumor.pubkey === me ? (rumorRecipients(rumor).find((p) => p !== me) ?? me) : rumor.pubkey;
}

function aggregate(ids: Iterable<string>, me: string): DmInbox {
    const byPeer = new Map<string, Conversation>();
    for (const id of ids) {
        const e = mem.get(id);
        if (!e || e.kind === 'drop' || e.kind === 'reply' || e.owner !== me) continue; // skip other accounts + non-convos
        const peer = e.peer;
        const prev = byPeer.get(peer);
        if (e.kind === 'msg') {
            const preview = e.from === me ? `You: ${e.text}` : e.text;
            if (!prev || prev.bucket === 'request' || e.at > prev.lastAt) byPeer.set(peer, { peer, lastAt: e.at, preview, bucket: 'inbox', secure: !e.legacy, unread: dmUnread(me, peer, e.at, e.from === me) });
        } else if (!prev) {
            byPeer.set(peer, { peer, lastAt: e.at, preview: e.legacy ? 'Encrypted message' : 'Message request', bucket: 'request', secure: !e.legacy, unread: dmUnread(me, peer, e.at, false) });
        }
    }
    const all = [...byPeer.values()].sort((a, b) => b.lastAt - a.lastAt);
    return { conversations: all.filter((c) => c.bucket === 'inbox'), requests: all.filter((c) => c.bucket === 'request') };
}

function threadMessages(ids: Iterable<string>, peer: string, me: string): DmMessage[] {
    const out: DmMessage[] = [];
    for (const id of ids) {
        const e = mem.get(id);
        if (e?.kind === 'msg' && e.owner === me && e.peer === peer) out.push({ id, from: e.from, at: e.at, text: e.text, legacy: e.legacy });
    }
    return out.sort((a, b) => a.at - b.at);
}

/** All decrypted private replies (in-memory) to `noteId`, oldest-first - shown inline in the note's
 * thread, badged private (replies others sent to your note + self-copies of ones you sent). */
export function privateRepliesForNip07(noteId: string, me: string): PrivateReply[] {
    const t0 = performance.now();
    const out: PrivateReply[] = [];
    for (const e of mem.values()) {
        if (e.kind === 'reply' && e.owner === me && e.parentId === noteId) out.push({ id: e.id, parent: e.parentId, from: e.from, at: e.at, content: e.text, tags: e.tags });
    }
    recordScan('privateRepliesForNip07', mem.size, performance.now() - t0);
    return out.sort((a, b) => a.at - b.at);
}

/** Every private reply OTHERS sent you (in-memory), newest-first - the notifications source. Self-copies
 * of replies you sent (from === me) are excluded, mirroring how notifications skip your own actions. */
export function allPrivateRepliesNip07(me: string): PrivateReply[] {
    const t0 = performance.now();
    const out: PrivateReply[] = [];
    for (const e of mem.values()) {
        if (e.kind === 'reply' && e.owner === me && e.from !== me) out.push({ id: e.id, parent: e.parentId, from: e.from, at: e.at, content: e.text, tags: e.tags });
    }
    recordScan('allPrivateRepliesNip07', mem.size, performance.now() - t0);
    return out.sort((a, b) => b.at - a.at);
}

// --- legacy NIP-04 discovery (kind-4; participants public, so query without decrypting) ----
/** Find kind-4 messages to fold in. For the inbox: the latest per peer (in-network -> queued
 * for a preview decrypt; strangers -> cached as deferred Requests). For a thread: every kind-4
 * with that peer, queued. Returns the decrypt queue + the event ids to add to the sync window
 * (so aggregate/threadMessages include them). NIP-04's conversation key is symmetric, so the
 * peer pubkey decrypts both directions. */
async function fetchLegacy(s: Session, view: SyncChain['view'], peer: string | undefined): Promise<{ queue: LegacyItem[]; ids: string[] }> {
    const me = s.me!;
    const relays = await myDmReadRelays(s);
    const muted = mutedPubkeys(s);
    const follows = new Set(s.followsRoute?.authors ?? []);
    const other = (ev: NostrEvent): string | undefined => (ev.pubkey === me ? ev.tags.find((t) => t[0] === 'p' && t[1])?.[1] : ev.pubkey);
    const queue: LegacyItem[] = [];
    const ids: string[] = [];
    if (view === 'thread' && peer) {
        const evs = await s.pool.query(relays, { kinds: [KIND_LEGACY], authors: [peer, me], '#p': [peer, me], limit: 400 }).catch(() => [] as NostrEvent[]);
        for (const ev of evs) {
            if (other(ev) !== peer) continue; // only this pair
            ids.push(ev.id);
            if (!mem.has(ev.id)) queue.push({ id: ev.id, peer, from: ev.pubkey, at: ev.created_at, ciphertext: ev.content });
        }
        return { queue, ids };
    }
    const [recv, sent] = await Promise.all([
        s.pool.query(relays, { kinds: [KIND_LEGACY], '#p': [me], limit: 200 }).catch(() => [] as NostrEvent[]),
        s.pool.query(relays, { kinds: [KIND_LEGACY], authors: [me], limit: 200 }).catch(() => [] as NostrEvent[]),
    ]);
    const latest = new Map<string, NostrEvent>(); // peer -> their newest kind-4
    for (const ev of [...recv, ...sent]) {
        const op = other(ev);
        if (!op || op === me || muted.has(op)) continue;
        const cur = latest.get(op);
        if (!cur || ev.created_at > cur.created_at) latest.set(op, ev);
    }
    for (const [op, ev] of latest) {
        ids.push(ev.id);
        if (mem.has(ev.id)) continue; // already decrypted/deferred
        if (follows.has(op)) queue.push({ id: ev.id, peer: op, from: ev.pubkey, at: ev.created_at, ciphertext: ev.content }); // in-network -> preview
        else memSet(ev.id, { kind: 'request', owner: me, peer: op, at: ev.created_at, legacy: true }); // stranger -> deferred Request
    }
    return { queue, ids };
}

// --- the sync chain (layer-1 -> triage -> layer-2 -> [legacy] -> render) ----------

export interface DecryptItem { pubkey: string; ciphertext: string }

/** Begin a mailbox sync: fetch a window of wraps, return the layer-1 decrypt batch (wrap
 * content -> seal) for the UNCACHED ones plus a chainId. An empty `items` means every
 * wrap is already cached - the caller finalizes straight from the cache. For a thread,
 * `until` pages older (one THREAD_WINDOW slice, `created_at <= until`) and a paging cursor
 * is computed; inbox/requests fetch the broad RECENT_WRAPS window. */
export async function beginSync(s: Session, view: SyncChain['view'], peer?: string, until?: number, nip04 = false, warm = false): Promise<{ chainId: string; items: DecryptItem[] }> {
    const relays = await myDmReadRelays(s);
    const thread = view === 'thread';
    const filter: Filter = { kinds: [KIND_GIFTWRAP], '#p': [s.me!], limit: thread ? THREAD_WINDOW : RECENT_WRAPS };
    if (until != null) filter.until = until;
    const wraps = await s.pool.query(relays, filter).catch(() => [] as NostrEvent[]);
    const allIds = wraps.map((w) => w.id);
    const uncached = wraps.filter((w) => !mem.has(w.id));
    // Only offer older paging when the window came back FULL (more likely exists); a
    // short window = everything seen, so no sentinel and no wasted extra round-trip.
    const cursor = thread && wraps.length >= THREAD_WINDOW ? Math.min(...wraps.map((w) => w.created_at)) - 1 : null;
    // Legacy NIP-04: only when the client can decrypt it (nip04 cap) and not while paging older
    // (kind-4 isn't windowed). The queue rides the chain to a 3rd batch step after the NIP-17 layers.
    const lg = nip04 && until == null ? await fetchLegacy(s, view, peer) : { queue: [], ids: [] };
    const chainId = putChain({
        kind: 'sync', view, warm, peer, older: until != null, cursor, allIds: [...allIds, ...lg.ids],
        l1: uncached.map((w) => ({ id: w.id, at: w.created_at })),
        l2: [], legacy: lg.queue, expires: Date.now() + CHAIN_TTL_MS,
    });
    return { chainId, items: uncached.map((w) => ({ pubkey: w.pubkey, ciphertext: w.content })) };
}

/** The pending legacy (kind-4) decrypt batch for a chain, or null if none. Peeks (doesn't drop)
 * the chain - called at the NIP-17 terminal to decide whether a 3rd (nip04) step is needed. */
export function legacyBatch(chainId: string): DecryptItem[] | null {
    const chain = takeSync(chainId);
    if (!chain || !chain.legacy.length) return null;
    return chain.legacy.map((l) => ({ pubkey: l.peer, ciphertext: l.ciphertext }));
}

/** Apply the nip04 results: cache each decrypted kind-4 as a legacy message. Caller finalizes. */
export function applyLegacy(s: Session, chainId: string, results: BatchResult[]): void {
    const chain = takeSync(chainId);
    if (!chain) return;
    const me = s.me!;
    chain.legacy.forEach((l, i) => {
        const r = results[i];
        if (!r || !r.ok) { memSet(l.id, { kind: 'drop' }); return; }
        memSet(l.id, { kind: 'msg', owner: me, peer: l.peer, from: l.from, at: l.at, text: String(r.value), legacy: true });
    });
    chains.set(chainId, chain);
}

/** Apply the layer-1 seals: triage each (muted -> drop at 1 decrypt; you/follow/the
 * opened peer -> needs layer-2; stranger -> deferred Request). Returns the layer-2
 * decrypt batch (seal content -> rumor) for the known set, or null when there's none
 * left to decrypt (caller finalizes). */
export async function applySeals(s: Session, chainId: string, seals: BatchResult[]): Promise<{ items: DecryptItem[] } | null> {
    const chain = takeSync(chainId);
    if (!chain) return null;
    const me = s.me!;
    const muted = mutedPubkeys(s);
    const items: DecryptItem[] = [];
    chain.l2 = [];
    for (let i = 0; i < chain.l1.length; i++) {
        // A cold sync can carry the whole wrap window (up to 500 seals) in ONE batch, and each
        // verifyEvent below is a synchronous schnorr verify - back to back that's a multi-second
        // event-loop stall on small boxes. Yield every 128, like the zap-receipt verifier.
        if (i && (i & 127) === 0) await new Promise<void>((r) => setImmediate(r));
        const w = chain.l1[i]!;
        const r = seals[i];
        if (!r || !r.ok) { memSet(w.id, { kind: 'drop' }); continue; }
        let seal: NostrEvent | null = null;
        // NIP-59 MUST: beyond shape, verify the seal's SIGNATURE - the seal is the only signed
        // layer, so it's what proves the sender the rumor will claim. verifyEvent also pins kind 13.
        try { const j = JSON.parse(String(r.value)); if (j && j.kind === 13 && typeof j.pubkey === 'string' && typeof j.content === 'string' && HEX64.test(j.pubkey) && verifyEvent(j as never)) seal = j; } catch { /* drop */ }
        if (!seal) { memSet(w.id, { kind: 'drop' }); continue; }
        const sender = seal.pubkey;
        if (sender !== me && muted.has(sender)) { memSet(w.id, { kind: 'drop' }); continue; }
        // Queue EVERYONE (non-muted) for layer-2 - not just follows. A private reply is
        // indistinguishable from a DM until the rumor is decrypted, and strangers reply privately to
        // public notes, so we can't defer them; applyRumors triages by the decrypted inner kind.
        chain.l2.push({ id: w.id, sealPubkey: sender });
        items.push({ pubkey: sender, ciphertext: seal.content });
    }
    chains.set(chainId, chain); // keep alive for layer-2 / finalize
    return items.length ? { items } : null;
}

/** Apply the layer-2 rumors: validate the sender can't be spoofed, then cache each
 * decrypted message. The caller finalizes the render after this. */
export function applyRumors(s: Session, chainId: string, rumors: BatchResult[]): void {
    const chain = takeSync(chainId);
    if (!chain) return;
    const me = s.me!;
    const follows = new Set(s.followsRoute?.authors ?? []);
    chain.l2.forEach((w, i) => {
        const r = rumors[i];
        if (!r || !r.ok) { memSet(w.id, { kind: 'drop' }); return; }
        let rumor: Rumor | null = null;
        try { rumor = rumorFromSeal(JSON.parse(String(r.value)), w.sealPubkey); } catch { /* drop */ }
        if (!rumor) { memSet(w.id, { kind: 'drop' }); return; }
        if (rumor.kind === KIND_PRIVATE_REPLY) { // private reply → keyed by parent note, shown from anyone
            const parent = replyParent({ tags: rumor.tags } as NostrEvent)?.id;
            memSet(w.id, parent ? { kind: 'reply', owner: me, parentId: parent, id: rumor.id, from: rumor.pubkey, at: rumor.created_at, text: rumor.content, tags: rumor.tags } : { kind: 'drop' });
            return;
        }
        if (rumor.kind === KIND_DM) { // DM: you/a follow/the open peer → message, else a deferred request
            const known = rumor.pubkey === me || follows.has(rumor.pubkey) || rumor.pubkey === chain.peer;
            memSet(w.id, known ? { kind: 'msg', owner: me, peer: peerOf(rumor, me), from: rumor.pubkey, at: rumor.created_at, text: rumor.content } : { kind: 'request', owner: me, peer: rumor.pubkey, at: rumor.created_at });
            return;
        }
        memSet(w.id, { kind: 'drop' }); // kind 7 reactions etc. - not handled yet
    });
    chains.set(chainId, chain);
}

/** Render payload from the cache for a completed sync, then drop the chain. For a thread
 * it returns just this window's messages + the older-paging cursor and whether this was a
 * scroll-up (prepend) page. */
export function finalizeSync(s: Session, chainId: string): { view: SyncChain['view']; peer?: string; inbox?: DmInbox; messages?: DmMessage[]; cursor?: number | null; older?: boolean } | null {
    const chain = takeSync(chainId);
    if (!chain) return null;
    const me = s.me!;
    dropChain(chainId);
    if (chain.view === 'thread' && chain.peer) return { view: 'thread', peer: chain.peer, messages: threadMessages(chain.allIds, chain.peer, me), cursor: chain.cursor, older: chain.older };
    const inbox = aggregate(chain.allIds, me);
    listMem.set(me, { inbox, at: Date.now() }); // cache the list so quick re-visits skip the chain
    return { view: chain.view, inbox };
}

// --- the send chain (encrypt-batch -> sign-batch -> local wrap + publish) ---------

/** Begin a send: build the kind-14 rumor, return the encrypt batch (rumor -> seal
 * ciphertext, to the peer AND to self) plus a chainId. Null on empty text. */
export function beginSend(s: Session, peer: string, text: string): { chainId: string; items: { pubkey: string; plaintext: string }[] } | null {
    const body = text.trim();
    if (!body) return null;
    const rumor = buildRumor(s.me!, [peer], body);
    const targets = [peer, s.me!];
    const plaintext = JSON.stringify(rumor);
    const chainId = putChain({ kind: 'send', peer, rumor, targets, text: body, expires: Date.now() + CHAIN_TTL_MS });
    return { chainId, items: targets.map((t) => ({ pubkey: t, plaintext })) };
}

/** Apply the seal ciphertexts: build the two unsigned kind-13 seal templates for the
 * sign batch. Null if either encrypt failed. */
export function sealStep(s: Session, chainId: string, results: BatchResult[]): { templates: UnsignedEvent[] } | null {
    const chain = takeSend(chainId);
    if (!chain) return null;
    const ciphers = chain.targets.map((_, i) => (results[i]?.ok ? String((results[i] as { value: unknown }).value) : null));
    if (ciphers.some((c) => c == null)) { dropChain(chainId); return null; }
    chains.set(chainId, chain);
    return { templates: ciphers.map((c) => sealTemplate(s.me!, c!)) };
}

/** Apply the signed seals for a DM send: wrap + publish, cache our own copy, and return the optimistic
 * bubble. Null if a seal didn't verify. (Private-reply sends use wrapPrivateReplyStep below.) */
export async function wrapStep(s: Session, chainId: string, results: BatchResult[]): Promise<DmMessage | null> {
    const chain = takeSend(chainId);
    if (!chain) return null;
    dropChain(chainId);
    const me = s.me!;
    const selfId = await finalizeAndPublish(s, chain, results);
    if (!selfId) return null;
    memSet(selfId, { kind: 'msg', owner: me, peer: chain.peer, from: me, at: chain.rumor.created_at, text: chain.text });
    listMem.delete(me); // the new message changes the conversation list
    return { id: selfId, from: me, at: chain.rumor.created_at, text: chain.text };
}

/** Shared seal-verify + wrap + publish. Returns the self-wrap id (our kept copy), or null on failure. */
async function finalizeAndPublish(s: Session, chain: SendChain, results: BatchResult[]): Promise<string | null> {
    const me = s.me!;
    const seals = chain.targets.map((_, i) => {
        const r = results[i];
        if (!r || !r.ok) return null;
        const ev = coerceEvent(r.value);
        return ev && ev.pubkey === me && ev.kind === KIND_SEAL ? ev : null;
    });
    const sealPeer = seals[0];
    const sealSelf = seals[1];
    if (!sealPeer || !sealSelf) return null;
    try {
        const toPeer = finalizeWrap(sealPeer, chain.peer);
        const toSelf = finalizeWrap(sealSelf, me);
        await publishWrapPair(s, chain.peer, toPeer, toSelf);
        return toSelf.id;
    } catch (e) { console.warn('[dms-nip07] send failed:', (e as Error)?.message ?? e); return null; }
}

/** Begin a private-reply send (nip07): the kind:1 reply rumor is built server-side (NIP-10 tags from the
 * compose pipeline); we wrap it to the note `author` AND self. Returns the encrypt batch + chainId. */
export function beginPrivateReplySend(s: Session, author: string, parentId: string, baseTags: string[][], content: string): { chainId: string; items: { pubkey: string; plaintext: string }[] } | null {
    const body = content.trim();
    if (!body) return null;
    const rumor = buildPrivateReplyRumor(s.me!, baseTags, body);
    const targets = [author, s.me!];
    const plaintext = JSON.stringify(rumor);
    const chainId = putChain({ kind: 'send', peer: author, rumor, targets, text: body, replyParent: parentId, expires: Date.now() + CHAIN_TTL_MS });
    return { chainId, items: targets.map((t) => ({ pubkey: t, plaintext })) };
}

/** Apply the signed seals for a private reply: wrap + publish, cache our own copy keyed by the parent
 * note, and return it as a PrivateReply for optimistic in-thread render. Null if a seal didn't verify. */
export async function wrapPrivateReplyStep(s: Session, chainId: string, results: BatchResult[]): Promise<PrivateReply | null> {
    const chain = takeSend(chainId);
    if (!chain || !chain.replyParent) return null;
    dropChain(chainId);
    const me = s.me!;
    const selfId = await finalizeAndPublish(s, chain, results);
    if (!selfId) return null;
    memSet(selfId, { kind: 'reply', owner: me, parentId: chain.replyParent, id: chain.rumor.id, from: me, at: chain.rumor.created_at, text: chain.text, tags: chain.rumor.tags });
    return { id: chain.rumor.id, parent: chain.replyParent, from: me, at: chain.rumor.created_at, content: chain.text, tags: chain.rumor.tags };
}

// --- the quiet unread dot (nip07) ------------------------------------------------

/** Are there recent wraps `#p=me` we haven't decrypted yet? Query-only (no decrypt),
 * so it's light enough to poll. Opening Messages decrypts+caches, clearing the dot. */
export async function hasUnprocessedWrapsNip07(s: Session): Promise<boolean> {
    if (!signsOnClient(s) || !s.me) return false;
    const relays = await myDmReadRelays(s);
    const wraps = await s.pool.query(relays, { kinds: [KIND_GIFTWRAP], '#p': [s.me], limit: 60 }).catch(() => [] as NostrEvent[]);
    return wraps.some((w) => !mem.has(w.id));
}
