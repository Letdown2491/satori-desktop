// NIP-46 remote signer ("bunker") - transport + signing ONLY. Hand-rolled (not
// nostr-tools' BunkerSigner) so it can send the `logout` method. Knows nothing
// about feeds, profiles, or relay lists - just connect, sign, logout.

import { generateSecretKey, getPublicKey, finalizeEvent } from 'nostr-tools/pure';
import { getConversationKey, encrypt, decrypt } from 'nostr-tools/nip44';
import type { Pool } from './pool.ts';
import type { NostrEvent, UnsignedEvent } from '../nostr/types.ts';
import { HEX64 } from '../nostr/tags.ts';

const NIP46_KIND = 24133;
// No bunker traffic for this long ⇒ the transport socket may be a zombie (a
// resume from sleep kills it silently) - rebuild it before/around a request.
const STALE_MS = 90_000;

// NIP-46 client identity + the methods we use, sent in the `connect` request (the bunker:// token carries
// no identity), so the signer shows "Satori" on the approval and can pre-authorize these in ONE grant
// instead of prompting per action. Bare `sign_event` requests signing of all kinds (Satori signs many:
// notes, reactions, lists, gift-wrapped DMs, profile...); a signer that doesn't honor it just falls back
// to per-action prompts (today's behavior), so requesting it has no downside.
const CLIENT_METADATA = JSON.stringify({
    name: 'Satori',
    url: 'https://github.com/Letdown2491/satori',
    image: 'https://i.nostr.build/resp/1080p/cQ3Z0JCOpgTHuaLA.png',
});
const REQUESTED_PERMS = 'sign_event,nip44_encrypt,nip44_decrypt,nip04_decrypt';

const toHex = (bytes: Uint8Array) => Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
const fromHex = (hex: string) => Uint8Array.from(hex.match(/.{1,2}/g)!.map((b) => parseInt(b, 16)));

export interface Session {
    secretHex: string;
    remotePubkey: string;
    relays: string[];
}

interface BunkerUri {
    remotePubkey: string;
    relays: string[];
    secret: string | undefined;
}

/** Parse a bunker:// URI by hand (relay values contain unescaped "://"). */
export function parseBunkerUri(uri: string): BunkerUri {
    const trimmed = uri.trim();
    if (!trimmed.startsWith('bunker://')) throw new Error('Not a bunker:// URI');
    const [pubkey, query = ''] = trimmed.slice('bunker://'.length).split('?');
    if (!pubkey || !HEX64.test(pubkey)) {
        throw new Error('bunker URI is missing a valid 64-char hex signer pubkey');
    }
    const params = new URLSearchParams(query);
    const relays = params.getAll('relay');
    if (relays.length === 0) throw new Error('bunker URI has no relay= parameter');
    return { remotePubkey: pubkey.toLowerCase(), relays, secret: params.get('secret') || undefined };
}

type Pending = { resolve: (v: unknown) => void; reject: (e: Error) => void };

/** The signing surface the app depends on - satisfied by the NIP-46 bunker
 * (below) and the NIP-07 browser extension (data/nip07.ts). Sign-in/resume use
 * the concrete classes; everything else only needs this. */
export interface Signer {
    signEvent(template: UnsignedEvent): Promise<NostrEvent>;
    getUserPubkey(): Promise<string>;
    nip44Encrypt(peerPubkey: string, plaintext: string): Promise<string>;
    nip44Decrypt(peerPubkey: string, ciphertext: string): Promise<string>;
    nip04Decrypt(peerPubkey: string, ciphertext: string): Promise<string>;
    logout(): Promise<unknown>;
    reconnect(): void;
    readonly transportRelays: string[];
    exportSession(): Record<string, unknown>;
    primeUserPubkey(pubkey: string): void;
}

export class BunkerSigner implements Signer {
    readonly clientSecret: Uint8Array;
    readonly clientPubkey: string;
    private remotePubkey = '';
    private relays: string[] = [];
    private convKey: Uint8Array | null = null;
    private userPubkey: string | null = null;
    private readonly pending = new Map<string, Pending>();
    private sub: { close: () => void } | null = null;
    private lastResponseAt = Date.now(); // last time the bunker channel proved alive
    onAuthUrl: ((url: string) => void) | null = null;

