// Relay access. Wraps nostr-tools' SimplePool and applies the .onion→Tor-proxy
// rewrite (toPoolUrl) centrally, so no call site has to remember it. This is the
// one place the app talks to relays.

import { SimplePool } from 'nostr-tools/pool';
import type { Filter } from 'nostr-tools';
import { toPoolUrls, toPoolUrl, fromPoolUrl, relayKey } from '../nostr/nip65.ts';
import { relaysViaTor } from '../privacy.ts';
import { localReadMode, localWriteMode, localRelayUrl, isLocalRelayUrl, localFetchMissing } from '../local-relay.ts';
import { recordSeen } from './seen-relays.ts';
import { recordLatency, recordReadDenial, relayBudget } from './relay-latency.ts';
import { isExpired } from '../nostr/nip40.ts';
import { recordDeletion, isDeletedEvent, shouldCheckDeletion } from './deletions.ts';
import { isAddressable, coordinateOf } from '../nostr/tags.ts';
import type { NostrEvent, UnsignedEvent } from '../nostr/types.ts';

/** True if at least one relay accepted a publish (a settled fan-out succeeds on any acceptance). Lives
 * here (the pool, the one place we publish) so publishers can import it without a publish.ts cycle. */
export const anyAccepted = (results: PromiseSettledResult<unknown>[]): boolean => results.some((r) => r.status === 'fulfilled');

export interface SubHandlers {
    onevent?: (e: NostrEvent) => void;
    oneose?: () => void;
}

// Read policy opts, layered on the private-relay routing:
//  - direct:   query EXACTLY these relays, ignore the private-relay policy (e.g. trust assertions).
//  - complete: union the private relay in even in Only mode - for your OWN replaceable lists, so a read
//              sees every version (private-only writes AND your NIP-65 copies) and a modify can't clobber.
//  - resolve:  in Only mode + "fetch missing" ON, try the private relay first, then fall back to these
//              (real) relays for what it lacks (other people's profiles / notes / quoted+replied events).
export interface ReadOpts { fast?: boolean; profile?: boolean; budget?: 'page' | 'adaptive'; direct?: boolean; complete?: boolean; resolve?: boolean; track?: QueryTrack }

/** Caller-supplied out-param: set truncated=true when the query hit the hard cap before every
 * relay EOSE'd, i.e. the result may be missing events that exist. Lets a read-modify-write
 * caller (own lists) tell "confirmed absent" from "didn't load in time". */
export interface QueryTrack { truncated: boolean }

const normUrl = relayKey; // the shared light key-normalizer (nip65)

// NIP-46 bunker RPC transport kind. Publishes of this kind are the signer channel (connect / sign
// requests) and must reach EXACTLY the bunker's relays - the local-relay write policy must never
// redirect or mirror them, or bunker signing breaks. Kept local to avoid a pool<->signer import.
const NIP46_KIND = 24133;
const GIFTWRAP_KIND = 1059; // NIP-59 gift wrap (NIP-17 DMs + private replies): strict recipient-inbox targets

// A relay CLOSED our REQ with an explicit refusal: write-only blasters ("does not accept REQs"),
// auth walls we deliberately won't sign for (stranger relays), paywalls. Feeds the read-dead
// memory so routing stops spending sockets on them. Matched against the relay's CLOSED reason
// only - our own close() reasons never look like this.
const READ_DENIAL_RE = /denied|not accept|restricted|blocked|forbidden|auth-required|payment/i;

// Reliability caps so one slow/dead relay can't stall a render: a query returns
// whatever arrived by the deadline instead of waiting for EVERY relay to EOSE.
// Healthy relays answer well under these, so a complete result is the norm - the
// cap only bites when a relay is actually sick (exactly when you want to cut it).
const LIST_MAX_WAIT = 4000;     // multi-event queries (feed/thread/profile/lists); also the DEFAULT/cold-start budget
const GET_MAX_WAIT = 6000;      // single-event get: null = a broken page, so more rope (id gets resolve on first hit; replaceable gets wait out EOSE for the newest version)
const CONNECT_MAX_WAIT = 4000;  // don't chase an unreachable relay's socket longer than this
// Adaptive per-relay budgets (feed reliability). `budget:'page'` = a TIGHT cap for the latency-critical first
// paint (the new-notes dot + scroll backfill what a fast paint misses). `budget:'adaptive'` scales a single
// relay's cap from LIST_MAX_WAIT up to *_CEILING by how budget-starved its latency profile says it is - used
// where latency is free/tolerable (the background dot poll, older-page scroll). Tunable against real data.
const PAGE_MAX_WAIT = 2000;     // first-paint hard cap (fast); slow relays fold in via the dot, not the paint
const LIST_CEILING = 8000;      // adaptive max (aggregator/runaway guard) for a single starved relay
const TOR_PAGE_MAX_WAIT = 6000;
const TOR_LIST_CEILING = 15000;
// Privacy Mode (Tor): building a Tor circuit cold takes seconds, well past the direct
// caps - so a COLD query (e.g. right after toggling on / recycling) would time out and
// return empty (a blank feed) before the relays even connect. Give Tor queries much
// more rope; once circuits are warm, relays still answer fast (the cap rarely bites).
const TOR_LIST_MAX_WAIT = 12000;
const TOR_GET_MAX_WAIT = 12000;
const TOR_CONNECT_MAX_WAIT = 11000;

