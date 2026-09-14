// Settings page + the network editors: NIP-65 relays (kind:10002) and Blossom
// media servers (kind:10063). Both are edited "form-as-state": the form carries
// the whole draft, so add/remove are stateless re-renders (helmjs swaps the
// section; zero-JS reloads the page) and Save publishes - bunker signs + publishes
// here; nip07 sign-and-continues. A relay save also updates s.myRelays + invalidates
// the routed-feed caches and persists, so the whole app uses the new list.

import { settingsPage, relaySection, localRelaySection, localRelayStatusLine, dmRelaySection, mediaSection, relayScoreChip, searchRelayEditor, privacySection, warmingDone, savedTick, contentFiltersForm, backupSection, relaysTwoPane, RELAY_PANES, SETTINGS_TABS, type SettingsView, type SettingsTab, type RelayPane } from '../render/settings.ts';
import { localRelay, setLocalRelay, localRelayUrl } from '../local-relay.ts';
import { getFilters, saveFilters } from '../data/filters.ts';
import { getContentPrefs, saveContentPrefs, timelineEntries, CONTENT_TYPES } from '../data/content-prefs.ts';
import { privacyMode, setPrivacyMode, isPrivacyMode } from '../privacy.ts';
import { accountMenu, feedSwitch } from '../render/layout.ts';
import { html } from '../html.ts';
import { fetchTrustScore } from '../data/trust.ts';
import { relayListTemplate, publishRelayList, publishRelayListSigned, clearRelayListCache } from '../data/relays.ts';
import { fetchMyDmRelays, dmRelayListTemplate, publishDmRelayList, publishDmRelayListSigned } from '../data/dm-relays.ts';
import { clearDmRelaysCache } from '../data/dm-routing.ts';
import { KIND_DM_RELAYS } from '../nostr/nip17.ts';
import { fetchBlossomServers, serverListTemplate, publishServerList, publishServerListSigned } from '../upload.ts';
import { readSignedEvent, verifySigned } from '../nip07.ts';
import { persistSession } from '../session.ts';
import { requireLogin, chromeFor, meFor, LAND_ON_FEED } from './common.ts';
import { feedDocument } from './feed.ts';
import { readAppearance, writeAppearance, parseRelayList } from '../theme.ts';
import { parseRelayList as parseRelayListEvent, normalizeRelayUrl } from '../nostr/nip65.ts';
import { BACKUP_KINDS, BACKUP_VERSION, gatherBackup, restoreTemplate, restoreTargets } from '../data/list-backup.ts';
import { anyAccepted } from '../data/pool.ts';
import { ensureRelayInfo } from '../data/relay-info.ts';
import { SEARCH_NOTE_RELAYS, SEARCH_PROFILE_RELAYS } from '../data/search.ts';
import { readForm, readUpload, readBatchResults, sendPage, sendFragment, sendSignRequest, notFound, redirect, type Ctx } from '../http.ts';
import { isSingleUser } from '../access.ts';
import type { Session } from '../session.ts';
import { signsOnClient } from '../session.ts';
import type { RelayEntry, RelayList, NostrEvent } from '../nostr/types.ts';

const KIND_RELAY_LIST = 10002;
const KIND_BLOSSOM_LIST = 10063;
const PLACE_RELAY = { 'H-Reswap': 'outer', 'H-Retarget': '#relay-section' };
const PLACE_DM_RELAY = { 'H-Reswap': 'outer', 'H-Retarget': '#dm-relay-section' };
const PLACE_BACKUP = { 'H-Reswap': 'outer', 'H-Retarget': '#backup-section' };
const PLACE_MEDIA = { 'H-Reswap': 'outer', 'H-Retarget': '#media-section' };

// --- drafts ----------------------------------------------------------------

/** The editable [{url,read,write}] draft for a RelayList (read∪write rows). */
function draftFromList(list: RelayList | null): RelayEntry[] {
    const l = list ?? { read: [], write: [] };
    const urls = [...new Set([...l.read, ...l.write])];
    return urls.map((url) => ({ url, read: l.read.includes(url), write: l.write.includes(url) }));
}

/** Reconstruct the relay draft from a submitted editor form (form-as-state). */
function relayDraftFromForm(form: URLSearchParams): RelayEntry[] {
    const read = new Set(form.getAll('read'));
    const write = new Set(form.getAll('write'));
    const seen = new Set<string>();
    const out: RelayEntry[] = [];
    for (const url of form.getAll('relay')) {
        if (seen.has(url)) continue;
        seen.add(url);
        out.push({ url, read: read.has(url), write: write.has(url) });
    }
    return out;
}

const mediaDraftFromForm = (form: URLSearchParams): string[] => [...new Set(form.getAll('server'))];

