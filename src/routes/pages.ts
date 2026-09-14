// Settings (appearance) + placeholders for the nav destinations not yet built.
// Each placeholder renders in the correct chrome so navigation never 404s and the
// look is complete; the real features arrive in later phases.

import { html } from '../html.ts';
import { profileCacheStats } from '../data/profile-cache.ts';
import { avatarCacheStats } from '../data/avatar-cache.ts';
import { engagementStats } from '../data/engagement-cache.ts';
import { dmScanStats } from '../data/dm-metrics.ts';
import { feedRecoveryStats } from '../data/feed-metrics.ts';
import { requireLogin, chromeFor } from './common.ts';
import { isOwner } from '../access.ts';
import { readAppearance, writeAppearance, parseZapPresets, THEMES, type Theme } from '../theme.ts';
import { prefToggle, PREF_FIELDS } from '../render/settings.ts';
import { readForm, redirect, sendPage, sendFragment, type Ctx } from '../http.ts';

// --- Settings (appearance) -------------------------------------------------
// The settings PAGE + relay editor live in routes/settings.ts; this is just the
// appearance-cookie write that the theme/text forms post to.

export async function postAppearance(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const form = await readForm(ctx.req);
    // Merge ONLY the fields this form actually sent, so a single-field post (e.g. a
    // number field) doesn't clobber theme, and vice versa.
    const next = { ...readAppearance(ctx) };
    if (form.has('theme')) { const t = form.get('theme'); if (t && THEMES.includes(t as Theme)) next.theme = t as Theme; }
    if (form.has('newNotesThreshold')) next.newNotesThreshold = Math.max(1, Math.min(50, Math.floor(Number(form.get('newNotesThreshold')) || 5)));
    // Boolean toggles, handled generically so a new PREF_FIELD never gets silently dropped here.
    for (const f of PREF_FIELDS) if (form.has(f)) next[f] = form.get(f) === '1';
    if (form.has('undoSeconds')) next.undoSeconds = Math.max(1, Math.min(30, Math.floor(Number(form.get('undoSeconds')) || 5)));
    writeAppearance(ctx, next);
    // A reload is needed when the change affects the page BEYOND its own widget and a local swap can't
    // reach it: theme lives on <html>. Other prefs (threshold, media toggle, undo) are self-contained,
    // so a no-reload field gets 204.
    const needsReload = form.has('theme');
    if (ctx.isPartial) {
        if (needsReload) { ctx.res.writeHead(200, { 'H-Refresh': 'true' }); ctx.res.end(); return; }
        // A pref toggle re-renders itself with the flipped state (its form swaps outer).
        for (const f of PREF_FIELDS) {
            if (form.has(f)) { sendFragment(ctx, prefToggle(f, next[f])); return; }
        }
        ctx.res.writeHead(204); ctx.res.end(); // number/upload-server fields: native value already shows
    } else redirect(ctx, '/settings');
}

// --- Wallet ----------------------------------------------------------------
// No spending credential is ever stored server-side (by design). The daemon holds
// no key: WebLN keeps its secret in the extension, and the NWC connection string
// lives only in the browser (hext.js localStorage), never POSTed here. So this page
// is just the secret-free parts: payment status (CSS-revealed via hext.js's
// data-webln / data-nwc flags), the NWC connect/balance seams (data-nwc-*, driven
// entirely client-side), your own lightning address (to receive), and the zap-preset
// prefs the zap dialog reads. The balance shown is read by the browser over NIP-47,
// not by the daemon.

/** GET /metrics - JSON perf counters. Login-gated, and engagement is scoped to the
 * caller (no cross-user aggregation), so it stays sound if ever run multi-tenant. */
export function getMetrics(ctx: Ctx): void {
    const s = requireLogin(ctx);
    if (!s) return;
    // Owner-only: several counters below (dmScans/profileCache/avatarCache/feedRecovery) are process-wide
    // volume metadata, so a trusted-group member must not read instance-wide activity. engagement is per-caller.
    if (!isOwner(s.me)) { ctx.res.writeHead(403, { 'Content-Type': 'text/plain; charset=utf-8' }); ctx.res.end('Forbidden'); return; }
    ctx.res.writeHead(200, { 'Content-Type': 'application/json; charset=utf-8' });
    ctx.res.end(JSON.stringify({ profileCache: profileCacheStats(), avatarCache: avatarCacheStats(), engagement: engagementStats(s.me), dmScans: dmScanStats(), feedRecovery: feedRecoveryStats() }, null, 2));
}