// Sleep watchdog (see the `watchdog` field): tick every 30s; a tick that lands 60s+ after the last one
// means the event loop was frozen that long - a real OS suspend, not GC/load jitter (which is sub-second).
const WATCHDOG_TICK = 30_000;
const SLEEP_GAP = 60_000;

export class Pool {
    readonly raw = new SimplePool();
    // NIP-42 AUTH: sign the kind:22242 challenge - but only for relays in your own
    // list (authing to strangers' relays would broadcast your identity everywhere).
    private authSign: ((tmpl: UnsignedEvent) => Promise<NostrEvent>) | null = null;
    private authRelays = new Set<string>();
    // Local relay sockets we've NIP-42-authenticated this session (normalized urls). Only used by the
    // nip07 manual-auth flow (bunker auto-auths via automaticallyAuth). Cleared on recycle since a fresh
    // socket starts unauthed.
    private authedLocal = new Set<string>();
    // nip07 only: a read/get to the local relay hit `auth-required` and we couldn't sign it server-side.
    // Drives the re-auth prompt (bunker signs in the background, so it never sets this). Sticky until a
    // successful auth clears it.
    private localAuthObservedRequired = false;
    // Warm-up tracking for the Privacy Mode indicator: which relays are dialing and
    // which have settled (connected OR failed), so the widget shows real progress.
    private warming: { urls: Set<string>; settled: Set<string> } | null = null;
    // Sleep/suspend detection. A laptop resume leaves ZOMBIE sockets behind: the TCP connection is
    // dead but the WebSocket still reads as OPEN (half-open), so nostr-tools reuses it and every
    // subscribe/get hangs to maxWait and returns EMPTY - a silent blank feed, and the "fetch missing"
    // fallback quietly fetching nothing from the (zombie) real relays. The bunker signer already
    // self-heals its transport (signer.ts); the read pool didn't. We detect the suspend by wall-clock
    // DRIFT (see healIfSuspended): the process can't advance the clock while frozen, so a jump far past a
    // live loop's cadence means we were suspended - recycle every socket so the next read dials fresh.
    // Checked two ways: the background `watchdog` (proactive, works even on an idle daemon with no reads)
    // AND lazily at the top of each read (so the FIRST read after waking heals before it hangs, not on the
    // next tick). `lastTick` is the last time we observed the clock on a live loop. unref'd so the timer
    // never keeps the process alive.
    private lastTick = Date.now();
    private readonly watchdog: ReturnType<typeof setInterval>;

    constructor() {
        this.raw.maxWaitForConnection = CONNECT_MAX_WAIT;
        this.raw.trackRelays = true; // populate seenOn so we can learn which relays actually carry an author's events

        this.watchdog = setInterval(() => this.healIfSuspended(), WATCHDOG_TICK);
        this.watchdog.unref?.(); // don't hold the event loop open for the timer

        (this.raw as { automaticallyAuth?: unknown }).automaticallyAuth = (url: string) => {
            // Authenticate (NIP-42) to relays in your own list, PLUS the explicitly-configured local relay -
            // a private aggregator/outbox relay is the whole point of that feature and is typically auth-
            // required. The "don't auth to strangers" concern doesn't apply: you deliberately set it as yours.
            if (!this.authSign || !(this.authRelays.has(normUrl(url)) || isLocalRelayUrl(url))) return undefined;
            const sign = this.authSign;
            return (tmpl: UnsignedEvent) => sign(tmpl);
        };
    }

    /** Provide the auth signer + the relays we're willing to authenticate to. */
    setAuth(sign: (tmpl: UnsignedEvent) => Promise<NostrEvent>, relays: string[]): void {
        this.authSign = sign;
        this.setAuthRelays(relays);
    }
    setAuthRelays(relays: string[]): void {
        this.authRelays = new Set(toPoolUrls(relays).map(normUrl));
    }

    // --- nip07 NIP-42 auth for the local relay ---------------------------------
    // A private (auth-required) local relay needs NIP-42. Bunker signs the challenge server-side
    // (automaticallyAuth). nip07 can't sign in the background, so the browser signs a challenge once,
    // interactively, and we keep that authenticated socket for subsequent reads/writes.

    /** Open a FRESH socket to the local relay and return its NIP-42 challenge (null if it doesn't
     * challenge within the window / is unreachable). Recycles first so each attempt starts clean - no
     * stale challenge and no cached auth promise (nostr-tools caches relay.authPromise, even a rejected
     * one, so a retry on the same socket would just re-reject). */
    async localRelayChallenge(): Promise<string | null> {
        const url = localRelayUrl();
        if (!url) return null;
        try {
            const pool = toPoolUrl(url);
            const relay = await this.raw.ensureRelay(pool, { connectionTimeout: CONNECT_MAX_WAIT }) as unknown as { challenge?: string };
            // The live socket usually ALREADY carries a challenge - in only-mode the background feed reads
            // constantly provoke the (lazy) relay's AUTH. Read it directly; that's instant and robust under
            // the daemon's concurrency (recycling + re-provoking here raced with that traffic and timed out).
            // Fall back to a throwaway REQ to provoke one if the socket somehow has none yet (e.g. add-mode).
            if (!relay.challenge) {
                let sub: { close(): void } | undefined;
                try { sub = this.raw.subscribeMany([pool], { kinds: [1], limit: 1 } as never, { onevent() {}, oneose() {}, onclose() {} } as never) as { close(): void }; } catch { /* provoke best-effort */ }
                for (let i = 0; i < 30 && !relay.challenge; i++) await new Promise((r) => setTimeout(r, 100));
                try { sub?.close(); } catch { /* already closed */ }
            }
            return relay.challenge ?? null;
        } catch (e) { console.warn('[local-relay] challenge failed:', (e as Error)?.message ?? e); return null; }
    }

