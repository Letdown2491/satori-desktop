// Owned HTTP server (Node http) - routing, static assets, and per-request session
// resolution. No web framework. Run with: node --experimental-strip-types src/server.ts

import { createServer, type IncomingMessage, type ServerResponse } from 'node:http';
import { readFile, stat } from 'node:fs/promises';
import { gzipSync } from 'node:zlib';
import { fileURLToPath } from 'node:url';
import { dirname, join as pathJoin } from 'node:path';
import { parseCookies, notFound, redirect, setPageRenderer, type Ctx } from './http.ts';
import { page } from './render/layout.ts';
import { installTorRouting } from './data/ws-tor.ts';
import { registerSatoriKinds } from './manifest/satori.ts';
import { Pool } from './data/pool.ts';
import { resumeHolds } from './undo.ts';
import { startScheduledSweep } from './data/scheduled.ts';
import { getScheduled, postScheduledCancel } from './routes/scheduled.ts';
import { prunePersisted, persistedPubkeys } from './store.ts';
import { adoptOwnerIfUnclaimed, accessMode } from './access.ts';
import { getSession, isLoggedIn } from './session.ts';
import { getFeed, getFollowers, getLongform, getTimeline, getRelay, getRelayPick, postRelayFavorite, getFaces, getNotesDot, getFeedSeen, getListPrime, postListPrimed } from './routes/feed.ts';
import { getLogin, postLogin, postLogout, postLoginNip07, postLoginNip07Verify } from './routes/login.ts';
import { getProfile, getProfileExtras, getThread, getThreadPrivate, postThreadPrivateSeals, postThreadPrivateRumors, getArticle, getEmbed } from './routes/read.ts';
import { getHandlers } from './routes/handlers.ts';
import { getAvatar } from './routes/avatar.ts';
import { getMedia, getVideoEmbed } from './routes/media.ts';
import { getYtCard, getYtThumb, getYtPlay, getYtPlaylistCard, getYtPlaylistPlay } from './routes/youtube.ts';
import { getCompose, getComposeClose, getComposePreview, postNote, postNotePublish, postPicture, postPicturePublish, postPrivateReplySeal, postPrivateReplyWrap, postNoteDraft, postPollDraft, getNoteTick, postNoteUndo } from './routes/note.ts';
import { postAppearance, getWallet, postWallet, getMetrics } from './routes/pages.ts';
import { getBookmarks, getMuted, getListDecrypt, postListDecrypted } from './routes/saved.ts';
import { getSettings, getSettingsTab, getRelaysPane, postRelaysEdit, postRelays, postRelaysPublish, postLocalRelay, postLocalRelayAuth, postLocalRelayAuthComplete, getLocalRelayStatus, postDmRelaysEdit, postDmRelays, postDmRelaysPublish, postMediaEdit, postMedia, postMediaPublish, getRelayScore, getBackupExport, postBackupImport, postBackupRestore, postSearchEdit, postSearchSave, postPrivacy, getPrivacyStatus, postContentPrefs, postContentFilters } from './routes/settings.ts';
import { getProfileEdit, postProfile, postProfilePublish } from './routes/profile.ts';
import { getNotifications, getNotifUnread } from './routes/notifications.ts';
import { getMessages, getRequests, getMessagesDot, getNewMessage, getThread as getDmThread, getThreadOlder, postSend, postReadAll, getDmSync, getThreadSync, postDmSeals, postDmRumors, postDmLegacy, postSendSeal, postSendWrap } from './routes/dms.ts';
import { postAction, postActionPublish, postActPrivateDec, postActPrivateEnc, postActPrivateSign } from './routes/actions.ts';
import { postUpload, postUploadFinish } from './routes/upload.ts';
import { getSuggest } from './routes/suggest.ts';
import { getSearch } from './routes/search.ts';
import { postLike, postLikePublish } from './routes/like.ts';
import { postDelete, postDeleteConfirm, postDeleteCancel, postDeletePublish } from './routes/delete.ts';
import { getZap, postZap, postZapInvoice, postZapPaid } from './routes/zap.ts';
import { postArticle, postArticlePublish, postDraft, getDrafts, postDraftDelete, postDraftDeleteFinish, postDraftSyncWrap, postDraftSyncPublish, getDraftsSync, postDraftsSyncApply } from './routes/article.ts';
import { postComment, postCommentPublish, getCommentForm } from './routes/comment.ts';
import { getPoll, getPollOption, postPoll, postPollPublish, postPollVote, postPollVotePublish } from './routes/poll.ts';
import { getCalIcs, getCalRsvp, postCalRsvp, postCalRsvpPublish } from './routes/calendar.ts';

