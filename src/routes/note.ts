// Compose + publish a note (kind:1) - notes, replies, quotes, with an optional
// content warning. Two signing modes, both keeping the key off the server:
//   bunker - signNote builds + signs via the bunker, publishes, 303 → / (zero-JS).
//   nip07  - signNote runs with a *capture signer* (returns the template unsigned)
//            so the tags are built by the SAME code; the extension signs it and
//            POSTs to /note/publish, which verifies + publishes.
// Reuse over re-implementation: signNote owns the NIP-10 reply tags, NIP-18 quote
// tags, content-warning, mention p-tags, and relay-hint/inbox routing - both modes
// go through it, so there's one source of truth for note construction.

import { signNote, signComment, signPicture, publishSigned, captureSigner, type Prepared, type ReplyTo, type QuoteRef, type CommentTarget, type CommentRef } from '../data/publish.ts';
import { KIND_PICTURE } from '../nostr/nip68.ts';
import { fetchEvent } from '../data/feeds.ts';
import { commentRoot, KIND_COMMENT } from '../nostr/nip22.ts';
import { isAddressable, tag1 } from '../nostr/tags.ts';
import { fetchRelayLists } from '../data/relays.ts';
import { INDEXER_RELAYS, writeRelaysFor } from '../nostr/nip65.ts';
import { chosenTargets, appendRelayTargets, parseScheduleAt } from '../actions.ts';
import { decode } from 'nostr-tools/nip19';
import { decodeNaddr } from '../nostr/nip19.ts';
import type { NostrEvent } from '../nostr/types.ts';
import { html, raw, type SafeHtml } from '../html.ts';
import { displayName } from '../render/util.ts';
import { icon } from '../render/svg.ts';
import { mediaItem, composeFileInput, undoToast, scheduleRow, composeTypes, relayPickerRow, relaysToggleBtn } from '../render/compose.ts';
import { noteCard, composePreview } from '../render/note.ts';
import { remainingSeconds, cancelPublish, commitIfDue, getHeld, getCommitted } from '../undo.ts';
import { tryUndoWindow, sendReplyToThread, stayPutCloseModal, landOnFeed, CLOSE_MODAL_OOB } from './undo-window.ts';
import { articleComposePage, draftsScreen, type ArticleComposeCtx } from '../render/article-compose.ts';
import { getDraft, saveDraft, listDrafts, newDraftId, type NoteDraft, type PollDraft } from '../drafts.ts';
import { saveDraftAndSync, savedStatus } from './article.ts';
import { holdScheduled, SCHEDULE_FULL_MSG, listScheduled } from '../data/scheduled.ts';
import { sendPrivateReply, syntheticReply } from '../data/dms.ts';
import { beginPrivateReplySend, sealStep, wrapPrivateReplyStep } from '../data/dms-nip07.ts';
import { page } from '../render/layout.ts';
import { requireLogin, chromeFor, ensureProfiles } from './common.ts';
import { requireSigned } from '../nip07.ts';
import { readForm, redirect, sendPage, sendFragment, sendSignRequest, readBatchResults, type Ctx } from '../http.ts';
import { pollComposeFields } from '../render/poll.ts';
import type { Session } from '../session.ts';
import { signsOnClient } from '../session.ts';

const MAX_INBOX_RELAYS = 4;

interface ComposeCtx { reply?: { nevent: string; name: string }; quote?: string; draft?: string; error?: string; isNew?: boolean; media?: string[][]; cw?: boolean; cwReason?: string; inThread?: string; draftId?: string; status?: string; relays?: string[]; title?: string }

/** The note compose form body (reply-to + form + help) - no wrapper/selector.
 * ONE multipart form with two submit buttons: "Attach" overrides it to POST the
 * whole form (incl. the file) to /upload, "Publish" posts to /note. Both work
 * zero-JS (native `formaction`); helmjs honors the same submitter overrides
 * (`h-target="#media" h-swap="append"`) so Attach appends a thumbnail in place
 * while Publish boost-swaps. Keeping the file in the compose form is what lets a
 * zero-JS attach preserve the typed text + already-attached media on re-render. */
