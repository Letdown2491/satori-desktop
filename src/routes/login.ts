// Login. Two paths, both keeping the key off the server:
//   NIP-46 bunker - the server is the NIP-46 client; the bunker signs. Works
//     with JS off (the zero-JS baseline). connect() may block on out-of-band
//     approval (an auth_url), surfaced as an "approve in your signer" page.
//   NIP-07 extension - JS-only enhancement. The browser extension proves the
//     pubkey by signing a server challenge (the nip07-hateoas sign-and-resubmit
//     primitive); the server holds only the pubkey and publishes signed events.

import { Pool } from '../data/pool.ts';
import { BunkerSigner } from '../data/signer.ts';
import { fetchMyRelays } from '../data/relays.ts';
import { createSession, createNip07Session, destroySession, persistSession, type Session } from '../session.ts';
import { accessAllows } from '../access.ts';
import { clearDmCache, prewarmDms } from '../data/dms.ts';
import { dmPrewarm } from '../render/dms.ts';
import { clearNip07DmCache } from '../data/dms-nip07.ts';
import { clearUserEmoji } from '../data/emoji-sets.ts';
import { html, type SafeHtml } from '../html.ts';
import { wordmark } from '../render/layout.ts';
import { issueChallenge, buildChallengeEvent, verifyChallenge } from '../nip07.ts';
import { readForm, readJson, redirect, sendPage, sendFragment, sendSignRequest, setCookie, type Ctx } from '../http.ts';
import { guestChrome, ensureProfiles, LAND_ON_FEED } from './common.ts';
import { feedDocument } from './feed.ts';

const SID = 'sid';
// How long POST /login waits for either connect-success or an auth_url before it
// gives the browser a response (the connect promise keeps running regardless).
const LOGIN_WAIT_MS = 1500;

/** Once the bunker connect resolves: resolve the user pubkey + their NIP-65 relays
 * and arm the pool for NIP-42 AUTH on the user's own relays. */
/** This instance's access policy denied the pubkey (private instance, not the owner / not allowlisted). */
const NOT_AUTHORIZED = 'This Satori instance is private. Your account is not authorized to sign in here.';

async function finishBunkerLogin(s: Session, signer: BunkerSigner): Promise<void> {
    const me = await signer.getUserPubkey();
    if (!accessAllows(me)) { try { signer.logout(); } catch { /* best effort */ } throw new Error(NOT_AUTHORIZED); }
    s.me = me;
    const relays = await fetchMyRelays(s.pool, me);
    s.myRelays = relays;
    s.pool.setAuth((tmpl) => signer.signEvent(tmpl), [...relays.write, ...relays.read]);
    await ensureProfiles(s, [me]); // so the chrome shows your name + avatar
}

function loginForm(error?: string | null): SafeHtml {
    return html`
      <div class="signin">
        ${wordmark()}
        <p class="signin-intro">Nostr is an open social network where your identity is yours to keep,
        with no company in the middle. Satori is a calm way to use it, built to respect your attention
        instead of farming it.</p>
        <p class="sub">Sign in with a Nostr signer. Your key never touches Satori.</p>
        <!-- NIP-07 extension first (Satori's order), revealed by nip07-hateoas when
             window.nostr is present (it sets <html data-nip07>). JS-only; the bunker
             form below is the zero-JS baseline. -->
        <div data-nip07-only>
          <form class="ext-form" action="/login/nip07" method="post" h-post>
            <button class="ext-signin" type="submit">Sign in with extension</button>
          </form>
          <div class="signin-or">or paste a bunker URI</div>
        </div>
        <!-- h-post is progressive: a plain POST with JS off (the zero-JS baseline), boosted with
             JS so the Connect button shows a "Connecting…" state while the bunker connect runs. -->
        <form action="/login" method="post" h-post>
          <input name="uri" type="text" placeholder="bunker://…" autocomplete="off" spellcheck="false" required>
          <button class="connect-btn busy-btn" type="submit"><span class="btn-label">Connect</span><span class="btn-busy">Connecting…</span></button>
        </form>
        ${error ? html`<p class="status"><span class="err">${error}</span></p>` : null}
        <p class="signin-help">New to Nostr? Get a signer:
          <a href="https://getalby.com" target="_blank" rel="noopener">Alby</a> (extension),
          <a href="https://github.com/greenart7c3/Amber" target="_blank" rel="noopener">Amber</a> (Android), or
          <a href="https://github.com/Letdown2491/signet" target="_blank" rel="noopener">Signet</a> (self-hosted bunker).</p>
      </div>`;
}

function approvePage(authUrl: string): SafeHtml {
    return html`
      <div class="signin">
        ${wordmark()}
        <p class="signin-intro">Your signer needs you to approve this connection.</p>
        <a class="ext-signin ghost" href="${authUrl}" target="_blank" rel="noopener noreferrer">Open your signer to approve →</a>
        <p class="signin-help"><a href="/login">I've approved, continue →</a></p>
      </div>`;
}

function waitingPage(): SafeHtml {
    return html`
      <div class="signin">
        ${wordmark()}
        <p class="signin-intro">Waiting for your signer… approve the connection request in your bunker app.</p>
        <p class="signin-help"><a href="/login">Continue →</a></p>
      </div>`;
}

