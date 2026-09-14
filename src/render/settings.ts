// Settings page (Satori's full-page settings, ported to HATEOAS). The local
// prefs (appearance/feed/media/posting) are plain forms over the appearance
// cookie; the NETWORK editors (relays kind:10002, media servers kind:10063) are
// "form-as-state" editors - the form itself carries the draft, so add/remove are
// stateless re-renders (helmjs swaps the section; zero-JS reloads the page) and
// Save publishes the whole list. No client-side draft array, no app JS.

import { html, raw, join, type SafeHtml } from '../html.ts';
import { shortRelay, shortHash } from './util.ts';
import { DEFAULT_BLOSSOM_SERVER } from '../upload.ts';
import { privacyMode, torAvailable, type PrivacyMode } from '../privacy.ts';
import type { LocalRelay } from '../local-relay.ts';
import { isSingleUser } from '../access.ts';
import type { RelayEntry } from '../nostr/types.ts';
import type { Appearance, Theme } from '../theme.ts';
import type { FeedFilters, SurfaceFlags } from '../data/filters.ts';
import { CONTENT_TYPES, isCoreType, type ContentPrefs } from '../data/content-prefs.ts';
import { BACKUP_LISTS } from '../data/list-backup.ts';
import type { TrustScore } from '../data/trust.ts';
import { relayInfoCached, lacksNip50 } from '../data/relay-info.ts';

/** Everything the settings page renders from. The network editors (relays now,
 * media servers in 7b) carry their draft here so a zero-JS add/remove can
 * re-render the whole page with the edited draft. */
export interface SettingsView {
    a: Appearance;
    relayDraft: RelayEntry[];
    relayStatus?: string;
    relayErr?: boolean;
    dmRelayDraft: string[];       // NIP-17 kind:10050 DM-relay list (editable)
    dmRelayStatus?: string;
    dmRelayErr?: boolean;
    mediaDraft: string[];
    mediaStatus?: string;
    mediaErr?: boolean;
    searchNoteDraft: string[];    // NIP-50 note-search relays (editable list)
    searchProfileDraft: string[]; // NIP-50 people-search relays
    filters: FeedFilters;         // server-side feed content filters
    contentPrefs: ContentPrefs;   // per-kind feed/profile visibility
    backupStatus?: string;        // lists backup/restore result message
    backupErr?: boolean;
    localRelay?: LocalRelay | null; // the private local relay (aggregator/outbox/blaster), daemon-side config
    localRelayStatus?: string;
    localRelayErr?: boolean;
    localRelayAuth?: { needsAuth: boolean; authed: boolean }; // nip07 NIP-42 auth state for the relay

}

/** The shared Save footer for a form-as-state relay editor: the busy-btn (with the section's own
 * `saveLabel`) + the status span. `status`/`statusErr` light the "Saved ✓" tint or the error message,
 * exactly as the four sections did inline. */
function relaySaveFooter(status?: string, statusErr = false, saveLabel = 'Save'): SafeHtml {
    return html`<div class="relay-save">
            <button type="submit" class="busy-btn${status && !statusErr ? ' saved' : ''}"><span class="btn-label">${saveLabel}</span><span class="btn-busy">Saving…</span><span class="btn-done">Saved ✓</span></button>
            ${status && statusErr ? html`<span class="settings-status err">${status}</span>` : html`<span class="settings-status"></span>`}
          </div>`;
}

/** NIP-11 identity on a settings relay row: the relay's self-declared name (description in the
 * tooltip) plus auth/paid chips when its document declares those requirements. Renders nothing
 * until a document is cached - the route warms the cache just before rendering. */
function relayInfoBadges(url: string): SafeHtml | null {
    const i = relayInfoCached(url);
    if (!i) return null;
    return html`${i.name ? html`<span class="relay-inf" title="${i.description ?? ''}">${i.name}</span>` : null}${i.auth ? html`<span class="relay-badge" title="Requires authentication (NIP-42)">auth</span>` : null}${i.payment ? html`<span class="relay-badge" title="Requires payment">paid</span>` : null}`;
}

/** Warn when a search relay is known to lack NIP-50 - search queries skip such relays (the
 * gate and this chip share one predicate, lacksNip50, so the row can't lie about the gate). */
function nip50Warn(url: string): SafeHtml | null {
    return lacksNip50(url)
        ? html`<span class="relay-badge warn" title="This relay doesn't advertise NIP-50 search, so search queries skip it">no search</span>`
        : null;
}

/** The shared ✕ remove button for a relay-editor row. `editPath` is the section's stateless
 * re-render route (e.g. /settings/relays/edit); `sectionId` the element it swaps (e.g. relay-section). */
function relayRemoveBtn(url: string, editPath: string, sectionId: string): SafeHtml {
    return html`<button class="remove-relay ghost" name="op" value="remove:${url}" formaction="${editPath}" h-post="${editPath}" h-target="#${sectionId}" h-swap="outer" title="Remove" aria-label="Remove">✕</button>`;
}

