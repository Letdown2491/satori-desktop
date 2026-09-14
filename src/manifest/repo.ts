// The git repository kind (kind:30617, NIP-34). An addressable repository announcement rendered like the
// other addressable kinds: a card (feed/profile row), a detail page (/a/), a clean preview when embedded,
// and the note-shaped row when reached via /t/. Render lives in render/note.ts (its own repo card shape,
// not the article one); this is the handler. Read-only - patches (1617) and issues (1621) come later.

import type { KindHandler } from './registry.ts';
import { type SatoriDeps, notWired } from './deps.ts';
import { repoRow, repoReader, repoEmbed, ARTICLE_ACTIONS } from '../render/note.ts';
import { articleLikeReplies } from './article.ts';
import { KIND_REPO } from '../nostr/nip34.ts';

export const repoHandler: KindHandler<SatoriDeps> = {
    kinds: [KIND_REPO],
    reader: true, // the full-page /a/ repo detail (clone/web/maintainers), not a markdown reader
    actions: ARTICLE_ACTIONS, // like/zap/bookmark/comment keyed by naddr, like the other addressable kinds
    ref: { as: 'article', label: '↗ repository', path: (bech) => `/a/${bech}` }, // inline naddr → the repo page
    prepare: articleLikeReplies, // NIP-22 reply-presence + replier avatars, shared with articles

    render(ev, surface, d) {
        if (surface === 'timeline') return repoRow(ev, d.profiles, d.s);
        if (surface === 'reader') return repoReader(ev, d.profiles, d.s);
        // A repo has no note-shaped thread body; reached via /t/ it shows the repo row (a valid <li> for the
        // thread list), not an empty focusedNote (repo content is empty). The /a/ route owns the detail page.
        if (surface === 'focused') return repoRow(ev, d.profiles, d.s);
        if (surface === 'embed') return repoEmbed(ev, d.naddr ?? d.bech ?? '', d.profiles);
        return notWired(surface);
    },
};