function noteFormPart(c: ComposeCtx, inModal = false): SafeHtml {
    const title = c.reply ? 'Reply' : c.quote ? 'Quote' : null;
    // Schedule is offered for new notes AND quotes (both are top-level posts to your write relays); a
    // reply is transient/thread-bound, so it stays off there.
    const canSchedule = c.isNew || !!c.quote;
    return html`
      ${c.error ? html`<div class="notice error">${c.error}</div>` : null}
      ${c.reply ? html`<div class="reply-to">Replying to ${c.reply.name}</div>` : null}
      <!-- Default action is /upload so selecting a file auto-uploads (the form's
           change-from-#compose-attach trigger, bound to the stable label, not the
           OOB-reset file input). Publish overrides back to /note. -->
      <form class="compose-box" action="/upload" method="post" h-post enctype="multipart/form-data"
        h-trigger="submit, change from:#compose-attach" h-target="#media" h-swap="append">
        ${inModal ? html`<input type="hidden" name="inmodal" value="1">` : null}
        ${c.reply ? html`<input type="hidden" name="reply" value="${c.reply.nevent}">` : null}
        ${c.inThread ? html`<input type="hidden" name="inthread" value="${c.inThread}">` : null}
        ${c.quote ? html`<input type="hidden" name="quote" value="${c.quote}">` : null}
        <input type="hidden" id="compose-draftid" name="draftid" value="${c.draftId ?? ''}">
        <textarea name="content" id="compose-text"
          aria-label="${c.quote ? 'Add a comment' : title ? 'Write a reply' : 'Write a note'}"
          h-get="/compose/suggest" h-trigger="input debounce:150" h-include="#compose-text" h-busy="false"
          h-selection="" h-target="#suggest" h-swap="inner" h-push-url="false" h-combobox="#suggest"
          placeholder="${c.quote ? 'Add a comment…' : title ? 'Write a reply…' : 'Write a note…'}">${c.draft ?? ''}</textarea>
        <div id="suggest" class="mention-box" role="listbox" aria-label="Suggestions"></div>
        <!-- CW is a foot icon toggle (the .cw-btn label flips this checkbox); the reason
             row reveals on :checked, pure CSS. The checkbox is also the submitted cw field. -->
        <input type="checkbox" id="cw-toggle" name="cw" value="1" class="cw-check"${c.cw ? raw(' checked') : raw('')}>
        <div class="cw-row"><input class="cw-reason-input" type="text" name="cw_reason" value="${c.cwReason ?? ''}" placeholder="Content warning reason (optional)" autocomplete="off"></div>
        <!-- Private reply (replies only): the .private-btn lock toggles this checkbox. When on it turns
             accent and reveals the caption; the form posts private=1 → a gift-wrapped reply, not a public one. -->
        ${c.reply ? html`<input type="checkbox" id="private-toggle" name="private" value="1" class="private-check">
        <div class="private-row">${icon('lock')}<span>Only ${c.reply.name === 'note' ? 'the author' : c.reply.name} can read this. It won't appear publicly.</span></div>` : null}
        <!-- Schedule (new notes + quotes): the .schedule-btn clock toggles this checkbox; the row
             reveals on :checked (pure CSS, like CW). The "Schedule" button sends do=schedule. -->
        ${canSchedule ? html`<input type="checkbox" id="schedule-toggle" class="sched-check">
        ${scheduleRow('/note', raw(' h-target="body" h-swap="inner"'))}` : null}
        <!-- Relays (new notes only): the .relays-btn globe toggles this checkbox; the row reveals on :checked
             (pure CSS, like Schedule). All your write relays start checked; uncheck to post to a subset, or add
             a one-off relay. Resets every open (the form is re-rendered fresh). -->
        ${c.isNew ? relayPickerRow(c.relays ?? []) : null}
        <div class="compose-media" id="media">${(c.media ?? []).map((m) => mediaItem(m))}</div>
        <!-- Live preview (above the foot): listens to the textarea (debounced) + attached
             media, rendered through the real note pipeline. h-busy="false" so typing never
             trips the global loader; :empty hides it until there's something to show. -->
        <div id="compose-preview" class="compose-preview" h-get="/compose/preview"
          h-trigger="input from:#compose-text debounce:180, compose-media from:body" h-include="#compose-text, #media input[name=imeta]"
          h-target="#compose-preview" h-swap="inner" h-busy="false" h-push-url="false"></div>
        <div class="compose-foot">
          <label class="attach-btn" id="compose-attach" title="Add photo or video" aria-label="Add photo or video">${icon('image')}${composeFileInput()}</label>
          <label class="attach-btn cw-btn" for="cw-toggle" title="Content warning" aria-label="Content warning">${icon('alert')}</label>
          ${c.reply ? html`<label class="attach-btn private-btn" for="private-toggle" title="Reply privately - only the author can read it" aria-label="Reply privately">${icon('lock')}</label>` : null}
          ${canSchedule ? html`<label class="attach-btn schedule-btn" for="schedule-toggle" title="Schedule for later" aria-label="Schedule for later">${icon('clock')}</label>` : null}
          ${c.isNew && c.relays?.length ? relaysToggleBtn() : null}
          <!-- Zero-JS only: with JS the file auto-uploads on select, so this fallback
               button is hidden (noscript). It submits the form to its /upload action. -->
          <noscript><button type="submit" class="attach-go">Attach</button></noscript>
          <span id="compose-status" class="compose-status">${c.status ?? ''}</span>
          <!-- Save draft (new notes only): saves locally, then (nip07) runs the relay-sync sign chain in
               this click's gesture and OOB-updates #compose-status + #compose-draftid; bunker syncs inline.
               Replies/quotes are transient, so no draft there. -->
          ${c.isNew ? html`<button type="submit" class="ghost" formaction="/note/draft" formmethod="post" h-target="${inModal ? '#modal' : 'body'}" h-swap="inner">Save draft</button>` : null}
          <button type="submit" class="publish-btn" formaction="/note" formmethod="post" h-target="body" h-swap="inner">${title ?? 'Publish'}</button>
        </div>
      </form>`;
}

/** The poll compose form body (question + poll fields → POST /poll). */
function pollFormPart(d: PollDraft | null = null, inModal = false, status = '', relays: string[] = []): SafeHtml {
    return html`
      <form class="compose-box" action="/poll" method="post" h-post>
        ${inModal ? html`<input type="hidden" name="inmodal" value="1">` : null}
        <input type="hidden" id="compose-draftid" name="draftid" value="${d?.id ?? ''}">
        <textarea name="content" id="poll-question" required placeholder="Ask a question…">${d?.question ?? ''}</textarea>
        ${pollComposeFields({ options: d?.options, multiple: d?.multi, duration: d?.duration })}
        ${relayPickerRow(relays)}
        <div class="compose-foot">
          ${relays.length ? relaysToggleBtn() : null}
          <span id="compose-status" class="compose-status">${status}</span>
          <button type="submit" class="ghost" formaction="/poll/draft" formmethod="post" h-target="${inModal ? '#modal' : 'body'}" h-swap="inner">Save draft</button>
          <button type="submit" class="publish-btn" formaction="/poll" formmethod="post" h-target="body" h-swap="inner">Post poll</button>
        </div>
      </form>`;
}

/** The picture (NIP-68 kind:20) compose form: a title + caption + the shared media strip (≥1 image required
 * to publish). Reuses the note's upload/attach + live-preview + @mention wiring; posts to /picture. Top-level
 * only, and lean (no reply/quote; scheduling + relay-picker supported). A local draft is deferred too - unlike a note/
 * poll draft, it would have to carry already-uploaded Blossom blobs, which is more machinery than v1 needs. */
function pictureFormPart(c: ComposeCtx, inModal = false): SafeHtml {
    return html`
      ${c.error ? html`<div class="notice error">${c.error}</div>` : null}
      <form class="compose-box" action="/upload" method="post" h-post enctype="multipart/form-data"
        h-trigger="submit, change from:#compose-attach" h-target="#media" h-swap="append">
        ${inModal ? html`<input type="hidden" name="inmodal" value="1">` : null}
        <input class="picture-title" type="text" name="title" id="picture-title" value="${c.title ?? ''}" placeholder="Title (optional)" autocomplete="off" maxlength="200">
        <textarea name="content" id="compose-text" aria-label="Caption"
          h-get="/compose/suggest" h-trigger="input debounce:150" h-include="#compose-text" h-busy="false"
          h-selection="" h-target="#suggest" h-swap="inner" h-push-url="false" h-combobox="#suggest"
          placeholder="Add a caption…">${c.draft ?? ''}</textarea>
        <div id="suggest" class="mention-box" role="listbox" aria-label="Suggestions"></div>
        <input type="checkbox" id="cw-toggle" name="cw" value="1" class="cw-check"${c.cw ? raw(' checked') : raw('')}>
        <div class="cw-row"><input class="cw-reason-input" type="text" name="cw_reason" value="${c.cwReason ?? ''}" placeholder="Content warning reason (optional)" autocomplete="off"></div>
        <div class="compose-media" id="media">${(c.media ?? []).map((m) => mediaItem(m))}</div>
        <div id="compose-preview" class="compose-preview" h-get="/compose/preview?type=picture"
          h-trigger="input from:#compose-text debounce:180, input from:#picture-title debounce:180, compose-media from:body" h-include="#picture-title, #compose-text, #media input[name=imeta]"
          h-target="#compose-preview" h-swap="inner" h-busy="false" h-push-url="false"></div>
        <!-- Schedule: the .schedule-btn clock toggles this checkbox; the row reveals on :checked (pure CSS,
             like the note composer). The "Schedule" button sends do=schedule to /picture. -->
        <input type="checkbox" id="schedule-toggle" class="sched-check">
        ${scheduleRow('/picture', raw(' h-target="body" h-swap="inner"'))}
        ${c.isNew ? relayPickerRow(c.relays ?? []) : null}
        <div class="compose-foot">
          <label class="attach-btn" id="compose-attach" title="Add a photo" aria-label="Add a photo">${icon('image')}${composeFileInput()}</label>
          <label class="attach-btn cw-btn" for="cw-toggle" title="Content warning" aria-label="Content warning">${icon('alert')}</label>
          <label class="attach-btn schedule-btn" for="schedule-toggle" title="Schedule for later" aria-label="Schedule for later">${icon('clock')}</label>
          ${c.isNew && c.relays?.length ? relaysToggleBtn() : null}
          <noscript><button type="submit" class="attach-go">Attach</button></noscript>
          <span class="compose-status">${c.status ?? ''}</span>
          <button type="submit" class="publish-btn" formaction="/picture" formmethod="post" h-target="body" h-swap="inner">Post picture</button>
        </div>
      </form>`;
}

/** Wrap compose body in Satori's modal overlay (the helmjs-enhanced presentation;
 * the /compose page is the zero-JS baseline). The ✕ clears #modal. Submitting the
 * form swaps the <body> (boosted) → the modal vanishes and the feed updates. */
function modalWrap(head: SafeHtml, body: SafeHtml): SafeHtml {
    return html`
      <div class="modal-overlay" id="compose-modal">
        <div class="modal">
          <div class="modal-head">${head}<button class="modal-close" h-get="/compose/close" h-target="#modal" h-swap="inner" h-push-url="false" title="Close" aria-label="Close">✕</button></div>
          ${body}
        </div>
      </div>`;
}

/** GET /compose/close - empties the modal mount (helmjs close). */
export function getComposeClose(ctx: Ctx): void {
    if (!requireLogin(ctx)) return;
    sendFragment(ctx, html``);
}

/** GET /compose/preview - a live render of the note being composed (debounced
 * textarea input + attached media), assembling the content exactly like /note does
 * (text + media urls). Empty draft → empty fragment, so the pane (:empty) stays
 * hidden. GET (read-only) on a <div>, since helm only honors h-get off a form;
 * h-busy="false" on the trigger keeps typing from flashing the loader. */
export function getComposePreview(ctx: Ctx): void {
    const s = requireLogin(ctx);
    if (!s) return;
    const text = (ctx.query.get('content') ?? '').trim();
    const imeta = ctx.query.getAll('imeta').map(parseImeta).filter((t): t is string[] => !!t);
    // Picture (kind:20): preview the real image-first card (title → images → caption), not the note layout.
    if (ctx.query.get('type') === 'picture') {
        const title = (ctx.query.get('title') ?? '').trim();
        if (!text && !title && imeta.length === 0) { sendFragment(ctx, html``); return; }
        sendFragment(ctx, composePreview(s.me, text, imeta, s.profiles, { picture: true, title }));
        return;
    }
    const mediaUrls = imeta.map((t) => t.find((x) => x.startsWith('url '))?.slice(4)).filter((u): u is string => !!u);
    const content = [text, ...mediaUrls].filter(Boolean).join('\n');
    if (!content) { sendFragment(ctx, html``); return; }
    sendFragment(ctx, composePreview(s.me, content, imeta, s.profiles));
}

/** Decode an nevent (or note1) into a reply target { id, pubkey }. */
function decodeReplyTo(nevent: string): ReplyTo | null {
    try {
        const d = decode(nevent);
        if (d.type === 'nevent') return { id: d.data.id, pubkey: d.data.author, kind: d.data.kind };
        if (d.type === 'note') return { id: d.data };
    } catch { /* */ }
    return null;
}

/** NIP-22: a reply to anything that ISN'T a kind:1 note must be a kind:1111 comment (the spec forbids
 * commenting on kind:1 - those stay NIP-10). Resolve the comment's root+parent scope from the target:
 *  - the target is itself a 1111 comment -> inherit its uppercase ROOT scope (fetched, since only the
 *    parent event carries it) and name the comment as the immediate parent (a nested comment);
 *  - the target is an addressable event (article/...) -> fetch its `d` tag to build the coordinate;
 *  - the target is any other event (picture 20, video 21/22, ...) -> it IS both root and parent.
 * Returns null when the kind is a note or the scope can't be resolved (caller falls back to a NIP-10 note). */
async function commentTargetFor(s: Session & { me: string }, rt: ReplyTo): Promise<CommentTarget | null> {
    const kind = rt.kind ?? 1;
    if (kind === 1) return null;
    // The target as its own scope - the degraded shape when we can't learn more. NIP-22 forbids a
    // kind:1 reply to a non-1 target outright, so every fallback below stays a comment (possibly
    // with an unknown root or author) rather than returning null into the NIP-10 path.
    const self: CommentRef = { kind, pubkey: rt.pubkey ?? '', id: rt.id };
    if (kind === KIND_COMMENT) {
        const ev = await fetchEvent(s.pool, rt.id, [], rt.pubkey).catch(() => null);
        if (!ev) return { root: self, parent: self }; // parent unfetchable: parent-as-root beats a forbidden kind:1
        const root = commentRoot(ev);
        const rootRef: CommentRef = root
            ? { kind: Number(root.kind) || 1, pubkey: root.pubkey ?? rt.pubkey ?? '', ...(root.type === 'A' ? { address: root.value } : { id: root.value }) }
            : { kind, pubkey: rt.pubkey ?? '', id: rt.id }; // malformed comment: treat it as its own root
        return { root: rootRef, parent: { kind, pubkey: rt.pubkey ?? '', id: rt.id } };
    }
    if (isAddressable(kind)) {
        const ev = await fetchEvent(s.pool, rt.id, [], rt.pubkey).catch(() => null);
        const pubkey = rt.pubkey ?? ev?.pubkey ?? '';
        if (!pubkey) return null; // no author anywhere: a coordinate can't be built (kindless-nevent edge)
        const d = ev ? tag1(ev, 'd') : '';
        const ref: CommentRef = { kind, pubkey, address: `${kind}:${pubkey}:${d}` };
        return { root: ref, parent: ref };
    }
    return { root: self, parent: self }; // picture/video/etc: root === parent (author may be unknown)
}

/** Decode an nevent/note (→ id) or naddr (→ article address) into a quote ref. */
function decodeQuote(entity: string): QuoteRef | null {
    try {
        const d = decode(entity);
        if (d.type === 'nevent') return { id: d.data.id, pubkey: d.data.author, relays: d.data.relays ?? [] };
        if (d.type === 'note') return { id: d.data };
    } catch { /* */ }
    const na = decodeNaddr(entity);
    if (na) return { id: '', address: na.coord, pubkey: na.pubkey, relays: na.relays };
    return null;
}

/** Parse a media item's hidden imeta input (JSON-encoded NIP-92 tag) back to an
 * array; reject anything that isn't a string[] led by "imeta". */
function parseImeta(v: string): string[] | null {
    try {
        const a = JSON.parse(v);
        return Array.isArray(a) && a[0] === 'imeta' && a.every((x) => typeof x === 'string') ? a as string[] : null;
    } catch { return null; }
}

/** Full-page compose (zero-JS baseline): selector (new) + form, in the view pad. */
function composePage(c: ComposeCtx): SafeHtml {
    return html`<div class="view-pad">${c.isNew ? composeTypes('note') : null}${noteFormPart(c)}</div>`;
}

/** Resolve a ComposeCtx from raw reply/quote nevents (+ carried fields), fetching
 * the reply author's name. Shared by the compose page and the zero-JS upload
 * re-render (so an attach with JS off keeps text/media/CW + the reply context). */
async function buildComposeCtx(
    s: Session & { me: string }, replyNevent: string | null, quoteNevent: string | null,
    extra: { draft?: string; media?: string[][]; cw?: boolean; cwReason?: string; error?: string } = {},
): Promise<ComposeCtx> {
    const c: ComposeCtx = { isNew: !replyNevent && !quoteNevent, ...extra };
    if (replyNevent) {
        const rt = decodeReplyTo(replyNevent);
        if (rt?.pubkey) await ensureProfiles(s, [rt.pubkey]);
        c.reply = { nevent: replyNevent, name: rt?.pubkey ? displayName(rt.pubkey, s.profiles) : 'note' };
    } else if (quoteNevent) {
        c.quote = quoteNevent;
    }
    return c;
}

/** Re-render the full /compose page (used by the zero-JS upload re-render). */
export async function respondComposePage(
    ctx: Ctx, s: Session & { me: string },
    opts: { reply?: string | null; quote?: string | null; draft?: string; media?: string[][]; cw?: boolean; cwReason?: string },
): Promise<void> {
    const c = await buildComposeCtx(s, opts.reply ?? null, opts.quote ?? null, opts);
    sendPage(ctx, composePage(c), chromeFor(ctx, s, { active: 'compose', title: c.reply ? 'Reply' : c.quote ? 'Quote' : 'Compose' }));
}

export async function getCompose(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const replyNevent = ctx.query.get('reply');
    const quoteNevent = ctx.query.get('quote');
    const isNew = !replyNevent && !quoteNevent;
    // helmjs opens compose as a modal overlay (#modal); zero-JS lands on the page.
    const inModal = ctx.isPartial && ctx.hTarget === '#modal';

    // A reopened draft (?draft=<id>): dispatch by its stored type.
    const draftId = isNew ? ctx.query.get('draft') : null;
    const draft = draftId ? getDraft(s.me, draftId) : null;

    if (isNew && (ctx.query.get('type') === 'poll' || draft?.type === 'poll')) {
        const pd = draft?.type === 'poll' ? draft : null;
        if (inModal) { sendFragment(ctx, modalWrap(composeTypes('poll', true), pollFormPart(pd, true, '', writeRelaysFor(s.myRelays)))); return; }
        sendPage(ctx, html`<div class="view-pad">${composeTypes('poll')}${pollFormPart(pd, false, '', writeRelaysFor(s.myRelays))}</div>`, chromeFor(ctx, s, { active: 'compose', title: 'Poll' }));
        return;
    }

    // Picture (NIP-68 kind:20) - a lean top-level composer (title + caption + images), like poll.
    if (isNew && ctx.query.get('type') === 'picture') {
        if (inModal) { sendFragment(ctx, modalWrap(composeTypes('picture', true), pictureFormPart({ isNew: true, relays: writeRelaysFor(s.myRelays) }, true))); return; }
        sendPage(ctx, html`<div class="view-pad">${composeTypes('picture')}${pictureFormPart({ isNew: true, relays: writeRelaysFor(s.myRelays) })}</div>`, chromeFor(ctx, s, { active: 'compose', title: 'Picture' }));
        return;
    }

    // Article (NIP-23) is a full-page composer (never a modal), like Satori.
    if (isNew && (ctx.query.get('type') === 'article' || draft?.type === 'article')) {
        const d = draft?.type === 'article' ? draft : null;
        const c: ArticleComposeCtx = d ? { identifier: d.identifier, title: d.title, summary: d.summary, image: d.image, topics: d.topics, body: d.body } : {};
        c.relays = writeRelaysFor(s.myRelays); // the relay-picker list
        sendPage(ctx, articleComposePage(c), chromeFor(ctx, s, { active: 'compose', title: 'Article' }));
        return;
    }

    const c: ComposeCtx = { isNew };
    if (isNew) c.relays = writeRelaysFor(s.myRelays); // the relay-picker list (top-level notes only)
    if (draft?.type === 'note') {
        c.draft = draft.content; c.media = draft.imeta; c.cw = draft.cw; c.cwReason = draft.cwReason; c.draftId = draft.id;
    } else if (replyNevent) {
        const rt = decodeReplyTo(replyNevent);
        if (rt?.pubkey) await ensureProfiles(s, [rt.pubkey]);
        c.reply = { nevent: replyNevent, name: rt?.pubkey ? displayName(rt.pubkey, s.profiles) : 'note' };
        c.inThread = ctx.query.get('inthread') ?? undefined; // reply from a thread → append there
    } else if (quoteNevent) {
        c.quote = quoteNevent;
        c.draft = `\n\nnostr:${quoteNevent}`;
    }
    const title = c.reply ? 'Reply' : c.quote ? 'Quote' : 'Compose';

    if (inModal) {
        const head = isNew ? composeTypes('note', true) : html`<span>${title}</span>`;
        sendFragment(ctx, modalWrap(head, noteFormPart(c, true)));
        return;
    }
    sendPage(ctx, composePage(c), chromeFor(ctx, s, { active: 'compose', title }));
}

/** POST /note/draft - save the note composer as a LOCAL draft, then re-render the composer in
 * place with a saved status (modal stays open; zero-JS re-renders the page). New notes only. */
export async function postNoteDraft(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const form = await readForm(ctx.req, 30 * 1024 * 1024);
    const content = (form.get('content') ?? '').trim();
    const imeta = form.getAll('imeta').map(parseImeta).filter((t): t is string[] => !!t);
    const cw = form.get('cw') === '1';
    const cwReason = (form.get('cw_reason') ?? '').trim();
    const existingId = (form.get('draftid') ?? '').trim();
    const inModal = ctx.isPartial && ctx.hTarget === '#modal';
    const render = (c: ComposeCtx): void => {
        if (inModal) { sendFragment(ctx, modalWrap(composeTypes('note', true), noteFormPart(c, true))); return; }
        sendPage(ctx, composePage(c), chromeFor(ctx, s, { active: 'compose', title: 'Compose' }));
    };
    if (!content && imeta.length === 0) {
        render({ isNew: true, draftId: existingId || undefined, status: 'Nothing to save yet.' });
        return;
    }
    const id = existingId || newDraftId();
    const prev = getDraft(s.me, id);
    const draft: NoteDraft = { type: 'note', id, content, imeta, cw, cwReason, savedAt: Date.now(), synced: prev?.synced, syncedAt: prev?.syncedAt };
    saveDraft(s.me, draft);
    // bunker syncs inline + renders "saved"; nip07 runs the sync chain in this click's gesture, then OOB-
    // updates #compose-status + #compose-draftid (the modal stays put meanwhile).
    await saveDraftAndSync(ctx, s, draft, (synced) =>
        render({ isNew: true, draft: content, media: imeta, cw, cwReason, draftId: id, status: savedStatus(synced) }));
}

/** POST /poll/draft - save the poll composer as a LOCAL draft, then re-render in place with a
 * saved status (modal stays open; zero-JS re-renders the page). */
export async function postPollDraft(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const form = await readForm(ctx.req);
    const question = (form.get('content') ?? '').trim();
    const options = form.getAll('option').map((o) => o.trim()).filter(Boolean);
    const multi = form.get('multiple') === '1';
    const duration = Number(form.get('duration') ?? '0') || 0;
    const id = (form.get('draftid') ?? '').trim() || newDraftId();
    const prev = getDraft(s.me, id);
    const draft: PollDraft = { type: 'poll', id, question, options, multi, duration, savedAt: Date.now(), synced: prev?.synced, syncedAt: prev?.syncedAt };
    const inModal = ctx.isPartial && ctx.hTarget === '#modal';
    const render = (status: string): void => {
        if (inModal) { sendFragment(ctx, modalWrap(composeTypes('poll', true), pollFormPart(draft, true, status, writeRelaysFor(s.myRelays)))); return; }
        sendPage(ctx, html`<div class="view-pad">${composeTypes('poll')}${pollFormPart(draft, false, status, writeRelaysFor(s.myRelays))}</div>`, chromeFor(ctx, s, { active: 'compose', title: 'Poll' }));
    };
    if (!question && options.length === 0) { render('Nothing to save yet.'); return; } // don't persist empty
    saveDraft(s.me, draft);
    // bunker syncs inline + renders "saved"; nip07 runs the sync chain in this click's gesture, then OOB-updates the composer.
    await saveDraftAndSync(ctx, s, draft, (synced) => render(savedStatus(synced)));
}


/** POST /picture - compose a NIP-68 picture (kind:20). Both signing families like notes/polls: bunker signs
 * server-side (with an undo window); nip07 returns a sign request continued at /picture/publish. */
export async function postPicture(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const form = await readForm(ctx.req, 30 * 1024 * 1024);
    const title = (form.get('title') ?? '').trim();
    const content = (form.get('content') ?? '').trim();
    const imeta = form.getAll('imeta').map(parseImeta).filter((t): t is string[] => !!t);
    const cw = form.get('cw') === '1';
    const cwReason = (form.get('cw_reason') ?? '').trim();
    const fromModal = form.get('inmodal') === '1';
    if (imeta.length === 0) { redirect(ctx, '/compose?type=picture'); return; } // a picture needs at least one image
    // NIP-68: kind 20 is images only (the spec enumerates image mimes). The shared file input also
    // accepts video for notes, so a video reaching the picture POST is rejected, not silently posted.
    const badMime = imeta.map((t) => t.find((v) => v.startsWith('m '))?.slice(2)).find((m) => m && !m.startsWith('image/'));
    if (badMime) { sendFragment(ctx, html`<div class="notice error">Picture posts accept images only (got ${badMime}) - post video as a note instead.</div>`, {}, 400); return; }
    // Schedule: sign now with created_at = the chosen time, hold on disk, the sweep broadcasts it then.
    let scheduledAt = 0;
    if (form.get('do') === 'schedule') {
        const r = parseScheduleAt(form.get('schedule') ?? '');
        if ('error' in r) { sendFragment(ctx, html`<div class="notice error">Pick a time in the future to schedule.</div>`, {}, 400); return; }
        scheduledAt = r.at;
    }
    const opts = { title, content, imeta, contentWarning: cw ? (cwReason || '') : null, ...(scheduledAt ? { createdAt: scheduledAt } : {}) };
    if (signsOnClient(s)) {
        const prepared = await signPicture(captureSigner, s.me, s.myRelays!, opts);
        const q = new URLSearchParams();
        if (fromModal) q.set('inmodal', '1');
        if (scheduledAt) q.set('schedule', String(scheduledAt)); // store, don't publish
        appendRelayTargets(q, form); // carry the relay-picker selection
        sendSignRequest(ctx, prepared.signed, q.toString() ? `/picture/publish?${q}` : '/picture/publish');
        return;
    }
    try {
        const prepared = await signPicture(s.signer!, s.me, s.myRelays!, opts);
        prepared.writeTargets = chosenTargets(form, s); // relay-picker selection
        if (scheduledAt) { // hold the signed picture for the sweep instead of publishing
            if (!holdScheduled(s.me, prepared.signed, scheduledAt, prepared.writeTargets)) { sendFragment(ctx, html`<div class="notice error">${SCHEDULE_FULL_MSG}</div>`, {}, 400); return; }
            redirect(ctx, '/drafts');
            return;
        }
        if (await tryUndoWindow(ctx, s, prepared, { fromModal })) return; // hold + countdown toast (helmjs)
        await publishSigned(s.pool, prepared);
    } catch (err) {
        sendFragment(ctx, html`<div class="notice error">Couldn't post the picture: ${err instanceof Error ? err.message : String(err)}</div>`, {}, 502);
        return;
    }
    if (ctx.isPartial) { if (fromModal) stayPutCloseModal(ctx); else await landOnFeed(ctx, s as Session & { me: string }); return; }
    redirect(ctx, '/');
}

