// On-disk persistence for undo-window holds, mirroring store.ts. A signed-but-not-
// yet-published event survives a server restart, so a crash/reload inside the brief
// undo window doesn't silently drop the post - resumeHolds() (undo.ts) re-arms the
// auto-commit on boot. Held events are already signed and bound for public relays
// (no new trust concern); the file is written 0600 and lives under the gitignored
// .data/ (bind-mounted in docker, so it survives the --watch reloads that bit us).

import { join } from 'node:path';
import { jsonStore } from './data/json-store.ts';
import type { Held } from './undo.ts';

const FILE = process.env.SATORI_HOLDS_FILE || join(process.cwd(), '.data', 'holds.json');

// mtime-cached read (gained for free here - was an uncached read+parse before) + 0o600 write.
const { readAll, writeAll } = jsonStore<Record<string, Held>>(FILE, 'undo-store');

export function loadHolds(): Record<string, Held> { return readAll(); }

export function saveHold(token: string, h: Held): void {
    const all = readAll();
    all[token] = h;
    writeAll(all);
}

export function removeHold(token: string): void {
    const all = readAll();
    if (token in all) { delete all[token]; writeAll(all); }
}