// Inject the app's full-page shell into the HTTP kernel (the one place engine and app fuse).
// The kernel's sendPage() calls this; with it the kernel no longer imports the app's layout.
setPageRenderer(page);
// Register Satori's kinds with the manifest registry (the "local manifest"). Render/route layers
// dispatch event rendering through the registry (renderEvent) instead of branching on ev.kind.
registerSatoriKinds();

const ROOT = dirname(fileURLToPath(import.meta.url));
const PUBLIC = pathJoin(ROOT, '..', 'public');

type Handler = (ctx: Ctx) => void | Promise<void>;
interface Route { method: string; segs: string[]; handler: Handler; }

function route(method: string, path: string, handler: Handler): Route {
    return { method, segs: path.split('/').filter(Boolean), handler };
}

// Flat, explicit route table. `:name` captures one path segment into ctx.params.
const ROUTES: Route[] = [
    route('GET', '/', getFeed),
    route('GET', '/login', getLogin),
    route('POST', '/login', postLogin),
    route('POST', '/login/nip07', postLoginNip07),
    route('POST', '/login/nip07/verify', postLoginNip07Verify),
    route('POST', '/logout', postLogout),
    route('GET', '/compose', getCompose),
    route('GET', '/compose/close', getComposeClose),
    route('GET', '/compose/preview', getComposePreview),
    route('GET', '/compose/suggest', getSuggest),
    route('GET', '/compose/poll-option', getPollOption),
    route('POST', '/note', postNote),
    route('POST', '/note/draft', postNoteDraft),
    route('POST', '/note/publish', postNotePublish),
    route('POST', '/picture', postPicture),
    route('POST', '/picture/publish', postPicturePublish),
    route('POST', '/note/private/seal', postPrivateReplySeal),
    route('POST', '/note/private/wrap', postPrivateReplyWrap),
    route('GET', '/note/tick', getNoteTick),
    route('POST', '/note/undo', postNoteUndo),
    route('POST', '/article', postArticle),
    route('POST', '/article/publish', postArticlePublish),
    route('POST', '/draft', postDraft),
    route('POST', '/draft/delete/:id', postDraftDelete),
    route('POST', '/draft/delete/:id/finish', postDraftDeleteFinish),
    route('POST', '/draft/sync/:id/wrap', postDraftSyncWrap),
    route('POST', '/draft/sync/:id/publish', postDraftSyncPublish),
    route('GET', '/comment/form', getCommentForm),
    route('POST', '/comment', postComment),
    route('POST', '/comment/publish', postCommentPublish),
    route('POST', '/upload', postUpload),
    route('POST', '/upload/finish', postUploadFinish),
    route('POST', '/like/:target', postLike),
    route('POST', '/like/:target/publish', postLikePublish),
    route('POST', '/delete/:id', postDelete),
    route('POST', '/delete/:id/confirm', postDeleteConfirm),
    route('POST', '/delete/:id/cancel', postDeleteCancel),
    route('POST', '/delete/:id/publish', postDeletePublish),
    route('GET', '/zap', getZap),
    route('POST', '/zap', postZap),
    route('POST', '/zap/invoice', postZapInvoice),
    route('POST', '/zap/paid', postZapPaid),
    route('POST', '/poll', postPoll),
    route('POST', '/poll/draft', postPollDraft),
    route('POST', '/poll/publish', postPollPublish),
    route('GET', '/poll/:id', getPoll),
    route('POST', '/poll/vote/:pollid', postPollVote),
    route('POST', '/poll/vote/:pollid/publish', postPollVotePublish),
    route('GET', '/cal/ics/:naddr', getCalIcs),
    route('GET', '/cal/rsvp/:naddr', getCalRsvp),
    route('POST', '/cal/rsvp/:naddr', postCalRsvp),
    route('POST', '/cal/rsvp/:naddr/publish', postCalRsvpPublish),
    route('GET', '/profile/edit', getProfileEdit),
    route('POST', '/profile/edit', postProfile),
    route('POST', '/profile/edit/publish', postProfilePublish),
    route('GET', '/u/:npub', getProfile),
    route('GET', '/u/:npub/extras', getProfileExtras),
    route('GET', '/t/:id', getThread),
    route('GET', '/t/:id/private', getThreadPrivate),
    route('POST', '/t/:id/private/seals', postThreadPrivateSeals),
    route('POST', '/t/:id/private/rumors', postThreadPrivateRumors),
    route('GET', '/a/:naddr', getArticle),
    route('GET', '/embed/:id', getEmbed),
    route('GET', '/handlers/:id', getHandlers),
    route('GET', '/settings', getSettings),
    route('POST', '/settings/appearance', postAppearance),
    route('POST', '/settings/privacy', postPrivacy),
    route('POST', '/settings/content-prefs', postContentPrefs),
    route('POST', '/settings/content-filters', postContentFilters),
    route('GET', '/settings/privacy/status', getPrivacyStatus),
    route('POST', '/settings/relays/edit', postRelaysEdit),
    route('POST', '/settings/relays', postRelays),
    route('POST', '/settings/relays/publish', postRelaysPublish),
    route('GET', '/settings/relays/:pane', getRelaysPane), // Relays tab sub-nav (General/DMs/Search/Private)
    route('POST', '/settings/local-relay', postLocalRelay),
    route('POST', '/settings/local-relay/auth', postLocalRelayAuth),
    route('POST', '/settings/local-relay/auth/complete', postLocalRelayAuthComplete),
    route('GET', '/settings/local-relay/status', getLocalRelayStatus),
    route('POST', '/settings/dm-relays/edit', postDmRelaysEdit),
    route('POST', '/settings/dm-relays', postDmRelays),
    route('POST', '/settings/dm-relays/publish', postDmRelaysPublish),
    route('GET', '/settings/relay-score', getRelayScore),
    route('GET', '/settings/backup/export', getBackupExport),
    route('POST', '/settings/backup/import', postBackupImport),
    route('POST', '/settings/backup/restore', postBackupRestore),
    route('POST', '/settings/search/edit', postSearchEdit),
    route('POST', '/settings/search', postSearchSave),
    route('POST', '/settings/media/edit', postMediaEdit),
    route('POST', '/settings/media', postMedia),
    route('POST', '/settings/media/publish', postMediaPublish),
    // One URL per settings tab. Registered AFTER the specific 2-segment GET (/settings/relay-score)
    // so that literal path wins the first-match scan; the 3-segment GETs (/settings/backup/export,
    // /settings/privacy/status) never collide since match() requires an equal segment count. Unknown
    // slugs redirect to /settings inside the handler, so a stray 2-segment GET is harmless.
    route('GET', '/settings/:tab', getSettingsTab),
    route('GET', '/followers', getFollowers),
    route('GET', '/longform', getLongform),
    route('GET', '/timeline/:id', getTimeline),
    route('GET', '/faces', getFaces),
    route('GET', '/relay', getRelay),
    route('GET', '/relay/pick', getRelayPick),
    route('POST', '/relay/favorite', postRelayFavorite),
    route('GET', '/notes/dot', getNotesDot),
    route('GET', '/feed/seen', getFeedSeen),
    route('GET', '/notes/list-prime', getListPrime),
    route('POST', '/notes/list-primed', postListPrimed),
    route('GET', '/bookmarks', getBookmarks),
    route('GET', '/list/:kind/decrypt', getListDecrypt),
    route('POST', '/list/:kind/decrypted', postListDecrypted),
    route('GET', '/drafts', getDrafts),
    route('GET', '/scheduled', getScheduled),
    route('POST', '/scheduled/cancel/:token', postScheduledCancel),
    route('GET', '/drafts/sync', getDraftsSync),
    route('POST', '/drafts/sync/apply', postDraftsSyncApply),
    route('GET', '/muted', getMuted),
    route('GET', '/search', getSearch),
    route('GET', '/notifications', getNotifications),
    route('GET', '/notifications/unread', getNotifUnread),
    route('GET', '/messages', getMessages),
    route('GET', '/messages/requests', getRequests),
    route('GET', '/messages/dot', getMessagesDot),
    route('GET', '/messages/sync', getDmSync),
    route('GET', '/messages/new', getNewMessage),
    route('GET', '/messages/:peer', getDmThread),
    route('GET', '/messages/:peer/sync', getThreadSync),
    route('GET', '/messages/:peer/older', getThreadOlder),
    route('POST', '/messages/sync/seals', postDmSeals),
    route('POST', '/messages/sync/rumors', postDmRumors),
    route('POST', '/messages/sync/legacy', postDmLegacy),
    route('POST', '/messages/read-all', postReadAll),
    route('POST', '/messages/:peer', postSend),
    route('POST', '/messages/:peer/seal', postSendSeal),
    route('POST', '/messages/:peer/wrap', postSendWrap),
    route('GET', '/avatar', getAvatar),
    route('GET', '/media', getMedia),
    route('GET', '/video', getVideoEmbed),
    route('GET', '/yt/card/:id', getYtCard),
    route('GET', '/yt/thumb/:id', getYtThumb),
    route('GET', '/yt/play/:id', getYtPlay),
    route('GET', '/yt/playlist/:list', getYtPlaylistCard),
    route('GET', '/yt/playlist/:list/play', getYtPlaylistPlay),
    route('GET', '/metrics', getMetrics),
    route('GET', '/wallet', getWallet),
    route('POST', '/wallet', postWallet),
    // The nip07 private-toggle chain - registered BEFORE /act/:action/:target so
    // these literal 3-segment paths win over the param route.
    route('POST', '/act/private/dec', postActPrivateDec),
    route('POST', '/act/private/enc', postActPrivateEnc),
    route('POST', '/act/private/sign', postActPrivateSign),
    route('POST', '/act/:action/:target', postAction),
    route('POST', '/act/:action/:target/publish', postActionPublish),
];