/** nip07 continuation for a composed picture (the browser-signed kind:20 comes back here). */
export async function postPicturePublish(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const signed = await requireSigned(ctx, s.me, KIND_PICTURE, 'the picture');
    if (!signed) return;
    const fromModal = ctx.query.get('inmodal') === '1';
    // Scheduled (nip07): the extension signed our future-dated picture; hold it for the sweep. Re-validate
    // the client time server-side - a past/0/NaN value just falls through to publish-now.
    const schedule = Number(ctx.query.get('schedule')) || 0;
    if (schedule > Math.floor(Date.now() / 1000)) {
        if (!holdScheduled(s.me, signed as NostrEvent, schedule, chosenTargets(ctx.query, s as Session & { me: string }))) {
            sendFragment(ctx, html`<div class="notice error">${SCHEDULE_FULL_MSG}</div>`, {}, 400);
            return;
        }
        sendFragment(ctx, page(draftsScreen(listScheduled(s.me), listDrafts(s.me)), chromeFor(ctx, s as Session & { me: string }, { active: 'drafts', title: 'Drafts' })),
            { 'H-Push-Url': '/drafts', 'H-Retarget': 'body', 'H-Reselect': 'body', 'H-Reswap': 'inner' });
        return;
    }
    const prepared: Prepared = { signed, isReply: false, writeTargets: chosenTargets(ctx.query, s as Session & { me: string }), inboxTargets: [] };
    if (await tryUndoWindow(ctx, s as Session & { me: string }, prepared, { requirePartial: false, fromModal })) return; // nip07 = always JS
    try { await publishSigned(s.pool, prepared); } catch (err) {
        sendFragment(ctx, html`<div class="notice error">Couldn't publish: ${err instanceof Error ? err.message : String(err)}</div>`, {}, 502);
        return;
    }
    if (fromModal) stayPutCloseModal(ctx); else await landOnFeed(ctx, s as Session & { me: string });
}