    /** Send a browser-signed kind:22242 as AUTH on the local relay's socket and await the relay's OK.
     * On success the socket is marked authed, so subsequent reads/writes ride it. */
    async completeLocalRelayAuth(signed: NostrEvent): Promise<boolean> {
        const url = localRelayUrl();
        if (!url) return false;
        try {
            const relay = await this.raw.ensureRelay(toPoolUrl(url)) as unknown as { auth(fn: () => Promise<NostrEvent>): Promise<unknown>; challenge?: string; authPromise?: unknown };
            // Clear any stuck auth attempt: a background read's onauth (nip07) REJECTS to flag re-auth, and
            // nostr-tools then leaves relay.authPromise pending forever (its catch never settles it). relay.auth
            // returns that cached pending promise, so a manual auth would hang. Reset it for a fresh attempt;
            // the socket keeps the same challenge, so the browser-signed event still matches.
            relay.authPromise = undefined;
            await relay.auth(async () => signed); // relay.auth rebuilds makeAuthEvent internally; we return the pre-signed one
            this.authedLocal.add(normUrl(url));
            this.localAuthObservedRequired = false; // clear the re-auth prompt
            console.log(`[local-relay] authenticated to ${url}`);
            return true;
        } catch (e) {
            console.warn('[local-relay] auth rejected:', (e as Error)?.message ?? e);
            // nostr-tools caches relay.authPromise (even a rejected one), so re-auth on this socket would
            // just re-reject. Drop it so the next Authenticate gets a fresh socket + challenge.
            this.recycle([url]);
            return false;
        }
    }

    /** Whether the current local relay's socket is NIP-42 authenticated (this session). */
    isLocalRelayAuthed(): boolean {
        const url = localRelayUrl();
        return !!url && this.authedLocal.has(normUrl(url));
    }

    /** Recycle zombie sockets if a wall-clock jump says we just resumed from an OS suspend. Shared by the
     * background watchdog (proactive) and the read path (so the first read after waking heals before it
     * hangs). Idempotent under the daemon's concurrency: whichever caller fires first recycles and resets
     * lastTick, so the others see no gap. A live loop resets lastTick on every read/tick, so `gap` only
     * exceeds SLEEP_GAP after the process was actually frozen - never from ordinary idle. */
    private healIfSuspended(): void {
        const now = Date.now();
        const gap = now - this.lastTick;
        this.lastTick = now;
        if (gap > SLEEP_GAP && this.raw.listConnectionStatus().size) {
            if (process.env.SATORI_REQ_LOG) console.log(`[pool] resume after ~${Math.round(gap / 1000)}s suspend: recycling zombie sockets`);
            this.recycle(); // long-lived subs (the bunker transport) rebuild on their own next request via signer self-heal
        }
    }

    /** Live status of the private relay for the Settings status line: can we open a socket, and does a
     * quick REQ get anything back? 'off' when it isn't in use, 'unreachable' when the socket won't open,
     * 'serving' when a probe REQ returns an event, 'connected' when it's reachable but returned nothing
     * (a fresh/empty relay, or one still awaiting auth - the Authenticate prompt covers that case). */
    async probeLocalRelay(): Promise<'off' | 'unreachable' | 'connected' | 'serving'> {
        const url = localRelayUrl();
        if (!url || (localReadMode() === 'off' && localWriteMode() === 'off')) return 'off';
        try {
            const relay = await this.raw.ensureRelay(toPoolUrl(url), { connectionTimeout: CONNECT_MAX_WAIT }) as unknown as { connected: boolean };
            if (!relay.connected) return 'unreachable';
            const evs = await this.query([url], { kinds: [1], limit: 1 }, { direct: true, fast: true });
            return evs.length ? 'serving' : 'connected';
        } catch { return 'unreachable'; }
    }