function match(method: string, path: string): { handler: Handler; params: Record<string, string> } | null {
    const parts = path.split('/').filter(Boolean);
    for (const r of ROUTES) {
        if (r.method !== method) continue;
        if (r.segs.length !== parts.length) continue;
        const params: Record<string, string> = {};
        let ok = true;
        for (let i = 0; i < r.segs.length; i++) {
            const seg = r.segs[i]!;
            if (seg.startsWith(':')) {
                // A malformed percent-escape (/u/%ZZ) throws here; treat as no-match so
                // dispatch falls through to a clean 404 instead of crashing the handler.
                try { params[seg.slice(1)] = decodeURIComponent(parts[i]!); }
                catch { ok = false; break; }
            } else if (seg !== parts[i]) { ok = false; break; }
        }
        if (ok) return { handler: r.handler, params };
    }
    return null;
}

// --- static assets (public/) ----------------------------------------------

const STATIC: Record<string, { file: string; type: string }> = {
    '/helm.js': { file: 'helm.js', type: 'application/javascript; charset=utf-8' },
    '/hext.js': { file: 'hext.js', type: 'application/javascript; charset=utf-8' },
    '/styles.css': { file: 'styles.css', type: 'text/css; charset=utf-8' },
};

// Conservative, string-safe CSS minify (the JS we serve is already dist-minified, so this
// only ever runs on styles.css). One pass: comment markers and whitespace tightening are
// applied to code segments only - string literals ('...' / "...") are copied verbatim so a
// `content:` value or `url("...")` is never touched. We deliberately do NOT tighten around
// `:` or combinators (`+`/`~`/`>`): the savings are sub-1% gzipped and the risk (breaking
// `calc(100% + 8px)` or `[x] :hover` descendant selectors) is not worth it. Stripping the
// port's ~36KB of prose comments is where ~all the win is: 35.4KB -> 17.2KB gzipped.
function minifyChunk(s: string): string {
    return s
        .replace(/\s+/g, ' ')
        .replace(/\s*([{};])\s*/g, '$1')
        .replace(/\s*,\s*/g, ',')
        .replace(/;}/g, '}');
}
function minifyCss(src: string): string {
    let out = '', code = '', i = 0;
    const n = src.length;
    const flush = () => { out += minifyChunk(code); code = ''; };
    while (i < n) {
        const c = src[i]!;
        if (c === '/' && src[i + 1] === '*') {          // comment -> drop, acts as a separator
            i += 2;
            while (i < n && !(src[i] === '*' && src[i + 1] === '/')) i++;
            i += 2;
            code += ' ';
            continue;
        }
        if (c === '"' || c === "'") {                   // string literal -> copy verbatim
            flush();
            const q = c;
            out += c; i++;
            while (i < n) {
                const ch = src[i]!;
                out += ch; i++;
                if (ch === '\\') { if (i < n) { out += src[i]!; i++; } continue; }
                if (ch === q) break;
            }
            continue;
        }
        code += c; i++;
    }
    flush();
    return out.trim();
}