export async function postNote(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    // Larger cap: the compose form is multipart and may still carry a selected (but
    // skipped) file if Publish is hit before the attach OOB reset lands.
    const form = await readForm(ctx.req, 30 * 1024 * 1024);
    const text = (form.get('content') ?? '').trim();
    const replyNevent = form.get('reply') || null;
    const inthread = (replyNevent && form.get('inthread')) || null; // reply from a thread
    const fromModal = form.get('inmodal') === '1'; // posted from the compose modal (stay put) vs the full /compose page (land on feed)
    const quoteNevent = form.get('quote') || null;
    const cw = form.get('cw') === '1';
    const cwReason = (form.get('cw_reason') ?? '').trim();
    const contentWarning = cw ? cwReason : null;
    // Uploaded media (Phase 3c): one NIP-92 imeta tag per item (hidden inputs filled
    // by /upload). The url lives in the imeta, so it's the single source of truth -
    // we append each url to the content (newline-joined, like Satori).
    const imeta = form.getAll('imeta').map(parseImeta).filter((t): t is string[] => !!t);
    const mediaUrls = imeta.map((t) => t.find((x) => x.startsWith('url '))?.slice(4)).filter((u): u is string => !!u);
    const content = [text, ...mediaUrls].filter(Boolean).join('\n');

    const back = (error: string, status = 400): void => {
        const c: ComposeCtx = { error, draft: text, isNew: !replyNevent && !quoteNevent, media: imeta, cw, cwReason };
        if (replyNevent) { const rt = decodeReplyTo(replyNevent); c.reply = { nevent: replyNevent, name: rt?.pubkey ? displayName(rt.pubkey, s.profiles) : 'note' }; }
        else if (quoteNevent) c.quote = quoteNevent;
        sendPage(ctx, composePage(c), chromeFor(ctx, s, { active: 'compose', title: c.reply ? 'Reply' : c.quote ? 'Quote' : 'Compose' }), status);
    };
    if (!content) { back('Write something or attach media first.'); return; }

    // Scheduling (top-level notes + quotes, not replies): sign now with created_at = the scheduled time,
    // hold it on disk, and the daemon's sweep broadcasts it then. The "Schedule" button sends do=schedule.
    let scheduledAt = 0;
    if (!replyNevent && form.get('do') === 'schedule') {
        const r = parseScheduleAt(form.get('schedule') ?? '');
        if ('error' in r) { back(r.error === 'empty' ? 'Pick a time to schedule this for.' : 'Pick a time in the future to schedule.'); return; }
        scheduledAt = r.at;
    }

    const opts = {
        content, contentWarning, imeta,
        replyTo: replyNevent ? decodeReplyTo(replyNevent) : null,
        quote: quoteNevent ? decodeQuote(quoteNevent) : null,
        ...(scheduledAt ? { createdAt: scheduledAt } : {}),
    };

    // Private reply (NIP-59 gift-wrapped): a reply only, delivered to the author's DM relays - never
    // published publicly. We build the EXACT NIP-10 tags via signNote+capture signer (so it threads and
    // renders like a public reply), then wrap instead of publish. Both signing families, parity with DMs.
    if (replyNevent && form.get('private') === '1') {
        const rt = decodeReplyTo(replyNevent);
        if (!rt?.pubkey || !rt.id) { back("Can't send a private reply without knowing the author."); return; }
        const tmpl = await signNote(captureSigner, s.pool, s.me, s.myRelays!, opts); // tags only; never signed/published
        const baseTags = tmpl.signed.tags;
        if (signsOnClient(s)) {
            const r = beginPrivateReplySend(s, rt.pubkey, rt.id, baseTags, content);
            if (!r) { back("Couldn't start the private reply."); return; }
            const q = new URLSearchParams({ reply: replyNevent, chain: r.chainId });
            if (inthread) q.set('inthread', inthread);
            sendSignRequest(ctx, { items: r.items }, `/note/private/seal?${q}`, 'nip44_encrypt_batch');
            return;
        }
        const pr = await sendPrivateReply(s, rt.pubkey, rt.id, baseTags, content);
        if (!pr) { back("Couldn't send the private reply.", 502); return; }
        if (inthread && ctx.isPartial) { sendReplyToThread(ctx, s, syntheticReply(pr), inthread, undefined, true); return; }
        redirect(ctx, `/t/${replyNevent}`);
        return;
    }

    // NIP-22: a public reply to anything that isn't a kind:1 note becomes a kind:1111 comment (the spec
    // bars commenting on kind:1; everything else - comments, pictures, videos - threads as a comment).
    const replyKind = opts.replyTo?.kind ?? 1;
    const commentTarget = opts.replyTo && replyKind !== 1 ? await commentTargetFor(s, opts.replyTo) : null;

    // client-signs: build the exact template via signNote/signComment+capture signer, hand to the extension/app.
    if (signsOnClient(s)) {
        const prepared = commentTarget
            ? await signComment(captureSigner, s.pool, s.me, s.myRelays!, { content, comment: commentTarget, contentWarning, imeta })
            : await signNote(captureSigner, s.pool, s.me, s.myRelays!, opts);
        const q = new URLSearchParams();
        if (replyNevent) q.set('reply', replyNevent);
        if (commentTarget) q.set('k', String(KIND_COMMENT)); // the continuation must verify a 1111, not a kind:1
        if (inthread) q.set('inthread', inthread);
        if (fromModal) q.set('inmodal', '1');
        if (!replyNevent && !quoteNevent) appendRelayTargets(q, form); // top-level: carry the relay-picker selection to the publish step
        if (scheduledAt) { q.set('schedule', String(scheduledAt)); sendSignRequest(ctx, prepared.signed, `/note/publish?${q}`); return; } // store, don't publish
        sendSignRequest(ctx, prepared.signed, q.toString() ? `/note/publish?${q}` : '/note/publish');
        return;
    }

    // bunker: sign + publish here (signNote/signComment handle write + recipient-inbox routing).
    try {
        const prepared = commentTarget
            ? await signComment(s.signer!, s.pool, s.me, s.myRelays!, { content, comment: commentTarget, contentWarning, imeta })
            : await signNote(s.signer!, s.pool, s.me, s.myRelays!, opts);
        if (!replyNevent && !quoteNevent) prepared.writeTargets = chosenTargets(form, s); // relay-picker (top-level only)
        if (scheduledAt) { // hold the signed note for the sweep instead of publishing
            if (!holdScheduled(s.me, prepared.signed, scheduledAt, prepared.writeTargets)) { back(SCHEDULE_FULL_MSG, 400); return; }
            redirect(ctx, '/drafts');
            return;
        }
        if (await tryUndoWindow(ctx, s, prepared, { inThread: inthread ?? undefined, fromModal })) return; // hold + optimistic UI (helmjs)
        await publishSigned(s.pool, prepared);
        // Undo off but a thread reply (helmjs): append the confirmed reply in place.
        if (inthread && ctx.isPartial) { sendReplyToThread(ctx, s, prepared.signed, inthread); return; }
    } catch (err) {
        back(`Couldn't publish: ${err instanceof Error ? err.message : String(err)}`, 502);
        return;
    }
    // JS: modal compose stays put (close the modal); the full /compose page lands on the feed. Zero-JS redirects.
    if (ctx.isPartial) { if (fromModal) stayPutCloseModal(ctx); else await landOnFeed(ctx, s); return; }
    redirect(ctx, '/'); // zero-JS: a real navigation to the feed
}

