// Instrumentation for the following-feed BACKFILL (0.5.0 feed reliability). The landing paints on a tight 2s
// budget; a background dot-poll fetches slow relays on a generous adaptive budget and folds late arrivals into
// a pending buffer that the landing then renders. This counts how many notes each landing RECOVERED - surfaced
// from that buffer but ABSENT from its own 2s fetch - so "the backfill helps" becomes a number and the
// per-relay budgets can be tuned on evidence. Surfaced at GET /metrics; a per-landing line is gated on
// SATORI_REQ_LOG. Pure measurement - no behavior change, four integers, O(1). Mirrors dm-metrics.ts.

interface FeedRecovery { landings: number; surfaced: number; recovered: number; maxRecovered: number }
const stat: FeedRecovery = { landings: 0, surfaced: 0, recovered: 0, maxRecovered: 0 };

/** Record one following landing: how many notes it surfaced, and how many of those the tight first paint
 * missed and the adaptive backfill recovered. */
export function recordFollowingLanding(surfaced: number, recovered: number): void {
    stat.landings++;
    stat.surfaced += surfaced;
    stat.recovered += recovered;
    if (recovered > stat.maxRecovered) stat.maxRecovered = recovered;
    if (process.env.SATORI_REQ_LOG && recovered > 0) console.log(`[feed-recovery] surfaced=${surfaced} recovered=${recovered}`);
}

/** Backfill-recovery counters for /metrics: totals + the recovery rate (recovered / surfaced). */
export function feedRecoveryStats(): FeedRecovery & { recoveryRate: number } {
    return { ...stat, recoveryRate: stat.surfaced ? Math.round((stat.recovered / stat.surfaced) * 1000) / 1000 : 0 };
}