// Minify (CSS only) + gzip on the fly, cached by mtime: the first request (or the first after
// an edit) builds + caches both buffers; later requests are a Map hit. Keyed by mtime so editing
// styles.css / swapping in a new helm.js is picked up automatically - no stale .min/.gz on disk
// to keep in sync, and no build step (the source stays readable + commented). `no-cache` keeps
// the live-edit-shows-on-reload behavior; `Vary` so a proxy can't cross-serve encodings.
const staticCache = new Map<string, { mtimeMs: number; raw: Buffer; gz: Buffer }>();
async function serveStatic(req: IncomingMessage, res: ServerResponse, path: string): Promise<boolean> {
    const entry = STATIC[path];
    if (!entry) return false;
    try {
        const full = pathJoin(PUBLIC, entry.file);
        const { mtimeMs } = await stat(full);
        let c = staticCache.get(entry.file);
        if (!c || c.mtimeMs !== mtimeMs) {
            const buf = await readFile(full);
            const body = entry.type.startsWith('text/css') ? Buffer.from(minifyCss(buf.toString('utf8'))) : buf;
            c = { mtimeMs, raw: body, gz: gzipSync(body) };
            staticCache.set(entry.file, c);
        }
        // ETag (mtime-keyed, already in hand) gives `no-cache` its validator: without one, every hard
        // load re-downloads all ~37KB of assets in full; with it, an unchanged asset is a 304.
        const etag = `"${entry.file}-${mtimeMs}"`;
        if (req.headers['if-none-match'] === etag) {
            res.writeHead(304, { 'ETag': etag, 'Cache-Control': 'no-cache', 'Vary': 'Accept-Encoding' });
            res.end();
            return true;
        }
        const gzip = (req.headers['accept-encoding'] ?? '').includes('gzip');
        const headers: Record<string, string> = { 'Content-Type': entry.type, 'Cache-Control': 'no-cache', 'Vary': 'Accept-Encoding', 'ETag': etag };
        if (gzip) headers['Content-Encoding'] = 'gzip';
        res.writeHead(200, headers);
        res.end(gzip ? c.gz : c.raw);
        return true;
    } catch {
        return false;
    }
}

