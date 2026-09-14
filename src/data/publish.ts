// Composing + publishing notes/replies. Split into sign (the remote-signer
// round-trip the UI waits on) and publish (background relay fan-out).

import type { Pool } from './pool.ts';
import type { Signer } from './signer.ts';
import type { NostrEvent, RelayList, UnsignedEvent } from '../nostr/types.ts';
import { INDEXER_RELAYS, writeRelaysFor } from '../nostr/nip65.ts';
import { fetchRelayLists } from './relays.ts';
import { fetchEvent } from './feeds.ts';
import { isHex64 } from '../nostr/tags.ts';
import { KIND_POLL, generateOptionId, buildPollTags } from '../nostr/nip88.ts';
import { KIND_ARTICLE } from '../nostr/nip23.ts';
import { KIND_PICTURE, firstCaptionLine } from '../nostr/nip68.ts';
import type { PollType } from '../nostr/nip88.ts';
import { tokenize } from '../nostr/content.ts';

const MAX_INBOX_RELAYS = 4;
const MAX_ANCESTOR_P = 20; // cap on p-tags copied from a (untrusted) parent event - anti mention-spam + bloat

/** Sanitize a relay hint carried over from another user's event: keep only a plausible, short ws(s) URL. */
const hintOf = (h?: string): string => (h && h.length <= 200 && /^wss?:\/\//i.test(h) ? h : '');

/** A no-op "signer" that returns the unsigned template verbatim - for capturing the event a signing
 * flow WOULD produce (e.g. the nip07 path, which signs in the browser) without actually signing. */
export const captureSigner: Signer = { signEvent: async (t: UnsignedEvent) => t as unknown as NostrEvent } as unknown as Signer;

export interface ReplyTo { id: string; pubkey?: string; kind?: number }
export interface QuoteRef { id: string; pubkey?: string; relays?: string[]; address?: string }

/** A NIP-22 reference: an article (address + kind 30023) or a comment (id + kind 1111). */
export interface CommentRef { kind: number; pubkey: string; address?: string; id?: string }
export interface CommentTarget { root: CommentRef; parent: CommentRef }

export interface Prepared {
    signed: NostrEvent;
    isReply: boolean;
    writeTargets: string[];  // your outbox
    inboxTargets: string[];  // recipient's inbox (replies)
}

export interface DeliveryReport { url: string; ok: boolean }

/** Build + sign a note (or reply). Resolves the recipient's relays for the NIP-10
 * hints + inbox delivery. Does NOT publish - returns what publishSigned needs. */
export async function signNote(
    signer: Signer,
    pool: Pool,
    me: string,
    myRelays: RelayList,
    { content, replyTo = null, quote = null, imeta = [], contentWarning = null, createdAt }:
        { content: string; replyTo?: ReplyTo | null; quote?: QuoteRef | null; imeta?: string[][]; contentWarning?: string | null; createdAt?: number },
): Promise<Prepared> {
    const tags: string[][] = [];
    const inboxRelays: string[] = [];
    if (contentWarning !== null) tags.push(contentWarning ? ['content-warning', contentWarning] : ['content-warning']);
    for (const m of imeta) tags.push(m); // NIP-92 metadata for uploaded media

    // NIP-18 quote: the nostr:nevent/naddr already sits in the content (rendered
    // as a quote/article card); the reference tag + author `p` make it a quote. The
    // q-tag's relay hint comes from the nevent if it carried one, else (the usual case -
    // our own neventFor emits none) we resolve the quoted author's NIP-65 write relay,
    // exactly as the reply path does below, so the quote is self-resolving for clients.
    const quoteRef = quote?.address || quote?.id;
    // The quote's and reply's relay-hint targets are independent: resolve both lists in ONE batched
    // call, instead of the two sequential awaits that stacked two indexer round-trips onto a
    // quote-reply to two uncached authors.
    const hintPks = [...new Set([
        quoteRef && quote!.pubkey && !quote!.relays?.[0] ? quote!.pubkey : null,
        replyTo?.id ? replyTo.pubkey : null,
    ].filter((p): p is string => !!p))];
    const hintLists = hintPks.length
        ? await fetchRelayLists(pool, INDEXER_RELAYS, hintPks).catch(() => new Map<string, RelayList>())
        : new Map<string, RelayList>();
    if (quoteRef) {
        let qWriteHint = quote!.relays?.[0] ?? '';
        let qReadHint = '';
        if (quote!.pubkey && !qWriteHint) {
            const list = hintLists.get(quote!.pubkey);
            qWriteHint = list?.write[0] ?? '';
            qReadHint = list?.read[0] ?? '';
        }
        tags.push(['q', quoteRef, qWriteHint, quote!.pubkey ?? '']);
        if (quote!.pubkey) tags.push(qReadHint ? ['p', quote!.pubkey, qReadHint] : ['p', quote!.pubkey]);
    }

    if (replyTo?.id) {
        const recipientList: RelayList | null = replyTo.pubkey ? hintLists.get(replyTo.pubkey) ?? null : null;
        const writeHint = recipientList?.write[0] ?? '';
        const readHint = recipientList?.read[0] ?? '';

        // NIP-10 (marked tags): fetch the parent so the reply carries the thread ROOT and the ancestry,
        // not just the immediate parent. Via the cached fetchEvent (the parent was usually just rendered,
        // so this is a cache hit + outbox routing); best-effort, degrades to a lone 'reply' e-tag on a miss.
        const parent = await fetchEvent(pool, replyTo.id, recipientList?.write ?? [], replyTo.pubkey, { maxWait: 3000 }).catch(() => null);
        const parentEtags = parent?.tags.filter((t) => t[0] === 'e') ?? [];
        // The parent's root: its explicit 'root' marker, else its first non-'mention' e-tag (positional
        // scheme), taken only if it's a valid event id. No valid root → the parent IS the thread root.
        const rootRef = parentEtags.find((t) => t[3] === 'root') ?? parentEtags.find((t) => t[3] !== 'mention') ?? null;
        const rootId = rootRef && isHex64(rootRef[1] ?? '') ? rootRef[1]!.toLowerCase() : '';
        // A marked e-tag: ['e', id, relayHint, marker, authorPubkey?] - drop the trailing author when
        // unknown/invalid. Values copied from the parent are lowercased (NIP-01: hex is lowercase;
        // republishing an uppercase id verbatim would poison every filter downstream of our reply).
        const eTag = (id: string, hint: string, marker: string, author?: string): string[] =>
            author && isHex64(author) ? ['e', id.toLowerCase(), hint, marker, author.toLowerCase()] : ['e', id.toLowerCase(), hint, marker];
        if (rootId) {
            tags.push(eTag(rootId, hintOf(rootRef![2]), 'root', rootRef![4]));
            tags.push(eTag(replyTo.id, writeHint, 'reply', replyTo.pubkey));
        } else {
            // Parent is the root (marker 'root'), or unknown (fall back to legacy 'reply').
            tags.push(eTag(replyTo.id, writeHint, parent ? 'root' : 'reply', replyTo.pubkey));
        }

        // p-tags (NIP-10): the parent's participants + the parent's author, deduped, so the thread is
        // notified. The parent is another user's event, so its tags are UNTRUSTED: copy only valid hex
        // pubkeys, cap the count (anti mention-spam + bloat), and sanitize each carried relay hint.
        const pSeen = new Set<string>();
        // Lowercase at the one point every p-tag flows through - parent-copied and our own alike.
        const pushP = (pk?: string, hint = '') => { const k = pk?.toLowerCase(); if (k && !pSeen.has(k)) { tags.push(hint ? ['p', k, hint] : ['p', k]); pSeen.add(k); } };
        let ancestors = 0;
        for (const t of parent?.tags ?? []) {
            if (t[0] === 'p' && isHex64(t[1] ?? '') && ancestors < MAX_ANCESTOR_P) { pushP(t[1], hintOf(t[2])); ancestors++; }
        }
        pushP(replyTo.pubkey, readHint);

        const read = recipientList?.read ?? [];
        inboxRelays.push(...(read.length ? read : INDEXER_RELAYS).slice(0, MAX_INBOX_RELAYS));
    }

    // p-tag any @mentions in the content (so the mentioned users are notified).
    const tagged = new Set(tags.filter((t) => t[0] === 'p').map((t) => t[1]));
    for (const tok of tokenize(content)) {
        if (tok.t === 'mention' && !tagged.has(tok.pubkey)) { tags.push(['p', tok.pubkey]); tagged.add(tok.pubkey); }
    }

    const signed = await signer.signEvent({
        kind: 1,
        created_at: createdAt ?? Math.floor(Date.now() / 1000), // a scheduled post bakes in its future time
        tags,
        content,
        pubkey: me,
    });
    const myWrite = writeRelaysFor(myRelays);
    return { signed, isReply: !!replyTo?.id, writeTargets: myWrite, inboxTargets: inboxRelays };
}

/** Build + sign a NIP-68 picture post (kind:20): a title + caption (content) + the uploaded images as
 * NIP-92 `imeta` tags. Top-level only (no reply/quote); mirrors signNote's content-warning + @mention
 * handling and its scheduling (`createdAt`). No pool round-trip - a picture post resolves no recipient. */
export async function signPicture(
    signer: Signer,
    me: string,
    myRelays: RelayList,
    { title, content, imeta = [], contentWarning = null, createdAt }:
        { title: string; content: string; imeta?: string[][]; contentWarning?: string | null; createdAt?: number },
): Promise<Prepared> {
    const tags: string[][] = [];
    // NIP-68: a picture event carries a `title` tag (structural). If the composer's title is blank we
    // derive one from the caption's first non-empty line so the tag is always present + meaningful.
    tags.push(['title', title.trim() || firstCaptionLine(content)]);
    if (contentWarning !== null) tags.push(contentWarning ? ['content-warning', contentWarning] : ['content-warning']);
    for (const m of imeta) tags.push(m); // NIP-92 image metadata - the picture's payload
    // NIP-68 also surfaces each image's hash (`x`) + mime (`m`) as TOP-LEVEL tags (relay/client filtering),
    // mirroring the values already inside the imeta: one `x` per image, `m` deduped across them.
    const xs = new Set<string>(), mimes = new Set<string>();
    for (const im of imeta) {
        const x = im.find((v) => v.startsWith('x '))?.slice(2); if (x) xs.add(x);
        const mm = im.find((v) => v.startsWith('m '))?.slice(2); if (mm) mimes.add(mm);
    }
    for (const x of xs) tags.push(['x', x]);
    for (const mm of mimes) tags.push(['m', mm]);
    const tagged = new Set<string>();
    for (const tok of tokenize(content)) {
        if (tok.t === 'mention' && !tagged.has(tok.pubkey)) { tags.push(['p', tok.pubkey]); tagged.add(tok.pubkey); }
    }
    const signed = await signer.signEvent({
        kind: KIND_PICTURE,
        created_at: createdAt ?? Math.floor(Date.now() / 1000),
        tags,
        content,
        pubkey: me,
    });
    return { signed, isReply: false, writeTargets: writeRelaysFor(myRelays), inboxTargets: [] };
}

/** Build + sign a NIP-22 comment (kind:1111) - root scope in uppercase tags, the
 * immediate parent in lowercase. Used for article comments (top-level + nested). */
export async function signComment(
    signer: Signer,
    pool: Pool,
    me: string,
    myRelays: RelayList,
    { content, comment, contentWarning = null, imeta = [] }: { content: string; comment: CommentTarget; contentWarning?: string | null; imeta?: string[][] },
): Promise<Prepared> {
    const { root, parent } = comment;
    const lists = parent.pubkey ? await fetchRelayLists(pool, INDEXER_RELAYS, [parent.pubkey]).catch(() => new Map<string, RelayList>()) : new Map<string, RelayList>();
    const recipientList = lists.get(parent.pubkey) ?? null;
    const writeHint = recipientList?.write[0] ?? '';
    const readHint = recipientList?.read[0] ?? '';

    const tags: string[][] = [];
    if (contentWarning !== null) tags.push(contentWarning ? ['content-warning', contentWarning] : ['content-warning']);
    for (const m of imeta) tags.push(m); // NIP-92 media metadata, like signNote (comment replies can carry uploads)
    // root (uppercase). The author can be unknown (a degraded scope from commentTargetFor) - an
    // empty pubkey must not become a malformed P/p tag, so those are skipped, not emitted blank.
    tags.push(root.address ? ['A', root.address, writeHint] : ['E', root.id!, writeHint]);
    tags.push(['K', String(root.kind)]);
    if (root.pubkey) tags.push(['P', root.pubkey, readHint]);
    // parent (lowercase)
    tags.push(parent.address ? ['a', parent.address, writeHint] : ['e', parent.id!, writeHint]);
    tags.push(['k', String(parent.kind)]);
    if (parent.pubkey) tags.push(['p', parent.pubkey, readHint]);

    const signed = await signer.signEvent({ kind: 1111, created_at: Math.floor(Date.now() / 1000), tags, content, pubkey: me });
    const myWrite = writeRelaysFor(myRelays);
    const read = recipientList?.read ?? [];
    return { signed, isReply: true, writeTargets: myWrite, inboxTargets: (read.length ? read : INDEXER_RELAYS).slice(0, MAX_INBOX_RELAYS) };
}

/** Build + sign a poll (kind:1068). Votes are routed to your write relays (the
 * poll's `relay` tags), where readers will publish their kind:1018 responses. */
export async function signPoll(
    signer: Signer,
    me: string,
    myRelays: RelayList,
    { question, options, multiple, endsAt }:
        { question: string; options: string[]; multiple: boolean; endsAt: number | null },
): Promise<Prepared> {
    const myWrite = writeRelaysFor(myRelays);
    const opts = options.map((label) => ({ id: generateOptionId(), label }));
    const type: PollType = multiple ? 'multiple' : 'single';
    const tags = buildPollTags(opts, type, endsAt, myWrite);
    const signed = await signer.signEvent({
        kind: KIND_POLL,
        created_at: Math.floor(Date.now() / 1000),
        tags,
        content: question,
        pubkey: me,
    });
    return { signed, isReply: false, writeTargets: myWrite, inboxTargets: [] };
}

export interface ArticleFields {
    identifier: string;   // the `d` slug (stable across edits → updates, not duplicates)
    title: string;
    summary?: string;
    image?: string;
    topics?: string[];
    body: string;         // markdown
    publishedAt?: number; // preserved on edit; set to now on first publish
    createdAt?: number;   // a scheduled article bakes in its future broadcast time (created_at + published_at)
}

/** Build + sign a long-form article (NIP-23 kind:30023). */
export async function signArticle(signer: Signer, me: string, myRelays: RelayList, a: ArticleFields): Promise<Prepared> {
    const now = a.createdAt ?? Math.floor(Date.now() / 1000);
    const tags: string[][] = [['d', a.identifier], ['title', a.title]];
    if (a.summary) tags.push(['summary', a.summary]);
    if (a.image) tags.push(['image', a.image]);
    tags.push(['published_at', String(a.publishedAt ?? now)]);
    for (const t of a.topics ?? []) if (t) tags.push(['t', t]);
    const signed = await signer.signEvent({
        kind: KIND_ARTICLE,
        created_at: now,
        tags,
        content: a.body,
        pubkey: me,
    });
    const myWrite = writeRelaysFor(myRelays);
    return { signed, isReply: false, writeTargets: myWrite, inboxTargets: [] };
}

/** Publish a signed note to your write relays + recipient inbox. Succeeds if at
 * least one of your write relays accepts. */
export async function publishSigned(pool: Pool, prepared: Prepared): Promise<{ write: DeliveryReport[]; inbox: DeliveryReport[] }> {
    const { signed, writeTargets, inboxTargets } = prepared;
    const targets = [...writeTargets, ...inboxTargets];
    // A relay shared by your writes and the recipient's inbox (common) must only be sent once:
    // nostr-tools rejects a duplicate url outright, which mis-reported the inbox delivery as
    // failed (+ a warn) on every such reply. Publish per unique relay, report per role.
    const unique = [...new Set(targets)];
    const results = await pool.publish(unique, signed);
    const okByUrl = new Map(unique.map((u, i) => [u, results[i]?.status === 'fulfilled']));
    const report: DeliveryReport[] = targets.map((url) => ({ url, ok: okByUrl.get(url) ?? false }));
    report.forEach((r) => { if (!r.ok) console.warn(`[publish] ${r.url} rejected`); });
    const write = report.slice(0, writeTargets.length);
    const inbox = report.slice(writeTargets.length);
    if (!write.some((r) => r.ok)) throw new Error('Failed to publish to any of your write relays');
    return { write, inbox };
}
