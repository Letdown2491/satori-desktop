// The kind-handler registry: the dispatch seam that turns Satori from hardcoded kind-switches into a
// manifest-driven (NATEOAS-shaped) app. Engine-tier - it knows NOTHING about any specific kind or about
// Satori's render code; the app registers handlers at boot (mirroring setPageRenderer in http.ts), and
// the render/route layers dispatch through `renderEvent` instead of branching on `ev.kind`.
//
// A handler owns three things, because that's what the code does per kind today:
//   - render(ev, surface, deps): the SAME kind renders differently per SURFACE (timeline / focused /
//     embed), exactly as noteCard vs focusedNote vs embedPreview do now.
//   - prepare(events, s): kind-specific PREFETCH (poll results for 1068, etc.) so routes stay generic.
//   - actions: the declared control vocabulary (ids), the seam where action templates slot in (Phase 4).
//
// Generic over a Deps type so the registry never imports render/ types (preserves the layering: the
// engine tier stays free of app render). Satori instantiates handlers with its own Deps shape.

import type { NostrEvent } from '../nostr/types.ts';
import type { SafeHtml } from '../html.ts';
import type { Session } from '../session.ts';

// timeline = feed/list row · focused = the anchor at the top of a thread · reader = a full-page view
// (the article reader) · embed = an inline quoted/referenced card.
export type Surface = 'timeline' | 'focused' | 'reader' | 'embed';

/** How an inline REFERENCE to this kind renders (a `nostr:naddr` link inside content): the embed
 * route's `as` tag, the link label, and the canonical in-app path. Absent ⇒ a reference to this kind
 * falls back to a generic external (njump) link. This is the reference/link seam, distinct from the
 * event-card render surfaces - so the content tokenizer stays free of `kind === ...` branches too. */
export interface RefDescriptor { as: string; label: string; path(bech: string): string; }


export interface KindHandler<D = unknown> {
    /** The event kinds this handler claims (e.g. [1, 1068] for notes+polls, [30023] for articles). */
    kinds: readonly number[];
    /** How an inline reference to this kind renders inline in content (optional). */
    ref?: RefDescriptor;
    /** True if this kind earns the full-page reader (`/a/`, markdown/asciidoc body) rather than a focused
     * card. The `/a/` route derives its reader set from this flag, so which kinds are "long-form" stays a
     * per-handler capability, not a literal the route hand-maintains. */
    reader?: boolean;
    /** Render this event for a given surface. The SAME code Satori runs today, relocated not rewritten. */
    render(ev: NostrEvent, surface: Surface, deps: D): SafeHtml;
    /** Optional kind-specific prefetch for a page of events (keeps routes from hardcoding hydration).
     * Called via prepareEvents, grouped by kind. Only fires for the logged-in user (prefetch warms
     * their engagement/reply state), so `s.me` is present. */
    prepare?(events: NostrEvent[], s: Session & { me: string }): Promise<void>;
    /** Declared action ids (the control vocabulary). Wired to templates in Phase 4. */
    actions?: readonly string[];
}

// eslint-disable-next-line @typescript-eslint/no-explicit-any -- heterogeneous handlers share one table
type AnyHandler = KindHandler<any>;

const handlers = new Map<number, AnyHandler>();
let fallback: AnyHandler | null = null;

/** Register a handler for its kinds (app calls this at boot). Last registration for a kind wins. */
export function registerKind<D>(h: KindHandler<D>): void {
    for (const k of h.kinds) handlers.set(k, h as AnyHandler);
}

/** The handler for kinds with no registered handler (the unknown-kind path: njump-style today, a
 * declarative/manifest renderer later). This is the NATEOAS frontier - graceful, not a crash. */
export function registerFallback<D>(h: KindHandler<D>): void {
    fallback = h as AnyHandler;
}

export function handlerFor(kind: number): AnyHandler | undefined {
    return handlers.get(kind) ?? fallback ?? undefined;
}

/** Every kind with a registered (non-fallback) handler - used by the boot audit that checks the content
 * catalog covers everything renderable. */
export function registeredKinds(): number[] {
    return [...handlers.keys()];
}

/** The inline-reference descriptor for a kind, or undefined (→ a generic external link). Uses the
 * REGISTERED handler only (not the fallback): a clean in-app reference link is opt-in per kind. */
export function refFor(kind: number): RefDescriptor | undefined {
    return handlers.get(kind)?.ref;
}

/** The declared action vocabulary (ids, in render order) for a kind, or undefined. Registered
 * handler only (mirrors refFor); the render layer falls back to its built-in default set. This is
 * what makes `handler.actions` LOAD-BEARING: the action row is assembled from the kind's manifest
 * declaration, not a hardcoded list, so a kind affords exactly what it declares. */
export function actionsFor(kind: number): readonly string[] | undefined {
    return handlers.get(kind)?.actions;
}

/** Dispatch: render an event for a surface via its handler (or the fallback). Throws only if neither a
 * handler nor a fallback is registered - a boot wiring error, surfaced loudly rather than silently. */
export function renderEvent<D>(ev: NostrEvent, surface: Surface, deps: D): SafeHtml {
    const h = handlerFor(ev.kind);
    if (!h) throw new Error(`registry: no handler or fallback registered (kind ${ev.kind}, surface ${surface})`);
    return h.render(ev, surface, deps);
}

/** Run each kind's prefetch for a page of MIXED events: group by handler, await each group's prepare.
 * Kinds whose handler declares no prepare are skipped (so poll/picture/unknown rows just don't warm
 * reply-presence, exactly as the old `kind === 1` filter did) - the route stays generic, with no
 * `kind === ...` to choose what to hydrate. The engine owns the grouping; handlers own the fetch. */
export async function prepareEvents(events: NostrEvent[], s: Session & { me: string }): Promise<void> {
    const groups = new Map<AnyHandler, NostrEvent[]>();
    for (const ev of events) {
        const h = handlers.get(ev.kind); // registered handler only; the fallback declares no prepare
        if (!h?.prepare) continue;
        const g = groups.get(h);
        if (g) g.push(ev); else groups.set(h, [ev]);
    }
    await Promise.all([...groups].map(([h, evs]) => h.prepare!(evs, s)));
}
