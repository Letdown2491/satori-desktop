// NIP-05: format a nip05 identifier for display and verify it against the
// domain's /.well-known/nostr.json. A nip05 is only a self-claim until verified.

import { isPublicHttpUrl } from '../ssrf.ts';
import { torFetch } from '../data/torfetch.ts';

/** Format a nip05 for display: `_@domain` renders as just `domain`. */
export function formatNip05(nip05: string | undefined): string {
    if (!nip05 || !nip05.includes('@')) return nip05 || '';
    const [local, domain] = nip05.split('@');
    return local === '_' ? (domain ?? '') : nip05;
}

/**
 * Verify a nip05 per NIP-05: fetch `https://<domain>/.well-known/nostr.json?name=
 * <local>` and confirm `names[local]` equals the pubkey. The spec forbids
 * redirects and requires permissive CORS, so this runs straight from the browser.
 * Returns false on any mismatch, network error, or CORS rejection.
 */
export async function verifyNip05(nip05: string | undefined, pubkey: string): Promise<boolean> {
    if (!nip05 || !nip05.includes('@')) return false;
    const [localRaw, domain] = nip05.split('@');
    const local = localRaw || '_';
    if (!domain) return false;
    try {
        const url = `https://${domain}/.well-known/nostr.json?name=${encodeURIComponent(local)}`;
        if (!isPublicHttpUrl(url)) return false; // SSRF guard: domain is from someone else's profile
        // Privacy-Mode-aware; redirects=0 preserves the old redirect:'error' (a 3xx → !200 → false).
        const res = await torFetch(url, 5000, 256 * 1024, 0);
        if (res.status !== 200) return false;
        const json = JSON.parse(res.body.toString('utf8')) as { names?: Record<string, string> };
        return json?.names?.[local]?.toLowerCase() === pubkey.toLowerCase();
    } catch {
        return false;
    }
}
