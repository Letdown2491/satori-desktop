// Edit-your-own-profile (kind:0). Two signing modes, both keeping the key off the
// server (mirrors note/relay editing): bunker signs + publishes here; nip07 gets a
// sign-request for the kind:0 template + continues at /profile/edit/publish. The
// editable fields are MERGED into the freshly-fetched raw metadata so unknown
// fields (banner, lud06, website the app doesn't show, …) survive a save - like
// Satori's saveProfile/loadMyProfile pair.

import { fetchProfileContent } from '../data/profiles.ts';
import type { Profile } from '../data/profiles.ts';
import { INDEXER_RELAYS, writeRelaysFor } from '../nostr/nip65.ts';
import { profileEditModal, profileEditPage, type ProfileEditCtx } from '../render/profile-edit.ts';
import { profileHeader } from '../render/note.ts';
import { npub } from '../render/util.ts';
import { requireSigned } from '../nip07.ts';
import { requireLogin, chromeFor } from './common.ts';
import { html } from '../html.ts';
import { readForm, redirect, sendPage, sendFragment, sendSignRequest, type Ctx } from '../http.ts';
import type { Session } from '../session.ts';
import { signsOnClient } from '../session.ts';
import { published } from '../actions.ts';
import { putProfile } from '../data/profile-cache.ts';

const KIND_METADATA = 0;
const PLACE_HEADER = { 'H-Reswap': 'outer', 'H-Retarget': '#profile-header' };

const str = (v: unknown): string => (typeof v === 'string' ? v : '');
const cleanAddr = (v: unknown): string | undefined => (typeof v === 'string' && v.includes('@') ? v.trim() : undefined);

/** Where we read/write the profile: your relays ∪ the indexers (so a fresh account
 * with no relay list still resolves + publishes somewhere findable). */
function profileRelays(s: Session & { me: string }): string[] {
    const r = s.myRelays ?? { read: [], write: [] };
    return [...new Set([...r.read, ...r.write, ...INDEXER_RELAYS])];
}
function writeTargets(s: Session & { me: string }): string[] {
    return [...new Set([...writeRelaysFor(s.myRelays), ...INDEXER_RELAYS])];
}

/** The cache Profile derived from full kind:0 content (so the header refreshes). */
function profileFromContent(c: Record<string, unknown>): Profile {
    return {
        name: str(c.display_name) || str(c.name) || undefined,
        display_name: str(c.display_name) || undefined,
        picture: typeof c.picture === 'string' ? c.picture : undefined,
        nip05: cleanAddr(c.nip05),
        about: typeof c.about === 'string' ? c.about : undefined,
        lud16: cleanAddr(c.lud16),
        website: typeof c.website === 'string' && c.website.trim() ? c.website.trim() : undefined,
        banner: typeof c.banner === 'string' && c.banner.trim() ? c.banner.trim() : undefined,
        bot: c.bot === true ? true : undefined,
    };
}

const ctxFromContent = (me: string, c: Record<string, unknown>): ProfileEditCtx => ({
    me, name: str(c.display_name) || str(c.name), about: str(c.about),
    picture: str(c.picture), lud16: str(c.lud16), nip05: str(c.nip05), website: str(c.website), banner: str(c.banner),
});
const ctxFromForm = (me: string, form: URLSearchParams): ProfileEditCtx => ({
    me, name: (form.get('name') ?? '').trim(), about: form.get('about') ?? '',
    picture: (form.get('picture') ?? '').trim(), lud16: (form.get('lud16') ?? '').trim(),
    nip05: (form.get('nip05') ?? '').trim(), website: (form.get('website') ?? '').trim(), banner: (form.get('banner') ?? '').trim(),
});

/** Merge the submitted editable fields into the current raw content (preserving
 * everything else), returning the content to publish. */
function mergeContent(raw0: Record<string, unknown>, form: URLSearchParams): Record<string, unknown> {
    const content = { ...raw0 };
    const set = (key: string, formKey: string) => {
        const v = (form.get(formKey) ?? '').trim();
        if (v) content[key] = v; else delete content[key];
    };
    set('display_name', 'name');
    set('about', 'about');
    set('picture', 'picture');
    set('lud16', 'lud16');
    set('nip05', 'nip05');
    set('website', 'website');
    set('banner', 'banner');
    return content;
}

/** GET /profile/edit - open the editor seeded from current kind:0 metadata. */
export async function getProfileEdit(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const raw0 = await fetchProfileContent(s.pool, profileRelays(s), s.me).catch(() => ({}));
    const c = ctxFromContent(s.me, raw0);
    if (ctx.isPartial && ctx.hTarget === '#modal') { sendFragment(ctx, profileEditModal(c)); return; }
    sendPage(ctx, profileEditPage(c), chromeFor(ctx, s, { active: 'profile', title: 'Edit profile' }));
}

/** POST /profile/edit - merge + publish kind:0 (bunker) / sign-and-continue (nip07). */
export async function postProfile(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const form = await readForm(ctx.req);
    const raw0 = await fetchProfileContent(s.pool, profileRelays(s), s.me).catch(() => ({}));
    const content = mergeContent(raw0, form);
    const template = { kind: KIND_METADATA, created_at: Math.floor(Date.now() / 1000), pubkey: s.me, content: JSON.stringify(content), tags: [] as string[][] };

    if (signsOnClient(s)) { sendSignRequest(ctx, template, '/profile/edit/publish'); return; }

    try {
        const signed = await s.signer!.signEvent(template);
        if (!await published(s, signed, writeTargets(s))) throw new Error('no relay accepted the profile update');
        const p = profileFromContent(content);
        s.profiles.set(s.me, p);
        putProfile(s.me, p); // keep the process-wide profile cache in step (other tabs / post-restart reads)
    } catch (err) {
        const c = ctxFromForm(s.me, form);
        c.status = err instanceof Error ? err.message : 'Could not publish.';
        c.err = true;
        sendPage(ctx, profileEditPage(c), chromeFor(ctx, s, { active: 'profile', title: 'Edit profile' }), 502);
        return;
    }
    redirect(ctx, '/u/' + npub(s.me)); // zero-JS reloads the profile; boosted fetch follows + body-swaps (modal closes)
}

/** POST /profile/edit/publish - nip07 continuation: verify + publish the signed
 * kind:0, refresh the header in place, and clear the modal (OOB). */
export async function postProfilePublish(ctx: Ctx): Promise<void> {
    const s = requireLogin(ctx);
    if (!s) return;
    const signed = await requireSigned(ctx, s.me, KIND_METADATA, 'the signed profile');
    if (!signed) return;
    try {
        if (!await published(s, signed, writeTargets(s))) throw new Error('no relay accepted the profile update');
    } catch (err) {
        sendFragment(ctx, html`<div class="notice error">Couldn’t publish: ${err instanceof Error ? err.message : String(err)}</div>`, {}, 502);
        return;
    }
    try { const p = profileFromContent(JSON.parse(signed.content)); s.profiles.set(s.me, p); putProfile(s.me, p); } catch { /* keep old cache */ }
    sendFragment(
        ctx,
        html`${profileHeader(s.me, s.profiles.get(s.me), s.profiles, s, true)}<div id="modal" h-oob="true"></div>`,
        PLACE_HEADER,
    );
}
