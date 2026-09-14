// NIP-17 DMs - the views. A focused single-column experience (like the rest of Satori: move
// through it, one thing at a time). A calm conversation list, then a full-width thread that
// reads like a letter: the peer's name in the serif, soft bubbles across the width, and time
// as quiet centered dividers rather than a stamp under every line. Calm by design: no read
// receipts, typing indicators, or counts - just quiet unread dots. See [[nip17-dms-plan]].

import { html, join, type SafeHtml } from '../html.ts';
import { avatar, displayName, npub, timeAgo, type ProfileMap } from './util.ts';
import { withEmoji } from './content.ts';
import { modalClose } from './compose.ts';
import { emptyItem, enso, icon } from './svg.ts';
import { quote } from './quotes.ts';
import type { Conversation, DmMessage, DmInbox } from '../data/dms.ts';

export type DmTab = 'messages' | 'requests';

// --- gate / sync shells / prewarm / nudge ----------------------------------------

/** Fallback when a nip07 client can't drive the batch decrypt chain. */
export function dmGate(): SafeHtml {
    return html`<div class="view-empty dm-gate">${enso(52, true)}
      <p class="dm-gate-title">Messages need a batch-capable signer</p>
      <p class="signin-help">Private messages are decrypted through your signer. Use a NIP-46 bunker, or a browser extension with a batch-capable nip07 integration, to use Messages.</p></div>`;
}

function decrypting(): SafeHtml {
    return html`<div class="dm-decrypting">${enso(40, true)}<span>Decrypting your messages…</span></div>`;
}

/** nip07 list decrypting shell (fills the list via the /messages/sync chain). */
export function listSyncShell(view: 'inbox' | 'requests'): SafeHtml {
    const url = view === 'requests' ? '/messages/sync?view=requests' : '/messages/sync';
    return html`<div id="dm-sync" h-get="${url}" h-trigger="load" h-target="#dm-sync" h-swap="outer" h-push-url="false">${decrypting()}</div>`;
}

/** nip07 thread decrypting shell (fills #dm-messages via the /messages/:peer/sync chain). */
function threadSyncShell(peer: string): SafeHtml {
    return html`<li id="dm-sync" h-get="/messages/${npub(peer)}/sync" h-trigger="load" h-target="#dm-messages" h-swap="inner" h-push-url="false">${decrypting()}</li>`;
}

/** Hidden fire-once trigger for the post-login landing (nip07): warms the decrypt cache in
 * the background so opening Messages is instant. `swap=none` keeps it a pure side-effect. */
export function dmPrewarm(): SafeHtml {
    // delay:3s so the DM decrypt prompt doesn't race the list-primer's at login - the feed
    // settles and the private lists decrypt first, then this warms the DM cache in the background.
    return html`<div id="dm-prewarm" h-get="/messages/sync?warm=1" h-trigger="load delay:3s" h-target="#dm-prewarm" h-swap="none" h-push-url="false" aria-hidden="true"></div>`;
}

/** Nudge when you have no kind-10050 DM relay list, so others can't reliably reach you. */
function dmRelayNudge(): SafeHtml {
    return html`<div class="dm-nudge">No DM relay list published yet, so some people may not be able to message you. <a href="/settings" h-scroll="top instant">Set your DM relays</a> under Settings → Relays.</div>`;
}

/** A small lock after the name = this conversation uses modern NIP-17 encryption. Legacy
 * NIP-04 shows nothing (a calm positive marker, not a "less private" nag). */
const secureBadge = html`<span class="dm-secure" title="End-to-end encrypted (NIP-17)">${icon('lock')}</span>`;

// --- conversation list -----------------------------------------------------------

/** One conversation row. A plain boosted link navigates to the full-width thread (and the
 * helmjs ink-wash carries the transition). A quiet dot marks unread. */
