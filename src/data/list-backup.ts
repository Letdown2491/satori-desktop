// Lists backup & restore: export/import your replaceable list events (NIP-51/65).
// Backups are the raw SIGNED events; private lists (mute/bookmark) keep their encrypted
// `content` ciphertext intact, so no plaintext private data is ever written to disk.
// Restore is REPLACE-ONLY: re-sign the backed-up event with a fresh created_at (the
// ciphertext rides along verbatim, no decryption) so relays accept it over any newer copy.

import type { NostrEvent, UnsignedEvent } from '../nostr/types.ts';
import type { Session } from '../session.ts';
import { INDEXER_RELAYS, normalizeRelayUrl, readRelaysFor as readRelaysForList } from '../nostr/nip65.ts';

export interface BackupListDef { kind: number; label: string; }

/** The lists we back up - all replaceable, all already held/fetched by the daemon. */
export const BACKUP_LISTS: BackupListDef[] = [
    { kind: 3, label: 'Follows' },
    { kind: 10000, label: 'Mute list' },
    { kind: 10003, label: 'Bookmarks' },
    { kind: 10002, label: 'Relays' },
    { kind: 10050, label: 'DM relays' },
    { kind: 10063, label: 'Media servers' },
];
export const BACKUP_KINDS = BACKUP_LISTS.map((l) => l.kind);

/** The shape of an exported backup file. */
export interface BackupFile {
    version: number;
    exportedAt: number;
    pubkey: string;
    lists: Record<string, NostrEvent>;
}
export const BACKUP_VERSION = 1;

/** Relays to query when gathering the current list events for export (thin wrapper over the
 * shared read∪write∪indexer helper, applied to your own relay list). */
function readRelaysFor(s: Session): string[] {
    return readRelaysForList(s.myRelays);
}

/** Fetch the live signed list events for `kinds` (newest replaceable per kind). */
export async function gatherBackup(s: Session & { me: string }, kinds: number[]): Promise<NostrEvent[]> {
    if (kinds.length === 0) return [];
    const raw = await s.pool.query(readRelaysFor(s), { kinds, authors: [s.me] }).catch(() => []);
    const newest = new Map<number, NostrEvent>();
    for (const ev of raw) {
        const cur = newest.get(ev.kind);
        if (!cur || ev.created_at > cur.created_at) newest.set(ev.kind, ev);
    }
    return [...newest.values()];
}

/** The unsigned template that restores a backed-up event: same kind/tags/content, but a
 * fresh created_at (so it wins the replaceable "newest" rule) under the current key. */
export function restoreTemplate(ev: NostrEvent, me: string): UnsignedEvent {
    return { kind: ev.kind, created_at: Math.floor(Date.now() / 1000), tags: ev.tags, content: ev.content, pubkey: me };
}

/** Where a restored list should be published: your write relays + indexers (+ the DM
 * relays it lists, for kind:10050, so the list lands on the inbox relays themselves). */
export function restoreTargets(ev: { kind: number; tags: string[][] }, s: Session): string[] {
    const base = [...new Set([...(s.myRelays?.write ?? []), ...INDEXER_RELAYS])];
    if (ev.kind === 10050) {
        // Validate the relays a (signed) backup lists before dialing them: ws/wss only.
        const listed = ev.tags.filter((t) => t[0] === 'relay' && t[1]).map((t) => normalizeRelayUrl(t[1]!)).filter((u): u is string => !!u);
        return [...new Set([...base, ...listed])];
    }
    return base;
}