/** The replied-to author's inbox (read) relays, for reply delivery - mirrors
 * signNote's inbox routing, used by the nip07 continuation. */
async function replyInbox(s: Session & { me: string }, replyNevent: string): Promise<string[]> {
    const rt = decodeReplyTo(replyNevent);
    if (!rt?.pubkey) return [];
    const lists = await fetchRelayLists(s.pool, INDEXER_RELAYS, [rt.pubkey]).catch(() => new Map());
    const read = lists.get(rt.pubkey)?.read ?? [];
    return (read.length ? read : INDEXER_RELAYS).slice(0, MAX_INBOX_RELAYS);
}

/** POST /note/publish - the NIP-07 continuation: verify the extension-signed event
 * and publish (to write relays + recipient inbox for a reply), then land on the feed. */
export async function postNotePublish(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const expectKind = Number(ctx.query.get('k')) || 1; // a NIP-22 comment reply signs a 1111, not a kind:1
    const signed = await requireSigned(ctx, s.me, expectKind, 'the signed note');
    if (!signed) return;

    // Scheduled (nip07): the extension signed our future-dated note; store it for the sweep.
    // Re-validate the client-supplied schedule time server-side: only hold it if it's actually in
    // the future. A tampered/stale value (past/0/NaN) just falls through to publish-now - the event's
    // created_at is already frozen by the signature, so this only governs broadcast timing.
    const schedule = Number(ctx.query.get('schedule')) || 0;
    if (schedule > Math.floor(Date.now() / 1000)) {
        const writeTargets = chosenTargets(ctx.query, s);
        if (!holdScheduled(s.me, signed as NostrEvent, schedule, writeTargets)) {
            sendFragment(ctx, html`<div class="notice error">${SCHEDULE_FULL_MSG}</div>`, {}, 400);
            return;
        }
        sendFragment(ctx, page(draftsScreen(listScheduled(s.me), listDrafts(s.me)), chromeFor(ctx, s as Session & { me: string }, { active: 'drafts', title: 'Drafts' })),
            { 'H-Push-Url': '/drafts', 'H-Retarget': 'body', 'H-Reselect': 'body', 'H-Reswap': 'inner' });
        return;
    }

    const replyNevent = ctx.query.get('reply');
    const inthread = (replyNevent && ctx.query.get('inthread')) || null;
    const fromModal = ctx.query.get('inmodal') === '1';
    const writeTargets = chosenTargets(ctx.query, s);
    const inboxTargets = replyNevent ? await replyInbox(s, replyNevent) : [];
    const prepared: Prepared = { signed: signed as NostrEvent, isReply: !!replyNevent, writeTargets, inboxTargets };
    if (await tryUndoWindow(ctx, s as Session & { me: string }, prepared, { requirePartial: false, inThread: inthread ?? undefined, fromModal })) return; // nip07 = always JS
    try {
        await publishSigned(s.pool, prepared);
    } catch (err) {
        sendFragment(ctx, html`<div class="notice error">Couldn't publish: ${err instanceof Error ? err.message : String(err)}</div>`, {}, 502);
        return;
    }
    // Undo off but a thread reply: append the confirmed reply; else (top-level) modal stays put, full page lands on feed.
    if (inthread) { sendReplyToThread(ctx, s as Session & { me: string }, prepared.signed, inthread); return; }
    if (fromModal) stayPutCloseModal(ctx); else await landOnFeed(ctx, s as Session & { me: string });
}