function convRow(c: Conversation, profiles: ProfileMap): SafeHtml {
    return html`
      <li class="dm-conv${c.unread ? ' unread' : ''}">
        <a class="dm-conv-link" href="/messages/${npub(c.peer)}" h-scroll="top instant">
          ${avatar(c.peer, profiles.get(c.peer)?.picture, 'sm')}
          <span class="dm-conv-text">
            <span class="dm-conv-top"><span class="dm-conv-name">${withEmoji(displayName(c.peer, profiles), profiles.get(c.peer)?.emoji)}${c.secure ? secureBadge : null}</span><span class="time">${timeAgo(c.lastAt)}</span></span>
            <span class="dm-conv-preview">${c.preview}</span>
          </span>
          ${c.unread ? html`<span class="notif-dot dm-unread"></span>` : null}
        </a>
      </li>`;
}

/** The empty-inbox state: a calm line, the enso below it, then a way to start. */
function listEmpty(): SafeHtml {
    return html`<li class="dm-list-empty"><p class="search-quote">${quote('empty')}</p>${enso(40, true)}<a class="empty-cta" href="/messages/new" h-target="#modal" h-swap="inner" h-focus="#dm-search-input" h-push-url="false">New message</a></li>`;
}

/** Conversation list body (the <ul>). */
export function convList(inbox: DmInbox, profiles: ProfileMap): SafeHtml {
    const body = inbox.conversations.length ? join(inbox.conversations.map((c) => convRow(c, profiles))) : listEmpty();
    return html`<ul class="feed dm-conv-list" id="dm-conv-list">${body}</ul>`;
}

/** The Requests bucket body (strangers). */
export function reqList(reqs: Conversation[], profiles: ProfileMap): SafeHtml {
    const body = reqs.length ? join(reqs.map((c) => convRow(c, profiles))) : emptyItem('No requests.');
    return html`<ul class="feed dm-conv-list" id="dm-conv-list">${body}</ul>`;
}

// --- list head: switcher + actions -----------------------------------------------

/** Messages/Requests switcher - a native <details> dropdown (mirrors the feed switcher).
 * Quiet unread dots: each item shows one when its bucket has new messages; the closed toggle
 * shows one when the OTHER (hidden) bucket does. */
function dmSwitch(tab: DmTab, unreadConvs: boolean, unreadReqs: boolean): SafeHtml {
    const dot = html`<span class="notif-dot"></span>`;
    const otherUnread = tab === 'requests' ? unreadConvs : unreadReqs;
    return html`
      <details class="feed-switch dm-switch">
        <summary class="feed-toggle"><span>${tab === 'requests' ? 'Requests' : 'Messages'}</span>${otherUnread ? dot : null} <span class="chevron">▾</span></summary>
        <div class="feed-menu">
          <a class="feed-item ${tab === 'messages' ? 'active' : ''}" href="/messages" h-scroll="top instant">Messages${unreadConvs ? dot : null}</a>
          <a class="feed-item ${tab === 'requests' ? 'active' : ''}" href="/messages/requests" h-scroll="top instant">Requests${unreadReqs ? dot : null}</a>
        </div>
      </details>`;
}

/** The New-message button - opens the recipient-search modal. */
function newBtn(): SafeHtml {
    return html`<a class="dm-new" href="/messages/new" h-target="#modal" h-swap="inner" h-focus="#dm-search-input" h-push-url="false" aria-label="New message" title="New message">${icon('compose')}</a>`;
}

/** "Mark all read" for the current tab - clears the unread dots on every conversation in this
 * bucket at once, then re-swaps the list. */
function markAllReadBtn(tab: DmTab): SafeHtml {
    return html`<form class="dm-readall" action="/messages/read-all" method="post" h-post h-target="#dm-conv-list" h-swap="outer" h-push-url="false"><input type="hidden" name="view" value="${tab}"><button type="submit" title="Mark all read">Mark all read</button></form>`;
}