    /** A NIP-42 onauth for subscriptions/gets. Bunker (server-side signer) signs the challenge for OUR
     * relays + the local relay, so a query auto-auths and RETRIES within the same request (fixes the
     * first-query-empty lag on lazy relays). nip07 can't sign in the background: for the local relay it
     * FLAGS that the manual Authenticate flow is needed, then rejects. Never signs for stranger relays. */
    private onauth = (evt: UnsignedEvent): Promise<NostrEvent> => {
        const relayUrl = (evt.tags?.find((t) => t[0] === 'relay')?.[1]) ?? '';
        const isTarget = this.authRelays.has(normUrl(relayUrl)) || isLocalRelayUrl(relayUrl);
        if (this.authSign && isTarget) return this.authSign(evt);
        if (isLocalRelayUrl(relayUrl)) this.localAuthObservedRequired = true;
        // nostr-tools logs this rejection verbatim - carry the relay + reason so the log line answers
        // "which relay demanded AUTH and why did we decline" on its own.
        return Promise.reject(new Error(this.authSign
            ? `NIP-42 declined for ${relayUrl || 'unknown relay'}: not one of your relays (authing would reveal your identity to it)`
            : `NIP-42 unavailable for ${relayUrl || 'unknown relay'}: no background signer (nip07 signs in the browser)`));
    };

    /** nip07: the local relay needs the manual Authenticate flow (it required auth on a read and we
     * couldn't sign it). False for bunker (auto-auths), when already authed, or when the relay is off. */
    localAuthMissing(): boolean {
        return this.localAuthObservedRequired && !this.authSign && !this.isLocalRelayAuthed()
            && !!localRelayUrl() && (localReadMode() !== 'off' || localWriteMode() !== 'off');
    }

    /** Record which relays each event actually arrived on (empirical outbox memory, keyed by author), then
     * DRAIN the consumed seenOn entries - nostr-tools never bounds that map, and we've extracted what we
     * need, so dropping them keeps enabling trackRelays from leaking. Relay urls are mapped back through
     * fromPoolUrl so a Tor-proxied .onion is stored by its real url, not the proxy. */
    private recordSeenOn(events: NostrEvent[]): void {
        if (!events.length) return;
        const seenOn = this.raw.seenOn;
        for (const e of events) {
            const relays = seenOn.get(e.id);
            if (!relays) continue;
            recordSeen(e.pubkey, [...relays].map((r) => fromPoolUrl(r.url)));
            seenOn.delete(e.id);
        }
    }

    // Resolves on all-relay EOSE or maxWait (the safe, complete default). `opts.fast` ALSO resolves once
    // the event stream goes QUIET (no new event for `quiet` ms after data starts) - because all-EOSE rarely
    // fires under wide outbox fan-out (one slow/dead relay never EOSEs), so a complete query rides the full
    // maxWait even when the page's events arrived in the first few hundred ms. `fast` is ONLY safe where the
    // data is redundant across relays (the 2x-routed feed, a profile's notes on the author's relays): a
    // straggler relay adds nothing a fast one didn't. NEVER use it for non-redundant fetches (gift-wrapped
    // DMs/drafts live on one relay set; a read-modify-write of your own lists must see every version) -
    // there an early resolve silently DROPS events. That asymmetry is why complete is the default.
    /** Apply the local-relay READ policy to a relay set. 'only' replaces it with the local relay;
     * 'add' unions the local relay in, EXCEPT for the profiled feed shards (profile=true) - those
     * pull the local relay via an explicit full-coverage route in feeds.ts, so per-relay latency
     * profiling stays single-relay. 'off'/unset leaves the set untouched. */
    private withLocalRead(relays: string[], opts: { profile?: boolean; complete?: boolean } = {}): string[] {
        const mode = localReadMode();
        const url = localRelayUrl();
        if (mode === 'off' || !url) return relays;
        const union = (): string[] => relays.some((r) => normUrl(r) === normUrl(url)) ? relays : [...relays, url];
        if (opts.complete) return union(); // own lists: full set (never confine/skip), newest wins
        if (mode === 'only') return [url];
        if (opts.profile) return relays;
        return union();
    }

    query(relays: string[], filter: Filter, opts: ReadOpts = {}): Promise<NostrEvent[]> {
        this.healIfSuspended(); // drop zombie sockets before we subscribe if we just woke from sleep
        if (opts.resolve && localFetchMissing()) return this.resolveQuery(relays, filter, opts);
        if (opts.complete && opts.track) return this.queryCompleteTracked(relays, filter, opts); // own-list reads: per-relay accounting
        return this.queryOnce(relays, filter, opts);
    }

    /** The NIP-09 chokepoint shared by every read path: kind:5 deletions are CONSUMED (recorded,
     * never returned - nothing renders a kind:5) and everything else is filtered against the
     * tombstones. Two passes, so a deletion takes effect even on the result set that carried it,
     * regardless of arrival order within it. */
    private consumeAndFilter(events: Map<string, NostrEvent>): NostrEvent[] {
        for (const e of events.values()) if (e.kind === 5) recordDeletion(e);
        const list: NostrEvent[] = [];
        for (const e of events.values()) if (e.kind !== 5 && !isDeletedEvent(e)) list.push(e);
        return list;
    }

