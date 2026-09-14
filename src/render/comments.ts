// NIP-22 article comments (kind:1111) under the article reader - a threaded list
// + a compose box, mirroring Satori. On a new comment the list is MORPHED in place
// (`#comment-list`, h-swap="morph") so unchanged comments - and their already-loaded
// avatars/scroll position - survive instead of the whole tree tearing down; the
// count and the top-level form are refreshed out-of-band (the form can't live inside
// the morph target, or morph would preserve the just-submitted text instead of
// clearing it). The new comment lands in its correct threaded position.

import { html, join, raw, type SafeHtml } from '../html.ts';
import { renderContent, withEmoji } from './content.ts';
import { parseEmojiTags } from '../nostr/nip30.ts';
import { avatar, displayName, timeAgo, npub, type ProfileMap } from './util.ts';
import type { NostrEvent } from '../nostr/types.ts';
import type { Session } from '../session.ts';
import { buildThreadTree, type TreeNode } from '../nostr/thread.ts';

/** The comment compose / reply form. pi/pp empty (top-level) → parent = the
 * article; otherwise pi=parent comment id, pp=its author. Submitting MORPHS the
 * list. `top` ids the form so the post can OOB-reset it; `oob` emits that reset. */
export function commentForm(ra: string, rp: string, pi: string, pp: string, placeholder: string, opts: { top?: boolean; oob?: boolean } = {}): SafeHtml {
    const id = opts.top ? raw(' id="comment-form"') : raw('');
    const oob = opts.oob ? raw(' h-oob="true"') : raw('');
    return html`<form class="comment-form"${id}${oob} action="/comment" method="post" h-post h-target="#comment-list" h-swap="morph">
        <input type="hidden" name="ra" value="${ra}">
        <input type="hidden" name="rp" value="${rp}">
        <input type="hidden" name="pi" value="${pi}">
        <input type="hidden" name="pp" value="${pp}">
        <textarea name="content" required placeholder="${placeholder}"></textarea>
        <div class="comment-foot"><button type="submit" class="busy-btn"><span class="btn-label">Post</span><span class="btn-busy">Posting…</span></button></div>
      </form>`;
}

/** The comment count heading. `oob` emits it as an out-of-band refresh (so a post
 * updates the count without it being inside the morph target). */
function commentTitle(n: number, oob = false): SafeHtml {
    return html`<h3 id="comments-title" class="comments-title"${oob ? raw(' h-oob="true"') : raw('')}>${n || 'No'} comment${n === 1 ? '' : 's'}</h3>`;
}

function renderNode(node: TreeNode, profiles: ProfileMap, ra: string, rp: string): SafeHtml {
    const ev = node.event;
    const slot = `creply-${ev.id.slice(0, 16)}`;
    return html`
      <li class="comment" id="c-${ev.id}">
        <div class="comment-head">
          <a href="/u/${npub(ev.pubkey)}" aria-label="author" h-get h-prefetch="hover" h-scroll="top instant">${avatar(ev.pubkey, profiles.get(ev.pubkey)?.picture, 'xs')}</a>
          <span class="comment-author">${withEmoji(displayName(ev.pubkey, profiles), profiles.get(ev.pubkey)?.emoji)}</span>
          <span class="time">· ${timeAgo(ev.created_at)}</span>
        </div>
        <div class="comment-body">${renderContent(ev.content, profiles, true, undefined, undefined, parseEmojiTags(ev))}</div>
        <div class="comment-actions">
          <button type="button" class="comment-reply-btn" h-get="/comment/form?ra=${encodeURIComponent(ra)}&rp=${rp}&pi=${ev.id}&pp=${ev.pubkey}" h-target="#${raw(slot)}" h-swap="inner" h-focus="#${raw(slot)} textarea" h-push-url="false">Reply</button>
        </div>
        <div id="${slot}" class="comment-reply-slot"></div>
        ${node.children.length ? html`<ul class="comment-children">${join(node.children.map((c) => renderNode(c, profiles, ra, rp)))}</ul>` : null}
      </li>`;
}

/** The list items (the morph payload + the initial render share these). */
function commentItems(comments: NostrEvent[], profiles: ProfileMap, ra: string, rp: string): SafeHtml {
    return join(buildThreadTree(comments).map((n) => renderNode(n, profiles, ra, rp)));
}

export function commentSection(_s: Session, ra: string, rp: string, comments: NostrEvent[], profiles: ProfileMap): SafeHtml {
    return html`
      <section id="comment-section" class="comments">
        ${commentTitle(comments.length)}
        ${commentForm(ra, rp, '', rp, 'Add a comment…', { top: true })}
        <ul id="comment-list" class="comment-list">${commentItems(comments, profiles, ra, rp)}</ul>
      </section>`;
}

/** The post-comment swap payload: the morphed list items (into `#comment-list`) plus
 * an OOB-refreshed count and an OOB-reset top-level form. */
export function commentUpdate(ra: string, rp: string, comments: NostrEvent[], profiles: ProfileMap): SafeHtml {
    return html`${commentItems(comments, profiles, ra, rp)}${commentTitle(comments.length, true)}${commentForm(ra, rp, '', rp, 'Add a comment…', { top: true, oob: true })}`;
}
