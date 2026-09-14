// Long-form (NIP-23) composer - title, summary, cover URL, topics, and a Markdown
// body with a formatting toolbar. The toolbar uses hateoas-extensions' h-insert to
// splice Markdown at the textarea caret (no app JS). Full-page (not a modal), like
// Satori. Cover upload and live preview are follow-ups.

import { html, raw, type SafeHtml } from '../html.ts';
import { icon, enso } from './svg.ts';
import { timeAgo } from './util.ts';
import { scheduleRow, composeTypes, relayPickerRow, relaysToggleBtn } from './compose.ts';
import { quote } from './quotes.ts';
import { scheduledSection } from './scheduled.ts';
import type { Draft } from '../drafts.ts';
import type { ScheduledPost } from '../data/scheduled.ts';

export interface ArticleComposeCtx {
    identifier?: string; title?: string; summary?: string; image?: string; topics?: string; body?: string; error?: string; status?: string;
    relays?: string[]; // the relay-picker's write-relay list (populated on a fresh compose)
}

const BODY = '#ac-body';

/** A toolbar button that inserts a Markdown snippet at the body caret (h-insert). */
function tool(label: SafeHtml | string, insert: string, title: string): SafeHtml {
    return html`<button type="button" class="ac-tool" h-insert="${insert}" h-insert-target="${raw(BODY)}" title="${title}" aria-label="${title}">${label}</button>`;
}

export function articleComposeForm(c: ArticleComposeCtx = {}): SafeHtml {
    return html`
      ${c.error ? html`<div class="notice error">${c.error}</div>` : null}
      <form class="ac-form" action="/article" method="post" h-post>
        <input type="hidden" id="compose-identifier" name="identifier" value="${c.identifier ?? ''}">
        <input class="ac-title" type="text" name="title" placeholder="Title" value="${c.title ?? ''}" required>
        <input class="ac-input" type="text" name="summary" placeholder="Summary (optional)" value="${c.summary ?? ''}" autocomplete="off">
        <div class="ac-cover-row"><input class="ac-input" type="text" name="image" placeholder="Cover image URL (optional)" value="${c.image ?? ''}" autocomplete="off" spellcheck="false"></div>
        <input class="ac-input" type="text" name="topics" placeholder="Topics, comma separated (optional)" value="${c.topics ?? ''}" autocomplete="off">
        <div class="ac-toolbar">
          ${tool('B', '**bold**', 'Bold')}${tool(html`<em>I</em>`, '*italic*', 'Italic')}${tool('H', '## ', 'Heading')}${tool('“', '> ', 'Quote')}${tool('•', '- ', 'List')}${tool('↗', '[text](https://)', 'Link')}${tool(icon('image'), '![alt](https://)', 'Image')}
        </div>
        <textarea class="ac-body" id="ac-body" name="body" placeholder="Write your article in Markdown…" spellcheck="true">${c.body ?? ''}</textarea>
        <!-- Schedule: the .schedule-btn clock toggles this checkbox; the row reveals on :checked
             (pure CSS, like the note composer). The "Schedule" button sends do=schedule to /article. -->
        <input type="checkbox" id="schedule-toggle" class="sched-check">
        ${scheduleRow('/article')}
        ${relayPickerRow(c.relays ?? [])}
        <div class="ac-foot">
          <label class="attach-btn schedule-btn" for="schedule-toggle" title="Schedule for later" aria-label="Schedule for later">${icon('clock')}</label>
          ${c.relays?.length ? relaysToggleBtn() : null}
          <span id="compose-status" class="compose-status">${c.status ?? ''}</span>
          <button type="submit" class="ghost" formaction="/draft" formmethod="post">Save draft</button>
          <button type="submit">Publish article</button>
        </div>
      </form>`;
}

/** Full composer page: the Note · Picture · Poll · Article selector + the form. */
export function articleComposePage(c: ArticleComposeCtx = {}): SafeHtml {
    return html`
      <div class="view-pad">
        ${composeTypes('article')}
        ${articleComposeForm(c)}
      </div>`;
}