    /** A complete+tracked read (the read-modify-write of your OWN lists) runs ONE SUB PER RELAY,
     * so a relay that REFUSED or dropped the request is distinguishable from one that cleanly
     * served an empty result. On the pooled path nostr-tools counts a relay CLOSE toward the
     * all-EOSE condition, so a read where every relay DENIED resolves empty and looks
     * confirmed-absent - exactly the state a list toggle must never rebuild from (the mute-list
     * clobber class). Here `track.truncated` stays false only when EVERY relay EOSE'd cleanly
     * (no relay-initiated close, no hard cap): nothing but a fully clean empty read counts as
     * "you have no list". Costs N single-relay subs instead of one pooled sub - fine for the
     * once-per-session own-list loads this serves. */
    private queryCompleteTracked(relays: string[], filter: Filter, opts: ReadOpts): Promise<NostrEvent[]> {
        relays = this.withLocalRead(relays, opts); // complete → union (the private relay may hold the newest version)
        if (!relays.length) { if (opts.track) opts.track.truncated = true; return Promise.resolve([]); } // asked nobody = unknown
        const tor = relaysViaTor();
        const maxWait = tor ? TOR_LIST_MAX_WAIT : LIST_MAX_WAIT;
        this.raw.maxWaitForConnection = tor ? TOR_CONNECT_MAX_WAIT : CONNECT_MAX_WAIT;
        return new Promise<NostrEvent[]>((resolve) => {
            const events = new Map<string, NostrEvent>();
            const state = relays.map(() => ({ eosed: false, closed: false }));
            const subs: { close: (reason?: string) => void }[] = [];
            let settled = false;
            let done = 0;
            let hardTimer: ReturnType<typeof setTimeout>;
            let decideTimer: ReturnType<typeof setTimeout> | undefined;
            const finish = (hardCap: boolean): void => {
                if (settled) return;
                settled = true;
                clearTimeout(hardTimer);
                clearTimeout(decideTimer);
                for (const su of subs) { try { su.close(); } catch { /* already closed */ } }
                if ((hardCap || !state.every((st) => st.eosed && !st.closed)) && opts.track) opts.track.truncated = true;
                const list = this.consumeAndFilter(events);
                this.recordSeenOn(list);
                resolve(list);
                if (!filter.kinds?.includes(5)) this.checkDeletions(relays, list);
            };
            // A denied relay fires oneose and THEN onclose in the same tick (nostr-tools maps close
            // to EOSE), so the verdict waits one macrotask after the last relay reports - letting
            // those synchronous closes land in `state` before cleanliness is judged.
            const maybeDecide = (): void => {
                if (++done < relays.length) return;
                decideTimer = setTimeout(() => finish(false), 0);
            };
            hardTimer = setTimeout(() => finish(true), maxWait);
            relays.forEach((r, i) => {
                try {
                    subs.push(this.raw.subscribeMany(toPoolUrls([r]), filter as never, {
                        onevent: (e: NostrEvent) => { if (!events.has(e.id) && !isExpired(e)) events.set(e.id, e); },
                        oneose: () => { if (!state[i]!.eosed) { state[i]!.eosed = true; maybeDecide(); } },
                        onclose: () => { if (!settled) state[i]!.closed = true; }, // relay-initiated (ours comes after settle)
                        onauth: this.onauth,
                        maxWait,
                    } as never) as { close: (reason?: string) => void });
                } catch { state[i]!.closed = true; maybeDecide(); }
            });
        });
    }

    /** Only mode + "fetch missing" ON: try the private relay, then fetch what it lacks from the given
     * (real) relays. localFetchMissing() already gates on Only mode, so this never fires in add/off. */
    private async resolveQuery(relays: string[], filter: Filter, opts: ReadOpts): Promise<NostrEvent[]> {
        const first = await this.queryOnce(relays, filter, opts); // withLocalRead -> [private] in Only mode
        // Id-based query (bookmarks, batched event resolution): the local relay may hold SOME of the ids
        // but not all, so a boolean "got anything? stop" would strand the rest. Diff the wanted ids against
        // what came back and fetch EXACTLY the misses from the real relays, then merge.
        const wanted = filter.ids;
        if (wanted?.length) {
            const have = new Set(first.map((e) => e.id));
            const missing = wanted.filter((id) => !have.has(id));
            if (!missing.length) return first;
            const rest = await this.queryOnce(relays, { ...filter, ids: missing }, { ...opts, direct: true });
            return [...first, ...rest];
        }
        // Open-ended query (author timeline, replies by #e): no id set to diff against, so completeness is
        // unknowable - supplement from the real relays only when the local relay returned nothing at all.
        if (first.length) return first;
        return this.queryOnce(relays, filter, { ...opts, direct: true });
    }