    private readonly pool: Pool;

    // Parameter properties aren't erasable, so we assign by hand - this project
    // runs TypeScript directly via Node's type-stripping (no compile step).
    constructor(pool: Pool, savedSecretHex: string | null = null) {
        this.pool = pool;
        this.clientSecret = savedSecretHex ? fromHex(savedSecretHex) : generateSecretKey();
        this.clientPubkey = getPublicKey(this.clientSecret);
    }

    /** The bunker transport relays (from the URI) - excluded from content relays. */
    get transportRelays(): string[] { return this.relays; }

    async connect(bunkerUri: string, onStatus: (m: string) => void = () => {}): Promise<void> {
        const { remotePubkey, relays, secret } = parseBunkerUri(bunkerUri);
        this.remotePubkey = remotePubkey;
        this.relays = relays;
        this.convKey = getConversationKey(this.clientSecret, remotePubkey);
        onStatus('Subscribing to bunker relays…');
        this._subscribe();
        onStatus('Sending connect request. Approve it in your signer…');
        // Params are positional per NIP-46: [signer_pubkey, secret, requested_perms, client_metadata].
        // perms must hold position 3 (empty would still need to be present) so the metadata lands in 4.
        const result = await this.request('connect', [remotePubkey, secret ?? '', REQUESTED_PERMS, CLIENT_METADATA], 120_000);
        if (result !== 'ack') throw new Error(`Unexpected connect result: ${JSON.stringify(result)}`);
    }

    resume({ remotePubkey, relays }: Session): void {
        this.remotePubkey = remotePubkey;
        this.relays = relays;
        this.convKey = getConversationKey(this.clientSecret, remotePubkey);
        this._subscribe();
    }

    exportSession(): Record<string, unknown> {
        return { method: 'bunker', secretHex: toHex(this.clientSecret), remotePubkey: this.remotePubkey, relays: this.relays };
    }

    private _subscribe(): void {
        this.sub = this.pool.subscribe(
            this.relays,
            { kinds: [NIP46_KIND], '#p': [this.clientPubkey] },
            { onevent: (event) => this.handleResponse(event) },
        );
    }

    /** Re-establish the bunker subscription on a fresh socket. The transport
     * socket dies (silently) across a resume from sleep, so responses stop
     * arriving; this rebuilds it. No-op if there's no active session. */
    reconnect(): void {
        if (!this.remotePubkey) return;
        try { this.sub?.close(); } catch { /* ignore */ }
        this.sub = null;
        this._subscribe();
    }

    /** Force a clean bunker channel: drop the (possibly zombie) transport sockets
     * so they rebuild fresh, then re-subscribe. Used to self-heal a stale signer. */
    private recover(): void {
        if (!this.remotePubkey) return;
        this.pool.recycle(this.relays);
        this.lastResponseAt = Date.now(); // optimistic: assume the rebuilt channel is live
        this.reconnect();
    }

    /** Seed the user pubkey from a cached session so resume can skip the signer
     * round-trip. The bunker-held key doesn't change across reloads. */
    primeUserPubkey(pubkey: string): void { this.userPubkey = pubkey; }

    async getUserPubkey(): Promise<string> {
        if (!this.userPubkey) this.userPubkey = await this.request('get_public_key', []) as string;
        return this.userPubkey;
    }

    /** Ask the bunker to sign an event template; returns the signed event. */
    async signEvent(template: UnsignedEvent): Promise<NostrEvent> {
        const result = await this.request('sign_event', [JSON.stringify(template)]);
        return typeof result === 'string' ? JSON.parse(result) : result as NostrEvent;
    }

    /** NIP-44 encrypt/decrypt via the bunker (for NIP-51 private list items, etc.).
     * `peerPubkey` is the counterparty - your own pubkey for self-encryption. */
    async nip44Encrypt(peerPubkey: string, plaintext: string): Promise<string> {
        return await this.request('nip44_encrypt', [peerPubkey, plaintext]) as string;
    }
    async nip44Decrypt(peerPubkey: string, ciphertext: string): Promise<string> {
        return await this.request('nip44_decrypt', [peerPubkey, ciphertext]) as string;
    }
    /** Legacy NIP-04 decrypt (read-only support for kind:4 DMs from old clients). */
    async nip04Decrypt(peerPubkey: string, ciphertext: string): Promise<string> {
        return await this.request('nip04_decrypt', [peerPubkey, ciphertext]) as string;
    }

