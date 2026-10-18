# Satori Desktop — Design & Build Spec

Native Linux desktop rewrite of Satori, a Nostr client, in Rust. The UI toolkit is
GPUI (Zed) per D13; the GTK4/libadwaita app in `crates/app` is the working
implementation being ported (§9.1). This document is the
build spec: principles → decisions → architecture → milestones. The TypeScript app in
`src/` + `public/` is the frozen reference implementation; its behavior documents *what*
the product does, not *how* (the how was shaped by the browser, a constraint the desktop
app does not have).

**Status:** living doc. Decisions in §8 are settled unless reopened with rationale.

---

## 1. Principles

Satori is a Nostr client that strictly follows cypherpunk ideology, HATEOAS principles,
and Buddhist/Taoist philosophy. These are not decoration — each one dictates architecture.

### 1.1 Cypherpunk — privacy as default, verification as law

- **Key custody**: the app never requires your private key. Primary sign-in is a NIP-46
  bunker. A local key is an explicit opt-in, sealed in the Secret Service, never in a
  plain config file.
- **Verify everything locally**: every event (including zap receipts, gift wraps, seals,
  deletions) is signature-checked client-side. Relays are untrusted pipes.
- **No telemetry. No analytics. No counts.** Nothing about the user leaves the machine
  except signed Nostr events to the user's chosen relays.
- **SSRF discipline on every fetched URL** (scheme + IP screening, DNS-rebinding-safe
  resolution, Tor-aware fetch paths). Ported from `src/ssrf.ts`.
- **No remote code execution surface**: no HTML engine, no JS engine. Images decode in
  glycin's sandbox. SVG is never rendered as a trusted format.
- **State at rest**: event cache in LMDB, app state in JSON files (0600). Decrypted DMs
  stay in memory by default — nothing decrypts to disk without an explicit user opt-in.
- **Tor as a mode, not a bolt-on**: per-host circuit isolation, fail-closed strict mode.

### 1.2 HATEOAS — the event stream is the state, the UI is a projection

The web app's HATEOAS core was never the HTTP seam; it was two patterns that port
directly:

1. **State lives in data, not widgets.** Views are pure projections of
   `(local event store ∪ relay subscriptions)`. An action mutates state and the UI
   re-renders from it — the native analogue of "an action returns new HTML."
   Optimistic updates are state mutations with rollback, not CSS class toggles.
2. **Affordances are declared by data.** The manifest registry
   (`src/manifest/registry.ts`) — kind → handler with a declared *action vocabulary* and
   named surfaces — is the app's affordance-discovery mechanism. It becomes a Rust trait
   (§6.1) and is the spine of the UI. Anything the UI can do to an event is listed in its
   handler's vocabulary; unknown kinds degrade honestly (NIP-31 `alt`, no masquerading).

helm.js itself dies. Its *semantics* survive as native idioms: fragment swap → view
update; `h-trigger: intersect` → lazy-load signal; `every Ns` polling dots → GLib
timeouts; `h-optimistic` → mutate-then-reconcile.

### 1.3 The uncarved block — design for exit, leave out what can be left out

- **Feeds end.** The caught-up ensō clearing with a Zen quote, deliberate "view older"
  batches, no infinite refill, no algorithms, no read-state anxiety mechanics.
- **The undo window is the breath before speech**: posts are held N seconds (default 5,
  max 30) before publishing, restart-safe, with a server-side backstop commit.
- **Quiet by default**: reactions off, no like/repost counters, dots instead of badges,
  instant scrolls, `prefers-reduced-motion` honored.
- **Sumi-e aesthetic** (§5): ink on washi, vermilion only for seal moments.
- **Leave things out**: no wallet in v1, no NIP-07 (browser-only), no multi-user daemon,
  no HTTP API, no SPA. Every feature must earn its place.

---

## 2. What carries over, what dies, what is re-expressed