/** Error fragment for a stalled private-reply send chain. Appended into the open thread (and the compose
 * modal closed) so it never wipes the page body the boosted form was targeting. */
function privateSendError(ctx: Ctx, msg: string): void {
    sendFragment(ctx, html`<li class="notice error">${msg}</li>${CLOSE_MODAL_OOB}`,
        { 'H-Retarget': '#thread', 'H-Reswap': 'append' }, 400);
}

/** POST /note/private/seal - nip07 continuation: encrypt results in, build the seal templates, sign-batch.
 * Mirrors the DM send chain but routed through the note composer (carries reply/inthread for the wrap). */
export async function postPrivateReplySeal(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return; if (!signsOnClient(s)) { privateSendError(ctx, 'Wrong signing mode.'); return; }
    const chainId = ctx.query.get('chain') ?? '';
    const results = await readBatchResults(ctx.req);
    const next = results ? sealStep(s, chainId, results) : null;
    if (!next) { privateSendError(ctx, "Couldn't send the private reply."); return; }
    const q = new URLSearchParams({ chain: chainId });
    const reply = ctx.query.get('reply'); if (reply) q.set('reply', reply);
    const inthread = ctx.query.get('inthread'); if (inthread) q.set('inthread', inthread);
    sendSignRequest(ctx, { templates: next.templates }, `/note/private/wrap?${q}`, 'sign_event_batch');
}