/** The Appearance section: theme (segmented), over the
 * appearance cookie. Both rewrite <html>, so postAppearance answers H-Refresh /
 * 303 (a full reload) - the merge-only-sent-fields write means each form carries
 * just its own field, no hidden mirror inputs needed. */
function appearanceSection(a: Appearance): SafeHtml {
    const themeBtn = (value: Theme, label: string) => html`<button type="submit" class="seg-btn ${a.theme === value ? 'active' : ''}" name="theme" value="${value}">${label}</button>`;
    return html`
      <section>
        <h3>Appearance</h3>
        <form class="row" action="/settings/appearance" method="post" h-post h-swap="none">
          <span>Theme</span>
          <div class="seg">${themeBtn('sumi-e', 'Light')}${themeBtn('sumi-e-dark', 'Dark')}</div>
        </form>
      </section>`;
}

/** Persisted on/off prefs (over the appearance cookie), keyed by field name. */
const PREF_LABELS = {
    autoLoadMedia: 'Auto-load images & videos',
    inlineVideo: 'Load nostr videos inline (fetches from the video host on load)',
    reactions: 'Show reactions button on notes & articles',
    reactionNotifs: 'Show reactions in notifications',
    undoEnabled: 'Undo window before publishing',
} as const;
export type PrefField = keyof typeof PREF_LABELS;
/** The on/off pref fields, so postAppearance can re-render whichever was toggled
 * without re-listing them (adding a pref only touches PREF_LABELS). */
export const PREF_FIELDS = Object.keys(PREF_LABELS) as PrefField[];

/** A persisted on/off pref toggle, in Satori's `.row` (label left, switch right)
 * layout. A boosted button-form (styled as the switch slider) that re-renders
 * ITSELF with the flipped state (helmjs swaps it outer; zero-JS reloads /settings
 * via the 303) - the switch is a submit so it degrades without client JS. */
export function prefToggle(field: PrefField, on: boolean): SafeHtml {
    const label = PREF_LABELS[field];
    return html`
      <form class="row" id="pref-${field}" action="/settings/appearance" method="post" h-post h-target="#pref-${field}" h-swap="outer">
        <span>${label}</span>
        <button type="submit" class="switch-btn ${on ? 'on' : ''}" name="${field}" value="${on ? '' : '1'}" role="switch" aria-checked="${on ? 'true' : 'false'}" aria-label="${label}"></button>
      </form>`;
}

/** The Feed section: how many new posts before the Notes button lights up. The
 * number auto-posts on change (helmjs, h-swap="none"); the Save button is the
 * zero-JS path (303 → reload). */
function feedSection(a: Appearance): SafeHtml {
    return html`
      <section>
        <h3>Feed</h3>
        <form class="row" action="/settings/appearance" method="post" h-post h-swap="none">
          <span>New notes before the Notes button lights up</span>
          <span class="row-controls">
            <input type="number" name="newNotesThreshold" min="1" max="50" value="${String(a.newNotesThreshold)}" h-post="/settings/appearance" h-trigger="change" h-swap="none">
            <noscript><button type="submit" class="ghost">Save</button></noscript>
          </span>
        </form>
      </section>`;
}

/** The Posting section (feed threshold + undo) for the Behavior tab. */
function behaviorPanel(a: Appearance): SafeHtml {
    return html`
      ${feedSection(a)}
      <section>
        <h3>Posting</h3>
        ${prefToggle('undoEnabled', a.undoEnabled)}
        <form class="row" action="/settings/appearance" method="post" h-post h-swap="none">
          <span>Undo seconds</span>
          <span class="row-controls">
            <input type="number" name="undoSeconds" min="1" max="30" value="${String(a.undoSeconds)}" h-post="/settings/appearance" h-trigger="change" h-swap="none">
            <noscript><button type="submit" class="ghost">Save</button></noscript>
          </span>
        </form>
      </section>
      <section>
        <h3>Reactions</h3>
        ${prefToggle('reactions', a.reactions)}
        ${prefToggle('reactionNotifs', a.reactionNotifs)}
      </section>`;
}

/** A NIP-50 search-relay list editor - same form-as-state pattern (and CSS) as the
 * NIP-65 relaySection, minus the read/write + trust chips. `kind` (note|profile)
 * rides in a hidden field; add/remove re-render this section, Save writes the cookie. */
/** Server-side feed content filters: a keyword/regex box + structural toggles. The whole
 * section re-renders on save (helmjs swaps #filters-section; zero-JS reloads). */
// Shared grid header (Type | Feeds | Profile [| extra]) for the show-allowlist and the hide-grid. The
// show-allowlist passes 'Timeline' for its third checkbox column; the hide-grid omits it (two columns).
const gridHead = (extra?: string): SafeHtml => html`<span class="filter-head-left">Type</span><span class="filter-col-head">Feeds</span><span class="filter-col-head">Profile</span>${extra ? html`<span class="filter-col-head">${extra}</span>` : null}`;

/** The inline "Saved ✓" affordance beside the "Content types" heading. Empty until a save; the `.show`
 * class runs a CSS fade. The auto-saving types form targets this span (outer swap), so a save updates ONLY
 * the tick, leaving the grid (and its scroll position) untouched. */