// --- request dispatch ------------------------------------------------------

// Reachable WITHOUT a session: only the login page + the login flow itself (which creates the
// session, so it can't require one). Static assets are served above, before routing. EVERYTHING
// else - including the /media, /avatar, /yt/* media proxies - requires a session, so an exposed
// instance (clearnet or .onion) shows a stranger only the login wall: never your data, and never
// your daemon as an open proxy. Default-DENY: a new route is private unless listed here.
const PUBLIC_ROUTES = new Set([
    'GET /login', 'POST /login', 'POST /login/nip07', 'POST /login/nip07/verify',
]);

async function handle(req: IncomingMessage, res: ServerResponse): Promise<void> {
    // A malformed request target (e.g. `//`) makes new URL throw; respond 400 rather
    // than letting the throw escape into the void-dispatched promise below.
    let url: URL;
    try { url = new URL(req.url ?? '/', 'http://localhost'); }
    catch { res.writeHead(400, { 'Content-Type': 'text/plain; charset=utf-8' }); res.end('Bad request'); return; }
    const method = req.method ?? 'GET';
    const path = url.pathname;

    if (method === 'GET' && await serveStatic(req, res, path)) return;

    // Per-request timing for the routed paths (statics already returned). Off by default; set
    // SATORI_REQ_LOG=1 to surface real navigation cost in `docker logs` - which GET /t/ or /u/ is slow.
    if (process.env.SATORI_REQ_LOG) {
        const started = Date.now();
        res.on('finish', () => console.log(`[req] ${method} ${path} ${res.statusCode} ${Date.now() - started}ms`));
    }

    const cookies = parseCookies(req.headers.cookie);
    const ctx: Ctx = {
        req, res, method, path,
        query: url.searchParams,
        params: {},
        cookies,
        secure: (req.socket as { encrypted?: boolean }).encrypted === true
            || String(req.headers['x-forwarded-proto'] ?? '').split(',')[0]!.trim().toLowerCase() === 'https',
        session: getSession(cookies.sid ?? null),
        isPartial: req.headers['h-request'] === 'true',
        hTarget: (req.headers['h-target'] as string | undefined) ?? null,
    };

    const m = match(method, path);
    if (!m) { notFound(ctx); return; }
    ctx.params = m.params;

    // Default-deny gate: no session + not a public (login) route → the login wall. Covers everything,
    // including the media proxies, so they can't be used unauthenticated on an exposed instance.
    if (!isLoggedIn(ctx.session) && !PUBLIC_ROUTES.has(`${method} ${path}`)) {
        redirect(ctx, '/login');
        return;
    }

    try {
        await m.handler(ctx);
    } catch (err) {
        console.error(`[server] ${method} ${path} failed:`, err);
        if (!res.headersSent) {
            res.writeHead(500, { 'Content-Type': 'text/plain; charset=utf-8' });
            res.end('Internal error');
        }
    }
}