/** POST /note/private/wrap - nip07 continuation: signed seals in, wrap + publish to the author's DM
 * relays, then append the optimistic (lock-badged) reply in-thread, or redirect to the thread. */
export async function postPrivateReplyWrap(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return; if (!signsOnClient(s)) { privateSendError(ctx, 'Wrong signing mode.'); return; }
    const chainId = ctx.query.get('chain') ?? '';
    const results = await readBatchResults(ctx.req);
    const sent = results ? await wrapPrivateReplyStep(s, chainId, results) : null;
    if (!sent) { privateSendError(ctx, "Couldn't send the private reply."); return; }
    const reply = ctx.query.get('reply');
    const inthread = (reply && ctx.query.get('inthread')) || null;
    if (inthread) { sendReplyToThread(ctx, s as Session & { me: string }, syntheticReply(sent), inthread, undefined, true); return; }
    redirect(ctx, reply ? `/t/${reply}` : '/');
}

/** GET /note/tick?token= - the undo countdown poller. Each second it re-renders the
 * countdown (the bottom toast for a feed post, or the pending reply card in a
 * thread); at the deadline it publishes and reconciles in place (feed body-swap, or
 * the confirmed reply card); if the held event is gone (undone) it clears. */
export async function getNoteTick(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    if (!ctx.isPartial) { redirect(ctx, '/'); return; } // a full nav → just go to the feed
    const token = ctx.query.get('token') ?? '';
    const held = getHeld(token, s.me);
    const remaining = remainingSeconds(token);
    if (remaining === null || !held) {
        // Gone. If it COMMITTED (published) - not undone - the poll raced the deadline publish (or
        // the backstop committed first), so re-render the confirmed reply card instead of wiping
        // the optimistic note. Undone tokens (and feed posts, no reply) just clear.
        const c = getCommitted(token, s.me);
        if (c?.reply) { sendFragment(ctx, noteCard(c.signed, s.profiles, s, { hideParent: true, depth: 0, inThread: c.reply.inThread })); return; }
        sendFragment(ctx, html``);
        return;
    }
    const reply = held.reply;
    if (remaining > 0) { // counting down
        sendFragment(ctx, reply
            ? noteCard(held.prepared.signed, s.profiles, s, { hideParent: true, depth: 0, pending: { token, seconds: remaining } })
            : undoToast(token, remaining));
        return;
    }
    await commitIfDue(s.pool, token); // due → publish (the undo window is over)
    if (reply) { sendFragment(ctx, noteCard(held.prepared.signed, s.profiles, s, { hideParent: true, depth: 0, inThread: reply.inThread })); return; }
    // A top-level note/poll publishes silently here: it does NOT appear in your OWN Following feed (a
    // follows feed excludes your own pubkey), so there is nothing to refresh. Just clear the toast and leave
    // the feed as you left it. Re-rendering feedDocument here recomputed followingBoundary AFTER the landing's
    // clearing had already advanced the high-water, which blanked the feed (the "empty timeline" bug).
    sendFragment(ctx, html``);
}

/** POST /note/undo?token= - cancel the held publish; remove the toast. */
export function postNoteUndo(ctx: Ctx): void {
    const s = requireLogin(ctx);
    if (!s) return;
    cancelPublish(ctx.query.get('token') ?? '', s.me);
    sendFragment(ctx, html``); // the undo button targets #undo-toast (outer) → removed
}
