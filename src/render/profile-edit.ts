// Edit-your-own-profile (kind:0) - Satori's profile-edit modal (src/ui/profile-edit.ts)
// ported to HATEOAS. The route fetches the full current metadata server-side so
// fields the app doesn't render (banner, lud06, …) are PRESERVED on save; the form
// carries only the edited fields and the route re-fetches + merges + republishes.
// helmjs presents it as a modal (#modal); the /profile/edit page is the zero-JS
// baseline. Avatar is pasted as a URL - live upload is deferred (like the article
// cover in 6a), since one-field avatar upload needs the multipart-attach plumbing.

import { html, type SafeHtml } from '../html.ts';
import { avatar } from './util.ts';
import { modalClose } from './compose.ts';

export interface ProfileEditCtx {
    me: string;
    name: string;
    about: string;
    picture: string;
    lud16: string;
    nip05: string;
    website: string;
    banner: string;
    status?: string;
    err?: boolean;
}

function field(label: string, control: SafeHtml): SafeHtml {
    return html`<label class="field"><span class="field-label">${label}</span>${control}</label>`;
}
function textField(name: string, value: string, placeholder: string): SafeHtml {
    return html`<input type="text" class="field-input" name="${name}" value="${value}" placeholder="${placeholder}" autocomplete="off" spellcheck="false">`;
}

/** The editor body (avatar + fields + save), shared by the modal + the page. The
 * form is boosted (no h-target) so a successful save's 303 → /u/<me> body-swaps -
 * closing the modal and showing the refreshed profile (zero-JS just navigates). */
export function profileEditForm(c: ProfileEditCtx): SafeHtml {
    return html`
      <form class="profile-edit-form" action="/profile/edit" method="post" h-post>
        <div class="profile-edit-body">
          <div class="profile-edit-avatar">${avatar(c.me, c.picture || undefined, 'lg')}</div>
          ${field('Name', textField('name', c.name, 'Your name'))}
          ${field('Bio', html`<textarea class="field-input bio-input" name="about" rows="3" placeholder="A short bio">${c.about}</textarea>`)}
          ${field('Picture URL', textField('picture', c.picture, 'https://…'))}
          ${field('Banner URL', textField('banner', c.banner, 'https://… (wide image)'))}
          ${field('Lightning address', textField('lud16', c.lud16, 'you@walletofsatoshi.com'))}
          ${field('NIP-05 (verified name)', textField('nip05', c.nip05, 'you@domain.com'))}
          ${field('Website', textField('website', c.website, 'https://…'))}
        </div>
        <div class="modal-foot">
          ${c.status ? html`<span class="compose-status ${c.err ? 'err' : ''}">${c.status}</span>` : html`<span class="compose-status"></span>`}
          <button type="submit" class="busy-btn"><span class="btn-label">Save profile</span><span class="btn-busy">Saving…</span></button>
        </div>
      </form>`;
}

/** helmjs modal presentation (mounted into #modal). The ✕ clears #modal. */
export function profileEditModal(c: ProfileEditCtx): SafeHtml {
    return html`
      <div class="modal-overlay" id="profile-edit-modal">
        <div class="modal">
          <div class="modal-head"><span class="page-title">Edit profile</span>${modalClose()}</div>
          ${profileEditForm(c)}
        </div>
      </div>`;
}

/** Zero-JS baseline: the editor as a full page (no overlay). */
export function profileEditPage(c: ProfileEditCtx): SafeHtml {
    return html`<div class="view-pad"><div class="modal-head"><span class="page-title">Edit profile</span></div>${profileEditForm(c)}</div>`;
}