| Web app layer | Fate in desktop |
|---|---|
| Relay pool policies: outbox routing (NIP-65 set-cover ≤70 relays, ≤2/author), adaptive budgets, latency EWMA profiles, read-dead detection, seen-on memory, NIP-42 auth, NIP-40 expiry, NIP-09 tombstones | **Port** (core crate; nostr-sdk primitives underneath) |
| Manifest registry + kind handlers + action vocabulary + surfaces | **Port as `KindHandler` trait** (§6.1) |
| All content cards (note, picture, video, article, poll, calendar, classified, podcast, highlight, wiki, repo, comment, custom NIP, fallback) | **Port** as GTK widgets |
| Undo window, scheduled sweep, drafts + NIP-37 sync, filters, content prefs, list backup, bookmark/mute/pin/follow, NIP-17 DMs, NIP-50 search, NIP-11 relay info/browse, Blossom upload, media proxy + NIP-B7 hash healing, SSRF guards, Tor modes | **Port** |
| Sumi-e design language | **Port** as GTK CSS provider (§5) |
| NIP-46 bunker client | **Use nostr-connect crate** (nostr-sdk) |
| NWC (NIP-47) | **Deferred** with wallet (nostr-sdk `nwc` crate when built) |
| NIP-07 / hext.js / sign-and-resubmit seam / batch chains / `H-Nostr-Sign` protocol | **Deleted** — existed only because the key lived in the browser |
| WebLN payment round-trips | **Deleted** (wallet deferred) |
| Session cookies, read-state cookies, appearance cookie | **Re-expressed** as local state (GSettings/JSON) |
| Zero-JS fallbacks (hidden-checkbox reveals, `:target` lightbox, `<details>` menus), CSS hacks | **Re-expressed** as native state/widgets |
| helm.js, HTTP server, HTML templating, route table | **Deleted** |
| `.data/*.json` app-state files | **Port semantics** (0600 JSON, debounced flush, restart-safe), new location (§3.3) |
| The "local relay" (private relay routing policy) | **Port later** (M5; it is config policy, not code) |

---

## 3. Architecture

### 3.1 Workspace

```
satori/
  Cargo.toml                # workspace
  crates/
    core/                   # satori-core: nostr domain. NO gtk dependency.
      src/
        pool.rs             # relay pool glue: outbox routing, budgets, auth, seen-on
        signer.rs           # bunker (nostr-connect) / local-key (Secret Service)
        dms.rs              # NIP-17: unwrap, routing, read state, request triage
        store.rs            # event cache (nostr-lmdb) + tombstones + expiry
        appstate.rs         # JSON state files: prefs, drafts, holds, scheduled, filters
        publish.rs          # tag building (NIP-10/22), relay targets, delivery report
        engagement.rs       # own-likes/reposts/replies/zapped sets
        notifications.rs    # notification stream assembly
        media.rs            # SSRF-guarded fetcher, media proxy, NIP-B7 healing
        tor.rs              # SOCKS transport, per-host isolation, privacy modes
        nip/                # thin NIPs not in nostr-sdk: 24,31,34,37,52,54,68,71,84,89,99,F4
        kinds.rs            # KindHandler trait + registry (§6.1)
    app/                    # satori: the GTK binary. Depends on core; core never sees GTK.
      src/
        main.rs             # AdwApplication, tokio↔GLib bridge
        theme/              # sumi-e.css (light), sumi-e-dark.css, fonts
        views/              # feed, thread, profile, article, messages, search,
                            # notifications, bookmarks, drafts, settings, relay, login
        widgets/            # note card, composer, media gallery, lightbox, ensō, dots
  packaging/
    flatpak/                # manifest, metainfo, icons, screenshots
  Containerfile.dev         # podman dev image (host is bootc; GTK headers live here)
```

### 3.2 Async model

- **tokio** runtime owns all I/O (relays, HTTP, Tor).
- **GLib MainContext** owns all UI. Bridge: UI submits commands via an async channel;
  tokio tasks send results back through a channel drained by `MainContext` — UI code
  never blocks, network code never touches widgets.
- State: a single `Core` handle (`Arc`) per profile exposing typed observables; views
  subscribe, re-render as projections. No widget-owned truth.

### 3.3 Storage

Flatpak-safe XDG paths (`~/.local/share/satori/` inside the sandbox):