    private queryOnce(relays: string[], filter: Filter, opts: ReadOpts = {}): Promise<NostrEvent[]> {
        if (!opts.direct) relays = this.withLocalRead(relays, opts);
        const tor = relaysViaTor();
        const base = tor ? TOR_LIST_MAX_WAIT : LIST_MAX_WAIT;
        // maxWait = the hard cap. Default (no budget) keeps today's fixed cap. 'page' = a tight first-paint cap;
        // 'adaptive' (single relay only - the outbox fan-out) scales per-relay from the profile: empty relays
        // bail fast to the page cap (floor), a rare rich-but-truncating relay gets extended toward the ceiling.
        const pageCap = tor ? TOR_PAGE_MAX_WAIT : PAGE_MAX_WAIT;
        const maxWait = opts.budget === 'page' ? pageCap
            : opts.budget === 'adaptive' && relays.length === 1 ? relayBudget(relays[0]!, pageCap, base, tor ? TOR_LIST_CEILING : LIST_CEILING)
            : base;
        const quiet = tor ? 1800 : 700;
        this.raw.maxWaitForConnection = tor ? TOR_CONNECT_MAX_WAIT : CONNECT_MAX_WAIT;
        return new Promise<NostrEvent[]>((resolve) => {
            const started = Date.now();
            const events = new Map<string, NostrEvent>();
            let settled = false;
            let lastEventAt = 0;   // when the most recent event arrived (for the latency profile)
            let truncated = false; // did we cut off at the hard cap while the relay was still delivering?
            let quietTimer: ReturnType<typeof setTimeout> | undefined;
            let hardTimer: ReturnType<typeof setTimeout>;
            let sub: { close: (reason?: string) => void } | undefined;
            const finish = (): void => {
                if (settled) return;
                settled = true;
                clearTimeout(quietTimer);
                clearTimeout(hardTimer);
                try { sub?.close(); } catch { /* already closed */ }
                const list = this.consumeAndFilter(events); // NIP-09 chokepoint (see consumeAndFilter)
                this.recordSeenOn(list);
                // Per-relay latency profiling (observation only - see relay-latency.ts). Gated on opts.profile
                // so ONLY the outbox following/followers fan-out feeds it - NOT the "browse a relay" firehose
                // (kinds-only, no author filter), which streams far more events and truncates readily, and would
                // otherwise inflate a browsed relay's outbox budget. Single-relay is a precondition (attribution);
                // measure to the LAST event, not to finish, so the quiet-collapse tail isn't counted as relay time.
                if (opts.profile && relays.length === 1) {
                    const ms = lastEventAt ? lastEventAt - started : Date.now() - started;
                    recordLatency(relays[0]!, ms, truncated, list.length);
                    if (process.env.SATORI_REQ_LOG) console.log(`[relay-latency] ${relays[0]} lastEvent=${ms}ms events=${list.length} truncated=${truncated}`);
                }
                if (truncated && opts.track) opts.track.truncated = true;
                resolve(list);
                // After resolve - the sweep never delays a response. Skip when this query IS the
                // sweep (it asks for kinds:[5]), which also stops recursion.
                if (!filter.kinds?.includes(5)) this.checkDeletions(relays, list);
            };
            hardTimer = setTimeout(() => { truncated = true; finish(); }, maxWait);
            try {
                sub = this.raw.subscribeMany(toPoolUrls(relays), filter as never, {
                    onevent: (e: NostrEvent) => {
                        if (events.has(e.id) || isExpired(e)) return; // NIP-40: expired events are ignored
                        events.set(e.id, e);
                        lastEventAt = Date.now();
                        if (opts.fast) { clearTimeout(quietTimer); quietTimer = setTimeout(finish, quiet); }
                    },
                    oneose: () => finish(), // every relay EOSE'd → nothing more is coming
                    onclose: (reasons: string[]) => {
                        // Empirical read-health (profiled single-relay shards only, so attribution is
                        // certain): an explicit refusal with nothing delivered marks the relay read-dead.
                        // recordReadDenial itself exempts the local relay (pre-auth CLOSEDs are expected).
                        if (opts.profile && relays.length === 1 && events.size === 0 && reasons?.some((r) => READ_DENIAL_RE.test(r ?? ''))) {
                            recordReadDenial(relays[0]!);
                            if (process.env.SATORI_REQ_LOG) console.log(`[relay-latency] read denial from ${relays[0]}: ${reasons.find((r) => READ_DENIAL_RE.test(r ?? ''))}`);
                        }
                    },
                    onauth: this.onauth, // NIP-42: bunker auths+retries here; nip07 flags the local relay for re-auth
                    maxWait,
                } as never) as { close: (reason?: string) => void };
            } catch { finish(); }
        });
    }

    /** Background NIP-09 sweep: ask the same relays for kind:5 deletions of the events just
     * returned - by id, and by coordinate for addressable kinds. TTL-gated per target (30 min)
     * so the steady-state cost is near zero; hits land as tombstones via the chokepoint above,
     * so the NEXT render drops the event. Deletion is best-effort by design - first paint is
     * never spent waiting on it. Fire-and-forget. */
    private checkDeletions(relays: string[], events: NostrEvent[]): void {
        const eIds: string[] = [];
        const aCoords: string[] = [];
        for (const e of events) {
            if (shouldCheckDeletion(e.id)) eIds.push(e.id);
            if (isAddressable(e.kind)) {
                const coord = coordinateOf(e);
                if (shouldCheckDeletion(coord)) aCoords.push(coord);
            }
        }
        // #e and #a are separate queries: filter fields AND together, so one combined filter
        // would only match kind:5s carrying BOTH tags. `direct` - `relays` is already the
        // policy-applied set of the query that produced these events.
        const opts: ReadOpts = { fast: true, budget: 'page', direct: true };
        if (eIds.length) void this.queryOnce(relays, { kinds: [5], '#e': eIds }, opts);
        if (aCoords.length) void this.queryOnce(relays, { kinds: [5], '#a': aCoords }, opts);
    }