/** The conversation-list page (full width). */
export function dmListPage(o: { tab: DmTab; list: SafeHtml; unreadConvs?: boolean; unreadReqs?: boolean; relayNudge?: boolean }): SafeHtml {
    const tabUnread = o.tab === 'requests' ? (o.unreadReqs ?? false) : (o.unreadConvs ?? false);
    return html`
      <div class="view-pad dm-page">
        <div class="dm-list-head">${dmSwitch(o.tab, o.unreadConvs ?? false, o.unreadReqs ?? false)}<div class="dm-head-actions">${tabUnread ? markAllReadBtn(o.tab) : null}${newBtn()}</div></div>
        ${o.relayNudge ? dmRelayNudge() : null}
        ${o.list}
      </div>`;
}

// --- thread (bubbles + time dividers) --------------------------------------------

function bubble(m: DmMessage, me: string): SafeHtml {
    return html`<li class="dm-msg ${m.from === me ? 'mine' : ''}" id="m-${m.id.slice(0, 16)}"><span class="dm-bubble">${m.text}</span></li>`;
}

/** A single bubble (the optimistic-send append payload). */
export const dmBubble = bubble;

const dayKey = (ts: number): string => { const d = new Date(ts * 1000); return `${d.getFullYear()}-${d.getMonth()}-${d.getDate()}`; };

/** A calm day label for a thread divider: Today / Yesterday / a date (year only when not this year). */
function dayLabel(ts: number): string {
    const d = new Date(ts * 1000);
    const now = new Date();
    const startOf = (x: Date): number => new Date(x.getFullYear(), x.getMonth(), x.getDate()).getTime();
    const days = Math.round((startOf(now) - startOf(d)) / 86_400_000);
    if (days === 0) return 'Today';
    if (days === 1) return 'Yesterday';
    return d.toLocaleDateString('en-US', { month: 'short', day: 'numeric', ...(d.getFullYear() === now.getFullYear() ? {} : { year: 'numeric' }) });
}

/** Bubbles oldest→newest with a quiet centered divider whenever the day changes. */
function bubblesWithDividers(messages: DmMessage[], me: string): SafeHtml {
    const out: SafeHtml[] = [];
    let last = '';
    for (const m of messages) {
        const k = dayKey(m.at);
        if (k !== last) { out.push(html`<li class="dm-daydiv"><span>${dayLabel(m.at)}</span></li>`); last = k; }
        out.push(bubble(m, me));
    }
    return join(out);
}

/** Top-of-thread "load older" sentinel: intersect-triggered, prepends the next older window. */
function olderLoader(peer: string, cursor: number): SafeHtml {
    return html`<li class="dm-older" id="dm-older" h-get="/messages/${npub(peer)}/older?until=${cursor}" h-trigger="intersect once" h-target="#dm-older" h-swap="outer" h-push-url="false">${enso(20, true)}</li>`;
}

/** The #dm-messages inner region: older-loader (if more history may exist) + bubbles, or a hint. */
export function threadInner(peer: string, messages: DmMessage[], me: string, cursor: number | null): SafeHtml {
    const older = cursor != null ? olderLoader(peer, cursor) : null;
    return messages.length ? html`${older}${bubblesWithDividers(messages, me)}` : html`${older}<li class="empty dm-empty">${enso(40, true)}<span>Say something.</span></li>`;
}

/** The prepend payload for a "load older" step (re-armed sentinel + older bubbles). */
export function olderFragment(peer: string, messages: DmMessage[], me: string, cursor: number | null): SafeHtml {
    return html`${cursor != null ? olderLoader(peer, cursor) : null}${bubblesWithDividers(messages, me)}`;
}

/** The full-width thread page: a header (back + serif peer name + lock), the messages, and a
 * compose box. `sync` (nip07) puts a decrypting trigger in #dm-messages. */
