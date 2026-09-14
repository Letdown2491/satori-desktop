// The "Scheduled" section of the /drafts page: your queued posts (signed, waiting for the
// daemon's sweep to broadcast them at their time) with a cancel control. Cancelling reverts a
// post to an editable draft (see routes/scheduled.ts). Reuses the .draft-row / .draft-list styling.

import { html, raw, type SafeHtml } from '../html.ts';
import { KIND_ARTICLE } from '../nostr/nip23.ts';
import type { ScheduledPost } from '../data/scheduled.ts';

const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];

/** Absolute, readable local time (server tz = user tz on a single-user daemon). */
function whenLabel(secs: number): string {
    const d = new Date(secs * 1000);
    let h = d.getHours();
    const ap = h >= 12 ? 'pm' : 'am';
    h = h % 12 || 12;
    return `${MONTHS[d.getMonth()]} ${d.getDate()}, ${h}:${String(d.getMinutes()).padStart(2, '0')}${ap}`;
}

function row(p: ScheduledPost): SafeHtml {
    // p.token is a randomBytes hex string ([0-9a-f]+), so it's safe to raw()-interpolate into the
    // id / #selector / action URL below. If token generation ever changes, escape these instead.
    const domId = `sched-${p.token}`;
    // An article's headline is its `title` tag, not its (markdown) content; notes/polls read from content.
    const text = p.signed.kind === KIND_ARTICLE
        ? (p.signed.tags.find((t) => t[0] === 'title')?.[1] || 'Untitled article')
        : p.signed.content.trim().slice(0, 90) || 'Media post';
    return html`
      <li class="draft-row" id="${raw(domId)}">
        <div class="draft-open">
          <span class="draft-title">${text}</span>
          <span class="draft-meta">Sends ${whenLabel(p.scheduledAt)}</span>
        </div>
        <form class="draft-del" action="/scheduled/cancel/${p.token}" method="post" h-post h-confirm="Cancel this scheduled post? It'll be kept as a draft." h-target="body" h-swap="inner">
          <button type="submit" class="ghost" title="Cancel" aria-label="Cancel scheduled post">✕</button>
        </form>
      </li>`;
}

/** The "Scheduled" section, shown atop /drafts when you have queued posts. Empty when none, so
 * the page falls back to just the Drafts section (and its calm empty state). */
export function scheduledSection(posts: ScheduledPost[]): SafeHtml {
    if (!posts.length) return html``;
    return html`<section class="draft-section"><h2 class="draft-head">Scheduled</h2><ul class="draft-list">${posts.map(row)}</ul></section>`;
}