function normalizeRelayInput(rawUrl: string): string | null {
    let url = rawUrl.trim();
    if (!url) return null;
    if (!/^wss?:\/\//i.test(url)) url = `wss://${url}`;
    try { new URL(url); } catch { return null; }
    return url;
}

function normalizeMediaInput(rawUrl: string): string | null {
    let url = rawUrl.trim();
    if (!url) return null;
    if (!/^https?:\/\//i.test(url)) url = `https://${url}`;
    try { new URL(url); } catch { return null; }
    return url.replace(/\/+$/, '');
}

// --- page assembly ---------------------------------------------------------

/** Build the whole settings view, fetching whatever a draft override doesn't
 * supply (the media-server list needs a relay query). Used for zero-JS full-page
 * re-renders; the helmjs path swaps just one section and skips the other fetch. */
async function buildView(ctx: Ctx, s: Session & { me: string }, ov: Partial<SettingsView> = {}, active: SettingsTab = 'general', pane: RelayPane = 'general'): Promise<SettingsView> {
    const a = readAppearance(ctx);
    const relayDraft = ov.relayDraft ?? draftFromList(s.myRelays);
    // Only fetch what the ACTIVE panel renders: media servers on General, DM relays only on the Relays >
    // DMs pane; everything else skips both uncached relay round-trips (a tab/pane GET, and helmjs hover-
    // prefetch across the nav, would otherwise re-pay them for panels that never use the result).
    const [mediaDraft, dmRelayDraft] = await Promise.all([
        ov.mediaDraft !== undefined ? Promise.resolve(ov.mediaDraft) : active === 'general' ? fetchBlossomServers(s.pool, s.me, s.myRelays).catch(() => []) : Promise.resolve([] as string[]),
        ov.dmRelayDraft !== undefined ? Promise.resolve(ov.dmRelayDraft) : (active === 'relays' && pane === 'dm') ? fetchMyDmRelays(s.pool, s.me, s.myRelays?.read ?? []).catch(() => []) : Promise.resolve([] as string[]),
    ]);
    const searchNoteDraft = ov.searchNoteDraft ?? a.searchNoteRelays;
    const searchProfileDraft = ov.searchProfileDraft ?? a.searchProfileRelays;
    // NIP-11: warm the relay identity cache for whatever the Relays tab is about to render (names,
    // auth/paid badges, the no-NIP-50 warning). Short race - fast documents make this paint, slow
    // ones land in the cache for the next visit; the tab never waits on a dead relay's HTTP.
    if (active === 'relays') {
        const urls = [...relayDraft.map((r) => r.url), ...dmRelayDraft, ...searchNoteDraft, ...searchProfileDraft];
        await ensureRelayInfo(urls, 1200);
    }
    const filters = ov.filters ?? getFilters(s.me);
    const contentPrefs = ov.contentPrefs ?? getContentPrefs(s.me);
    return { a, relayDraft, mediaDraft, dmRelayDraft, searchNoteDraft, searchProfileDraft, filters, contentPrefs, localRelay: localRelay(), localRelayAuth: localAuthUI(s), ...ov };
}

/** POST /settings/content-prefs - the AUTO-SAVING content-types grid: per-kind feed/profile visibility plus
 * the promoted-timeline column. Reads ONLY its own fields (feed_/profile_/timeline_ over CONTENT_TYPES). On
 * the helmjs path it returns just the "Saved ✓" tick (swaps #content-saved, leaving the grid untouched) plus
 * an OOB switcher rebuild (a timeline toggle changes the promoted list); no-JS falls back to a full reload. */
export async function postContentPrefs(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const form = await readForm(ctx.req);
    const pick = (p: 'feed' | 'profile' | 'timeline') => Object.fromEntries(CONTENT_TYPES.map((c) => [c.id, form.get(`${p}_${c.id}`) === '1']));
    saveContentPrefs(s.me, { feed: pick('feed'), profile: pick('profile'), timeline: pick('timeline') });
    if (ctx.isPartial) {
        // Promoting/demoting a type changes the header switcher's timeline list, so OOB-rebuild the switcher
        // (the settings page's onPage variant, summary "Settings") - otherwise the new timeline only appears
        // after a full page reload.
        const timelines = timelineEntries(s.me);
        sendFragment(ctx, html`${savedTick(true)}${feedSwitch({ title: 'Settings', timelines, oob: true })}`);
    } else redirect(ctx, '/settings/content');
}

/** POST /settings/content-filters - the EXPLICIT-Save content-filtering form: keyword/regex patterns + the
 * hide-post-types flags (a separate store from the types grid). Reads ONLY its own fields. On the helmjs path
 * it re-renders its own form with the "Saved ✓" status; no-JS falls back to a full reload. */
export async function postContentFilters(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const form = await readForm(ctx.req);
    const patterns = (form.get('patterns') ?? '').split('\n').map((l) => l.trim()).filter(Boolean);
    const flags = (p: 'feed' | 'profile') => ({
        hideReplies: form.get(`${p}_hideReplies`) === '1',
        hideQuotes: form.get(`${p}_hideQuotes`) === '1',
        hideLinkOnly: form.get(`${p}_hideLinkOnly`) === '1',
    });
    saveFilters(s.me, { patterns, feed: flags('feed'), profile: flags('profile') });
    if (ctx.isPartial) sendFragment(ctx, contentFiltersForm(getFilters(s.me), 'Saved ✓'));
    else redirect(ctx, '/settings/content');
}

/** A zero-JS full-page re-render, landing on `active` so a no-JS save (or nav) stays on its
 * own tab instead of snapping back to General. `active` defaults to General (the bare /settings). */
async function sendFullPage(ctx: Ctx, s: Session & { me: string }, ov: Partial<SettingsView>, active: SettingsTab = 'general', pane: RelayPane = 'general'): Promise<void> {
    sendPage(ctx, settingsPage(await buildView(ctx, s, ov, active, pane), active, pane), chromeFor(ctx, s, { active: 'settings', title: 'Settings' }));
}

/** GET /settings - the settings page, General tab (the bare entry point). */
export async function getSettings(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    await sendFullPage(ctx, s, {}, 'general');
}

/** GET /settings/:tab - one URL per tab (deep-linkable, reload-stable). Unknown slugs redirect to
 * /settings. On the helmjs path (a tab-link partial) it swaps just #settings-page; no-JS renders the
 * full page on that tab. The deeper GETs (/settings/backup/export, /settings/privacy/status) are
 * 3-segment routes registered before this 2-segment `:tab`, so they never fall through to here. */
export async function getSettingsTab(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const slug = ctx.params.tab;
    if (slug === 'search') { redirect(ctx, '/settings/relays/search'); return; } // Search folded into Relays > Search
    const tab = SETTINGS_TABS.find((t) => t.slug === slug)?.slug;
    if (!tab) { redirect(ctx, '/settings'); return; }
    const view = await buildView(ctx, s, {}, tab);
    if (ctx.isPartial) sendFragment(ctx, settingsPage(view, tab));
    else sendPage(ctx, settingsPage(view, tab), chromeFor(ctx, s, { active: 'settings', title: 'Settings' }));
}

/** GET /settings/relays/:pane - the Relays tab's two-pane hub (General / DMs / Search / Private). A
 * left-nav link partial-swaps #relays-two-pane and pushes /settings/relays/<pane>; a reload / deep-link
 * renders the whole settings page on the Relays tab with that pane active. */
export async function getRelaysPane(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const pane = RELAY_PANES.find((p) => p.slug === ctx.params.pane)?.slug ?? 'general';
    const view = await buildView(ctx, s, {}, 'relays', pane);
    if (ctx.isPartial) sendFragment(ctx, relaysTwoPane(view, pane));
    else sendPage(ctx, settingsPage(view, 'relays', pane), chromeFor(ctx, s, { active: 'settings', title: 'Settings' }));
}

/** GET /settings/relay-score?url= - lazily resolve a relay's trust assertion (kind 30385, read off
 * nostr) into the chip (self-swaps via helmjs intersect). Best-effort; null → '?'. */
export async function getRelayScore(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const url = ctx.query.get('url') ?? '';
    // Preserve the caller's chip id (search lists namespace it), but it's reflected into an
    // attribute/h-target via raw() in relayScoreChip, so allowlist it (legit ids are rscore-…).
    const rawId = ctx.query.get('id') || undefined;
    const id = rawId && /^[A-Za-z0-9_-]+$/.test(rawId) ? rawId : undefined;
    // Trust assertions are always on (the per-user toggle was removed); an operator can still disable the
    // whole feature by setting SATORI_TRUST_PROVIDER empty, which makes fetchTrustScore resolve null.
    if (!url) { sendFragment(ctx, relayScoreChip(url, null, id)); return; }
    const score = await fetchTrustScore(s.pool, url).catch(() => null);
    sendFragment(ctx, relayScoreChip(url, score, id));
}

/** POST /settings/privacy - set the server-wide Tor Privacy Mode (off/balanced/
 * strict) and re-render the section. Applies to new relay dials + fetches at once;
 * existing relay connections switch as they reconnect. */
export async function postPrivacy(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const form = await readForm(ctx.req);
    const m = form.get('mode') ?? '';
    const changed = isPrivacyMode(m) && m !== privacyMode();
    if (changed) {
        setPrivacyMode(m);
        // Drop this session's relay sockets so they immediately re-dial under the new
        // routing (Tor vs direct), instead of waiting for natural reconnect.
        s.pool.recycle();
        // recycle() also tore down the bunker's long-lived NIP-46 subscription, so rebuild it
        // now - otherwise the next signed action hangs ~30s until recover() kicks in. No-op for
        // nip07 (browser holds the key, no server-side subscription).
        s.signer?.reconnect();
    }
    if (ctx.isPartial) {
        // Re-render the section, and (on change) OOB-swap the account hub so the bar
        // avatar flips to/from the shield live, no reload.
        sendFragment(ctx, html`${privacySection()}${changed ? accountMenu(meFor(s), true) : html``}`);
        return;
    }
    await sendFullPage(ctx, s, {}, 'privacy');
}

/** GET /settings/privacy/status - the warming indicator's poller. Drives the relay
 * warm-up (idempotent) and reports real connected/total progress; removes itself (an
 * empty outer-swap) when ready, dismissed, or Privacy Mode is off. */
export async function getPrivacyStatus(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    if (!ctx.isPartial) { redirect(ctx, '/settings/privacy'); return; }
    const read = s.myRelays?.read ?? [];
    if (ctx.query.get('dismiss') || privacyMode() === 'off' || !read.length) { sendFragment(ctx, html``); return; } // remove the widget
    s.pool.warm(read);              // drive the warm-up (idempotent)
    const p = s.pool.warmProgress();
    const done = !!p && (p.ready || (p.total > 0 && p.connected >= p.total));
    if (done) { sendFragment(ctx, warmingDone(p!.connected, p!.total)); return; }
    // Still warming: don't redraw the sweeping bar (H-Reswap:none = no swap), just let
    // the poller keep checking, so the animation stays continuous.
    sendFragment(ctx, html``, { 'H-Reswap': 'none' });
}

// --- relays (kind:10002) ---------------------------------------------------

async function respondRelays(ctx: Ctx, s: Session & { me: string }, draft: RelayEntry[], status?: string, err = false): Promise<void> {
    if (ctx.isPartial) sendFragment(ctx, relaySection(draft, status, err));
    else await sendFullPage(ctx, s, { relayDraft: draft, relayStatus: status, relayErr: err }, 'relays', 'general');
}

/** After a relay list changes: invalidate routed feeds (their outbox routing is
 * now stale) + the relay-list cache, refresh the NIP-42 auth scope (else a newly
 * added auth-required relay isn't authed to until the next login/restart), and
 * persist so a refresh uses the new set. */
function applyNewRelays(s: Session & { me: string }, next: RelayList): void {
    s.myRelays = next;
    s.followsRoute = null;
    s.followersRoute = null;
    s.pool.setAuthRelays([...next.write, ...next.read]);
    clearRelayListCache();
    persistSession(s);
}

/** POST /settings/relays/edit - add (newurl) / remove (op="remove:<url>") a row. */
export async function postRelaysEdit(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const form = await readForm(ctx.req);
    let draft = relayDraftFromForm(form);
    const op = form.get('op') ?? '';
    if (op === 'add') {
        const url = normalizeRelayInput(form.get('newurl') ?? '');
        if (url && !draft.some((r) => r.url === url)) draft.push({ url, read: form.has('newread'), write: form.has('newwrite') });
    } else if (op.startsWith('remove:')) {
        const url = op.slice('remove:'.length);
        draft = draft.filter((r) => r.url !== url);
    }
    await respondRelays(ctx, s, draft);
}

/** POST /settings/relays - publish kind:10002 (bunker) / sign-and-continue (nip07). */
export async function postRelays(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const draft = relayDraftFromForm(await readForm(ctx.req));
    if (signsOnClient(s)) { sendSignRequest(ctx, relayListTemplate(s.me, draft), '/settings/relays/publish'); return; }
    try {
        const next = await publishRelayList(s.pool, s.signer!, s.me, draft);
        applyNewRelays(s, next);
        await respondRelays(ctx, s, draftFromList(next), 'Saved ✓');
    } catch (err) {
        await respondRelays(ctx, s, draft, err instanceof Error ? err.message : 'Could not publish.', true);
    }
}

/** nip07 continuation: verify + publish the extension-signed kind:10002. */
export async function postRelaysPublish(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const signed = await readSignedEvent(ctx.req);
    if (!signed || signed.pubkey !== s.me || signed.kind !== KIND_RELAY_LIST) {
        sendFragment(ctx, relaySection(draftFromList(s.myRelays), 'Couldn’t verify the signed relay list.', true), PLACE_RELAY, 400);
        return;
    }
    try {
        const next = await publishRelayListSigned(s.pool, signed);
        applyNewRelays(s, next);
        sendFragment(ctx, relaySection(draftFromList(next), 'Saved ✓'), PLACE_RELAY);
    } catch (err) {
        sendFragment(ctx, relaySection(draftFromList(s.myRelays), err instanceof Error ? err.message : 'Could not publish.', true), PLACE_RELAY, 502);
    }
}

// --- DM relays (NIP-17 kind:10050) -----------------------------------------
// Same form-as-state + publish flow as kind:10002, but a flat url list (no read/write)
// and a different kind. A save invalidates the shared per-pubkey DM-relay cache (one cache
// for both DM engines now) so the next read/send/dot-poll uses the freshly published list.

const dmRelayDraftFromForm = (form: URLSearchParams): string[] => form.getAll('dmrelay').map(String);

function applyNewDmRelays(s: Session & { me: string }): void {
    clearDmRelaysCache(s.me);
}

async function respondDmRelays(ctx: Ctx, s: Session & { me: string }, draft: string[], status?: string, err = false): Promise<void> {
    if (ctx.isPartial) sendFragment(ctx, dmRelaySection(draft, status, err));
    else await sendFullPage(ctx, s, { dmRelayDraft: draft, dmRelayStatus: status, dmRelayErr: err }, 'relays', 'dm');
}

/** POST /settings/dm-relays/edit - add (newurl) / remove (op="remove:<url>") a row. */
export async function postDmRelaysEdit(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const form = await readForm(ctx.req);
    let draft = dmRelayDraftFromForm(form);
    const op = form.get('op') ?? '';
    if (op === 'add') {
        const url = normalizeRelayInput(form.get('newurl') ?? '');
        if (url && !draft.includes(url) && draft.length < 8) draft.push(url);
    } else if (op.startsWith('remove:')) {
        draft = draft.filter((u) => u !== op.slice('remove:'.length));
    }
    await respondDmRelays(ctx, s, draft);
}

/** POST /settings/dm-relays - publish kind:10050 (bunker) / sign-and-continue (nip07). */
export async function postDmRelays(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const draft = dmRelayDraftFromForm(await readForm(ctx.req));
    if (signsOnClient(s)) { sendSignRequest(ctx, dmRelayListTemplate(s.me, draft), '/settings/dm-relays/publish'); return; }
    try {
        const next = await publishDmRelayList(s.pool, s.signer!, s.me, draft, s.myRelays?.write ?? []);
        applyNewDmRelays(s);
        await respondDmRelays(ctx, s, next, 'Saved ✓');
    } catch (err) {
        await respondDmRelays(ctx, s, draft, err instanceof Error ? err.message : 'Could not publish.', true);
    }
}

/** nip07 continuation: verify + publish the extension-signed kind:10050. */
export async function postDmRelaysPublish(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const signed = await readSignedEvent(ctx.req);
    if (!signed || signed.pubkey !== s.me || signed.kind !== KIND_DM_RELAYS) {
        const current = await fetchMyDmRelays(s.pool, s.me, s.myRelays?.read ?? []).catch(() => []);
        sendFragment(ctx, dmRelaySection(current, 'Couldn’t verify the signed DM relay list.', true), PLACE_DM_RELAY, 400);
        return;
    }
    try {
        const next = await publishDmRelayListSigned(s.pool, signed, s.myRelays?.write ?? []);
        applyNewDmRelays(s);
        sendFragment(ctx, dmRelaySection(next, 'Saved ✓'), PLACE_DM_RELAY);
    } catch (err) {
        const current = await fetchMyDmRelays(s.pool, s.me, s.myRelays?.read ?? []).catch(() => []);
        sendFragment(ctx, dmRelaySection(current, err instanceof Error ? err.message : 'Could not publish.', true), PLACE_DM_RELAY, 502);
    }
}

// --- lists backup & restore -------------------------------------------------
// Export = download the live signed list events as JSON (private lists keep their
// ciphertext). Restore = REPLACE: re-sign each backed-up event with a fresh created_at
// (no decryption) and republish, so it wins the replaceable "newest" rule.

async function respondBackup(ctx: Ctx, s: Session & { me: string }, status?: string, err = false): Promise<void> {
    if (ctx.isPartial) sendFragment(ctx, backupSection(status, err), err ? PLACE_BACKUP : undefined);
    else await sendFullPage(ctx, s, { backupStatus: status, backupErr: err }, 'backup');
}

/** Publish each re-signed list to its relays (write + indexers, + listed DM relays). */
async function publishRestored(s: Session & { me: string }, signed: NostrEvent[]): Promise<number> {
    let ok = 0;
    await Promise.all(signed.map(async (ev) => {
        const results = await s.pool.publish(restoreTargets(ev, s), ev).catch(() => [] as PromiseSettledResult<string>[]);
        if (anyAccepted(results)) ok++;
    }));
    return ok;
}

/** After a restore: refresh session state + invalidate caches the same way the
 * individual editors do, so the running app reflects the restored lists at once. */
function applyRestored(s: Session & { me: string }, signed: NostrEvent[]): void {
    for (const ev of signed) {
        if (ev.kind === KIND_RELAY_LIST) { applyNewRelays(s, parseRelayListEvent(ev)); }
        else if (ev.kind === 3) { s.followsRoute = null; s.followersRoute = null; }
        else if (ev.kind === KIND_DM_RELAYS) { clearDmRelaysCache(s.me); }
        if (ev.kind === 10000 || ev.kind === 10003) { s.lists.set(ev.kind, ev); s.privateTags.delete(ev.kind); }
    }
    persistSession(s);
}

/** GET /settings/backup/export?list=<kind>&... - download the selected lists as JSON. A
 * non-boosted native navigation (the form sets h-boost="false"), so this is a real download. */
export async function getBackupExport(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const picked = ctx.query.getAll('list').map(Number).filter((k) => BACKUP_KINDS.includes(k));
    const events = await gatherBackup(s, picked.length ? picked : BACKUP_KINDS);
    const lists: Record<string, NostrEvent> = {};
    for (const ev of events) lists[String(ev.kind)] = ev;
    const body = JSON.stringify({ version: BACKUP_VERSION, exportedAt: Math.floor(Date.now() / 1000), pubkey: s.me, lists }, null, 2);
    ctx.res.writeHead(200, {
        'Content-Type': 'application/json; charset=utf-8',
        'Content-Disposition': 'attachment; filename="satori-lists-backup.json"',
        'Cache-Control': 'no-store',
    });
    ctx.res.end(body);
}

/** Pull the restorable signed events for the selected kinds out of an uploaded backup. */
function eventsFromBackup(parsed: unknown, selected: number[], me: string): NostrEvent[] {
    const lists = (parsed as { lists?: Record<string, unknown> } | null)?.lists;
    if (!lists || typeof lists !== 'object') return [];
    const out: NostrEvent[] = [];
    for (const k of selected) {
        const ev = verifySigned(lists[String(k)]);
        if (ev && ev.kind === k && ev.pubkey === me) out.push(ev);
    }
    return out;
}

/** POST /settings/backup/import - upload a backup, restore the selected lists (replace).
 * Bunker signs + publishes here; nip07 sign-and-continues a batch. */
export async function postBackupImport(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const { fields, file } = await readUpload(ctx.req, 1024 * 1024); // a lists backup is a few KB; cap tight
    if (!file) { await respondBackup(ctx, s, 'Choose a backup file to restore.', true); return; }
    let parsed: unknown;
    try { parsed = JSON.parse(file.bytes.toString('utf8')); } catch { await respondBackup(ctx, s, 'That file isn’t a valid backup (couldn’t read JSON).', true); return; }
    const filePubkey = (parsed as { pubkey?: unknown } | null)?.pubkey;
    if (typeof filePubkey === 'string' && filePubkey !== s.me) { await respondBackup(ctx, s, 'This backup is from a different account, so it can’t be restored here.', true); return; }
    const selected = fields.getAll('list').map(Number).filter((k) => BACKUP_KINDS.includes(k));
    const events = eventsFromBackup(parsed, selected, s.me);
    if (events.length === 0) { await respondBackup(ctx, s, 'Nothing to restore (no matching lists in the file).', true); return; }
    const templates = events.map((ev) => restoreTemplate(ev, s.me));
    if (signsOnClient(s)) { sendSignRequest(ctx, { templates }, '/settings/backup/restore', 'sign_event_batch'); return; }
    try {
        const signed = await Promise.all(templates.map((t) => s.signer!.signEvent(t) as Promise<NostrEvent>));
        if (!signed.every((ev) => ev.pubkey === s.me)) { await respondBackup(ctx, s, 'Restore aborted: the signer returned a mismatched key.', true); return; }
        const n = await publishRestored(s, signed);
        applyRestored(s, signed);
        await respondBackup(ctx, s, `Restored ${n} list${n === 1 ? '' : 's'} ✓`);
    } catch (err) {
        await respondBackup(ctx, s, err instanceof Error ? err.message : 'Could not restore.', true);
    }
}

/** nip07 continuation: the batch-signed list events come back; publish them. */
export async function postBackupRestore(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    if (!signsOnClient(s)) { notFound(ctx); return; } // client-signs-only continuation
    const results = await readBatchResults(ctx.req);
    if (!results) { sendFragment(ctx, backupSection('Could not restore (no signed events).', true), PLACE_BACKUP); return; }
    const signed: NostrEvent[] = [];
    for (const r of results) {
        if (!r.ok) continue;
        const ev = verifySigned(r.value);
        if (ev && ev.pubkey === s.me && BACKUP_KINDS.includes(ev.kind)) signed.push(ev);
    }
    if (signed.length === 0) { sendFragment(ctx, backupSection('Could not restore (couldn’t verify the signed lists).', true), PLACE_BACKUP); return; }
    const n = await publishRestored(s, signed);
    applyRestored(s, signed);
    sendFragment(ctx, backupSection(`Restored ${n} list${n === 1 ? '' : 's'} ✓`), PLACE_BACKUP);
}

// --- search relays (NIP-50, cookie-backed) ---------------------------------
// Same form-as-state editor as relays, but local (no publish): edit re-renders the
// list, save writes the appearance cookie. The draft rides in the form's hidden
// `relay` inputs; `kind` (note|profile) selects which list.

const searchKind = (form: URLSearchParams): 'note' | 'profile' => (form.get('kind') === 'profile' ? 'profile' : 'note');

async function respondSearch(ctx: Ctx, s: Session & { me: string }, kind: 'note' | 'profile', urls: string[], status?: string): Promise<void> {
    if (ctx.isPartial) sendFragment(ctx, searchRelayEditor(kind, urls, status));
    else await sendFullPage(ctx, s, kind === 'note' ? { searchNoteDraft: urls } : { searchProfileDraft: urls }, 'relays', 'search');
}

/** POST /settings/search/edit - add (newurl) / remove (op="remove:<url>") a row. */
export async function postSearchEdit(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const form = await readForm(ctx.req);
    const kind = searchKind(form);
    let draft = form.getAll('relay').map(String);
    const op = form.get('op') ?? '';
    if (op === 'add') {
        const [url] = parseRelayList(form.get('newurl') ?? '', []);
        if (url && !draft.includes(url) && draft.length < 8) draft.push(url);
    } else if (op.startsWith('remove:')) {
        draft = draft.filter((u) => u !== op.slice('remove:'.length));
    }
    await respondSearch(ctx, s, kind, draft);
}

/** POST /settings/search - persist the edited list to the appearance cookie. */
export async function postSearchSave(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const form = await readForm(ctx.req);
    const kind = searchKind(form);
    const fallback = kind === 'note' ? SEARCH_NOTE_RELAYS : SEARCH_PROFILE_RELAYS;
    const saved = parseRelayList(form.getAll('relay').map(String).join('\n'), fallback);
    const a = readAppearance(ctx);
    if (kind === 'note') a.searchNoteRelays = saved; else a.searchProfileRelays = saved;
    writeAppearance(ctx, a);
    await respondSearch(ctx, s, kind, saved, 'Saved ✓');
}

// --- private relay (daemon-side aggregator/outbox/blaster, NOT published) -----
// One private relay: a master `use` on/off, then Read + Write each add|only. Persisted server-side
// (local-relay.ts), kept OUT of your published NIP-65 list. "only" routes exclusively here.

const localUse = (form: URLSearchParams, name: 'read' | 'write'): 'add' | 'only' => (form.get(name) === 'only' ? 'only' : 'add');

/** POST /settings/local-relay - set (or clear) the private relay + its use/read/write policy.
 * An invalid URL leaves the current config untouched (a typo mustn't wipe it). On any real change
 * we drop the routed-feed cache + recycle sockets so it takes effect on the next fetch/publish. */
export async function postLocalRelay(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    if (!isSingleUser()) { notFound(ctx); return; } // single-user only: don't let a shared instance clobber the global config
    const form = await readForm(ctx.req);
    const rawUrl = (form.get('url') ?? '').trim();
    const enabled = form.get('use') === 'on';
    const read = localUse(form, 'read');
    const write = localUse(form, 'write');
    const fetchMissing = form.get('fetchmissing') === 'on';
    let status: string;
    let err = false;
    if (!rawUrl) {
        setLocalRelay('', false, 'add', 'add', false);
        status = 'Private relay removed ✓';
    } else if (!normalizeRelayUrl(rawUrl, { assumeWss: true })) {
        status = 'Not a valid relay URL (use ws://, wss:// or a host).';
        err = true; // leave the current config untouched
    } else {
        setLocalRelay(rawUrl, enabled, read, write, fetchMissing);
        status = enabled ? `Saved ✓ (read: ${read}, write: ${write})` : 'Saved ✓ (off)';
    }
    if (!err) {
        s.followsRoute = null;
        s.followersRoute = null;
        s.pool.recycle();      // re-dial (or drop) the private relay now, like the Privacy toggle
        s.signer?.reconnect(); // recycle tore down the bunker subscription; rebuild it (no-op for nip07)
    }
    // Fold NIP-42 auth into Save (nip07, JS path) when the relay is ON: the moment you turn it on is exactly
    // when an auth prompt makes sense, so a nip07 user never lands on a silently-blank feed. If it issues a
    // challenge, chain into the sign round-trip now. Bunker auto-auths (skip); a relay that needs no auth
    // returns no challenge, so we just save. No-JS can't sign, so it falls through too.
    if (!err && rawUrl && enabled && ctx.isPartial && signsOnClient(s)) {
        const lr = localRelay();
        if (lr && lr.enabled) {
            const challenge = await s.pool.localRelayChallenge();
            if (challenge) {
                const template = { kind: 22242, created_at: Math.floor(Date.now() / 1000), tags: [['relay', lr.url], ['challenge', challenge]], content: '', pubkey: s.me };
                sendSignRequest(ctx, template, '/settings/local-relay/auth/complete');
                return;
            }
        }
    }
    if (ctx.isPartial) { sendFragment(ctx, localRelaySection(localRelay(), status, err, localAuthUI(s))); return; }
    await sendFullPage(ctx, s, { localRelayStatus: status, localRelayErr: err }, 'relays', 'private');
}

// --- local relay NIP-42 auth (nip07 only; bunker auto-auths via automaticallyAuth) ---
const PLACE_LOCAL_RELAY = { 'H-Reswap': 'outer', 'H-Retarget': '#local-relay-section' };

/** The auth UI state for the local-relay section: show the Authenticate button only for a nip07 session
 * with a relay that's actually in use (read or write on). Bunker signs the challenge server-side. */
function localAuthUI(s: Session): { needsAuth: boolean; authed: boolean } {
    const lr = localRelay();
    return {
        needsAuth: signsOnClient(s) && !!lr && lr.enabled,
        authed: s.pool.isLocalRelayAuthed(),
    };
}

/** POST /settings/local-relay/auth - nip07: connect, read the relay's NIP-42 challenge, and ask the
 * browser extension to sign it (the daemon can't sign a background challenge for a nip07 key). */
export async function postLocalRelayAuth(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    // `reauth` = the app-wide banner (from any page); else the Settings button. The banner reloads on
    // done/failure (its target section isn't on the page); the button swaps its section inline.
    const reauth = ctx.query.get('reauth') === '1';
    const fail = (status: string): void => {
        if (reauth) { ctx.res.writeHead(200, { 'H-Refresh': 'true' }); ctx.res.end(); return; }
        sendFragment(ctx, localRelaySection(localRelay(), status, true, localAuthUI(s)), PLACE_LOCAL_RELAY);
    };
    if (!signsOnClient(s)) { fail('Bunker logins authenticate automatically.'); return; }
    const url = localRelayUrl();
    if (!url) { fail('Set a local relay URL first.'); return; }
    const challenge = await s.pool.localRelayChallenge();
    if (!challenge) { fail('No auth challenge from the relay (it may not require auth, or is unreachable).'); return; }
    const template = { kind: 22242, created_at: Math.floor(Date.now() / 1000), tags: [['relay', url], ['challenge', challenge]], content: '', pubkey: s.me };
    sendSignRequest(ctx, template, reauth ? '/settings/local-relay/auth/complete?reauth=1' : '/settings/local-relay/auth/complete');
}

/** nip07 continuation: the extension-signed kind:22242 comes back; send it as AUTH on the relay socket. */
export async function postLocalRelayAuthComplete(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    if (!signsOnClient(s)) { notFound(ctx); return; }
    const reauth = ctx.query.get('reauth') === '1';
    // The banner path lands on the feed via the sign-continuation seam (a body swap the continuation DOES
    // honor - H-Refresh only works on a direct helm.js request, not here, which is why the page didn't
    // reload on its own). On success the feed loads authed and the banner is gone; on failure it re-renders
    // with the banner so you can retry. The Settings-button path swaps its own section inline as before.
    const landOnFeed = async (): Promise<void> => { s.followsRoute = null; s.followersRoute = null; sendFragment(ctx, await feedDocument(ctx, s), LAND_ON_FEED); };
    const signed = await readSignedEvent(ctx.req);
    if (!signed || signed.pubkey !== s.me || signed.kind !== 22242) {
        if (reauth) { await landOnFeed(); return; }
        sendFragment(ctx, localRelaySection(localRelay(), 'Couldn’t verify the signed auth event.', true, localAuthUI(s)), PLACE_LOCAL_RELAY, 400);
        return;
    }
    const ok = await s.pool.completeLocalRelayAuth(signed);
    if (ok) { s.followsRoute = null; s.followersRoute = null; } // re-route reads over the now-authed socket
    if (reauth) { await landOnFeed(); return; }
    sendFragment(ctx, localRelaySection(localRelay(), ok ? 'Authenticated ✓' : 'The relay rejected the authentication.', !ok, localAuthUI(s)), PLACE_LOCAL_RELAY, ok ? 200 : 502);
}

/** GET /settings/local-relay/status - a live probe (reachable? serving?) for the status line, fetched on
 * pane load and by the "recheck" link. */
export async function getLocalRelayStatus(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const state = await s.pool.probeLocalRelay();
    sendFragment(ctx, localRelayStatusLine(state));
}

// --- media servers (kind:10063) --------------------------------------------

async function respondMedia(ctx: Ctx, s: Session & { me: string }, draft: string[], status?: string, err = false): Promise<void> {
    if (ctx.isPartial) sendFragment(ctx, mediaSection(draft, readAppearance(ctx), status, err));
    else await sendFullPage(ctx, s, { mediaDraft: draft, mediaStatus: status, mediaErr: err }, 'general');
}

/** POST /settings/media/edit - add / remove a Blossom server row. */
export async function postMediaEdit(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const form = await readForm(ctx.req);
    let draft = mediaDraftFromForm(form);
    const op = form.get('op') ?? '';
    if (op === 'add') {
        const url = normalizeMediaInput(form.get('newurl') ?? '');
        if (url && !draft.includes(url)) draft.push(url);
    } else if (op.startsWith('remove:')) {
        const url = op.slice('remove:'.length);
        draft = draft.filter((u) => u !== url);
    }
    await respondMedia(ctx, s, draft);
}

/** POST /settings/media - publish kind:10063 (bunker) / sign-and-continue (nip07). */
export async function postMedia(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const draft = mediaDraftFromForm(await readForm(ctx.req));
    if (signsOnClient(s)) { sendSignRequest(ctx, serverListTemplate(s.me, draft), '/settings/media/publish'); return; }
    try {
        await publishServerList(s.pool, s.signer!, s.me, s.myRelays, draft);
        await respondMedia(ctx, s, draft, 'Saved ✓');
    } catch (err) {
        await respondMedia(ctx, s, draft, err instanceof Error ? err.message : 'Could not publish.', true);
    }
}

/** nip07 continuation: verify + publish the extension-signed kind:10063. */
export async function postMediaPublish(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const signed = await readSignedEvent(ctx.req);
    if (!signed || signed.pubkey !== s.me || signed.kind !== KIND_BLOSSOM_LIST) {
        sendFragment(ctx, mediaSection([], readAppearance(ctx), 'Couldn’t verify the signed media-server list.', true), PLACE_MEDIA, 400);
        return;
    }
    const draft = signed.tags.filter((t) => t[0] === 'server' && t[1]).map((t) => t[1]!);
    try {
        await publishServerListSigned(s.pool, signed, s.myRelays);
        sendFragment(ctx, mediaSection(draft, readAppearance(ctx), 'Saved ✓'), PLACE_MEDIA);
    } catch (err) {
        sendFragment(ctx, mediaSection(draft, readAppearance(ctx), err instanceof Error ? err.message : 'Could not publish.', true), PLACE_MEDIA, 502);
    }
}