- `events/` — LMDB event store (nostr-lmdb): the projection source of truth.
- `*.json` — app state, 0600, debounced flush + flush-on-exit (ports `json-store.ts`):
  `prefs.json`, `drafts.json`, `holds.json`, `scheduled.json`, `filters.json`,
  `read-state.json`, `dm-read.json`, `relays-cache.json`, `seen-relays.json`,
  `relay-latency.json`, `relay-info.json`, `relay-favorites.json`, `engagement.json`,
  `deletions.json`, `profiles.json`.
- `media-cache/` — avatar/media blob cache (LRU, size-capped), keyed by sha256.
- Secrets: bunker transport secret + local nsec (opt-in) in the **Secret Service**
  (`keyring` crate; Flatpak permission `--talk-name=org.freedesktop.secrets`). File
  fallback 0600 only if the service is absent.

**Decision:** decrypted DMs are memory-only (LRU ~8000 entries). No plaintext DMs on
disk, unlike the web app's bunker path.

### 3.4 Relay pool (port of `pool.ts` + `nip65.ts` semantics, on nostr-sdk)

- Bootstrap indexers for discovery; NIP-65 outbox routing via quality-weighted greedy
  set-cover (≤70 sockets, ≤2 write relays/author, latency profile weights).
- Deadline-not-completeness queries (`maxWait` caps, quiet-window early-resolve for
  redundant reads; doubled budgets on Tor).
- Suspend watchdog: wall-clock gap detection recycles sockets after laptop resume.
- NIP-42 auto-auth for own relays only; stranger-relay auth opt-out.
- NIP-09 tombstones + NIP-40 expiry enforced at the pool chokepoint.
- seen-on memory persisted per-author (≤6 relays, LRU 8000).

### 3.5 Tor (bundled tor via SOCKS)

- Flatpak manifest bundles **C tor** as a module; the app launches it as a child process
  (SOCKS on a sandbox-local port) when Tor mode is enabled. Not a system service.
- Per-host circuit isolation via unique SOCKS usernames (C tor `IsolateSOCKSAuth`) —
  same trick as `ws-tor.ts`.
- Privacy modes port as-is: `off` / `balanced` (5s Tor attempt, fail-open) / `strict`
  (fail-closed, Tor-only). `.onion` relays always tunnel. Transport behind a trait so
  arti can replace tor later without UI changes.

---

## 4. Security posture (checklist)

- [ ] All inbound events signature-verified (sdk) + gift-wrap seals verified (NIP-59).
- [ ] Zap receipts: schnorr-verify embedded request, spoof-proof anon fallback.
- [ ] SSRF guard on every outbound URL: scheme allowlist, IP-literal rejection,
      DNS-rebinding-safe resolution on direct paths, per-redirect re-checks, byte caps,
      wall-clock killers.
- [ ] Media: decode via glycin sandbox; SVG never trusted; aspect-ratio reserved;
      NIP-B7 sha256 verification before cache.
- [ ] Secrets only in Secret Service (or 0600 file fallback); no key material in logs,
      core dumps off by default (`RUST_BACKTIME` n/a — no panic logs with secrets).
- [ ] No telemetry, no update phone-home (Flatpak updates only).
- [ ] All state files 0600 under user-owned XDG dir.

---

## 5. UI design language ("sumi-e ink on washi")

### 5.1 Tokens (GTK CSS `@define-color`, both themes)

| Token | Light (`sumi-e`) | Dark (`sumi-e-dark`) |
|---|---|---|
| bg (washi) | `#e3dac4` | `#15140f` |
| panel | `#d9cfb5` | `#1e1c16` |
| border (hairline) | `#c9bd9f` | `#322e25` |
| text (ink) | `#191714` | `#e9e3d6` |
| muted | `#6e695f` | `#8b8275` |
| accent (vermilion seal) | `#b23a26` | `#df6a4f` |
| ok | `#6f7d5e` | `#8a9a7b` |
| warn | `#9a7320` | `#cfa14e` |
| err | `#9e2b1c` | `#d9543a` |
| live-dot | `#4caf6a` (theme-independent) | same |