export function savedTick(saved: boolean): SafeHtml {
    return html`<span id="content-saved" class="content-saved${saved ? ' show' : ''}" aria-live="polite">${saved ? 'Saved ✓' : ''}</span>`;
}

/** "Show these kinds" FIELDS (no form/button) - the allowlist grid. Checked = SHOWN. The third column
 * ("Own timeline") promotes a type to its own entry in the header switcher (its kinds, from your follows). */
function showKindsFields(prefs: ContentPrefs): SafeHtml {
    const row = (id: string, label: string): SafeHtml => html`
      <span class="filter-grid-label">${label}</span>
      <label class="filter-cell"><input type="checkbox" name="feed_${id}" value="1"${prefs.feed[id] ? raw(' checked') : raw('')}></label>
      <label class="filter-cell"><input type="checkbox" name="profile_${id}" value="1"${prefs.profile[id] ? raw(' checked') : raw('')}></label>
      <label class="filter-cell"><input type="checkbox" name="timeline_${id}" value="1"${prefs.timeline[id] ? raw(' checked') : raw('')}></label>`;
    const byLabel = (a: { label: string }, b: { label: string }): number => a.label.localeCompare(b.label);
    const core = CONTENT_TYPES.filter((c) => isCoreType(c.id)).sort(byLabel); // notes/polls/articles/pictures/videos
    const tail = CONTENT_TYPES.filter((c) => !isCoreType(c.id)).sort(byLabel); // the niche kinds, behind "Show more"
    return html`
      <h3>Content types ${savedTick(false)}</h3>
      <p class="filter-help">Pick where each type of post shows up: in your main feed, on profiles, or as its own timeline in the header switcher (that type, from the people you follow). Pictures are off in the feed by default, since most are posted as ordinary notes. Choices apply only to what you see, and never leave this machine.</p>
      <div class="filter-grid show-kinds">${gridHead('Own timeline')}${join(core.map((c) => row(c.id, c.label)))}</div>
      <details class="more-types">
        <summary class="more-toggle"></summary>
        <div class="filter-grid show-kinds">${join(tail.map((c) => row(c.id, c.label)))}</div>
      </details>`;
}

/** "Content filtering" FIELDS (no form/button) - keyword/regex box + the hide-post-types grid. The hide
 * grid is the OPPOSITE polarity (checked = HIDDEN), kept a distinct section under the filtering heading. */
function filterFields(f: FeedFilters): SafeHtml {
    const row = (label: string, flag: keyof SurfaceFlags): SafeHtml => html`
      <span class="filter-grid-label">${label}</span>
      <label class="filter-cell"><input type="checkbox" name="feed_${flag}" value="1"${f.feed[flag] ? raw(' checked') : raw('')}></label>
      <label class="filter-cell"><input type="checkbox" name="profile_${flag}" value="1"${f.profile[flag] ? raw(' checked') : raw('')}></label>`;
    return html`
      <h3 class="filter-divide">Content filtering</h3>
      <p class="filter-help">Hide posts containing a word, or matching a /regex/. Applied everywhere; case-insensitive, and matching runs on the daemon, so your filters never leave this machine.</p>
      <textarea class="filter-box" name="patterns" rows="4" spellcheck="false" autocapitalize="none" placeholder="One filter per line. Plain text matches anywhere; wrap in /slashes/ for a regex.">${f.patterns.join('\n')}</textarea>
      <p class="filter-help">Or hide whole categories of post, set independently for your timeline and profile pages.</p>
      <div class="filter-grid">
        ${gridHead()}
        ${row('Hide replies', 'hideReplies')}
        ${row('Hide quote posts', 'hideQuotes')}
        ${row('Hide link-only posts', 'hideLinkOnly')}
      </div>`;
}

/** Content filtering as its OWN form (explicit Save). Kept separate from the auto-saving types grid so the
 * keyword/regex textarea never saves mid-typing; the hide-post-types toggles ride with it under this Save.
 * Re-rendered whole on save (helmjs swaps #content-filters-form; the button shows `status`'s "Saved ✓"). */
export function contentFiltersForm(f: FeedFilters, status?: string): SafeHtml {
    return html`
        <form id="content-filters-form" action="/settings/content-filters" method="post" h-post h-target="#content-filters-form" h-swap="outer">
          ${filterFields(f)}
          <div class="row-controls">
            <button type="submit" class="busy-btn${status ? ' saved' : ''}"><span class="btn-label">Save</span><span class="btn-busy">Saving…</span><span class="btn-done">Saved ✓</span></button>
          </div>
        </form>`;
}

/** The Content tab: TWO independent forms over SEPARATE stores, so neither half is a footgun for the other.
 * Form 1 (content types) AUTO-SAVES - each checkbox change posts to /settings/content-prefs and updates only
 * the inline "Saved ✓" tick (the grid, capped-and-scrolled, is never re-rendered so its scroll is kept); the
 * noscript button is the zero-JS save. Form 2 (content filtering) keeps its explicit Save. */