    /** Best-effort logout: publish the `logout` request and tear down. Per NIP-46
     * this is a courtesy hint (not a security boundary), so we don't wait for the
     * ack - just get it on the wire, then close. */
    async logout(): Promise<void> {
        try {
            if (this.remotePubkey && this.convKey) {
                const content = encrypt(JSON.stringify({ id: crypto.randomUUID(), method: 'logout', params: [] }), this.convKey);
                const ev = finalizeEvent({
                    kind: NIP46_KIND, created_at: Math.floor(Date.now() / 1000),
                    tags: [['p', this.remotePubkey]], content,
                }, this.clientSecret) as NostrEvent;
                await this.pool.publish(this.relays, ev);
            }
        } catch { /* best effort */ }
        this.close();
    }

    /** Encrypt + publish a NIP-46 request; returns a promise for its correlated
     * response. Self-heals a stale transport (resume from sleep): rebuilds the
     * channel proactively if it's been quiet too long, and once more on timeout
     * before giving up. Signing is idempotent on our side - we publish only the
     * response we actually receive - so a retried sign can't double-post. */
    request(method: string, params: string[], timeoutMs = 30_000): Promise<unknown> {
        // Proactive: if the channel has been silent long enough to be stale, rebuild
        // it before publishing rather than eating a full timeout first.
        if (this.remotePubkey && Date.now() - this.lastResponseAt > STALE_MS) this.recover();
        return this.send(method, params, timeoutMs, true);
    }

    /** One request attempt. On timeout, recover the transport and retry once. */
    private send(method: string, params: string[], timeoutMs: number, retry: boolean): Promise<unknown> {
        const id = crypto.randomUUID();
        const content = encrypt(JSON.stringify({ id, method, params }), this.convKey!);
        const reqEvent = finalizeEvent({
            kind: NIP46_KIND,
            created_at: Math.floor(Date.now() / 1000),
            tags: [['p', this.remotePubkey]],
            content,
        }, this.clientSecret) as NostrEvent;

        const promise = new Promise<unknown>((resolve, reject) => {
            const timer = setTimeout(() => {
                if (!this.pending.has(id)) return;
                this.pending.delete(id);
                if (retry) { this.recover(); this.send(method, params, timeoutMs, false).then(resolve, reject); }
                else reject(new Error(`Request timed out: ${method}`));
            }, timeoutMs);
            this.pending.set(id, {
                resolve: (v) => { clearTimeout(timer); resolve(v); },
                reject: (e) => { clearTimeout(timer); reject(e); },
            });
        });

        this.pool.publish(this.relays, reqEvent).then((results) => {
            if (results.every((r) => r.status === 'rejected')) {
                const pending = this.pending.get(id);
                if (pending) { this.pending.delete(id); pending.reject(new Error(`Failed to publish ${method} to any relay`)); }
            }
        });

        return promise;
    }

    private handleResponse(event: NostrEvent): void {
        if (event.pubkey !== this.remotePubkey) return; // only the signer
        let resp: { id: string; result?: unknown; error?: string };
        try {
            resp = JSON.parse(decrypt(event.content, this.convKey!));
        } catch {
            return; // not a message we can read
        }
        this.lastResponseAt = Date.now(); // any readable message proves the channel is live
        if (resp.result === 'auth_url') { // approve out-of-band, then real reply on same id
            if (this.onAuthUrl && typeof resp.error === 'string') this.onAuthUrl(resp.error);
            return;
        }
        const pending = this.pending.get(resp.id);
        if (!pending) return;
        this.pending.delete(resp.id);
        if (resp.error) pending.reject(new Error(resp.error));
        else pending.resolve(resp.result);
    }

    close(): void {
        try { this.sub?.close(); } catch { /* ignore */ }
    }
}