Conventions: hairline 1px borders at ~55% opacity between notes; radius ladder
inputs 8px / media 10px / cards 12px / modals 14px / pills 999px; accent reserved for
seal moments (primary action, active states, ensō); links are ink-colored, underlined,
not accent-colored. Hover washes: `text @ 5%` alpha.

### 5.2 Typography

- **Display serif** (h1, article titles, blockquotes, DM peer names, empty-state quotes):
  bundle an OFL serif (default choice: **Source Serif 4**) — the web stack
  (Iowan/Palatino/Georgia) does not exist on Linux.
- **UI text**: system sans (Adwaita Sans), 15px body, 1.6 line-height; pre-wrap semantics
  for note bodies.
- **Monospace**: Adwaita Mono for npubs, invoices, clone URLs.

### 5.3 Shell

- Single 720px reading column, centered, on the washi background.
- Custom header bar (`AdwToolbarView` + `GtkHeaderBar` with custom center widget):
  home+bell left, feed switcher center (popover: Following, timelines, Browse a relay),
  search + avatar menu right. Avatar shows a privacy shield when Tor mode is on.
- Compose FAB bottom-right; scroll-top button above it (revealed past ~800px).
- Views map: feed/thread/profile/article → navigation stack (`AdwNavigationView`);
  modals (compose, zap placeholder, edit profile, new message) → `AdwDialog`;
  settings → `AdwPreferencesWindow`-style custom page with the 5 tabs
  (General, Backup, Content, Privacy, Relays — Relays keeps 4 sub-panes).

### 5.4 Components (web → GTK)

| Web component | GTK implementation |
|---|---|
| Note card (avatar, head row, body, action glyphs, reply-faces) | Custom widget: `GtkBox`; body = label runs + child media widgets; action row = flat icon buttons, no counts |
| Feed (infinite scroll, sentinel, inkfade, caught-up ensō) | `Gtk.ListView` + factory (built-in recycling ≫ `content-visibility`); bottom-edge signal loads a deliberate older batch; ensō row terminates the list |
| Note body rich content | Feed cards: `GtkLabel` runs + media rows (lightweight). Thread/reader: read-only `Gtk.TextView` (selection, links). Perf-check early (M1) |
| Lightbox (`:target` CSS) | `Gtk.Overlay` blackout + swipe paging; trivial as state |
| Gallery (scroll-snap tiles) | `Gtk.ScrolledWindow` horizontal + snap via `Gtk.ScrolledWindow` policy/custom settle |
| Content warning blur | Stack of normal + blurred snapshot (`Gsk` blur node); reveal = state |
| CW/collapse "show more" | `Gtk.Revealer` + estimated-height clamp (~62 chars/line logic) |
| Dropdown menus | `Gtk.Popover` / `PopoverMenu` |
| Optimistic class toggles | Mutate state → reconcile on response; rollback on error |
| Undo toast (countdown pill) | `AdwToast`-style overlay w/ countdown label + Undo button; backstop commit in core |
| Polls (ballot/results) | Custom widget: radio/check rows → results bars (accent fill, `.mine` emphasis) |
| Composer (@mention/`:emoji` typeahead, preview, CW/lock/schedule/relay toggles, draft) | `Gtk.TextView` + `Gtk.Popover` suggestion list (caret token detection ported); live preview pane; toggles as `Adw` switches |
| Video/audio facades (`preload=none`, Tor degrades to link) | Poster `Gtk.Picture` + play button → swap to `Gtk.MediaFile`/GStreamer on demand; strict-Tor shows external link |
| YouTube facade | Proxied thumbnail + title (oEmbed); click opens external browser; strict-Tor degrades to link |
| Notification/DM/new-notes dots | Quiet dots (not numbers), GLib poll timers |
| Manifest fallback card ("Unsupported event · label", NIP-31 alt, "Open in" chips) | Port as-is, incl. NIP-89 lazy handler discovery |
| Ensō (brush stroke, breathing animation) | Pre-rendered SVG→`Gtk.Picture` raster; breathe via property animation; `prefers-reduced-motion` honored |