export function getWallet(ctx: Ctx): void {
    const s = requireLogin(ctx);
    if (!s) return;
    const a = readAppearance(ctx);
    const lud16 = s.profiles.get(s.me)?.lud16;
    const content = html`
      <div class="wallet view-pad">
        <!-- Unified Payment method card: WebLN + NWC as peers. Exactly one status chip shows (driven by
             data-webln / data-nwc / data-pay-method). The NWC string is captured + stored by hext.js in this
             browser's localStorage and is NEVER sent to the daemon (the input has no name and posts nowhere;
             Connect/Disconnect/Refresh are data-nwc-* seams). hext.js flips data-nwc / data-pay-method. -->
        <section class="wallet-card">
          <h3>Payment method</h3>
          <div class="pay-status">
            <span class="wallet-chip off pay-state pay-none">No wallet connected · zaps open an invoice in any external wallet</span>
            <span class="wallet-chip on pay-state pay-webln-only">⚡ Paying zaps with WebLN</span>
            <span class="wallet-chip on pay-state pay-nwc-only">⚡ Paying zaps with NWC</span>
            <span class="wallet-chip on pay-state pay-active">⚡ Paying zaps with <span class="pays-w">WebLN</span><span class="pays-n">NWC</span></span>
            <span class="wallet-chip warn pay-state pay-pick">Choose which wallet pays your zaps ↓</span>
          </div>
          <!-- Each method is a row; when BOTH are connected, hext.js shows the radios and CSS turns each row into
               a selectable card (the active payer gets the accent ring). The [data-pay-method] radios live in the
               heads, so picking is co-located with the methods. The balance + Refresh/Disconnect sit OUTSIDE the
               label so tapping them never changes the selection. -->
          <div class="pay-method pmo-webln">
            <label class="pay-method-head">
              <input type="radio" name="paymethod" value="webln" data-pay-method class="pay-radio">
              <span class="pay-method-text">
                <span class="pay-method-title">WebLN extension</span>
                <span class="pay-method-sub">A browser extension like <a href="https://getalby.com" target="_blank" rel="noopener">Alby</a>. The key never leaves it.</span>
              </span>
              <span class="pay-badge ok webln-yes">Detected</span>
            </label>
          </div>
          <div class="pay-method pmo-nwc">
            <label class="pay-method-head">
              <input type="radio" name="paymethod" value="nwc" data-pay-method class="pay-radio">
              <span class="pay-method-text">
                <span class="pay-method-title">Nostr Wallet Connect</span>
                <span class="pay-method-sub">Pay from any NWC wallet, no extension. Stored only in this browser, never sent to the server.</span>
              </span>
              <span class="pay-badge ok nwc-yes">Connected</span>
            </label>
            <div class="nwc-off nwc-connect">
              <input class="wallet-input" type="password" data-nwc-input placeholder="nostr+walletconnect://…" autocomplete="off" spellcheck="false" aria-label="NWC connection string">
              <button type="button" data-nwc-save>Connect</button>
            </div>
            <div class="nwc-detail">
              <div class="nwc-balance">
                <span class="nwc-balance-label">Balance</span>
                <span class="nwc-balance-amt"><span class="bolt">⚡</span> <span data-nwc-balance>…</span></span>
              </div>
              <div class="nwc-actions">
                <button type="button" class="ghost" data-nwc-refresh>Refresh</button>
                <button type="button" class="ghost" data-nwc-clear>Disconnect</button>
              </div>
            </div>
          </div>
        </section>
        <section class="wallet-card">
          <h3>Receive zaps</h3>
          ${lud16
            ? html`<div class="wallet-addr-hero"><span class="bolt">⚡</span><span class="addr">${lud16}</span></div><p class="wallet-hint">Others can zap you at this lightning address.</p>`
            : html`<p class="wallet-hint">No lightning address set. Add a <code>lud16</code> to your <a href="/profile/edit" h-scroll="top instant">profile</a> to receive zaps.</p>`}
        </section>
        <section class="wallet-card">
          <h3>Zap amounts</h3>
          <form class="wallet-presets" action="/wallet" method="post" h-post>
            <input class="wallet-input" type="text" name="presets" value="${a.zapPresets.join(', ')}" placeholder="21, 100, 500, 1000, 5000" autocomplete="off" spellcheck="false">
            <button type="submit">Save</button>
          </form>
          <p class="wallet-hint">The amounts offered in the zap dialog.</p>
        </section>
        <section class="wallet-card">
          <h3>Your keys, your wallet</h3>
          <p class="faint">Satori never holds a spending key — WebLN keeps it in your extension, NWC stays in this browser. Nothing to seize, nothing to leak: your wallet stays yours.</p>
        </section>
      </div>`;
    sendPage(ctx, content, chromeFor(ctx, s, { active: 'wallet', title: 'Wallet' }));
}

export async function postWallet(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const form = await readForm(ctx.req);
    writeAppearance(ctx, { ...readAppearance(ctx), zapPresets: parseZapPresets(form.get('presets') ?? '') });
    redirect(ctx, '/wallet'); // boosted fetch follows the 303 → re-render; zero-JS reloads
}