export function dmThreadPage(peer: string, messages: DmMessage[], profiles: ProfileMap, me: string, opts: { sync?: boolean; cursor?: number | null } = {}): SafeHtml {
    const name = withEmoji(displayName(peer, profiles), profiles.get(peer)?.emoji);
    const secure = !messages.length || messages.some((m) => !m.legacy);
    const inner = opts.sync ? threadSyncShell(peer) : threadInner(peer, messages, me, opts.cursor ?? null);
    return html`
      <div class="view-pad dm-page dm-thread">
        <div class="dm-thread-head">
          <a class="dm-back" href="/messages" aria-label="Back to messages" h-scroll="top instant">${icon('back')}</a>
          <a class="dm-peer" href="/u/${npub(peer)}" h-scroll="top instant">${avatar(peer, profiles.get(peer)?.picture, 'sm')}<span class="dm-peer-name">${name}</span>${secure ? secureBadge : null}</a>
        </div>
        <ul class="dm-messages" id="dm-messages">${inner}</ul>
        <form class="dm-compose" action="/messages/${npub(peer)}" method="post" h-post h-target="#dm-messages" h-swap="append" h-reset h-focus=".dm-compose textarea">
          <textarea name="text" required placeholder="Message…" rows="1" autocomplete="off"></textarea>
          <button type="submit" class="dm-send busy-btn"><span class="btn-label">Send</span><span class="btn-busy">…</span></button>
        </form>
      </div>`;
}

// --- new-message recipient search (modal) ----------------------------------------

/** A person result in the New-message modal. A plain link does a real navigation to the
 * thread (which opens the conversation and clears the modal). */
function recipientRow(pubkey: string, profiles: ProfileMap): SafeHtml {
    return html`
      <li class="dm-pick">
        <a class="dm-pick-link" href="/messages/${npub(pubkey)}">
          ${avatar(pubkey, profiles.get(pubkey)?.picture, 'sm')}
          <span class="dm-pick-name">${withEmoji(displayName(pubkey, profiles), profiles.get(pubkey)?.emoji)}</span>
        </a>
      </li>`;
}

/** The New-message modal: a people search to start a conversation. */
export function newMessageModal(query: string, results: { pubkey: string }[], profiles: ProfileMap): SafeHtml {
    return html`
      <div class="modal-overlay" id="dm-new-modal">
        <div class="modal dm-new-modal">
          <div class="modal-head"><span class="page-title">New message</span>${modalClose()}</div>
          <form class="dm-search-form" action="/messages/new" method="get" h-get="/messages/new" h-target="#dm-pick-results" h-swap="inner" h-push-url="false">
            <input id="dm-search-input" name="q" type="search" value="${query}" placeholder="Search people…" autocomplete="off">
          </form>
          <ul class="dm-pick-list" id="dm-pick-results">${recipientResults(query, results, profiles)}</ul>
        </div>
      </div>`;
}

/** Full-page recipient picker: the no-JS / full-navigation fallback for /messages/new (same
 * search form + results as the modal, but in page chrome). The search form is a real GET and
 * recipient rows are plain links, so finding + starting a conversation works without JS. */
export function newMessagePage(query: string, results: { pubkey: string }[], profiles: ProfileMap): SafeHtml {
    return html`
      <div class="dm-new-page view-pad">
        <h1 class="page-title">New message</h1>
        <form class="dm-search-form" action="/messages/new" method="get" h-get="/messages/new" h-target="#dm-pick-results" h-swap="inner" h-push-url="false">
          <input id="dm-search-input" name="q" type="search" value="${query}" placeholder="Search people…" autocomplete="off">
        </form>
        <ul class="dm-pick-list" id="dm-pick-results">${recipientResults(query, results, profiles)}</ul>
      </div>`;
}

/** Just the results list (the swap payload for a search). */
export function recipientResults(query: string, results: { pubkey: string }[], profiles: ProfileMap): SafeHtml {
    if (!query) return html`<li class="empty dm-pick-hint"><span>Search by name to start a conversation.</span></li>`;
    if (!results.length) return html`<li class="empty dm-pick-hint"><span>No people found.</span></li>`;
    return join(results.map((r) => recipientRow(r.pubkey, profiles)));
}