Icon set: port the 28 Feather-style 1.9-stroke icons as SVG assets (filled variants for
active states). Motion inventory to preserve: inkfade card entrance (subtle, skippable),
ensō breath, nav progress pulse, busy-button breathing. Everything honors
reduced-motion; scrolls stay instant.

---

## 6. Domain model

### 6.1 The `KindHandler` trait (port of `src/manifest/registry.ts`)

```rust
pub enum Surface { Timeline, Focused, Reader, Embed }

pub struct ActionVocabulary { /* reply, quote, like, zap, bookmark, mute, pin, delete */ }

pub trait KindHandler: Send + Sync {
    fn kinds(&self) -> &[Kind];
    fn reader(&self) -> bool { false }
    fn actions(&self) -> &ActionVocabulary;
    fn prepare(&self, events: &[Event], ctx: &PrepareCtx);   // batch prefetch (profiles, reply-faces)
    fn render(&self, ev: &Event, surface: Surface) -> CardWidget;
}
```

Registry: `register(handler)` (last wins), fallback handler for unknown kinds, dispatch
by kind. Hand-coded handlers for every curated kind; the declarative `engine.ts` idea
(wire-delivered manifests) stays out of scope until a real use appears.

### 6.2 NIP coverage

**Provided by rust-nostr workspace** (do not reimplement):
01, 02, 04(read), 05, 09, 10, 11, 17, 18, 19, 21, 22, 23, 25, 30, 31, 36, 40, 42, 44,
46 (`nostr-connect`), 47 (`nwc`, deferred), 50, 51, 57, 59, 65, 70, 77, 88, 94, 98,
B7 (`nostr-blossom`).

**Thin implementations in `core/src/nip/`** (tag/parse/render semantics — no protocol
risk): 24 (profile extras), 31 (alt fallback), 34 (git repos), 37 (draft sync wraps),
52 (calendar + ICS export), 54 (wiki + AsciiDoc subset), 68/71/92 (imeta media), 84
(highlights), 89 (handler discovery), 99 (classifieds), F4 (podcasts). NIP-96 stays
rejected (Blossom only).

### 6.3 Engagement & read state (ported semantics)

- Own likes/reposts/replies/zapped targets in disk-backed sets → glyph fill states.
- Read high-waters per account, monotonic advance; feed "seen" marks only on catching
  up; new-notes dot counts past threshold (default 5); bell dot on anything newer than
  high-water; DM dot lights on undecrypted wraps only.
- No counts anywhere. Notification stream: replies/mentions/zaps/poll votes; reactions
  in notifications off by default.

---

## 7. Feature checklist (the "everything ports" guarantee)

| Area | Features | Milestone |
|---|---|---|
| Identity | bunker login (waiting/approve states), logout, owner model (single-user), profile edit (kind:0 merge semantics) | M0, M3 |
| Feed | following feed, new-since-visit window + caught-up clearing, view-older batches, followers feed, promoted timelines, content-type promotion prefs, relay browse + favorites + NIP-11 bar | M0, M2 |
| Posting | note, picture (NIP-68), poll (NIP-88), article (NIP-23); reply (NIP-10/22), quote (NIP-18); CW + private (NIP-59) reply; relay-target picker; live preview; @mention/`:emoji` typeahead; drafts; scheduled posts (restart-safe sweep); undo window (backstop commit); delete (NIP-09); Blossom upload (BUD-01/02, 25MB, sha256) | M1, M4 |
| Engagement | like (NIP-25, emoji palette, opt-in pref), zap (NIP-57, anon throwaway key, invoice view — **payment deferred**), bookmark (private 10003), mute (private 10000), pin (10001), follow (kind:3), RSVP (NIP-52) | M1, M2 |
| Reading | threads (depth 4, private replies folded), profiles (pinned/articles strips), article reader + comments (NIP-22), embeds (lazy cards), search (NIP-50 operators `by:/p:/t:/has:/since:/site:`), notifications | M2 |
| Messages | NIP-17 inbox + requests, threads, optimistic send, read state, legacy NIP-04 read-only | M3 |
| Settings | appearance (light/dark), content prefs + filters (keyword/regex, ReDoS-guarded), privacy modes, relays (NIP-65/10050/search/private panes), backup export/restore, undo prefs, zap presets (stored, UI deferred) | M3, M5 |
| Media | proxy + cache (SSRF-guarded, Tor-aware), NIP-B7 hash healing, avatars (hue-bucket fallback), galleries, video/audio facades, YouTube facade | M2, M4 |
| Rich cards | picture, video, podcast, calendar (+.ics), classified, highlight, wiki, repo, custom NIP, poll, comment, fallback+NIP-89 | M4 |
| Sovereignty | Tor modes (off/balanced/strict) with bundled tor, warm-up indicator, drafts NIP-37 sync, list backup, metrics (owner debug), private-relay routing policy | M5 |

