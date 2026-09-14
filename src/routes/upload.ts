// Media upload (Phase 3c). The browser sends the file to /upload; this server
// signs the auth (bunker) or asks the extension to sign it (nip07 sign-and-
// resubmit, holding the file between steps) and then does the upload itself -
// Blossom (your kind:10063 server list, or DEFAULT_BLOSSOM_SERVER when unset). The
// result is a hidden <input name="media"> + an <input name="imeta"> + a
// thumbnail, appended into the compose form's #media container (helmjs append);
// POST /note reads them back and appends the URLs + NIP-92 imeta tags.

import { randomBytes } from 'node:crypto';
import { requireLogin } from './common.ts';
import { readUpload, sendFragment, sendSignRequest, type Ctx, type UploadedFile } from '../http.ts';
import { readSignedEvent } from '../nip07.ts';
import { fetchBlossomServers, sha256Hex, blossomAuthTemplate, blossomUploadAll, DEFAULT_BLOSSOM_SERVER, type Upload } from '../upload.ts';
import { mediaItem, composeFileInput } from '../render/compose.ts';
import { respondComposePage } from './note.ts';
import { html } from '../html.ts';
import type { Signer } from '../data/signer.ts';
import { signsOnClient } from '../session.ts';

const MAX_BYTES = 25 * 1024 * 1024;

/** Parse the existing media items carried in the upload POST (so a zero-JS attach
 * accumulates rather than replacing). Each is a JSON-encoded NIP-92 imeta tag. */
function carriedImeta(fields: URLSearchParams): string[][] {
    return fields.getAll('imeta').map((v) => {
        try { const a = JSON.parse(v); return Array.isArray(a) && a[0] === 'imeta' ? a as string[] : null; }
        catch { return null; }
    }).filter((t): t is string[] => !!t);
}

// nip07 holds the file server-side between the upload POST and the signed-auth
// continuation (the browser sends the file once; the continuation is just JSON).
interface Held { file: UploadedFile; hash: string; servers: string[]; expires: number }
const held = new Map<string, Held>();
const HOLD_MS = 5 * 60 * 1000;
function hold(h: Omit<Held, 'expires'>): string {
    const token = randomBytes(18).toString('base64url');
    for (const [k, v] of held) if (v.expires < Date.now()) held.delete(k); // sweep
    held.set(token, { ...h, expires: Date.now() + HOLD_MS });
    return token;
}

const tooBig = (f: UploadedFile) => f.bytes.length > MAX_BYTES;
const uploadError = (ctx: Ctx, msg: string, status = 400): void =>
    // Place into #media (and re-assert swap) so the error is visible even after the
    // nip07 sign chain, whose H-Reswap:none would otherwise no-op the swap.
    sendFragment(ctx, html`<div class="notice error media-error">${msg}</div>`,
        { 'H-Retarget': '#media', 'H-Reswap': 'append' }, status);

/** POST /upload - receive the file; bunker signs+uploads now, nip07 gets a sign
 * request (the file is held for the continuation). */
export async function postUpload(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    let file: UploadedFile | null;
    let fields: URLSearchParams;
    try { ({ file, fields } = await readUpload(ctx.req, MAX_BYTES + 1024)); }
    catch { uploadError(ctx, 'That file is too large (max 25 MB).', 413); return; }
    if (!file || !file.bytes.length) { uploadError(ctx, 'Choose a file to upload.'); return; }
    if (tooBig(file)) { uploadError(ctx, 'That file is too large (max 25 MB).', 413); return; }

    const hash = await sha256Hex(file.bytes);
    const fetched = await fetchBlossomServers(s.pool, s.me, s.myRelays);
    const servers = fetched.length ? fetched : [DEFAULT_BLOSSOM_SERVER]; // out-of-box default when no kind:10063 list

    // client-signs: hold the file, ask the extension/app to sign the Blossom auth (kind:24242).
    if (signsOnClient(s)) {
        const token = hold({ file, hash, servers });
        sendSignRequest(ctx, blossomAuthTemplate(s.me, hash), `/upload/finish?token=${token}`);
        return;
    }

    // bunker: sign the auth here and upload now.
    let upload: Upload;
    try {
        upload = await doUpload(s.signer!, s.me, servers, file, hash);
    } catch (e) {
        uploadError(ctx, `Couldn't upload: ${e instanceof Error ? e.message : String(e)}`, 502);
        return;
    }
    // Enhanced (helmjs): append just the new thumbnail into #media. Zero-JS: the
    // whole compose form posted here, so re-render the compose page with the typed
    // text + already-attached media + this upload preserved.
    // H-Trigger-After-Swap fires `compose-media` once the thumbnail is appended to
    // #media, so the live preview (listening for it) re-renders WITH the new image.
    if (ctx.isPartial) { sendFragment(ctx, html`${mediaItem(upload.imeta)}${composeFileInput(true)}`, { 'H-Trigger-After-Swap': 'compose-media' }); return; }
    await respondComposePage(ctx, s, {
        reply: fields.get('reply'), quote: fields.get('quote'), draft: fields.get('content') ?? '',
        media: [...carriedImeta(fields), upload.imeta],
        cw: fields.get('cw') === '1', cwReason: fields.get('cw_reason') ?? '',
    });
}

/** POST /upload/finish?token= - nip07 continuation: the extension-signed auth +
 * the held file → upload, then append the preview (via the attach form's cfg). */
export async function postUploadFinish(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const token = ctx.query.get('token') ?? '';
    const h = held.get(token);
    held.delete(token);
    if (!h || h.expires < Date.now()) { uploadError(ctx, 'The upload expired. Please try again.', 410); return; }

    const signed = await readSignedEvent(ctx.req);
    if (!signed || signed.pubkey !== s.me || signed.kind !== 24242) { // Blossom auth (BUD-01/02)
        uploadError(ctx, "Couldn't verify the upload authorization.", 400); return;
    }
    try {
        const upload = await blossomUploadAll(h.servers, h.file.bytes, h.file.contentType, h.hash, signed);
        // Re-assert placement: the sign-request set H-Reswap:none (don't swap the JSON
        // template), which mutates the request's swap to "none" - without these the
        // thumbnail never appends into #media (mirrors the actions.ts sign continuation).
        sendFragment(ctx, html`${mediaItem(upload.imeta)}${composeFileInput(true)}`,
            { 'H-Retarget': '#media', 'H-Reswap': 'append', 'H-Trigger-After-Swap': 'compose-media' });
    } catch (e) {
        uploadError(ctx, `Couldn't upload: ${e instanceof Error ? e.message : String(e)}`, 502);
    }
}

/** Sign the Blossom auth via the bunker signer and upload to all servers. */
async function doUpload(signer: Signer, me: string, servers: string[], file: UploadedFile, hash: string): Promise<Upload> {
    const signed = await signer.signEvent(blossomAuthTemplate(me, hash));
    return blossomUploadAll(servers, file.bytes, file.contentType, hash, signed);
}