    // `maxWait` lets a best-effort caller shorten the wait - a decorative quote preview shouldn't
    // hold a relay connection for the full 6s when the event isn't there. Under Tor the caller's cap
    // is doubled (circuits are slow; a too-short wait would just always miss) but still honored, so
    // a 3s best-effort fetch is 6s on Tor, not the full 12s default.
    get(relays: string[], filter: Filter, maxWait?: number, opts: ReadOpts = {}): Promise<NostrEvent | null> {
        this.healIfSuspended(); // drop zombie sockets before we subscribe if we just woke from sleep
        if (opts.resolve && localFetchMissing()) return this.resolveGet(relays, filter, maxWait, opts);
        return this.getOnce(relays, filter, maxWait, opts);
    }

    private async resolveGet(relays: string[], filter: Filter, maxWait: number | undefined, opts: ReadOpts): Promise<NostrEvent | null> {
        const first = await this.getOnce(relays, filter, maxWait, opts);
        return first ?? this.getOnce(relays, filter, maxWait, { ...opts, direct: true });
    }

    private getOnce(relays: string[], filter: Filter, maxWait?: number, opts: ReadOpts = {}): Promise<NostrEvent | null> {
        if (!opts.direct) relays = this.withLocalRead(relays, opts);
        const tor = relaysViaTor();
        const cap = tor ? (maxWait ? Math.min(maxWait * 2, TOR_GET_MAX_WAIT) : TOR_GET_MAX_WAIT) : (maxWait ?? GET_MAX_WAIT);
        this.raw.maxWaitForConnection = tor ? TOR_CONNECT_MAX_WAIT : CONNECT_MAX_WAIT;
        if (filter.ids?.length) return this.getById(relays, filter, cap);
        return (this.raw.get(toPoolUrls(relays), filter, { maxWait: cap, onauth: this.onauth } as never) as Promise<NostrEvent | null>)
            .then((ev) => { // NIP-40 expiry + NIP-09 tombstone checks
                if (!ev || isExpired(ev) || isDeletedEvent(ev)) return null;
                this.recordSeenOn([ev]);
                if (!filter.kinds?.includes(5)) this.checkDeletions(relays, [ev]);
                return ev;
            });
    }

    // An id filter names an IMMUTABLE event, so the first copy to arrive IS the answer - resolve on
    // it instead of riding raw.get's contract (all-relay EOSE), which prices every hit at the slowest
    // relay in the set. Only a MISS still waits out EOSE/cap: absence needs every relay's word.
    // Replaceable-kind gets stay on raw.get, where all-EOSE is what finds the newest version.
    private getById(relays: string[], filter: Filter, maxWait: number): Promise<NostrEvent | null> {
        return new Promise((resolve) => {
            let settled = false;
            let sub: { close: (reason?: string) => void } | undefined;
            const finish = (ev: NostrEvent | null): void => {
                if (settled) return;
                settled = true;
                clearTimeout(timer);
                try { sub?.close(); } catch { /* already closed */ }
                if (ev) { this.recordSeenOn([ev]); if (!filter.kinds?.includes(5)) this.checkDeletions(relays, [ev]); }
                resolve(ev);
            };
            const timer = setTimeout(() => finish(null), maxWait);
            try {
                sub = this.raw.subscribeMany(toPoolUrls(relays), filter as never, {
                    // NIP-40: an expired copy doesn't count as found; NIP-09: nor does a deleted one
                    onevent: (e: NostrEvent) => { if (!isExpired(e) && !isDeletedEvent(e)) finish(e); },
                    oneose: () => finish(null),
                    onauth: this.onauth,
                    maxWait,
                } as never) as { close: (reason?: string) => void };
            } catch { finish(null); }
        });
    }

    /** Publish with the local-relay WRITE policy. 'add' publishes to `relays` as usual AND
     * best-effort mirrors to the local relay (the mirror is NOT reflected in the returned
     * results, so callers that zip results with their own target list stay aligned). 'only'
     * sends EXCLUSIVELY to the local relay (skipping `relays`) and reports its outcome once per
     * requested target, so anyAccepted()/index-zipping still hold - a self-hosted blaster relay
     * then re-broadcasts. 'off'/unset publishes to `relays` unchanged. */
    async publish(relays: string[], event: NostrEvent): Promise<PromiseSettledResult<string>[]> {
        // Bunker RPC transport (NIP-46) AND gift-wrapped DMs / private replies (NIP-59, kind 1059) bypass
        // the local-relay write policy entirely: both carry STRICT targets - the bunker's relays, or the
        // recipient's NIP-17 inbox relays. Redirecting ('only') would break signing / silently drop a DM to
        // a relay the recipient never reads (the fabricated per-target results would still report "sent");
        // mirroring ('add') would leak an extra copy of a private message. Always send to exactly `relays`.
        if (event.kind === NIP46_KIND || event.kind === GIFTWRAP_KIND) return Promise.allSettled(this.raw.publish(toPoolUrls(relays), event as never));
        const url = localWriteMode() !== 'off' ? localRelayUrl() : null;
        if (url && localWriteMode() === 'only') {
            const res = await Promise.allSettled(this.raw.publish(toPoolUrls([url]), event as never));
            const ok = anyAccepted(res);
            if (process.env.SATORI_REQ_LOG) console.log(`[local-relay] exclusive publish kind:${event.kind} -> ${url}: ${ok ? 'ok' : 'rejected'}`);
            if (!relays.length) return res;
            return relays.map(() => ok
                ? ({ status: 'fulfilled', value: url } as PromiseFulfilledResult<string>)
                : ({ status: 'rejected', reason: new Error(`local relay ${url} rejected`) } as PromiseRejectedResult));
        }
        const results = await Promise.allSettled(this.raw.publish(toPoolUrls(relays), event as never));
        if (url && !relays.some((r) => normUrl(r) === normUrl(url))) {
            try {
                void Promise.allSettled(this.raw.publish(toPoolUrls([url]), event as never))
                    .then((rs) => { if (process.env.SATORI_REQ_LOG) console.log(`[local-relay] mirror kind:${event.kind} -> ${url}: ${anyAccepted(rs) ? 'ok' : 'rejected'}`); });
            } catch (e) { console.warn('[local-relay] mirror failed:', (e as Error)?.message ?? e); }
        }
        return results;
    }