const PORT = Number(process.env.PORT ?? 8787);
const HOST = process.env.HOST ?? '127.0.0.1';

// Install .onion→Tor routing (if TOR_SOCKS is set) BEFORE accepting connections,
// so the WebSocket impl is in place before any relay socket opens.
void installTorRouting().finally(() => {
    createServer((req, res) => {
        // Backstop: a synchronous throw before handle()'s own try block (or any stray
        // rejection) must end the response, never become an unhandled rejection that
        // takes down the single-user daemon.
        void handle(req, res).catch((err) => {
            console.error('[server] unhandled:', err);
            if (!res.headersSent) { res.writeHead(500, { 'Content-Type': 'text/plain; charset=utf-8' }); res.end('Internal error'); }
        });
    }).listen(PORT, HOST, () => {
        console.log(`satori → http://${HOST}:${PORT}  (local single-user daemon)`);
        // Access control: adopt an already-logged-in user as owner if the policy is unclaimed (so
        // adding the owner lock doesn't log out an existing self-host), then log the effective mode.
        adoptOwnerIfUnclaimed(persistedPubkeys());
        console.log(`[access] ${accessMode()}`);
        // Prune persisted sessions whose cookies (maxAge 7d) are long dead, so stale bunker
        // transport secrets don't linger at rest forever.
        const pruned = prunePersisted(30 * 24 * 60 * 60 * 1000);
        if (pruned) console.log(`[store] pruned ${pruned} stale session(s)`);
        // Re-publish any undo-window holds that outlived a restart, and start the scheduled-posts
        // sweep - both on one standalone background pool (no session signer → best-effort for
        // auth-only relays). The sweep broadcasts due posts every 30s, restart-safe (re-reads disk).
        const bgPool = new Pool();
        resumeHolds(bgPool);
        startScheduledSweep(bgPool);
    });
});
