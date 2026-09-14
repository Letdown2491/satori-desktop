// Shared on-disk JSON store + cache plumbing, factored out of the per-feature stores (filters / content-
// prefs / scheduled / undo) and the SWR caches (profile-cache / relays / trust / engagement / dm-read),
// which each hand-rolled the same mtime-cached read, 0o600 write, debounced flush, and LRU eviction - and
// crucially NONE flushed on shutdown, so a clean exit inside the debounce window silently dropped the last
// writes. This module adds a single shared flush-on-exit so that can't happen.

import { statSync, readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { dirname } from 'node:path';

/** A per-user / keyed JSON store: mtime-cached reads (cheap to call every request - re-parses only when
 * the file changed), 0o600 writes, mkdir-on-write. */
export function jsonStore<T extends Record<string, unknown>>(file: string, tag: string): {
    readAll(): T; writeAll(all: T): void;
} {
    let parsed: { mtime: number; data: T } | null = null;
    return {
        readAll(): T {
            try {
                const mtime = statSync(file).mtimeMs;
                if (parsed && parsed.mtime === mtime) return parsed.data;
                const data = JSON.parse(readFileSync(file, 'utf8')) as T;
                parsed = { mtime, data };
                return data;
            } catch { parsed = null; return {} as T; }
        },
        writeAll(all: T): void {
            try {
                mkdirSync(dirname(file), { recursive: true });
                writeFileSync(file, JSON.stringify(all), { mode: 0o600 });
                // Keep the just-written data as the cache - dropping it would force the next readAll
                // to re-read and re-parse the file this process just wrote.
                parsed = { mtime: statSync(file).mtimeMs, data: all };
            } catch (e) { console.warn(`[${tag}] persist failed:`, (e as Error)?.message ?? e); parsed = null; }
        },
    };
}

// --- debounced flush with a single shared flush-on-exit -----------------------------------------------

type Pending = () => void;
const pendingFlushers = new Set<Pending>();
let exitHooked = false;
function hookExit(): void {
    if (exitHooked) return;
    exitHooked = true;
    const flushAll = (): void => { for (const f of [...pendingFlushers]) { try { f(); } catch { /* best effort */ } } };
    process.on('exit', flushAll); // normal termination / process.exit()
    // SIGINT/SIGTERM (docker stop/restart) don't fire 'exit' by default - having a listener also
    // suppresses the default terminate, so we flush then exit explicitly.
    for (const sig of ['SIGINT', 'SIGTERM'] as const) process.on(sig, () => { flushAll(); process.exit(0); });
}

/** A debounced flusher: `schedule()` (re)arms a write `debounceMs` out; the write itself is `flush`.
 * Registers with the shared exit handler so a pending write is never lost on shutdown. The trailing
 * debounce alone would let CONTINUOUS activity (a live subscription rescheduling on every event)
 * postpone the write for the whole session - leaving hours of accumulation one crash away from lost -
 * so a pending write older than `maxDelayMs` flushes immediately instead of re-arming. */
export function debouncedFlush(flush: () => void, debounceMs: number, maxDelayMs = 60_000): { schedule(): void } {
    hookExit();
    let timer: ReturnType<typeof setTimeout> | null = null;
    let pendingSince = 0;
    const fire = (): void => {
        if (timer) clearTimeout(timer);
        timer = null; pendingSince = 0;
        pendingFlushers.delete(doFlush);
        flush();
    };
    const doFlush: Pending = () => { if (timer) fire(); };
    return {
        schedule(): void {
            const now = Date.now();
            if (!pendingSince) pendingSince = now;
            if (now - pendingSince >= maxDelayMs) { fire(); return; }
            if (timer) clearTimeout(timer);
            pendingFlushers.add(doFlush);
            timer = setTimeout(fire, debounceMs);
        },
    };
}

/** Evict least-recently-used entries from a Map of `{ lastUsed }` down past `cap` (+10% slack). */
export function lruEvictByLastUsed<K>(cache: Map<K, { lastUsed: number }>, cap: number): void {
    if (cache.size <= cap) return;
    const oldest = [...cache.entries()].sort((a, b) => a[1].lastUsed - b[1].lastUsed);
    const drop = Math.ceil(cache.size - cap + cap * 0.1);
    for (let i = 0; i < drop && i < oldest.length; i++) cache.delete(oldest[i]![0]);
}

/** Trim a Map to `cap` by insertion order (FIFO), dropping the oldest entries. */
export function trimOldest<K, V>(map: Map<K, V>, cap: number): void {
    if (map.size <= cap) return;
    let over = map.size - cap;
    for (const k of map.keys()) { if (over-- <= 0) break; map.delete(k); }
}

/** Reinsert-and-evict: set `key` as most-recently-used (Map insertion order = recency) and drop
 * the oldest entries over `cap`. The recency idiom the persisted stores share (seen-relays,
 * deletions, relay-info) - the third copy earned the helper. */
export function lruSet<K, V>(map: Map<K, V>, key: K, val: V, cap: number): void {
    map.delete(key);
    map.set(key, val);
    trimOldest(map, cap);
}