    subscribe(relays: string[], filter: Filter, handlers: SubHandlers) {
        // trackRelays populates seenOn for these events too, but only query()/get() drain it - so a
        // long-lived subscription (e.g. the NIP-46 bunker) would accumulate seenOn entries forever.
        // receivedEvent runs before onevent (relay.js), so seenOn is ready here: record + drain per event.
        const wrapped: SubHandlers = { ...handlers, onevent: (e) => { if (isExpired(e)) return; this.recordSeenOn([e]); handlers.onevent?.(e); } }; // NIP-40
        return this.raw.subscribeMany(toPoolUrls(relays), filter as never, wrapped as never);
    }

    /** Live status of content relays - every open relay minus the given transport
     * relays - with .onion relays shown by their real URL (not the proxy URL). */
    contentRelays(transport: string[]): { url: string; connected: boolean }[] {
        const t = new Set(transport.map(normUrl));
        const out: { url: string; connected: boolean }[] = [];
        for (const [url, connected] of this.raw.listConnectionStatus()) {
            if (t.has(normUrl(url))) continue;
            out.push({ url: fromPoolUrl(url), connected });
        }
        return out.sort((a, b) => a.url.localeCompare(b.url));
    }

    /** Force-drop relay connections (closing their subscriptions and deleting the
     * relay so the next use builds a fresh socket) - clears the zombie sockets a
     * resume from sleep leaves behind. Pass specific URLs to recycle just those
     * (e.g. the bunker transport); omit to recycle every open relay. Callers must
     * re-establish any long-lived subscriptions afterwards; one-shot queries
     * reconnect on their own. */
    recycle(urls?: string[]): void {
        this.warming = null; // a recycle invalidates any in-flight warm progress
        // A recycled socket re-opens unauthed, so drop its NIP-42 auth memory (all, if recycling everything).
        if (urls?.length) for (const u of urls) this.authedLocal.delete(normUrl(u));
        else this.authedLocal.clear();
        const targets = urls?.length ? toPoolUrls(urls) : [...this.raw.listConnectionStatus().keys()];
        if (targets.length) this.raw.close(targets);
    }

    /** Pre-open connections to the given relays in the background (best-effort) and
     * track progress, so a warming indicator can show real connected/total and know
     * when every attempt has settled (healthy connected, the rest failed). Idempotent:
     * a no-op while a cycle is still in flight; starts a fresh cycle once it settles.
     * Dials with the Tor connection timeout when Privacy Mode is on (cold circuits). */
    warm(relays: string[]): void {
        if (this.warming) {
            if (this.warming.settled.size < this.warming.urls.size) return; // cycle in flight
            // Completed cycle: don't restart (which would reset progress and re-loop the
            // indicator forever) while every warmed relay is still connected.
            let up = 0;
            for (const [url, isUp] of this.raw.listConnectionStatus()) if (isUp && this.warming.urls.has(normUrl(url))) up++;
            if (up >= this.warming.urls.size) return;
        }
        const poolUrls = toPoolUrls(relays);
        if (!poolUrls.length) { this.warming = null; return; }
        const timeout = relaysViaTor() ? TOR_CONNECT_MAX_WAIT : CONNECT_MAX_WAIT;
        this.raw.maxWaitForConnection = timeout;
        const state = { urls: new Set(poolUrls.map(normUrl)), settled: new Set<string>() };
        this.warming = state;
        for (const url of poolUrls) {
            const n = normUrl(url);
            try { void this.raw.ensureRelay(url, { connectionTimeout: timeout }).then(() => state.settled.add(n), () => state.settled.add(n)); }
            catch { state.settled.add(n); }
        }
    }

    /** Warm-up progress for the current cycle: relays connected vs total, and whether
     * every attempt has settled (= done; healthy ones connected, the rest failed). */
    warmProgress(): { connected: number; total: number; ready: boolean } | null {
        const w = this.warming;
        if (!w || w.urls.size === 0) return null;
        let connected = 0;
        for (const [url, up] of this.raw.listConnectionStatus()) if (up && w.urls.has(normUrl(url))) connected++;
        return { connected, total: w.urls.size, ready: w.settled.size >= w.urls.size };
    }

    closeAll(): void {
        clearInterval(this.watchdog);
        try { this.raw.destroy(); } catch { /* ignore */ }
    }
}