---

## 8. Decision log

| # | Decision | Rationale |
|---|---|---|
| D1 | Rust + gtk4-rs + libadwaita, no relm4 (start) | Native GNOME app; plain gtk4-rs + explicit state projections fits the HATEOAS model; relm4 revisit only if factories get painful |
| D2 | Bunker-first signing; local key opt-in via Secret Service | Preserves "never sees your key"; NIP-07 deleted |
| D3 | nostr-sdk workspace for protocol layer | Eliminates the single biggest port risk (relay pool); NIP coverage verified §6.2 |
| D4 | Event store: nostr-lmdb; app state: 0600 JSON files | LMDB for query-heavy event cache; JSON keeps small state inspectable + compatible habits |
| D5 | Decrypted DMs memory-only | Cypherpunk at-rest posture; web app's plaintext cache was a compromise |
| D6 | Wallet (WebLN/NWC UI) deferred | Uncarved block; NWC lands with wallet milestone later |
| D7 | Tor = bundled C tor child process via SOCKS, per-host isolation | Bootc host can't layer a tor service; isolation trick is proven; arti behind trait later |
| D8 | Flatpak is the distribution (GNOME runtime); dev in podman container | Bootc host (Fedora atomic); host lacks GTK dev headers |
| D9 | Design language: sumi-e tokens as GTK CSS provider, bundled OFL serif, Adw idioms with custom note-card/lightbox/ensō | Faithful-adaptation base, pixel-faithful where it defines Satori's identity |
| D10 | Single-user (one profile per instance), multi-account deferred | Matches product philosophy; simplifies state |
| D11 | Note body: label-runs in feeds, TextView in focused views | Perf vs capability split; revisit with measurements (M1) |
| D12 | App ID: `io.github.Letdown2491.Satori` (placeholder) | Flathub convention pending a real domain |
| D13 | **UI toolkit: GPUI (Zed) replaces GTK4/libadwaita.** New `crates/app-gpui` ports the views; `crates/core` is untouched (this is the §1.3 design-for-exit payoff — the domain layer never knew about GTK). GTK app (`crates/app`) stays the daily driver until the GPUI build reaches parity, then is retired. | A one-screen spike (feed + note cards + stroke-drawn glyphs, real session/relays through `crates/core`) was convincing where arguments weren't: GPU rendering makes the sumi-e identity easier (glyphs painted as raw paths at any scale, no SVG/CSS toolchain), scrolling and text feel alive, and the whole UI stack is pure Rust. Accepted costs, recorded honestly: gpui 0.2.2 needs a vendored one-line patch to compile on Linux (xattr/ENOATTR, `vendor/xattr`); its published API already diverges from Zed main (API churn risk); silent failure modes (a render bug shows a blank window, not a warning); no CSS theming — sumi-e tokens become Rust constants (§5 tokens port to `theme.rs`). GPUI rules learned in the spike: never read an entity during its own render (panic) — snapshot data into item closures; `list()` has no intrinsic height — give it `.h_full()`; tokio receivers await fine on gpui's executor (the `drain` seam ports as-is). |

---

## 9. Milestones

- **M0 — Skeleton walks.** Workspace builds in container; window opens with sumi-e
  theme (both palettes); bunker login connects and subscribes; read-only following feed
  renders via outbox routing; tokio↔GLib bridge proven; LMDB store persists.
  *Accept: launch from container, log in with bunker, read feed, restart → resume.*