/** GET /login - form, or the approve/waiting page when a connect is in flight. */
export function getLogin(ctx: Ctx): void {
    const s = ctx.session;
    if (s?.me) { redirect(ctx, '/'); return; }
    let content: SafeHtml;
    if (s?.authUrl && s.connecting) content = approvePage(s.authUrl);
    else if (s?.connecting) content = waitingPage();
    else content = loginForm(s?.error);
    sendPage(ctx, content, guestChrome(ctx, { title: 'Sign in' }));
}

/** POST /login - start the bunker connect; respond with feed / approve / waiting. */
export async function postLogin(ctx: Ctx): Promise<void> {
    const form = await readForm(ctx.req);
    const uri = (form.get('uri') ?? '').trim();
    if (!uri) { sendPage(ctx, loginForm('Paste a bunker:// connection string.'), guestChrome(ctx, { title: 'Sign in' }), 400); return; }

    const pool = new Pool();
    const signer = new BunkerSigner(pool);
    const session = createSession(signer, pool);
    setCookie(ctx, SID, session.id, { maxAge: 60 * 60 * 24 * 7 });
    ctx.session = session;

    signer.onAuthUrl = (url) => { session.authUrl = url; };
    session.connecting = signer.connect(uri)
        .then(() => finishBunkerLogin(session, signer))
        .then(() => { persistSession(session); session.connecting = null; session.authUrl = null; prewarmDms(session); })
        .catch((err: unknown) => {
            session.error = err instanceof Error ? err.message : String(err);
            session.connecting = null;
        });

    // Give the browser a response without waiting out a full 120s approval window.
    await Promise.race([session.connecting, new Promise((r) => setTimeout(r, LOGIN_WAIT_MS))]);

    // Success: no-JS gets a 303 to the feed; a boosted (h-post) request gets the feed swapped in
    // with the URL set to / (a bare 303 would land the feed body under the /login URL).
    if (session.me) {
        if (ctx.isPartial) { sendFragment(ctx, await feedDocument(ctx, session as Session & { me: string }), LAND_ON_FEED); return; }
        redirect(ctx, '/'); return;
    }
    // error / approve / waiting all use sendPage, which already retargets the <body> on a 4xx and
    // boosts cleanly on a 2xx, so they need no isPartial branch here.
    if (session.error) { sendPage(ctx, loginForm(session.error), guestChrome(ctx, { title: 'Sign in' }), 502); return; }
    if (session.authUrl) { sendPage(ctx, approvePage(session.authUrl), guestChrome(ctx, { title: 'Approve' })); return; }
    sendPage(ctx, waitingPage(), guestChrome(ctx, { title: 'Connecting' }));
}

// --- NIP-07 (extension) login ----------------------------------------------

/** POST /login/nip07 - issue a single-use challenge for the extension to sign. */
export function postLoginNip07(ctx: Ctx): void {
    const nonce = issueChallenge();
    const base = `http://${ctx.req.headers.host ?? 'localhost'}`;
    const template = buildChallengeEvent(nonce, `${base}/login/nip07/verify`);
    sendSignRequest(ctx, template, '/login/nip07/verify');
}

/** POST /login/nip07/verify - verify the signed challenge, open a nip07 session. */
export async function postLoginNip07Verify(ctx: Ctx): Promise<void> {
    const signed = await readJson(ctx.req).catch(() => null);
    const pubkey = verifyChallenge(signed);
    if (!pubkey) {
        sendFragment(ctx, html`<div class="notice error">Sign-in verification failed. Please try again.</div>`, {}, 401);
        return;
    }
    if (!accessAllows(pubkey)) {
        sendFragment(ctx, html`<div class="notice error">${NOT_AUTHORIZED}</div>`, {}, 403);
        return;
    }

    const pool = new Pool();
    const session = createNip07Session(pool, pubkey);
    setCookie(ctx, SID, session.id, { maxAge: 60 * 60 * 24 * 7 });
    ctx.session = session;
    session.myRelays = await fetchMyRelays(pool, pubkey).catch(() => null);
    persistSession(session); // survive restarts (buildFeed below loads the chrome profile)

    // Land on the feed via a swap (the seam doesn't follow redirect headers). The hidden
    // dmPrewarm trigger warms the DM cache in the background (nip07 can't be warmed server-
    // side), so opening Messages later is instant - at the cost of one decrypt prompt now.
    sendFragment(ctx, await feedDocument(ctx, session as Session & { me: string }, dmPrewarm()), LAND_ON_FEED);
}

/** POST /logout - best-effort signer logout, drop the session + cookie. */
export function postLogout(ctx: Ctx): void {
    const me = ctx.session?.me; // scope the cache wipes to the departing account - never evict another signed-in user's
    if (ctx.session) destroySession(ctx.session.id);
    if (me) {
        clearDmCache(me);      // this account's plaintext DMs at rest (never let the next account read them)
        clearNip07DmCache(me); // and its nip07 in-memory decrypt state
        clearUserEmoji(me);    // the next account must not inherit this account's custom-emoji set
    }
    setCookie(ctx, SID, '', { maxAge: 0 });
    redirect(ctx, '/login');
}
