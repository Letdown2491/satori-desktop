// Instrumentation for the DM cache scans. The "instant" cached renders (aggregateCached /
// cachedThread, and the nip07 equivalents) scan the WHOLE decrypt cache per Messages/thread render -
// O(history), not O(conversation). This records how long that actually takes + how many entries, so
// the Option B peer-index decision is made on numbers. It matters most under MULTI-TENANT, where one
// process's cache holds many users' entries, so the per-render scan grows with total traffic, not
// just yours. Surfaced at GET /metrics; a single scan over WARN_MS also logs a one-liner. See
// [[dm-peer-index]]. Pure measurement - no behavior change, negligible overhead (one perf.now pair).

interface ScanStat { count: number; totalMs: number; maxMs: number; lastMs: number; maxEntries: number; lastEntries: number }
const stats = new Map<string, ScanStat>();
const WARN_MS = 5; // a clearly-slow scan surfaces in logs without polling /metrics

/** Record one cache-scan: its label, how many entries it walked, how long it took (ms). */
export function recordScan(label: string, entries: number, ms: number): void {
    let s = stats.get(label);
    if (!s) { s = { count: 0, totalMs: 0, maxMs: 0, lastMs: 0, maxEntries: 0, lastEntries: 0 }; stats.set(label, s); }
    s.count++; s.totalMs += ms; s.lastMs = ms; s.lastEntries = entries;
    if (ms > s.maxMs) s.maxMs = ms;
    if (entries > s.maxEntries) s.maxEntries = entries;
    if (ms >= WARN_MS) console.warn(`[dm-scan] ${label}: ${ms.toFixed(1)}ms over ${entries} entries`);
}

/** Per-scan-site stats for /metrics: count, avg/max/last ms, max/last entries walked. */
export function dmScanStats(): Record<string, ScanStat & { avgMs: number }> {
    const out: Record<string, ScanStat & { avgMs: number }> = {};
    for (const [k, s] of stats) out[k] = { ...s, avgMs: s.count ? Math.round((s.totalMs / s.count) * 1000) / 1000 : 0 };
    return out;
}
