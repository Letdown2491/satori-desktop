// Disk-backed, size-bounded LRU cache for proxied avatar images. Keyed by source
// URL (avatar URLs are effectively immutable - a new picture means a new URL), so no
// staleness handling: fetch once, serve from disk forever until LRU eviction. Single-
// user daemon, so one shared cache. Files live as <sha256(url)> in the cache dir; a
// small JSON index tracks content-type + size + last-used for the LRU.

import { createHash } from 'node:crypto';
import { readFile, writeFile, mkdir, unlink } from 'node:fs/promises';
import { readFileSync, writeFileSync, readdirSync, unlinkSync, mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { debouncedFlush } from './json-store.ts';

const DIR = process.env.SATORI_AVATAR_CACHE || join(process.cwd(), '.data', 'avatars');
const INDEX = join(DIR, 'index.json');
const CAP_BYTES = 200 * 1024 * 1024; // ~200 MB

interface Meta { ct: string; size: number; lastUsed: number }
const index = new Map<string, Meta>();
let totalBytes = 0;
let hits = 0, misses = 0;

(function load(): void {
    try {
        const raw = JSON.parse(readFileSync(INDEX, 'utf8')) as Record<string, Meta>;
        for (const [h, m] of Object.entries(raw)) if (m?.size) { index.set(h, m); totalBytes += m.size; }
    } catch { /* no cache yet */ }
    // Reconcile the dir against the index: blob bytes hit disk immediately but the index rides a
    // debounced flush, so a restart inside that window leaves ORPHAN files - invisible to totalBytes
    // (the cap under-counts and never reclaims them) and guaranteed re-fetch misses anyway. They're
    // plain cache blobs, so drop them; the next request re-fetches and re-indexes.
    try {
        let dropped = 0;
        for (const f of readdirSync(DIR)) {
            if (f === 'index.json' || index.has(f)) continue;
            try { unlinkSync(join(DIR, f)); dropped++; } catch { /* best effort */ }
        }
        if (dropped) console.log(`[avatar-cache] dropped ${dropped} orphaned files (index lagged a restart)`);
    } catch { /* no dir yet */ }
})();

const hashUrl = (url: string) => createHash('sha256').update(url).digest('hex');
const fileFor = (h: string) => join(DIR, h);

// Shared debounced flusher (sync write, small index) so the exit hook covers it - the hand-rolled
// async timer this replaced wasn't exit-hooked, which is exactly what minted the orphans above.
const flusher = debouncedFlush(() => {
    try { mkdirSync(DIR, { recursive: true }); writeFileSync(INDEX, JSON.stringify(Object.fromEntries(index)), { mode: 0o600 }); }
    catch (e) { console.warn('[avatar-cache] flush failed:', (e as Error)?.message ?? e); }
}, 8000);

async function evictIfNeeded(): Promise<void> {
    if (totalBytes <= CAP_BYTES) return;
    for (const [h, m] of [...index.entries()].sort((a, b) => a[1].lastUsed - b[1].lastUsed)) {
        if (totalBytes <= CAP_BYTES * 0.9) break; // drop down to 90% in one pass
        index.delete(h); totalBytes -= m.size;
        await unlink(fileFor(h)).catch(() => {});
    }
}

/** Cached image bytes + content-type for a source url, or null (miss / file gone). */
export async function getAvatarBytes(url: string): Promise<{ bytes: Buffer; ct: string } | null> {
    const h = hashUrl(url);
    const m = index.get(h);
    if (!m) { misses++; return null; }
    try { const bytes = await readFile(fileFor(h)); m.lastUsed = Date.now(); hits++; return { bytes, ct: m.ct }; }
    // file vanished → treat as miss. Only decrement if this entry is still the live one, so two
    // concurrent reads of the same vanished hash don't double-subtract and drift totalBytes low.
    catch { if (index.get(h) === m) { index.delete(h); totalBytes -= m.size; } misses++; return null; }
}

/** Cache an image's bytes + content-type under its source url (LRU-evicting if over cap). */
export async function putAvatarBytes(url: string, bytes: Buffer, ct: string): Promise<void> {
    const h = hashUrl(url);
    try { await mkdir(DIR, { recursive: true }); await writeFile(fileFor(h), bytes, { mode: 0o600 }); }
    catch (e) { console.warn('[avatar-cache] write failed:', (e as Error)?.message ?? e); return; }
    const old = index.get(h);
    if (old) totalBytes -= old.size;
    index.set(h, { ct, size: bytes.length, lastUsed: Date.now() });
    totalBytes += bytes.length;
    await evictIfNeeded();
    flusher.schedule();
}

export function avatarCacheStats(): { count: number; mb: number; hits: number; misses: number; hitRate: number } {
    const total = hits + misses;
    return { count: index.size, mb: +(totalBytes / 1048576).toFixed(1), hits, misses, hitRate: total ? +(hits / total).toFixed(3) : 0 };
}
