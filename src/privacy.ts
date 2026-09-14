// Privacy Mode: how much of the daemon's OUTBOUND traffic is routed through Tor.
// Browser <-> daemon is loopback (never Tor); this governs only daemon <-> world.
//   off      - direct (today's behavior; .onion relays still Tor'd, the only way).
//   balanced - relays + server fetches via Tor, FAIL-OPEN (fall back to direct).
//   strict   - everything via Tor, FAIL-CLOSED (no fallback; never leaks).
// Server-wide (the relay WS impl is process-global, so routing is instance-wide, not
// per-user - fine for the single-user daemon). Persisted to .data, live-toggleable.
// All levels are inert unless TOR_SOCKS is configured (no proxy = nothing to route).

import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { dirname } from 'node:path';

export type PrivacyMode = 'off' | 'balanced' | 'strict';
export const PRIVACY_MODES: PrivacyMode[] = ['off', 'balanced', 'strict'];
export const isPrivacyMode = (m: string): m is PrivacyMode => (PRIVACY_MODES as string[]).includes(m);

const FILE = process.env.SATORI_PRIVACY_FILE ?? '.data/privacy.json';
let mode: PrivacyMode = 'off';

try { const j = JSON.parse(readFileSync(FILE, 'utf8')) as { mode?: string }; if (j.mode && isPrivacyMode(j.mode)) mode = j.mode; } catch { /* default off */ }

export function privacyMode(): PrivacyMode { return mode; }

export function setPrivacyMode(m: PrivacyMode): void {
    if (!isPrivacyMode(m)) return;
    mode = m;
    try { mkdirSync(dirname(FILE), { recursive: true }); writeFileSync(FILE, JSON.stringify({ mode }), { mode: 0o600 }); } catch { /* best effort */ }
}

/** Whether Tor is even available to route through (the proxy is configured). */
export const torAvailable = (): boolean => !!process.env.TOR_SOCKS?.trim();

/** Clearnet relays should route via Tor (balanced or strict). */
export const relaysViaTor = (): boolean => mode !== 'off';

/** Fail-closed: server fetches must NOT fall back to a direct request when Tor fails. */
export const torStrict = (): boolean => mode === 'strict';