/** One draft row. Articles open full-page; notes/polls open the compose modal (with JS) or
 * the page (no-JS). The `id` is user-settable for articles, so sanitize it to a safe DOM-id. */
/** Stable per-row DOM id (also the swap target for sync continuations). */
export const draftDomId = (id: string): string => `draft-${id.replace(/[^a-zA-Z0-9_-]/g, '')}`;

/** nip07 decrypt-on-load trigger: fires the /drafts/sync chain once, which decrypts your synced
 * wraps and re-swaps the whole list (#drafts-view). No-op (and harmless) with JS off. */
export function draftsSyncShell(): SafeHtml {
    return html`<div id="drafts-sync" h-get="/drafts/sync" h-trigger="load" h-target="#drafts-view" h-swap="outer" h-push-url="false" aria-hidden="true"></div>`;
}

export function draftRow(d: Draft): SafeHtml {
    const domId = draftDomId(d.id);
    const enc = encodeURIComponent(d.id);
    let label: string, title: string, href: string, modal: boolean;
    if (d.type === 'article') {
        label = 'Article'; title = d.title || 'Untitled';
        href = `/compose?type=article&draft=${enc}`; modal = false;
    } else if (d.type === 'note') {
        label = 'Note'; title = d.content.slice(0, 70) || (d.imeta.length ? 'Media note' : 'Empty note');
        href = `/compose?draft=${enc}`; modal = true;
    } else {
        label = 'Poll'; title = d.question || 'Untitled poll';
        href = `/compose?type=poll&draft=${enc}`; modal = true;
    }
    const openAttrs = modal ? raw(' h-target="#modal" h-swap="inner" h-push-url="false"') : raw('');
    return html`
      <li class="draft-row" id="${raw(domId)}">
        <a class="draft-open" href="${href}"${openAttrs}>
          <span class="draft-title">${title}</span>
          <span class="draft-meta">${label} saved ${timeAgo(d.savedAt / 1000)}</span>
        </a>
        <details class="draft-del" h-dismiss>
          <summary title="Delete draft" aria-label="Delete draft">✕</summary>
          <form action="/draft/delete/${enc}" method="post" h-post h-target="#${raw(domId)}" h-swap="outer" h-optimistic="class:deleting" h-optimistic-target="#${raw(domId)}">
            <button type="submit" class="del-confirm">Delete</button>
          </form>
        </details>
      </li>`;
}

/** The Drafts section (#drafts-view) - each row opens in the composer; ✕ deletes it. This is the
 * NIP-37 sync swap target, so it's always present (even empty) for the decrypt-load to populate.
 * `labeled` adds the "Drafts" heading, used when a Scheduled section sits above it; on its own the
 * page needs no heading (the chrome bar titles it) and an empty store shows the calm empty state. */
export function draftsView(drafts: Draft[], labeled: boolean): SafeHtml {
    const head = labeled ? html`<h2 class="draft-head">Drafts</h2>` : null;
    if (!drafts.length) {
        const empty = labeled
            ? html`<p class="draft-empty">No saved drafts.</p>`
            : html`<div class="view-empty"><p class="search-quote">${quote('empty')}</p>${enso(40, true)}</div>`;
        return html`<div id="drafts-view">${head}${empty}</div>`;
    }
    return html`<div id="drafts-view">${head}<ul class="draft-list">${drafts.map(draftRow)}</ul></div>`;
}

/** The full /drafts screen: a "Scheduled" section (when you have queued posts) above the Drafts
 * section. Both are co-equal sections so the auto-sending commitment reads distinctly from inert
 * drafts. The Drafts heading only appears when Scheduled is present (else it'd be redundant). */
export function draftsScreen(scheduled: ScheduledPost[], drafts: Draft[]): SafeHtml {
    return html`<div class="view-pad">${scheduledSection(scheduled)}${draftsView(drafts, scheduled.length > 0)}</div>`;
}
