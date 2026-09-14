// Zap modal (Satori's zap UI, adapted to HATEOAS): a recipient row, amount (a
// custom field + preset submit buttons), comment, and privacy. Submitting resolves
// the invoice; since no wallet credential lives on the server, the result is the
// bolt11 + a `lightning:` link to pay in any external wallet.

import { html, type SafeHtml } from '../html.ts';
import { avatar, displayName } from './util.ts';
import { modalClose } from './compose.ts';
import { DEFAULT_ZAP_PRESETS } from '../theme.ts';
import type { Session } from '../session.ts';

export const ZAP_DEFAULT = 100;
export const ZAP_MAX = 1_000_000;

export interface ZapCtx { recipient: string; lud16: string; eventId?: string; naddr?: string; error?: string; presets?: number[] }

/** The compose-style modal wrapper (overlay + close), reused for both the form
 * and the resolved-invoice view. */
function wrap(head: SafeHtml, body: SafeHtml): SafeHtml {
    return html`
      <div class="modal-overlay" id="zap-modal">
        <div class="modal">
          <div class="modal-head zap-head">${head}${modalClose()}</div>
          ${body}
        </div>
      </div>`;
}

export function zapModal(s: Session, c: ZapCtx): SafeHtml {
    const head = html`<div class="zap-recipient">${avatar(c.recipient, s.profiles.get(c.recipient)?.picture, 'sm')}
        <div class="zap-recipient-id"><span class="zap-recipient-name">${displayName(c.recipient, s.profiles)}</span><span class="zap-recipient-addr">${c.lud16}</span></div></div>`;
    // The hero pre-fills with the FIRST preset (the user's top choice), not a fixed 100.
    const presets = c.presets?.length ? c.presets : DEFAULT_ZAP_PRESETS;
    const defaultSats = presets[0] ?? ZAP_DEFAULT;
    return wrap(head, html`
      ${c.error ? html`<div class="notice error">${c.error}</div>` : null}
      <form class="zap-form" action="/zap" method="post" h-post h-target="#modal" h-swap="inner">
        <input type="hidden" name="recipient" value="${c.recipient}">
        <input type="hidden" name="lud16" value="${c.lud16}">
        ${c.eventId ? html`<input type="hidden" name="event" value="${c.eventId}">` : null}
        ${c.naddr ? html`<input type="hidden" name="naddr" value="${c.naddr}">` : null}
        <div class="zap-hero-row"><input class="zap-hero" id="zap-hero" type="number" name="sats" min="1" max="${ZAP_MAX}" value="${defaultSats}" inputmode="numeric"><span class="zap-unit">sats</span></div>
        <!-- Presets SET the amount (h-insert replaces the hero value); they must be
             type="button" so they don't submit - only Zap submits. -->
        <div class="zap-presets">${presets.map((v) => html`<button type="button" class="zap-preset" h-insert="${v}" h-insert-target="#zap-hero" h-insert-replace="^.*$">${v.toLocaleString()}</button>`)}</div>
        <input class="zap-comment" id="zap-comment" type="text" name="comment" placeholder="Message (optional)" maxlength="200" autocomplete="off"
          h-get="/compose/suggest?target=%23zap-comment&emoji=1" h-trigger="input debounce:150" h-include="#zap-comment" h-busy="false"
          h-selection="" h-target="#zap-suggest" h-swap="inner" h-push-url="false" h-combobox="#zap-suggest">
        <div id="zap-suggest" class="mention-box" role="listbox" aria-label="Emoji suggestions"></div>
        <div class="zap-privacy">
          <label class="zap-priv"><input type="radio" name="privacy" value="public" checked> Public</label>
          <label class="zap-priv"><input type="radio" name="privacy" value="anonymous"> Anonymous</label>
        </div>
        <div class="modal-foot"><button type="submit" class="zap-go">Zap ⚡</button></div>
      </form>`);
}

/** Shown after a WebLN one-tap payment succeeds (the /zap/paid continuation): a bottom
 * auto-fading toast (no dismiss button - it fades itself via CSS). Swapped into #modal,
 * which both closes the zap dialog and shows the toast; the zap glyph also fills in the
 * timeline (OOB). The calm confirmation - you zapped, it tells you, it gets out of the way. */
export function zappedToast(s: Session, recipient: string, sats: number): SafeHtml {
    return html`<div class="toast zap-toast" role="status" aria-live="polite">⚡ Zapped ${sats.toLocaleString()} sats to ${displayName(recipient, s.profiles)}</div>`;
}

export function invoiceView(s: Session, recipient: string, sats: number, invoice: string): SafeHtml {
    return wrap(html`<span>Zap ${sats.toLocaleString()} sats to ${displayName(recipient, s.profiles)}</span>`, html`
      <p class="zap-invoice-help">Pay this invoice in your Lightning wallet:</p>
      <a class="zap-pay-link" href="lightning:${invoice}">⚡ Open in wallet</a>
      <textarea class="zap-invoice" readonly rows="4" aria-label="Lightning invoice">${invoice}</textarea>
      <p class="signin-help">Select the invoice to copy it, or tap “Open in wallet”. The key never reaches this server, and no wallet is connected here.</p>`);
}
