// Single-flight (request coalescing) helpers. The "register an in-flight promise, share it with
// concurrent callers, delete-if-still-mine on settle" logic is subtle (the `=== p` re-check guards
// against deleting a newer registration) and was hand-written 5× across data/. One tested home now.

/** Single-flight a BATCH: run `run(todo)` once for the keys not already in flight, register a shared
 * promise per new key, and await the union - so concurrent callers for overlapping keys share one fetch.
 * `run` is catch-wrapped BEFORE storing, so a rejection can't surface as an unhandled-rejection crash. */
export async function coalesceBatch<K>(inflight: Map<K, Promise<void>>, keys: K[], run: (todo: K[]) => Promise<void>): Promise<void> {
    const todo = keys.filter((k) => !inflight.has(k));
    if (todo.length) {
        const p = run(todo).catch(() => {});
        for (const k of todo) {
            inflight.set(k, p);
            void p.finally(() => { if (inflight.get(k) === p) inflight.delete(k); });
        }
    }
    await Promise.all(keys.map((k) => inflight.get(k)).filter(Boolean));
}

/** Single-flight ONE key: return the in-flight promise if present, else run `make`, cache it, clear on settle. */
export function coalesceOne<K, V>(inflight: Map<K, Promise<V>>, key: K, make: () => Promise<V>): Promise<V> {
    const pending = inflight.get(key);
    if (pending) return pending;
    const p = make().finally(() => { if (inflight.get(key) === p) inflight.delete(key); });
    inflight.set(key, p);
    return p;
}