export function contentTabPanel(prefs: ContentPrefs, f: FeedFilters, status?: string): SafeHtml {
    return html`
      <section id="content-tab">
        <form id="content-types-form" action="/settings/content-prefs" method="post" h-post h-target="#content-saved" h-swap="outer" h-trigger="change">
          ${showKindsFields(prefs)}
          <noscript><div class="row-controls"><button type="submit" class="busy-btn"><span class="btn-label">Save types</span></button></div></noscript>
        </form>
        ${contentFiltersForm(f, status)}
      </section>`;
}

/** Backup tab: export your list events to a JSON file, or restore (replace) them from one.
 * Export is a non-boosted GET (h-boost="false") so the browser downloads the attachment
 * rather than helmjs trying to swap it. Import is a multipart upload that re-signs + republishes. */
export function backupSection(status?: string, statusErr = false): SafeHtml {
    const checkboxes = BACKUP_LISTS.map((l) => html`<label class="backup-item"><input type="checkbox" name="list" value="${String(l.kind)}" checked> ${l.label}</label>`);
    return html`
      <section id="backup-section">
        <h3>Export</h3>
        <p class="filter-help">Download the selected lists as a JSON file to keep or restore later. Private lists (mute, bookmarks) stay encrypted in the file: their contents never leave this machine in the clear.</p>
        <form class="backup-form" action="/settings/backup/export" method="get" h-boost="false">
          <div class="backup-grid">${checkboxes}</div>
          <div class="relay-save"><button type="submit">Export selected</button></div>
        </form>
        <h3 class="filter-subhead">Restore</h3>
        <p class="filter-help">Import a backup file to republish the selected lists. Restore <strong>replaces</strong> each list with the backup (it does not merge), re-signed under your key. Only a backup from this same account can be restored.</p>
        <form class="backup-form" action="/settings/backup/import" method="post" enctype="multipart/form-data" h-post h-target="#backup-section" h-swap="outer">
          <input class="backup-file" type="file" name="backup" accept="application/json,.json" required>
          <div class="backup-grid">${checkboxes}</div>
          <div class="relay-save">
            <button type="submit">Restore selected</button>
            ${status ? html`<span class="settings-status ${statusErr ? 'err' : ''}">${status}</span>` : html`<span class="settings-status"></span>`}
          </div>
        </form>
      </section>`;
}

export function searchRelayEditor(kind: 'note' | 'profile', urls: string[], status?: string, statusErr = false): SafeHtml {
    const id = `search-${kind}-section`;
    const label = kind === 'note' ? 'Note search' : 'People search';
    const rows = urls.length === 0
        ? html`<li class="relay-empty">No relays set. Search falls back to the defaults. Add one below.</li>`
        : urls.map((url) => html`
            <li class="relay-edit-row">
              <input type="hidden" name="relay" value="${url}">
              ${relayScoreChip(url, undefined, `rscore-${kind}-${shortHash(url)}`)}
              <span class="relay-url" title="${url}">${shortRelay(url)}</span>
              ${relayInfoBadges(url)}
              ${nip50Warn(url)}
              ${relayRemoveBtn(url, '/settings/search/edit', id)}
            </li>`);
    return html`
      <section id="${raw(id)}">
        <h3>${label}</h3>
        <form action="/settings/search" method="post" h-post h-target="#${raw(id)}" h-swap="outer">
          <input type="hidden" name="kind" value="${kind}">
          <ul class="relay-editor">${rows}</ul>
          <div class="add-relay">
            <input type="text" name="newurl" placeholder="wss://search.example" autocomplete="off" spellcheck="false">
            <button class="ghost" name="op" value="add" formaction="/settings/search/edit" h-post="/settings/search/edit" h-target="#${raw(id)}" h-swap="outer">Add</button>
          </div>
          ${relaySaveFooter(status, statusErr)}
        </form>
      </section>`;
}

/** Privacy Mode (Tor routing) selector for the Network tab. A segmented control that
 * posts the chosen level and re-renders itself. Reads the live server-wide mode. */
/** The warming indicator under the Tor routing row: an element that polls
 * /settings/privacy/status, which drives + reports the connect. JS-only (no-JS shows
 * nothing - the longer Tor query timeouts cover the cold dial regardless). */
/** The warming indicator: ONE continuous sweeping bar (like the page-load bar) shown
 * the whole time relays connect, plus a hidden poller. The poller checks status; while
 * warming the route answers H-Reswap:none so this element is NEVER redrawn (the sweep
 * stays smooth), and only when done does it swap in the ✓. JS-only; no-JS leaves a
 * static bar (harmless - the longer Tor timeouts cover the cold dial). */
function warmingWidget(): SafeHtml {
    return html`<div id="privacy-warming" class="privacy-warming">
        <div class="warm-row"><span class="warm-label">Establishing Tor circuits…</span></div>
        <div class="warm-bar sweep"></div>
        <span class="warm-poll" h-get="/settings/privacy/status" h-trigger="load delay:1s, every 1s" h-target="#privacy-warming" h-swap="outer" h-push-url="false"></span>
      </div>`;
}

/** The done state (by the status route once relays are connected): ✓ then self-remove
 * after 3s. */
