// Nest kind-1 replies (NIP-10) and kind-1111 comments (NIP-22) into one parent->children tree. Shared by
// the /t/ note-thread view (renderReplyTree) and the /a/ article comment section (renderNode) so the
// threading LOGIC can't drift between them - only the per-context card rendering differs.

import type { NostrEvent } from './types.ts';
import { replyParent } from './nip10.ts';
import { commentParent, KIND_COMMENT } from './nip22.ts';

export interface TreeNode { event: NostrEvent; children: TreeNode[] }

/** The id of the event a reply/comment hangs off. A NIP-22 comment (kind:1111) names its parent with a
 * lowercase `e` tag (read via commentParent, which skips `q`-quoted events; NIP-22 e-tags carry no marker,
 * so a stray NIP-27 mention e-tag is distinguished only by the parent being emitted first). A parent that's
 * addressable or external (an article coord, an `i` URL) has no event id -> null = a root. Everything else
 * is a NIP-10 reply. */
function threadParent(ev: NostrEvent): string | null {
    if (ev.kind === KIND_COMMENT) { const p = commentParent(ev); return p?.type === 'e' ? p.value : null; }
    return replyParent(ev)?.id ?? null;
}

/** Nest replies/comments into a tree by their parent. `rootId` (the focused event) roots its direct children
 * at the top level; pass undefined when the root isn't an event in the set (e.g. an article addressed by
 * coord). Orphans (parent not in the set) also root, so a missing mid-thread event never hides its
 * descendants. Sorted oldest-first so children read top to bottom. */
export function buildThreadTree(events: NostrEvent[], rootId?: string): TreeNode[] {
    const nodes = new Map<string, TreeNode>(events.map((e) => [e.id, { event: e, children: [] }]));
    const roots: TreeNode[] = [];
    for (const e of [...events].sort((a, b) => a.created_at - b.created_at)) {
        const node = nodes.get(e.id)!;
        const pid = threadParent(e);
        const parent = pid && pid !== rootId ? nodes.get(pid) : undefined;
        if (parent) parent.children.push(node); else roots.push(node);
    }
    return roots;
}