- **M1 — Speak.** Note card final layout; composer (note type) with preview, CW, relay
  picker; optimistic actions (reply/like/bookmark/pin/mute) with reconcile; undo window
  with backstop; drafts (local); delete.
- **M2 — Read deeply.** Threads, profiles (+strips), notifications, search, embeds,
  media proxy + avatars, picture/video cards, seen-state model (dots, caught-up
  boundary).
- **M3 — Correspond.** NIP-17 DMs end-to-end (inbox, requests, threads, send, read
  state, legacy read-only), relay management panes, settings (appearance/content),
  profile edit, follow/mute management views.
  *Postponed — owner decision; DMs are the least-important part. Most non-DM scope
  (relays, settings, profile edit, follow/mute) already landed under M0/M4.*
- **M4 — Show everything.** Manifest registry complete (all rich cards), polls + votes,
  calendar + RSVP + .ics, highlights, wiki, repo, classified, podcast, custom NIP,
  fallback + NIP-89; Blossom upload wired into composer; galleries + lightbox.
- **M5 — Sovereign.** Bundled tor + privacy modes + warm-up; NIP-37 draft sync;
  scheduled posts; list backup; backup/restore; Flatpak manifest polished (metainfo,
  icons, screenshots); private-relay policy; reduced-motion & a11y pass.

---

## 9.1 Port milestones (GPUI, per D13)

The GTK milestones above describe *what* exists; the P-series below re-lands it on
GPUI. Feature scope is already built in `crates/core` + the GTK app — the port is a
re-expression of views, not a feature rebuild.

- **P0 — Spike becomes the app (done).** `crates/app-gpui` scaffold: workspace dep +
  xattr patch, `theme.rs` (sumi-e tokens as constants), `icons.rs` (PathBuilder
  glyphs), feed view with real note cards. *Accept: real session, real feed, scroll,
  like posts a real reaction.*
- **P1 — Shell & navigation (done).** `shell.rs`: header (back/title/settings seal) +
  nav stack of panes (feed/thread/profile/settings); content views emit `Nav`
  events, the shell owns the stack. Thread view (anchor + flat replies, reply
  click opens its own thread), profile view (name/about/follow seal + notes),
  settings view (relays + media servers, read-only). Escape-to-back via root
  key-down + focused shell handle. *Accept: click note → thread, Escape →
  back, all four routes render.* Finding for P2: GPUI's Wayland CSD drag strip
  overlays our header — in fullscreen, clicks on the top ~30px start a window
  drag instead of hitting header buttons; interactive header elements need to
  clear the CSD inset (or `TitlebarOptions::appear` revisited).
- **P2 — Speak & read deeply.** Composer (undo window, multi-image attach via
  `upload_chain`), thread view + comments, full note-card fidelity (replies, quoted
  events, npub/URL rendering), optimistic bookmark/reply, profiles + avatars (core
  image fetch → gpui `img`).
- **P3 — Everything else.** Settings panes, notifications, search, rich cards from
  the manifest registry, media proxying.
- **P4 — Cutover.** Binary named `satori`; retire `crates/app`; Flatpak/packaging
  update; both themes; reduced-motion equivalent.

Port convention: each ported view cites its GTK/TS origin
(`// TODO(port): <origin> — P<n>`), same discipline as `crates/core`.

---

## 10. Open questions / deferred

- Wallet milestone (NWC via `nwc` crate) — revisit after M4.
- Multi-account / profiles-per-window — deferred (D10).
- arti in-process — behind transport trait, evaluate post-M5.
- YouTube embeds in-app (webkitgtk view) vs external browser — **external browser for
  v1**; a webview is a code-execution surface and can be revisited with a hard argument.
- AsciiDoc depth for NIP-54 — subset matching `src/nostr/asciidoc.ts` coverage.
- `engine.ts` wire-manifest idea — out of scope until external demand exists.
- M3 (NIP-17 DMs) — **postponed by owner decision until explicitly re-prioritized**;
  least-important milestone. Land M5 + `TODO(port)` chokepoint cleanup first.