export function warmingDone(connected: number, total: number): SafeHtml {
    return html`<div id="privacy-warming" class="privacy-warming done">
        <span class="warm-ok">✓ Connected through Tor (${String(connected)}/${String(total)} relays)</span>
        <span class="warm-poll" h-get="/settings/privacy/status?dismiss=1" h-trigger="load delay:3s" h-target="#privacy-warming" h-swap="outer" h-push-url="false"></span>
      </div>`;
}

const PRIVACY_LEGEND: { mode: PrivacyMode; label: string; desc: string }[] = [
    { mode: 'off', label: 'Off', desc: 'Direct connections (fastest). Tor is used only for .onion relays.' },
    { mode: 'balanced', label: 'Balanced', desc: 'Relays and previews route through Tor; if an exit is blocked it falls back to a direct request, so nothing breaks (but a fallback can leak).' },
    { mode: 'strict', label: 'Strict', desc: 'Everything routes through Tor with no fallback: most private, but slower, and relays/media that block Tor exits may not load.' },
];

export function privacySection(): SafeHtml {
    const mode = privacyMode();
    const torOk = torAvailable();
    const btn = (value: PrivacyMode, label: string) => html`<button type="submit" class="seg-btn ${mode === value ? 'active' : ''}" name="mode" value="${value}">${label}</button>`;
    // All three meanings are shown (current one highlighted) so the choice is informed
    // BEFORE selecting, not just a description of whatever's already active.
    const legend = PRIVACY_LEGEND.map((o) => html`
      <div class="privacy-opt ${o.mode === mode ? 'active' : ''}">
        <dt>${o.label}${o.mode === mode ? html`<span class="privacy-cur"> · current</span>` : html``}</dt>
        <dd>${o.desc}</dd>
      </div>`);
    return html`
      <section id="privacy-section">
        <h3>Privacy Mode</h3>
        <form class="row" action="/settings/privacy" method="post" h-post h-target="#privacy-section" h-swap="outer">
          <span>Tor routing</span>
          <div class="seg">${btn('off', 'Off')}${btn('balanced', 'Balanced')}${btn('strict', 'Strict')}</div>
        </form>
        ${mode !== 'off' ? warmingWidget() : html``}
        <dl class="privacy-legend">${join(legend)}</dl>
        ${!torOk ? html`<p class="settings-status err">Tor isn't configured (TOR_SOCKS), so these levels have no effect until the Tor sidecar is set up.</p>` : html``}
      </section>`;
}

/** The six settings tabs, one URL per tab (`/settings/<slug>`). The active tab is in the
 * URL, not client-side radio state, so tabs are deep-linkable and reload-stable. */
export type SettingsTab = 'general' | 'backup' | 'content' | 'privacy' | 'relays';
export const SETTINGS_TABS: { slug: SettingsTab; label: string }[] = [
    { slug: 'general', label: 'General' },
    { slug: 'backup', label: 'Backup' },
    { slug: 'content', label: 'Content' },
    { slug: 'privacy', label: 'Privacy' },
    { slug: 'relays', label: 'Relays' },
];

/** The Relays tab's left-pane sub-nav: four relay purposes, one URL each (/settings/relays/<slug>). */
export type RelayPane = 'general' | 'dm' | 'search' | 'private';
export const RELAY_PANES: { slug: RelayPane; label: string }[] = [
    { slug: 'general', label: 'General' },
    { slug: 'dm', label: 'DMs' },
    { slug: 'search', label: 'Search' },
    { slug: 'private', label: 'Private' },
];

/** The active relay pane's content. General = NIP-65; DMs = NIP-17; Search = NIP-50 search relays
 * (a local pref, not published); Private = your personal relay. */
function relayPaneContent(v: SettingsView, pane: RelayPane): SafeHtml {
    switch (pane) {
        case 'dm': return dmRelaySection(v.dmRelayDraft, v.dmRelayStatus, v.dmRelayErr);
        case 'search': return html`${searchRelayEditor('note', v.searchNoteDraft)}${searchRelayEditor('profile', v.searchProfileDraft)}`;
        case 'private': return localRelaySection(v.localRelay ?? null, v.localRelayStatus, v.localRelayErr, v.localRelayAuth);
        case 'general':
        default: return relaySection(v.relayDraft, v.relayStatus, v.relayErr);
    }
}

/** The Relays tab's two-pane: a left sub-nav (four relay purposes) + the active pane. The nav links
 * swap the whole two-pane (so the active item updates too) and push /settings/relays/<slug>. */
export function relaysTwoPane(v: SettingsView, pane: RelayPane): SafeHtml {
    const nav = RELAY_PANES.map((p) => html`<a href="/settings/relays/${p.slug}" class="relay-nav-item${p.slug === pane ? ' active' : ''}"${p.slug === pane ? raw(' aria-current="page"') : raw('')} h-get h-target="#relays-two-pane" h-swap="outer" h-push-url="true" h-prefetch="hover">${p.label}</a>`);
    return html`
      <div class="relays-two-pane" id="relays-two-pane">
        <nav class="relay-nav" aria-label="Relay categories">${join(nav)}</nav>
        <div class="relay-pane" id="relay-pane">${relayPaneContent(v, pane)}</div>
      </div>`;
}

/** Only the active tab's sections. The Relays tab is a two-pane hub (General/DMs/Search/Private);
 * the General tab folds appearance + media + feed/posting behaviour together. */
function settingsPanel(v: SettingsView, active: SettingsTab, pane: RelayPane = 'general'): SafeHtml {
    switch (active) {
        case 'backup': return backupSection(v.backupStatus, v.backupErr);
        case 'content': return contentTabPanel(v.contentPrefs, v.filters);
        case 'privacy': return privacySection();
        case 'relays': return relaysTwoPane(v, pane);
        case 'general':
        default: return html`
          ${appearanceSection(v.a)}
          <section>
            <h3>Media</h3>
            ${prefToggle('autoLoadMedia', v.a.autoLoadMedia)}
            ${prefToggle('inlineVideo', v.a.inlineVideo)}
          </section>
          ${mediaSection(v.mediaDraft, v.a, v.mediaStatus, v.mediaErr)}
          ${behaviorPanel(v.a)}`;
    }
}

/** The settings page: one URL per tab. The tab labels are `<a>` links - helmjs partial-swaps
 * `#settings-page` (outer) and pushes the tab URL (with hover-prefetch); no-JS does a full-page
 * nav to that tab. Only the ACTIVE tab's panel is rendered, so a reload / deep-link lands on it.
 * Within-tab forms keep their own section-id partial swaps (each form is only present on its tab). */
export function settingsPage(v: SettingsView, active: SettingsTab, pane: RelayPane = 'general'): SafeHtml {
    const label = SETTINGS_TABS.find((t) => t.slug === active)?.label ?? 'General';
    const tabs = SETTINGS_TABS.map((t) => html`<a href="/settings/${t.slug}" class="tabset-tab${t.slug === active ? ' active' : ''}"${t.slug === active ? raw(' aria-current="page"') : raw('')} role="tab" h-get h-target="#settings-page" h-swap="outer" h-push-url="true" h-prefetch="hover">${t.label}</a>`);
    return html`
      <div class="settings-page view-pad" id="settings-page">
        <div class="tabset-list" role="tablist">${join(tabs)}</div>
        <div class="tabset-panel" role="tabpanel" aria-label="${label}">${settingsPanel(v, active, pane)}</div>
      </div>`;
}

/** A read/write chip: a hidden native checkbox (submits the state) under a label
 * styled active via CSS `:has(input:checked)`. Zero-JS, no toggle script. */
function rwChip(name: string, value: string, label: string, on: boolean): SafeHtml {
    return html`<label class="rw-chip"><input type="checkbox" name="${name}" value="${value}"${on ? raw(' checked') : raw('')}>${label}</label>`;
}

/** A stable element id for a relay's score chip (so it can self-swap). */
const scoreId = (url: string): string => `rscore-${shortHash(url)}`;

/** A human tooltip for a trust assertion: "Trust 88/100 · reliability 78 · quality 97 · accessibility 92 ·
 * high confidence · specialized" (missing components omitted). */
function scoreTitle(s: TrustScore): string {
    const parts = [`Trust ${s.score}/100`];
    if (s.reliability !== undefined) parts.push(`reliability ${s.reliability}`);
    if (s.quality !== undefined) parts.push(`quality ${s.quality}`);
    if (s.accessibility !== undefined) parts.push(`accessibility ${s.accessibility}`);
    if (s.confidence) parts.push(`${s.confidence} confidence`);
    if (s.policy) parts.push(s.policy);
    return parts.join(' · ');
}

/** The trust-score chip (trustedrelays, read from on-nostr kind:30385 assertions). With `score` undefined
 * it's a lazy loader (helmjs intersect → /settings/relay-score → swaps in the resolved chip); `null` = no
 * score (unevaluated/unreachable); an evaluated assertion colours by tier and tooltips the breakdown. */
export function relayScoreChip(url: string, score?: TrustScore | null, id: string = scoreId(url)): SafeHtml {
    if (score === undefined) {
        return html`<span class="relay-score score-unknown" id="${raw(id)}" h-get="/settings/relay-score?url=${encodeURIComponent(url)}&id=${encodeURIComponent(id)}" h-trigger="load" h-target="#${raw(id)}" h-swap="outer" h-push-url="false" title="Trust score (trustedrelays)">?</span>`;
    }
    if (score === null) return html`<span class="relay-score score-unknown" id="${raw(id)}" title="No trust score">?</span>`;
    const tier = score.score >= 75 ? 'high' : score.score >= 50 ? 'mid' : 'low';
    return html`<span class="relay-score score-${tier}" id="${raw(id)}" title="${scoreTitle(score)}">${String(score.score)}</span>`;
}

/** A live connection indicator for the private relay (rendered by the status endpoint): just a colour-coded
 * dot beside the URL, with the verdict in its tooltip. 'off' when the relay isn't in use. Re-probes on pane
 * load and on every Save, so it needs no recheck control. */
export function localRelayStatusLine(state: 'off' | 'unreachable' | 'connected' | 'serving'): SafeHtml {
    const map = {
        off: { cls: 'off', text: 'Not in use' },
        unreachable: { cls: 'err', text: "Can't reach this relay - check the URL or that it's running" },
        connected: { cls: 'wait', text: 'Connected, no events yet' },
        serving: { cls: 'good', text: 'Connected and serving' },
    } as const;
    const s = map[state];
    return html`<span id="local-relay-status" class="lr-dot lr-dot-${raw(s.cls)}" title="${s.text}"></span>`;
}

/** Placeholder that probes the relay on render (swapped by localRelayStatusLine). Kept separate so the
 * result has no `load` trigger and can't re-fetch in a loop. */
function localRelayStatusProbe(): SafeHtml {
    return html`<span id="local-relay-status" class="lr-dot lr-dot-checking" title="Checking connection…" h-get="/settings/local-relay/status" h-trigger="load" h-target="#local-relay-status" h-swap="outer" h-push-url="false"></span>`;
}

/** The private-relay section (Relays > Private): ONE relay - a self-hosted aggregator/outbox/blaster -
 * the daemon reads from and mirrors writes to, kept OUT of your published NIP-65 list. You enter the URL,
 * a live status line reports whether it's reachable and serving, then Use turns it on and Read/Write each
 * pick Add (alongside your normal relays) or Only (exclusively here). Not form-as-state (a single relay),
 * just a plain save. */
export function localRelaySection(lr: LocalRelay | null, status?: string, statusErr = false, auth?: { needsAuth: boolean; authed: boolean }): SafeHtml {
    // Single-user only: the config is process-global, so on a shared instance it stays off (a note, no form)
    // rather than letting one account route another's traffic. A normal self-host is single-user.
    if (!isSingleUser()) {
        return html`<section id="local-relay-section"><h3>Private relay</h3>
          <p class="filter-help">A private relay is available on single-user instances only. This one allows more than one account, so it stays off, since its routing would apply to everyone.</p></section>`;
    }
    // Per-direction is add|only; the master "Use" (on|off) enables/disables the whole relay.
    const seg = (name: string, current: string, opts: { v: string; label: string }[]): SafeHtml => html`<div class="rw-group">${join(opts.map((u) =>
        html`<label class="rw-chip"><input type="radio" name="${name}" value="${u.v}"${u.v === current ? raw(' checked') : raw('')}>${u.label}</label>`))}</div>`;
    const ON_OFF = [{ v: 'on', label: 'On' }, { v: 'off', label: 'Off' }];
    const ADD_ONLY = [{ v: 'add', label: 'Add' }, { v: 'only', label: 'Only' }];
    const enabled = lr?.enabled ?? false;
    // nip07 + a private (auth-required) relay: the browser signs a one-time NIP-42 challenge and the daemon
    // keeps that connection. Bunker logins auth automatically (server-side), so no button there.
    const authBlock = auth?.needsAuth
        ? html`<div class="local-relay-auth">
            <span class="luse-label">${auth.authed ? 'Authenticated ✓' : 'Not authenticated'}</span>
            <form action="/settings/local-relay/auth" method="post" h-post h-target="#local-relay-section" h-swap="outer">
              <button type="submit" class="ghost">${auth.authed ? 'Re-authenticate' : 'Authenticate'}</button>
            </form>
            <p class="filter-help">If this relay requires NIP-42 auth, your browser extension signs a one-time challenge and the daemon reuses that connection. Re-authenticate after the relay or daemon restarts. (Bunker logins do this automatically.)</p>
          </div>`
        : html``;
    return html`
      <section id="local-relay-section">
        <h3>Private relay</h3>
        <p class="filter-help">A relay that only you use. Satori keeps it off your published list, so no one else connects to it. Set Read and Write to Only and your feed and posts go through just this relay, so the relays you'd normally use don't see your reads or posts, as long as it pulls in other people's posts and forwards yours.</p>
        <form action="/settings/local-relay" method="post" h-post h-target="#local-relay-section" h-swap="outer">
          <div class="lr-routing">
            <span class="luse-row lr-use-row"><span class="luse-label">Enable</span>${seg('use', enabled ? 'on' : 'off', ON_OFF)}</span>
            <div class="lr-routing-config">
              <div class="local-relay-dirs">
                <div class="luse-rw">
                  <span class="luse-row"><span class="luse-label">Read</span>${seg('read', lr?.read ?? 'add', ADD_ONLY)}</span>
                  <span class="luse-row"><span class="luse-label">Write</span>${seg('write', lr?.write ?? 'add', ADD_ONLY)}</span>
                </div>
              </div>
              <div class="lr-fetch-missing-row">
                <span class="luse-row"><span class="luse-label">Fetch missing</span>${seg('fetchmissing', (lr?.fetchMissing ?? false) ? 'on' : 'off', ON_OFF)}</span>
                <p class="filter-help lr-fm-hint">Backfill what your relay doesn't have from your normal relays. Off stays isolated.</p>
              </div>
            </div>
          </div>
          <div class="add-relay">
            ${lr?.url ? localRelayStatusProbe() : html``}
            <input type="text" name="url" value="${lr?.url ?? ''}" placeholder="ws://localhost:4869 (or wss:// / .onion)" autocomplete="off" spellcheck="false">
          </div>
          ${relaySaveFooter(status, statusErr, 'Save private relay')}
        </form>
        ${authBlock}
      </section>`;
}

/** The relays section (kind:10002 NIP-65). `draft` is the current editable list;
 * a successful save re-renders this same section with `status`. */
export function relaySection(draft: RelayEntry[], status?: string, statusErr = false): SafeHtml {
    const rows = draft.length === 0
        ? html`<li class="relay-empty">No relays yet. Add one below.</li>`
        : draft.map((r) => html`
            <li class="relay-edit-row">
              <input type="hidden" name="relay" value="${r.url}">
              ${relayScoreChip(r.url)}
              <span class="relay-url" title="${r.url}">${shortRelay(r.url)}</span>
              ${relayInfoBadges(r.url)}
              <div class="rw-group">${rwChip('read', r.url, 'Read', r.read)}${rwChip('write', r.url, 'Write', r.write)}</div>
              ${relayRemoveBtn(r.url, '/settings/relays/edit', 'relay-section')}
            </li>`);
    return html`
      <section id="relay-section">
        <h3>General relays</h3>
        <form action="/settings/relays" method="post" h-post h-target="#relay-section" h-swap="outer">
          <ul class="relay-editor">${rows}</ul>
          <div class="add-relay">
            <input type="text" name="newurl" placeholder="wss://relay.example (or .onion)" autocomplete="off" spellcheck="false">
            <div class="rw-group">${rwChip('newread', '1', 'read', true)}${rwChip('newwrite', '1', 'write', true)}</div>
            <button class="ghost" name="op" value="add" formaction="/settings/relays/edit" h-post="/settings/relays/edit" h-target="#relay-section" h-swap="outer">Add</button>
          </div>
          ${relaySaveFooter(status, statusErr, 'Save relay list')}
        </form>
      </section>`;
}

/** The DM-relay section (kind:10050 NIP-17). A flat url list (no read/write split): these
 * are the relays you tell other clients to deliver your encrypted DMs to. Same form-as-state
 * + publish flow as the NIP-65 relaySection above. */
export function dmRelaySection(draft: string[], status?: string, statusErr = false): SafeHtml {
    const rows = draft.length === 0
        ? html`<li class="relay-empty">No DM relays published. Other clients can’t reliably tell where to send you messages. Add a few below.</li>`
        : draft.map((url) => html`
            <li class="relay-edit-row">
              <input type="hidden" name="dmrelay" value="${url}">
              ${relayScoreChip(url, undefined, `rscore-dm-${shortHash(url)}`)}
              <span class="relay-url" title="${url}">${shortRelay(url)}</span>
              ${relayInfoBadges(url)}
              ${relayRemoveBtn(url, '/settings/dm-relays/edit', 'dm-relay-section')}
            </li>`);
    return html`
      <section id="dm-relay-section">
        <h3>Direct message relays</h3>
        <p class="filter-help">Where other clients deliver your encrypted (NIP-17) direct messages. A small, reliable set you read often works best; without it some people can’t reach you.</p>
        <form action="/settings/dm-relays" method="post" h-post h-target="#dm-relay-section" h-swap="outer">
          <ul class="relay-editor">${rows}</ul>
          <div class="add-relay">
            <input type="text" name="newurl" placeholder="wss://relay.example (or .onion)" autocomplete="off" spellcheck="false">
            <button class="ghost" name="op" value="add" formaction="/settings/dm-relays/edit" h-post="/settings/dm-relays/edit" h-target="#dm-relay-section" h-swap="outer">Add</button>
          </div>
          ${relaySaveFooter(status, statusErr, 'Save DM relays')}
        </form>
      </section>`;
}

/** The media-servers section (kind:10063 Blossom BUD-03). Same form-as-state. */
export function mediaSection(draft: string[], _a: Appearance, status?: string, statusErr = false): SafeHtml {
    const rows = draft.length === 0
        ? html`<li class="relay-empty">No media servers set. Uploads use ${DEFAULT_BLOSSOM_SERVER.replace(/^https?:\/\//, '')} by default. Add your own below.</li>`
        : draft.map((url) => html`
            <li class="relay-edit-row">
              <input type="hidden" name="server" value="${url}">
              <span class="relay-url" title="${url}">${url.replace(/^https?:\/\//, '')}</span>
              ${relayRemoveBtn(url, '/settings/media/edit', 'media-section')}
            </li>`);
    return html`
      <section id="media-section">
        <h3>Media servers</h3>
        <form action="/settings/media" method="post" h-post h-target="#media-section" h-swap="outer">
          <ul class="relay-editor">${rows}</ul>
          <div class="add-relay">
            <input type="text" name="newurl" placeholder="https://blossom.example" autocomplete="off" spellcheck="false">
            <button class="ghost" name="op" value="add" formaction="/settings/media/edit" h-post="/settings/media/edit" h-target="#media-section" h-swap="outer">Add</button>
          </div>
          ${relaySaveFooter(status, statusErr, 'Save media servers')}
        </form>
      </section>`;
}
