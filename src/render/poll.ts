// NIP-88 poll rendering - the string port of Satori's PollSection/renderPoll and
// the poll composer. A poll shows a ballot (vote forms) until you vote or it ends,
// then results (bars). Voting is a real <form> POST, so it works with JS off; the
// box also lazily hydrates (h-trigger="intersect once") to correct voted/ended polls.

import { html, join, raw, type SafeHtml } from '../html.ts';
import { parsePollOptions, parsePollType, isPollEnded, type PollTally } from '../nostr/nip88.ts';
import { neventFor } from './note.ts';
import type { NostrEvent } from '../nostr/types.ts';

/** Results view - bars + counts (Satori's pollResultRow + poll-meta). */
function pollResults(poll: NostrEvent, tally: PollTally): SafeHtml {
    const options = parsePollOptions(poll);
    const ended = isPollEnded(poll);
    const total = tally.total;
    const mine = tally.mine ?? [];
    const rows = options.map((opt) => {
        const count = tally.counts[opt.id] ?? 0;
        const pct = total > 0 ? Math.round((count / total) * 100) : 0;
        const isMine = mine.includes(opt.id);
        return html`
          <div class="poll-result ${isMine ? 'mine' : ''}">
            <span class="poll-bar"><span class="poll-bar-fill pw-${pct}"></span></span>
            <span class="poll-result-label">${isMine ? `${opt.label} ✓` : opt.label}</span>
            <span class="poll-pct">${pct}%</span>
          </div>`;
    });
    return html`${join(rows)}<div class="poll-meta">${total} vote${total === 1 ? '' : 's'}${ended ? ' · ended' : ''}</div>`;
}

/** Ballot - vote forms. Single: each option is its own submit; multiple:
 * checkboxes + a Vote button. The form swaps the poll box in place with results.
 * `h-optimistic="class:voting"` flips the box to a "tallying…" pending state the
 * instant you vote (the response reconciles to results; an h:error reverts). The
 * `.poll-pending` line lives in the ballot so it's swapped away with it on success. */
function pollBallot(poll: NostrEvent): SafeHtml {
    const options = parsePollOptions(poll);
    const id = `poll-${poll.id}`;
    const pending = html`<div class="poll-pending">Tallying your vote…</div>`;
    if (parsePollType(poll) === 'single') {
        return html`${join(options.map((opt) => html`
          <form class="poll-opt-form" action="/poll/vote/${poll.id}" method="post" h-post h-target="#${id}" h-swap="inner" h-optimistic="class:voting" h-optimistic-target="#${id}">
            <button type="submit" name="option" value="${opt.id}" class="poll-option"><span class="poll-mark">○</span><span class="poll-label">${opt.label}</span></button>
          </form>`))}${pending}`;
    }
    return html`
      <form class="poll-multi" action="/poll/vote/${poll.id}" method="post" h-post h-target="#${id}" h-swap="inner" h-optimistic="class:voting" h-optimistic-target="#${id}">
        ${join(options.map((opt) => html`<label class="poll-option"><input type="checkbox" class="poll-check" name="option" value="${opt.id}"><span class="poll-mark"></span><span class="poll-label">${opt.label}</span></label>`))}
        <button type="submit" class="poll-vote">Vote</button>
      </form>${pending}`;
}

/** Ballot or results: results once you've voted (tally.mine) or the poll ended. */
export function pollSection(poll: NostrEvent, tally: PollTally | null): SafeHtml {
    const showResults = !!tally && ((tally.mine?.length ?? 0) > 0 || isPollEnded(poll));
    return showResults && tally ? pollResults(poll, tally) : pollBallot(poll);
}

/** The poll box inside a note: an instant ballot (votable with JS off) that lazily
 * hydrates to the correct ballot/results state via /poll/<nevent>. */
export function pollBox(poll: NostrEvent): SafeHtml {
    return html`<div class="poll" id="poll-${poll.id}" h-get="/poll/${neventFor(poll)}" h-trigger="intersect once" h-target="#poll-${poll.id}" h-swap="inner" h-push-url="false">${pollBallot(poll)}</div>`;
}

const DURATIONS = ['No end date', 'Ends in 1 day', 'Ends in 3 days', 'Ends in 1 week'];

/** The poll composer fields (Satori's poll-fields): question is the shared compose
 * textarea; here the options list (+ Add via helmjs), multiple toggle, duration. */
export function pollComposeFields(c: { options?: string[]; multiple?: boolean; duration?: number } = {}): SafeHtml {
    const opt = (v = '') => html`<li class="poll-opt-row"><input class="poll-opt" type="text" name="option" placeholder="Option" maxlength="90" value="${v}"></li>`;
    const rows = (c.options ?? []).slice();
    while (rows.length < 2) rows.push(''); // always at least two option rows
    return html`
      <div class="poll-fields">
        <ul class="poll-opts" id="poll-opts">${join(rows.map((v) => opt(v)))}</ul>
        <div class="poll-controls">
          <button type="button" class="ghost add-opt" h-get="/compose/poll-option" h-target="#poll-opts" h-swap="append" h-trigger="click" h-push-url="false">+ Add option</button>
          <div class="poll-settings">
            <label class="toggle"><input type="checkbox" name="multiple" value="1"${c.multiple ? raw(' checked') : raw('')}> Allow multiple choices</label>
            <select class="theme-select" name="duration">
              ${join(DURATIONS.map((d, i) => html`<option value="${i}"${i === c.duration ? raw(' selected') : raw('')}>${d}</option>`))}
            </select>
          </div>
        </div>
      </div>`;
}

/** A single option input row - returned by GET /compose/poll-option for "+ Add". */
export function pollOptionRow(): SafeHtml {
    return html`<li class="poll-opt-row"><input class="poll-opt" type="text" name="option" placeholder="Option" maxlength="90"></li>`;
}

export const POLL_DURATION_DAYS: (number | null)[] = [null, 1, 3, 7];
