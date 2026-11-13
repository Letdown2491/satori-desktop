# Satori Desktop: Design & Build Spec

Native Linux desktop rewrite of Satori, a Nostr client, in Rust. The UI toolkit is
GPUI (Zed) per D13; the old GTK4/libadwaita app has been retired (§9.1, P4
pulled forward). This document is the
build spec: principles → decisions → architecture → milestones. The original TypeScript
web app (`src/` + `public/`) was the frozen reference implementation; its behavior
documented *what* the product does, not *how* (the how was shaped by the browser, a
constraint the desktop app does not have). It was retired at P4 parity (§8 D19): the
code lives in git history at commit `a4b09e8` (`git show a4b09e8:src/<file>`), and the
many `TODO(port)` citations across `crates/` point there.

**Status:** living doc. Decisions in §8 are settled unless reopened with rationale.

---

## 1. Principles

Satori is a Nostr client that strictly follows cypherpunk ideology, HATEOAS principles,
and Buddhist/Taoist philosophy. These are not decoration; each one dictates architecture.

### 1.1 Cypherpunk: privacy as default, verification as law

- **Key custody**: the app never requires and never touches your private key (D17).
  Sign-in is NIP-46 only: a `bunker://` paste or a `nostrconnect://` QR, both holding
  only an ephemeral transport secret. (The D2-era local-key opt-in is removed; the
  core's local-mode code survives to read old session files.)
- **Verify everything locally**: every event (including zap receipts, gift wraps, seals,
  deletions) is signature-checked client-side. Relays are untrusted pipes.
- **No telemetry. No analytics. No counts.** Nothing about the user leaves the machine
  except signed Nostr events to the user's chosen relays.
- **SSRF discipline on every fetched URL** (scheme + IP screening, DNS-rebinding-safe
  resolution, Tor-aware fetch paths). Ported from `src/ssrf.ts`.
- **No remote code execution surface**: no HTML engine, no JS engine. Images decode in
  glycin's sandbox. SVG is never rendered as a trusted format.
- **State at rest**: event cache in LMDB, app state in JSON files (0600). Decrypted DMs
  stay in memory by default; nothing decrypts to disk without an explicit user opt-in.
- **Tor as a mode, not a bolt-on**: per-host circuit isolation, fail-closed strict mode.

### 1.2 HATEOAS: the event stream is the state, the UI is a projection

The web app's HATEOAS core was never the HTTP seam; it was two patterns that port
directly:

1. **State lives in data, not widgets.** Views are pure projections of
   `(local event store ∪ relay subscriptions)`. An action mutates state and the UI
   re-renders from it, the native analogue of "an action returns new HTML."
   Optimistic updates are state mutations with rollback, not CSS class toggles.
2. **Affordances are declared by data.** The manifest registry
   (`src/manifest/registry.ts`); kind → handler with a declared *action vocabulary* and
   named surfaces; is the app's affordance-discovery mechanism. It becomes a Rust trait
   (§6.1) and is the spine of the UI. Anything the UI can do to an event is listed in its
   handler's vocabulary; unknown kinds degrade honestly (NIP-31 `alt`, no masquerading).

helm.js itself dies. Its *semantics* survive as native idioms: fragment swap → view
update; `h-trigger: intersect` → lazy-load signal; `every Ns` polling dots → GLib
timeouts; `h-optimistic` → mutate-then-reconcile.

### 1.3 The uncarved block: design for exit, leave out what can be left out

- **The feed runs to the end of your history, then rests.** Endless scroll
  top-ups from the relays until the store runs dry (user decision, P3; over
  the GTK app's deliberate "view older" batches, which made catching up a
  chore); the caught-up ensō clearing still marks the bottom. No algorithms,
  no read-state anxiety mechanics.
- **The undo window is the breath before speech**: posts are held N seconds (default 5,
  max 30) before publishing, restart-safe, with a server-side backstop commit.
- **Quiet by default**: reactions off, no like/repost counters, dots instead of badges,
  instant scrolls, `prefers-reduced-motion` honored.
- **Sumi-e aesthetic** (§5): ink on washi, vermilion only for seal moments.
- **Leave things out**: no NIP-07 (browser-only), no multi-user daemon,
  no HTTP API, no SPA. Every feature must earn its place. (The D6 "no
  wallet in v1" carve was un-deferred for the NWC payer alone — D20.)

---

## 2. What carries over, what dies, what is re-expressed

| Web app layer | Fate in desktop |
|---|---|
| Relay pool policies: outbox routing (NIP-65 set-cover ≤70 relays, ≤2/author), adaptive budgets, latency EWMA profiles, read-dead detection, seen-on memory, NIP-42 auth, NIP-40 expiry, NIP-09 tombstones | **Port** (core crate; nostr-sdk primitives underneath) |
| Manifest registry + kind handlers + action vocabulary + surfaces | **Port as `KindHandler` trait** (§6.1) |
| All content cards (note, picture, video, article, poll, calendar, classified, podcast, highlight, wiki, repo, comment, custom NIP, fallback) | **Port**; built on GTK, re-expressed as gpui views/cards.rs (P3) |
| Undo window, scheduled sweep, drafts + NIP-37 sync, filters, content prefs, list backup, bookmark/mute/pin/follow, NIP-17 DMs, NIP-50 search, NIP-11 relay info/browse, Blossom upload, media proxy + NIP-B7 hash healing, SSRF guards, Tor modes | **Port** |
| Sumi-e design language | **Port**; GTK CSS provider first, now Rust token constants (§5, `theme.rs`) |
| NIP-46 bunker client | **Use nostr-connect crate** (nostr-sdk) |
| NWC (NIP-47) | **Port**; the `nwc` crate, un-deferred (D20) |
| NIP-07 / hext.js / sign-and-resubmit seam / batch chains / `H-Nostr-Sign` protocol | **Deleted**; existed only because the key lived in the browser |
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
  Cargo.toml                # workspace: crates/core + crates/app-gpui
  crates/
    core/                   # satori-core: nostr domain. NO UI dependency.
      src/
        manager.rs          # the Core handle: every channel pump the views read
        pool.rs             # relay pool glue: outbox routing, budgets, auth, seen-on
        outbox.rs           # NIP-65 read/write routing, per-shard budgets (D33)
        outbox_queue.rs     # the restart-safe pending-publish queue (D36)
        relay_latency.rs    # per-relay EWMA latency/health, read-dead/write-dead (D33/D36)
        seen.rs             # seen-on memory (relay attribution)
        hints.rs            # author-keyed relay hints (NIP-65 lists, live bell)
        store.rs            # event cache (nostr-lmdb) + tombstones + expiry + prune
        signer.rs           # bunker (nostr-connect) / local-key (Secret Service)
        session.rs          # session persistence/resume
        secrets.rs          # keyring-first, 0600-file fallback (D26)
        publish.rs          # tag building (NIP-10/22), relay targets, delivery report
        engagement.rs       # own-likes/reposts/replies/zapped sets
        notifications.rs    # notification stream assembly
        media.rs            # SSRF-guarded fetcher, media proxy, NIP-B7 healing
        upload.rs           # Blossom upload (NIP-B7)
        nip11.rs            # relay documents: capability whispers
        net_guard.rs        # paste-time relay screening for nwc/bunker URIs (D51)
        nip40.rs            # NIP-40 expiry
        wallet.rs           # NIP-47 NWC payer (D20)
        dms.rs              # NIP-17: unwrap, routing, read state, request triage
        tor.rs              # SOCKS transport stub, privacy modes (M5 pending)
        privacy.rs          # off / balanced / strict modes
        dedup.rs            # duplicate-content spam filter (D41)
        wot.rs              # web of trust: qualified set, relay set-cover (D41)
        rate.rs             # reply-velocity gate (D43)
        deletions.rs        # NIP-09 tombstones
        scheduled.rs        # scheduled-post holds + sweep
        undo.rs             # the undo window
        read_state.rs       # high-waters, notification state
        profiles.rs         # profile cache service (10k cap, 12h TTL)
        blocktime.rs        # block heights by timestamp (the third stamp mode)
        viewport.rs         # viewport-scoped engagement subscriptions
        kinds.rs            # KindHandler trait + registry (§6.1)
        config.rs           # filesystem layout + identity constants
        diag.rs / error.rs  # the diagnostic log; login/session errors
        nip/                # thin NIPs: article (23), comment (22), poll (88),
                            # calendar (52), classified (99), highlight (84),
                            # wiki (54), repo (34), podcast (F4), imeta (68/71/92),
                            # nip10 (threads), nip78 (app-data)
    app-gpui/               # satori-desktop (package satori-gpui): the GPUI binary.
      src/
        main.rs             # Application, the Root view (login doors → shell)
        theme.rs            # sumi-e tokens (Adwaita Sans/C059, night/washi palettes; D28)
        icons.rs            # PathBuilder stroke glyphs
        media.rs            # the media cache (D14/D22), pin/unpin machinery
        video.rs            # inline player (D21), https-only inline gate
        poster.rs           # video head-sniff + blurhash (D32)
        markdown.rs / input.rs / emoji.rs / timefmt.rs / desktop.rs
        prefs.rs            # config-cache reads (the view side)
        uris.rs             # nostr: URI routing (D27)
        views/              # shell, feed, thread, profile, reader, search,
                            # notifications, composer, settings, wallet, library,
                            # login, edit_profile, actions (the one action row, D54),
                            # cards, content, states, nav, zap
  packaging/                # deb/rpm metadata, desktop file, metainfo, icons
  self/                     # the flatpak-shaped bundle: assemble.sh + Orchard publish
  .github/workflows/        # tag-driven release.yml (deb/rpm/flatpak/orchard), pr.yml CI
  Containerfile.dev         # podman dev image (host is bootc; no GTK/Vulkan headers)
  vendor/                   # the D13/D29/D30 patches, all RETIRED by D35's
                            # gpui-pre graph (no [patch] section remains);
                            # kept for re-vendoring if the chains return
```


### 3.2 Async model

- **tokio** runtime owns all I/O (relays, HTTP, Tor).
- **GPUI's executor** owns all UI (P4 cutover; the GTK-era `GLib MainContext`
  bridge is retired with `crates/app`). UI code spawns on gpui's executor,
  network results cross back through channel pumps; UI never blocks, network
  code never touches elements.
- State: a single `Core` handle (`Arc`) per profile exposing typed observables; views
  subscribe, re-render as projections. No element-owned truth.

### 3.3 Storage

Flatpak-safe XDG paths (`~/.local/share/satori/` inside the sandbox):

- `events/`: LMDB event store (nostr-lmdb), the projection source of truth.
- `*.json`: app state, 0600, debounced flush + flush-on-exit (ports `json-store.ts`;
  the full register lives in `config.rs::files`): `prefs.json`, `session.json`,
  `drafts.json`, `holds.json` (the undo window), `scheduled.json`, `filters.json`,
  `read-state.json`, `dm-read.json`, `relays-cache.json`, `relay-hints.json`,
  `seen-relays.json`, `relay-latency.json`, `relay-info.json`, `relay-favorites.json`,
  `engagement.json`, `deletions.json`, `profiles.json`, `blocktime.json`,
  `wot-sources.json`, `dedup-state.json`, `rate-state.json`, `suppressed-notifs.json`
  (D48's persistence trio), `outbox-pending.json` (D36's queue).
- `media-cache/`: avatar/media blob cache (LRU, size-capped), keyed by sha256.
- `satori.log`: the diag mirror (stderr plus file).
- Secrets: the bunker transport secret only (D17; no master key in the app),
  stored Secret-Service-first via the `keyring` crate (2s deadline; the
  keyring path can block on niri sessions without gnome-keyring), 0600-file
  fallback. What rides with P4 packaging is the sandbox permission
  (`--talk-name=org.freedesktop.secrets`).

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
- Per-host circuit isolation via unique SOCKS usernames (C tor
  `IsolateSOCKSAuth`), the same trick as `ws-tor.ts`.
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
      core dumps off by default (`RUST_BACKTIME` n/a; no panic logs with secrets).
- [ ] No telemetry, no update phone-home (Flatpak updates only).
- [ ] All state files 0600 under user-owned XDG dir.

---

## 5. UI design language ("sumi-e ink on washi")

*(The token table below is the GTK-era spec, carried verbatim into
`theme.rs`; values match both palettes. The mechanics around it changed in
the P4 cutover: tokens are Rust constants in `crates/app-gpui/src/theme.rs`,
live-switchable via `ThemeSignal`, not a CSS provider; glyphs are
`PathBuilder` strokes in `icons.rs`, not icon files. §5.2's faces were
replaced in the P3 polish pass; see below.)*

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
  bundle an OFL serif (default choice: **Source Serif 4**); the web stack
  (Iowan/Palatino/Georgia) does not exist on Linux.
  *(P3 polish, as built: **P052**, the Palatino clone with the big x-height, for
  body serif; Nimbus Roman read cramped at 13px.)*
- **UI text**: system sans (Adwaita Sans), 15px body, 1.6 line-height; pre-wrap semantics
  for note bodies. *(As built: **Cantarell** at 12–13.5px; the desktop reads
  denser than the web.)*
- **Monospace**: Adwaita Mono for npubs, invoices, clone URLs.

### 5.3 Shell (as built on GPUI; the GTK-era text retired with D13/P4)

- A 52px left rail, icons only, top to bottom: the user's avatar (the
  Profile spot — re-click scrolls the profile back up), Feed, Articles,
  Library, Search, Notifications (bell; the vermilion dot lights on
  anything newer than the read high-water, 90s poll), Settings, Compose
  (the pen wears vermilion). Active state is a small vermilion brush
  tick, not a pill; every rail button carries a tooltip.
- One reading column (720px max) on the washi background; panes push and
  pop on a stack (feed → thread → profile → reader → …); the composer is
  a pane, not a modal (the hold countdown and the draft live with it).
- Full-screen takeovers: the lightbox (galleries) and the video overlay
  (one player per process, D21), both ✕/Esc/backdrop-dismissable.
- Floating affordances: the hold toast (undo window countdown, bottom
  center over any pane), the back-to-top whisper (bottom right, sealed
  click, revealed when scrolled deep), the new-notes pill (top of the
  feed; counts what arrived while reading, loads on click), the outbox
  pending pill (D36's honesty state).

### 5.4 Components (web → GTK)

*(The table below is the P2/P3 PORTING LEDGER: it records how each web
component mapped to GTK and where the semantics landed. The P4 cutover
(D13) retired GTK; the implementations now live in `crates/app-gpui/src/
views/` — cards.rs (note cards), content.rs (bodies/media/galleries),
actions.rs (the one action row, D54), states.rs (empty/error strips),
composer.rs, settings.rs, wallet.rs — with the tokens as Rust constants
(§5.1) and glyphs as PathBuilder strokes. Read the table for the
BEHAVIOR contracts, the Rust views for the code.)*

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
46 (`nostr-connect`), 47 (`nwc`; D20), 50, 51, 57, 59, 65, 70, 77, 88, 94, 98,
B7 (`nostr-blossom`).

**Thin implementations in `core/src/nip/`** (tag/parse/render semantics; no protocol
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
| Identity | bunker login (waiting/approve states), logout, single-user model, profile edit (kind:0 merge semantics) | M0, M3 |
| Feed | following feed, new-since-visit window + caught-up clearing, view-older batches, followers feed, promoted timelines, content-type promotion prefs, relay browse + favorites + NIP-11 bar | M0, M2 |
| Posting | note, picture (NIP-68), poll (NIP-88), article (NIP-23); reply (NIP-10/22), quote (NIP-18); CW + private (NIP-59) reply; relay-target picker; live preview; @mention/`:emoji` typeahead; drafts; scheduled posts (restart-safe sweep); undo window (backstop commit); delete (NIP-09); Blossom upload (BUD-01/02, 25MB, sha256) | M1, M4 |
| Engagement | like (NIP-25, emoji palette, opt-in pref), zap (NIP-57, anon throwaway key, invoice view; **payment deferred**), bookmark (private 10003), mute (private 10000), pin (10001), follow (kind:3), RSVP (NIP-52) | M1, M2 |
| Reading | threads (depth 4, private replies folded), profiles (D30 notes-as-feed, photos/videos media strips), article reader + comments (NIP-22), embeds (lazy cards), search (NIP-50 operators `by:/p:/t:/has:/since:/site:`), notifications | M2 |
| Messages | NIP-17 inbox + requests, threads, optimistic send, read state, legacy NIP-04 read-only | M3 |
| Settings | appearance (light/dark), content prefs + filters (keyword/regex, ReDoS-guarded), privacy modes, relays (NIP-65/10050/search/private panes), backup export/restore, undo prefs, zap presets (stored, UI deferred) | M3, M5 |
| Media | proxy + cache (SSRF-guarded, Tor-aware), NIP-B7 hash healing, avatars (hue-bucket fallback), galleries, video/audio facades, YouTube facade | M2, M4 |
| Rich cards | picture, video, podcast, calendar (+.ics), classified, highlight, wiki, repo, custom NIP, poll, comment, fallback+NIP-89 | M4 |
| Sovereignty | Tor modes (off/balanced/strict) with bundled tor, warm-up indicator, drafts NIP-37 sync, list backup, debug metrics, private-relay routing policy | M5 |

---

## 8. Decision log

| # | Decision | Rationale |
|---|---|---|
| D1 | Rust + gtk4-rs + libadwaita, no relm4 (start) | Native GNOME app; plain gtk4-rs + explicit state projections fits the HATEOAS model; relm4 revisit only if factories get painful |
| D2 | Bunker-first signing; local key opt-in via Secret Service | Preserves "never sees your key"; NIP-07 deleted |
| D3 | nostr-sdk workspace for protocol layer | Eliminates the single biggest port risk (relay pool); NIP coverage verified §6.2 |
| D4 | Event store: nostr-lmdb; app state: 0600 JSON files | LMDB for query-heavy event cache; JSON keeps small state inspectable + compatible habits |
| D5 | Decrypted DMs memory-only | Cypherpunk at-rest posture; web app's plaintext cache was a compromise |
| D6 | Wallet (WebLN/NWC UI) deferred; **NWC side un-deferred by D20** | Uncarved block; NWC lands with wallet milestone later |
| D7 | Tor = bundled C tor child process via SOCKS, per-host isolation | Bootc host can't layer a tor service; isolation trick is proven; arti behind trait later |
| D8 | Flatpak is the distribution (GNOME runtime); dev in podman container | Bootc host (Fedora atomic); host lacks GTK dev headers |
| D9 | Design language: sumi-e tokens as GTK CSS provider, bundled OFL serif, Adw idioms with custom note-card/lightbox/ensō | Faithful-adaptation base, pixel-faithful where it defines Satori's identity |
| D10 | Single-user (one profile per instance), multi-account deferred | Matches product philosophy; simplifies state |
| D11 | Note body: label-runs in feeds, TextView in focused views | Perf vs capability split; revisit with measurements (M1) |
| D12 | App ID: **`io.github.Letdown2491.satori`** (chosen; lowercase app name is the house style: `io.github.Letdown2491.gertie` / `.riddim`). Byte-identical across the `.desktop` file ID, `Icon=`, the keyring service (`secrets.rs`), and the window's Wayland `app_id` (`WindowOptions.app_id` in main.rs); GNOME matches them exactly. The rename orphans keyring entries under the old `Satori` string: one-time re-login, done pre-release. Flathub note (M5): if verification ever matters, the last component must match the GitHub repo name. | Maintainer picked the lowercase form (Nov 2026) |
| D13 | **UI toolkit: GPUI (Zed) replaces GTK4/libadwaita.** New `crates/app-gpui` ports the views; `crates/core` is untouched (this is the §1.3 design-for-exit payoff; the domain layer never knew about GTK). GTK app (`crates/app`) stays the daily driver until the GPUI build reaches parity, then is retired. | A one-screen spike (feed + note cards + stroke-drawn glyphs, real session/relays through `crates/core`) was convincing where arguments weren't: GPU rendering makes the sumi-e identity easier (glyphs painted as raw paths at any scale, no SVG/CSS toolchain), scrolling and text feel alive, and the whole UI stack is pure Rust. Accepted costs, recorded honestly: gpui 0.2.2 needs a vendored one-line patch to compile on Linux (xattr/ENOATTR, `vendor/xattr`); its published API already diverges from Zed main (API churn risk); silent failure modes (a render bug shows a blank window, not a warning); no CSS theming; sumi-e tokens become Rust constants (§5 tokens port to `theme.rs`). GPUI rules learned in the spike: never read an entity during its own render (panic); snapshot data into item closures; `list()` has no intrinsic height; give it `.h_full()`; tokio receivers await fine on gpui's executor (the `drain` seam ports as-is). |
| D14 | **Perf/memory posture after the P3 review (measured on the GPUI binary):** the *decoded-image* cache is bounded (128 entries / 256 MB decoded-RGBA-equivalent, oldest-first eviction, re-entry re-reads the sha256 disk cache; tightened from 256/768 MB in the P4 leak review); decoded pixels are the only truly unbounded RAM sink a timeline can produce. The cache stores the ORIGINAL encoded bytes (gpui decodes on its own asset thread and caches the texture by content hash); an earlier handoff of raw RGBA that the UI re-encoded to PNG *on the main thread* was the endless-scroll hang (hundreds of ms of deflate per photo, ×40 per top-up page). **P4 leak review (measured): gpui's own asset cache (`loading_assets`) NEVER evicts**; `fetch_asset` inserts the finished decode (a decoded `RenderImage`, full RGBA) and nothing in gpui removes it, so every photo ever seen in a session stayed decoded in RAM forever even after our cache evicted (567 MB RSS after an evening of scrolling; our eviction now calls `remove_asset` on the evicted handle, dropping gpui's decoded copy with the entry; fresh session 243 MB). **Eviction's other half: re-request on visibility.** An evicted URL used to be gone for the session; `want_avatar` fired only on profile arrival, so an evicted avatar wore its ink initial forever (and the tighter caps made it happen mid-scroll). `media::fetch` is now idempotent and render-safe; Ready/not-media are silent no-ops, only an actual landing notifies (waiter `EntityId`s ride the cache's `Loading` entry), so the feed's lazy list re-requests each visible card's media at prepaint, and the bounded views (thread/notifications/search/profile/reader/shell rail/composer quote) sweep theirs at render; an evicted avatar comes back the moment its card scrolls into view, via the disk cache. The Feed render keeps a snapshot (`Arc`-held) rebuilt once per data change, not per frame; `cx.notify()` fires for every profile resolution and image arrival, so per-frame deep copies of the notes vec/profile map were the frame-budget leak. The media *disk* cache trim runs every 10th write (a 50-write cadence overshot its 200 MB cap by ~150 MB; files average whole MBs). Deliberately NOT bounded: the in-memory notes vec (~1 KB/note; a 10k-note session ≈ 10 MB, and capping the oldest end would fight endless scroll for a trivial win) and the LMDB event store on disk (~25 MB after months of use; it is the cold-start reservoir; `bootstrap_inner` paints page 1 from it instantly, and pruning it would trade that instant paint for nothing). Idle CPU (~6% of a core, evenly across tokio workers + main) is relay-pool keepalive + the 90s bell poll; accepted. | `/proc` RSS/CPU sampling of the running app plus a static audit of the hot paths; media-cache dir measured at 327 MB against its 200 MB cap (trim cadence), LMDB at 25 MB (decisively small); P4: gpui 0.2.2 `app.rs` `fetch_asset`/`remove_asset` read; insert-without-eviction confirmed in source. |
| D15 | **The GPUI binary is named `satori-desktop`** (package stays `satori-gpui`). | User preference during P4 prep; the P4 "binary named `satori`" goal is superseded. |
| D17 | **The app never handles the master key.** The nsec login door is removed; sign-in is NIP-46 only; bunker:// paste or nostrconnect:// QR (advertised on `relay.nip46.com`), both holding only an ephemeral transport secret in `BUNKER_CLIENT_SECRET`. The core's `login_nsec_inner` survives as a `#[cfg(test)]` fixture for the logout/re-login flow tests; `SessionMode::Local` + its resume arm stay for reading old session files. nostrconnect metadata: `name: "Satori"`, `description: "A quiet Nostr reader."`; `url`/`icons` stay unset until the app has hosted art (both optional per NIP-46). The updated NIP-46 also defines `optional_client_metadata` (name/url/image) as the 4th `connect` param for the bunker flow; nostr-sdk 0.45's `connect_bunker` doesn't expose it (sends only pk+secret), noted as an SDK gap; likewise the SDK advertises nostrconnect metadata as a `metadata=` JSON blob while the current spec spells flat `name`/`url`/`image` query params. | User: "remove the sign in via nsec, that's not secure"; prompted by the discovery that a month-old local session had the master key sitting user-level-readable in the Secret Service. |
| D18 | **The notification stream mirrors the reference's shape, not just kind 1.** Four layers were kind-1-only (relay mention filter, relay reply filter, cached store phase, render gate); after Amethyst's NIP-22 migration real notifications were invisible. Stream = kind 1 + 1111 (replies/mentions, 1111 always a reply), 9735 zaps (embedded 9734 must verify + p/e/a match the receipt; bolt11 amount authoritative; unverifiable = anonymous, never a spoofed name), 1018 poll votes (#e my poll ids), kind 7 reactions behind the `reactionNotifs` pref (default off), `-` dislikes never. Mutes gate at both phases. Rows carry action words ("· replied", "· zapped 21 sats"); a comment row opens the PARENT's thread. | User approved all five notification types, zaps last ("maybe zaps last but all five sounds good"); reference is `src/data/notifications.ts`. |
| D19 | **The TypeScript reference is retired at P4 parity.** `src/` + `public/` (1.8 MB, build tooling already gone; it was source-as-documentation only) deleted once the port's remaining slices closed; the code is unchanged since the initial commit, so the pinned address `a4b09e8` is its complete final state (`git show a4b09e8:src/<file>`). The ~118 `TODO(port)`/doc citations across `crates/` and this document stay as historical pointers to that address. Rationale: nothing executed it, and the product behavior it specified now lives in the Rust code it specified it for; the tree should say what the project IS, not what it was rewritten from. | User decision ("shouldn't we delete all the stuff that doesn't belong to this project now?"); the parity trigger was the P4-minus-packaging sweep (reduced-motion audit, reactionNotifs surface, cached-phase engagement kinds). |
| D20 | **The wallet milestone is un-deferred: NIP-47 zaps via the `nwc` crate (0.45 line, same as the other rust-nostr deps).** D6's carve (§1.3 "no wallet in v1") is reopened, explicitly and only for the NWC *payer*; D17 is untouched (an NWC connection secret is a separate, revocable wallet credential, never master-key material, stored through `secrets.rs` (keyring-first, 0600-file fallback) and never in `prefs.json`). Zaps are **public**: the kind-9734 request is signed through the user's bunker signer; the anon invoice mode stays as the no-wallet fallback. Feather's payment-discipline rules port as law: **never automatically retry an uncertain payment** (a timeout or unknown NIP-47 response is a terminal `Uncertain` state surfaced to the UI; only an explicit user action retries), **store the signed zap request before paying** (a crash between sign and pay can't double-zap: the explicit retry reuses the same event ID), and **one in-flight payment per note**. Tap = 21 sats, no confirmation dialog; long-press = amount picker; NIP-47 error codes (`INSUFFICIENT_BALANCE`, `RATE_LIMITED`, `QUOTA_EXCEEDED`, …) surface as honest, quiet error states; LNURL-pay fetches ride the SSRF-guarded path. | The Feather-inspired work plan (Nov 2026): zaps are the one load-bearing gap left for a reader-first desktop client; receipts already verify (D18), the UI already shows them; only paying was missing. Feather's own rule "NWC uses its separate connection secret" is what makes this D17-compatible. |
| D21 | **Inline video, click-to-play, one player per process (Phase 3).** `gpui-video-player` 0.1.3 (GStreamer-backed, MIT, targets our gpui 0.2) is the integration path: a video tile renders its poster until tapped; the tap opens the inline player (`video.rs`; a process-wide registry holding at most one player, replaced on each new open; Feather's autoplay/multi-player system is out of scope, and nothing fetches until the user asks; the media facade rule). Inline-ability is gated three ways: video by extension or imeta mime only, never YouTube watch/shorts pages (GStreamer can't stream them; they stay browser links), and never under strict Tor (a GStreamer pipeline can't ride the Tor transport; the tap degrades to the browser link, DESIGN.md §4's facade rule). Threading honors the crate's contract: `Video::new` blocks up to ~5s, so it runs on the background executor; `Drop` joins the worker, so a replaced player's drop defers to a spare OS thread. Packaging cost accepted: the Flatpak manifest (M5) adds the `org.freedesktop.Platform.GStreamer` platform extension; dev builds need `gstreamer1{-plugins-base}-devel` in the container and, on the host, the minimal runtime plugin set (verified against gst-inspect on base-only kumaOS): `gstreamer1-plugins-good` supplies the demuxers (qtdemux/isomp4, matroskademux) and `gstreamer1-libav` the codecs (the distro ships ffmpeg-libs, so libav's avdec plugins cover H.264/H.265/AAC). No openh264, RPM Fusion, or bad-freeworld packages. | User: "Do Phase 3 (full)" at the §5 approval gate (Nov 2026), taking both named costs (Flatpak extension + Tor degradation); "Recommend: click-to-play inline cards" adopted as specced. |
| D22 | **Media reliability contract (polish pass).** Every network step in the media pipeline is timed and byte-capped; failures are KINDED (`MediaError::Transient` vs `NotMedia`) and the UI's policy follows the kind: transient failures cool off with a doubling backoff (2s → 60s, one timer-scheduled retry per cool-off) and are never written off; `NotMedia` is a verdict only (the bytes arrived whole and do not decode), and is in-memory, never persisted. Bytes are validated by decode BEFORE the disk write, and the legacy (pre-validation) cache dir is purged once at startup; a poisoned cache must not survive restarts. Fetches ride one shared client with a browser-style User-Agent (CDNs 403 UA-less requests, and a 403 read as a verdict killed avatars permanently) and hard timeouts (a hung fetch pinned the cache entry `Loading` forever, joining every later waiter onto a corpse). Disc-sized renders (avatars) decode from a ≤128px thumb re-encode, not the full 48MB pixel dump. The decoded cache is least-RECENTLY-used: every fetch call for a held entry is a use (visible cards re-request their media per frame), so an on-screen image is never the evict-front; the flash-then-refetch flicker. AVIF decodes core-side and re-encodes to PNG once (gpui's pipeline maps png/jpeg/webp/gif only). | User report (Nov 2026): "avatars not loading/disappearing/flickering, images and videos don't load a lot of the time"; root causes were compounding permanent-failure bugs (no timeout, terminal NotMedia, disk-write-before-validate, UA 403s, insertion-order eviction), each individually fatal; the fix is a contract, not a patch: nothing media-shaped is written off for weather. |
| D23 | **Quiet affordances (polish pass).** Interactivity is discoverable without chrome: links/mentions/hashtags ride ONE token system (muted ink + underline at rest, vermilion on hover; the seal color marks "pressable"), action glyphs dim slightly on hover (opacity, zero layout shift; the glyphs are baked-color canvas paths), timestamp toggles lift to ink, quote-embed cards lift their left rule to vermilion, and the caught-up seal wears a short hairline (the boundary is felt, not announced). The video tile breathes its play mark while the pipeline builds (honoring the reduced-motion pref via a cx-less static mirror, the `prefs::privacy_mode` pattern); a tap is acknowledged without a spinner widget. Reply whispers show the parent author's NAME (parent authors hydrate through the same profile map as headers) and fall back to a quiet "↳ reply", never raw hex; previews (whisper snippets, quote embeds) read as text only; URLs are dropped core-side (`plain_snippet`). | Maintainer: "full polish pass… super clean and polished while sticking to our philosophy" (Nov 2026); screenshot evidence: all four visible reply whispers showed hex, one quote preview leaked a mangled blossom URL. |
| D24 | **Bounded resources (four-lens review pass: performance / system usage / memory / security).** Every shared resource in the hot path now has an explicit ceiling: media downloads are gated at six concurrent (one gate slot per ATTEMPT, released during backoff sleeps; a dead CDN can't pin a slot; JSON/LNURL stay ungated; a zap invoice never queues behind pictures); the image decode runs on tokio's blocking pool, off the 2-worker relay runtime that serves relays/zaps/wallet; the decode ceiling (40MP ≈ 160MB RGBA peak) is checked from the container HEADER before any pixel allocation, so a 20MB compressed bomb (≈1.6GB RGBA) dies unallocated; the home timeline windows its notes (1000 → trim to 400, reader 80+ rows above the trim line; splice preserves heights/anchor above the removed range) with the paging anchor moved OFF the window (`oldest_ts` = oldest timestamp ever absorbed, so `older()` never re-fetches trimmed rows); and the cool-off retry is scheduled exactly once, where the `Failed` entry is born (per-peek timer pileup caught in review). The review also caught gpui's `ListState::splice(old..old, n)` taking the REPLACEMENT count, not the new total; appends that passed the new total double-counted the item tree (probed empirically: 10-item list + splice(10..10,16) → 26). Accepted as future work, not bugs: SSRF redirect re-validation (M2/M4 TODO), Tor-routed media + UA reconciliation (M5), LMDB GC (59MB, watch item). | Maintainer chose "quick wins + windowing" from the review report (Nov 2026); review evidence: media cache 159MB/282 files vs 200MB cap (trim sound), animations transient-only (no idle spin), no live process to sample. |
| D25 | **Release shape, pre-flatpak (M5).** `[profile.release]`: `strip = "symbols"`, `lto = "thin"`, `codegen-units = 1`; the 25GB `target/debug` tree is dev-only and deleted once the release build lands (next dev-profile gate run recompiles deps, ~minutes from the warm registry volume). Local install (`packaging/install-local.sh`): binary → `~/.local/libexec/satori-desktop`, wrapper → `~/.local/bin/satori` (sets `LD_LIBRARY_PATH=$HOME/.local/lib` only when the host lacks `libxkbcommon-x11` (bootc base images do); the flatpak runtime ends this at M5), `.desktop` → applications, ensō icon → hicolor (`packaging/icons/`, generated by `generate.sh` from the in-app `enso_glyph`; the same 60° start / 320° sweep / 9.2→8.1 taper arc on the LIGHT washi disc + hairline ring; SVG master + magick raster at 512/256/128/64/48/32). `nostr:` (NIP-21) is DECLARED in the desktop file (`MimeType=x-scheme-handler/nostr;`); URI handling is its own future task. | Maintainer: "delete the debug build… run a release build… get a .desktop file going, an icon" (Nov 2026); all four decision points taken as recommended. |
| D26 | **Rename fallout + login-page affordances.** The D12 app-id rename orphaned keyring entries under the old capitalized service; instead of a one-time re-login, `secrets::load` migrates on first miss (read legacy entry → copy to current service → delete the orphan; copy failure keeps the orphan for a retry, and the existing no-Secret-Service fallback path is untouched; the container can't exercise the keyring happy path, so the file-fallback test guards the no-op direction). Resume failures split by severity: `LoginError::CredentialGone` (credential definitively gone; session file auto-forgotten, notice shows once) vs plain `Resume` (transient; relay down at launch, store timeout; session kept, next launch retries; auto-forgetting THAT class would force bunker re-approvals after a network blip). The doors banner styles these as states (amber `warn`), not errors (red stays for runtime handshake failures, `error_is_notice` flag). Login QR affordances unified: BOTH QRs (doors 240px + waiting 280px) open the system's `nostrconnect://` handler on click with hairline border + hover lift (muted), the doors URI row shows a one-line brief (`uri_brief`, 26 chars + ellipsis) labeled "· click to copy" (full URI still rides the clipboard), and the caption is one instruction ("Click to open your signer — the key never touches Satori"). | Maintainer, per-item (Nov 2026): all eight proposed changes taken as recommended; kumaOS click-chain test deferred; the OS side (kuma-nostr-panel.desktop → kuma-shell msg nostr %u) verified present by inspection. |
| D27 | **`nostr:` URI handling (NIP-21); single instance + one routing seam.** The D25-declared scheme becomes handled: the desktop file hands `nostr:…` argv to the wrapper; `uris.rs` parses it (scheme case-insensitive per NIP-21, bare bech32 accepted for senders that strip it, `nostrconnect://` deliberately NOT ours; kumaOS owns that verb, D26) into exactly three routing targets (Thread / Profile / Article), which route through the SAME shell navigation in-app taps use (`open_thread`/`open_profile`/`open_article`, pub(crate)). "Already running" is solved single-instance: a connect-first unix socket at `$XDG_RUNTIME_DIR/satori.sock` (per-session tmpfs; no stale files across reboots, no cross-user collisions; connect_timeout doesn't exist for std UnixStream, and an AF_UNIX connect to a live listener can't park the way TCP can). A second launch forwards its argv URI to the primary and exits; never a second window (two windows would double the relay work and race the shared LMDB store); a URI-less launch forwards an empty message = activate only. The primary pumps the socket channel into the window root on a tokio unbounded channel (`cx.spawn` on Root); a URI arriving before sign-in parks in `Root::pending` and routes on `Login::Approved`. naddr resolves through `Core::event_by_address` (newest-wins, NIP-01 addressable semantics) and opens kind 30023 as an article, anything else that resolved as the thread containing it; unresolvable naddrs (relay never had it) stay silent. Lost bind races / missing `XDG_RUNTIME_DIR` degrade to "runs unowned, no IPC" rather than not running. | Maintainer picked it from the survey (Nov 2026): the .desktop file declared-but-unhandled the scheme; clicking a `nostr:` link anywhere did nothing; D25 said "URI handling is its own future task." |
| D28 | **Font stack: Adwaita Sans (UI sans) + C059 (serif display), replacing Cantarell + P052.** `theme.rs` SANS/SERIF consts only; MONO unchanged. Both ship in Fedora's base image (no bundled fonts), and Adwaita Sans is the current GNOME voice; closer to the reference's SF Pro feel than Cantarell's humanist drift; C059 keeps the P052 slot as a Times-adjacent serif for article titles. Accepted: the softness in rendered glyphs is gpui 0.2.2's unhinted grayscale AA (no AA knobs exist), not a font defect. | Maintainer found the old stack "ok i suppose but could be better" (Nov 2026); A/B'd Cantarell/Adwaita Sans and P052/C059 swaps together on the live feed and picked the combined swap. |
| D29 | **Emoji: vendor-patch cosmic-text to prefer the CBDT face (second `vendor/` crate, same mechanism as D13's xattr fix).** Root cause (probed empirically with a headless cosmic-text+swash probe): Fedora ships Noto Color Emoji TWICE under one family name (a 5MB COLRv1 face and a 10MB CBDT face), and cosmic-text 0.14's matcher sorts faces by `(weight_diff, weight, fontdb id)`, tie-breaking to the COLRv1 face; swash (COLRv0-only) returns EMPTY 0×0 images for COLRv1 glyphs (no base outlines), so every emoji only NCE covers rendered as an invisible hole with its advance reserved (the "odd first post" gap; 🇸🇻 confirmed byte-level F0 9F 87 B8 F0 9F 87 BB in the LMDB store). The CBDT face renders its color bitmaps fine under swash: the patch (`vendor/cosmic-text/src/font/system.rs`, `prefer_color_bitmap_faces` + a sfnt-table sniffer) filters faces whose postscript name contains "Emoji" (cosmic-text's `Attrs::matches` makes those ride along in EVERY run's candidate list) down to CBDT-backed ones when duplicates exist. Not a system-font fix (host fonts untouched, rpm-ostree stays admin-only); upstream cosmic-text fixed COLRv1 only in 0.15+, beyond gpui 0.2.2's pin. | Maintainer: "there's some rendering issues… odd first post" (Nov 2026); probe evidence: face 38 (COLRv1) → 0×0 on all emoji glyphs, face 39 (CBDT) → 3837 color bitmaps, CBDT-only-db sim → 🇸🇻 ligates AND renders 18×17 `content=Color`. |
| D30 | **The profile page's Notes tab IS a feed.** An embedded `Feed` entity in a new `FeedMode::Profile` mode renders the timeline's exact rows (reactions, wallet-aware zaps, cards, quotes, endless scroll; core gains `profile_older`, an author-scoped mirror of `older`) instead of profile-local note rows; profile keeps the header (banner strip, NIP-05 verdict badge, npub/lud click-to-copy rows, live about via `word_flow`) and the media strips, which page the same way and dedupe by id. The rejected alternative, genericizing `note_card` over a trait and duplicating engagement state into `ProfileView`, lost on state divergence risk. `Nav` is Clone now so hosted views forward child navs; `proc-macro-error2` is the third `vendor/` crate (E0365 future-incompat, the compiler's own one-word fix). | Maintainer picked four profile improvements, then "make the posts look like the timeline" (Nov 2026); one Feed, many modes, won. |
| D31 | **Distribution: deb, rpm, and an Orchard-published flatpak-shaped bundle (the gertie stack), tag-driven CI.** `cargo deb` / `cargo generate-rpm` metadata blocks on the bin crate (assets from `packaging/`, crate-relative climbs; both Provide/Conflict the app id; the GStreamer plugins are Recommends — dlopen'd at pipeline build, never linked). No flatpak-builder: `self/assemble.sh` builds an app tree (binary + desktop entry + metainfo + icons + `metadata`), `flatpak build-export`/`build-bundle` produce the `.flatpak` single-file per arch (x86_64 + aarch64 legs); the freedesktop Platform 25.08 runtime supplies every linked library AND the GStreamer demuxers/codecs (verified in-sandbox: `qtdemux` + `avdec_h264` present with no addon and no host packages — D21's addon expectation retired). The sandbox needs `sockets=x11;wayland`, `devices=dri`, `shared=network`, and session-bus talk to `org.freedesktop.secrets` for the keyring; the XDG data dir maps to `~/.var/app/<id>` with no filesystem grants. Orchard: `self/satori.publish.toml` (icon must be a PNG at an absolute URL; `relay.orchard.rocks` required), `orchard-publish` pinned v0.3.0, prepare per arch + one signing run gated on `ORCHARD_NSEC_SET`; `orchard.txt` is written by `--prove` at publish time. Two traps found and dodged (both bitten gertie v1.0.1): cargo-deb reads ONLY `[package.metadata.deb]` — a `cargo-deb`-named table silently vanishes, taking the desktop file/metainfo/icon with it; and `-p` for `cargo generate-rpm` takes the package directory path. Third, caught locally: a binary built against a NEWER host glibc than the runtime's dies at launch (`GLIBC_2.43 not found`) — CI's ubuntu-24.04 runner (glibc 2.39 < the 25.08 runtime's) is safe, and a local bundle must build inside the SDK via `org.freedesktop.Sdk.Extension.rust-stable` (`flatpak build-init` + `flatpak build` sourcing `/usr/lib/sdk/rust-stable/enable.sh`). | Maintainer: "should we go ahead and do deb/rpm/flatpak of satori" + "publish to orchard once we have everything ready" (Nov 2026); first tag v0.1.0; both arches; the owner adds `ORCHARD_NSEC` to the repo when ready. |
| D32 | **Video previews: a three-source ladder behind the play mark (poster → blurhash → local first-frame sniff), never the video.** The publisher's NIP-92 poster (`image`, with `thumb` as fallback — the attr the old parse dropped), else a locally decoded `blurhash` gradient (no network), else a SNIFF: the video's head 2MB (the guarded, capped `fetch_video_head` path — the file is never played or fully downloaded) demuxes to its first non-black frame via a `decodebin`+`appsink` pipeline (black openers seek ~1s; moov-at-end files fail and stay bare), encoded PNG into the media cache. The virtual keys (`poster:<url>`, `blurhash:<hash>`) ride media.rs's whole discipline — LRU eviction, backoff, snapshots — and a two-slot sniff workshop (over-full = a short backoff, not a queued promise). Strict Tor sniffs nothing (the facade rule); a switch re-keys the memo (`{strict}#{id}`). The tap still gates playback: a preview commits to picture-grade bytes, exactly what a photo already commits to. Store copy now says "the video itself never downloads before you tap" (the old "nothing fetched" line would have been false). | Maintainer: "can we not do previews for videos instead of just showing that play button? lets discuss" (Nov 2026); chose "i think a b and c probably right?" — thumb attr + blurhash + sniff, D-style hover/muted autoplay rejected as anti-quiet. |
| D33 | **Relay memory (outbox slice 2): learned state in JSON sidecars, not LMDB; targeted queries everywhere.** The seen-on memory (`seen.rs`, seen-relays.ts port) and per-relay latency/read-dead profiles (`relay_latency.rs`, relay-latency.ts port) live as 0600 JSON sidecars in the data dir (the pre-reserved `SEEN_RELAYS`/`RELAY_LATENCY` files), NOT in the events DB (LMDB is owned by nostr-lmdb; learned caches are disposable, so they get the read_state discipline: temp-sibling + rename, debounced flusher thread on `park_timeout` + a dirty flag, LRU caps so files can't grow). Wiring: `fetch_route` now queries ONE relay per shard via manual `ReqTarget`s (the old `fetch_events(filter)` was a client-level req that went to EVERY read relay per shard — slice-1 routing was silently undone); each shard records its latency profile and seen-on (per-relay attribution is only possible targeted); known-empty relays bail to the floor budget, rich-but-truncating ones earn rope to the ceiling, routing weights them by learned quality. Single-event fetches (`event_by_id`, the nevent path, batch parents) race the TS resolveEvent set: nevent hints + declared writes + seen-on, read-dead filtered, indexers as fallback, via the new `Pool::get_from`. Write paths hint the relays: `e`/`q` tags ride `seen::hint_tag`/`seen::first` (actions.ts addTag). The live loop records seen-on straight off the notification's `relay_url` and detects read denials from `CLOSED` reasons on the same broadcast stream (no second task). New `spawn_list_refresher`: hourly, follows with kind:10002 lists older than 7d (or missing) get a targeted indexer refetch, chunks of 200 — a desktop-only addition (the TS cached in memory and never refreshed); a long-lived session should heal its routing. Divergences documented in module docs: ms is time-to-COMPLETE (the SDK merges internally; time-to-last-event is not observable), and denial detection records any matching CLOSED, not just shards that delivered nothing. | Maintainer approved slice 2 ("start", Nov 2026) after the RAM fix (mimalloc) landed; storage choice (sidecars vs LMDB) delegated to the implementation with rationale recorded here. |
| D34 | **Outbox completion: NIP-40 expiry + NIP-09 tombstones at the pool chokepoint (NIP-42 was already done).** The remaining pool.ts chokepoint items ported. (a) `consume_and_filter` in pool.rs is the one door every network read walks through (`get`, `fetch_route`, `get_from`): two passes, first RECORDS kind:5s as tombstones wherever they arrive, then drops kind:5 + NIP-40-expired + author-deleted from the results — a deletion takes effect even on the result set that carried it. Latency profiles and seen-on record on the RAW counts (filtering is not the relay's fault). (b) New `deletions.rs` is the full deletions.ts port: id tombstones keyed `target-id:deleter` (the NIP-09 author MUST enforced structurally at lookup), `a` tombstones validated at record time (coordinate pubkey == kind:5 author) and suppressing only versions with created_at at or before the deletion (re-publish supersedes), tag-spam bounded (100 tags/event, 256-byte keys), LRU caps (5000 relay-fed / 1000 pinned), the user's OWN deletions pinned in maps relay traffic can never evict, persisted to `deletions.json` (the D33 sidecar discipline, 10s debounced flusher). (c) New `nip40.rs` (`is_expired`): a malformed/zero timestamp never expires (the spec ties behavior to the timestamp); enforced at the chokepoint plus every store-paint re-check (the SDK persists expired copies). (d) Manager: the follows-scoped 60s `deleted_cache` store-scan is GONE — `drop_deleted` is now a sync O(1) tombstone/expiry lookup on every render path (feeds, top-ups, threads, notifications union), plus a one-time kind:5 hydration from the store (limit 5000) so pre-existing tombstones don't resurrect, spawned once behind an AtomicBool. (e) Single-event resolution (`event_by_id` path) re-checks the gates on the STORE hit and, when it resolves, spawns a background deletion sweep — a targeted kind:5-by-e-tag query, TTL-gated by `deletions::should_check` (30 min per id, deletions.ts checkDeletions). (f) The live loop records kind:5s then skips dead/expired before the bell. NIP-42 AUTH needs no work: `PoolAuthenticator` (kind 22242 over the session identity, fails with no session) shipped with the pool and is tested. | Owner approved ("finish the outbox model completely", Nov 2026); the store-scan cache died because the tombstone store makes per-render network+query cost structurally unnecessary. |
| D35 | **Renderer: migrate gpui from the blade backend to the wgpu backend (P5).** Evidence (Nov 3 2026): the app died once (exit 101, ~30 min in) at gpui 0.2.2's `blade_atlas.rs:229` — `flush_initializations` unwrapped a `storage[None]` slot: `insert_texture` queues the id in `initializations` before the texture is placed, `remove` can free that same slot in the same frame (create-then-evict before any flush), and the next flush panics. Pure upstream atlas state bug (kernel log clean: no GPU fault, no suspend); the crash class is well known on Linux/AMD, and Zed's own fix was replacing the blade renderer with wgpu entirely (zed PR #46758). No published fix exists on the `gpui` crates.io line (ends at 0.2.2, Oct 2025 — the D13 pin). Route chosen: the `gpui-pre` crates.io snapshots (0.3.11 = snapshot of zed@f816e1f, published by the gpui-component maintainer huacnlee, NOT zed-official — supply-chain posture recorded; the snapshot keeps the blade dep out and the xattr/async-tar chain gone, so D13's vendored `vendor/xattr` patch and D29's stance need re-validation). Rejected alternatives: a watchdog relauncher (masks the bug, wrong layer); a git dependency on zed's repo (heavier graph, less reproducible; fallback if the snapshot route fails). Costs, honestly: API churn 0.2.2 → zed-main-2026 across `app-gpui`'s views (core is gpui-free — the §1.3 exit seam holds); both `vendor/` patches re-validated or retired; the emoji path (D29, cosmic-text 0.14 pin) may move with the new tree's cosmic-text; flatpak SDK rustc must meet the new MSRV; renderer swap needs a real soak (the crash took ~30 min to surface). | Owner chose "Plan the wgpu migration" (Nov 2026) after the first blade-atlas death; monitoring alone rejected — the crash kills the whole app, and the upstream fix path is clear. Scope and steps: P5. |
| D36 | **Publish reliability: the full outbox (owner's explicit scope — "full outbox", after the minimal honest-failure option was rejected).** Evidence (Nov 3 2026): the owner posted a note that appeared in NO other app. Probes with a raw websocket client: the three healthy write relays (nos.lol, premium.primal.net, relay.primal.net) hold nothing past Nov 2 01:23 UTC, and the fourth declared write relay (relay.nostr.build — trailing slash in the kind:10002 tag) holds ZERO owner notes ever (auth-walled). Root cause: `pool.publish` did `let _ = send_event(...).await?; Ok(())` — it discarded the SDK's per-relay `Output{success, failed}` (success = `EventSendStatus::Ack`, i.e. OK true; OK false/auth-required/timeout land in `failed` with the relay's message). Per-relay failures produce a top-level `Ok(())`, so total publish failure was indistinguishable from success: the note saved locally, painted in the feed, and never left the app. The fix, built as the contract (not a patch): (1) `pool.publish` returns a `PublishReport { ok, failed }` and logs every relay's outcome — decision = ≥1 relay ACKed; (2) `commit()` (the single funnel ALL publishes ride — notes, reactions, deletions, relay lists, restores) became: save to store FIRST → enqueue to the outbox WRITE-AHEAD (synchronously persisted, so a crash mid-publish still retries) → publish → ≥1 ACK dequeues, zero keeps the item and surfaces `Failed`; (3) `outbox_queue.rs`: persisted pending queue (deletions.rs sidecar discipline: 0600 temp+rename, bounded 500, dedupe), backoff retry 30s doubling cap 10 min, indefinite, survives restart; replaceable kinds (0/3/10000-19999, 30000-39999) queue under their COORDINATE with newest-version eviction — a retried relay list can never resurrect a superseded set; ephemeral kinds (20000-29999) never queue; (4) ONE drainer task (spawned once per process, armed on first bootstrap) owns all retries serialized — a login drain cannot double-fire against an in-flight backoff (the per-item-loop sketch could); logged-out stretches reschedule +60s; (5) write-deadness learning: `relay_latency.record_write_result`/`is_write_dead` (3 consecutive failures drop a relay from the write fan-out, success resets; the all-dead fallback keeps attempting every relay so revival is always possible) — relay.nostr.build stops poisoning every publish; (6) the write set normalizes trailing slashes (`outbox::relay_key`) before fan-out. The composer shows Committed only on first acceptance; a later outbox success is logged, not re-toasted. | Owner: "full outbox" (Nov 3 2026), after seeing the swallowed-publish evidence. |
| D37 | **Publish reliability round two + the round-four audit (Nov 3 2026).** DELIVERY EVIDENCE: the three stuck notes (6d904c9d "and I can't wait!", 14838c33, 5d415881 "does this work? who knows lol") — recovered out of the LMDB store with a hand-rolled flatbuffers reader (nostr-lmdb 0.45 stores events as flatbuffers: vtable 18, POSITIVE soffset, id/pubkey/sig INLINE, tags as StringVector tables), hash-validated against the NIP-01 id, injected into the outbox queue file — went out through the drainer ("delivered after 4 queued attempt(s)") and are INDEPENDENTLY CONFIRMED on nos.lol and relay.primal.net by raw websocket probes; queue file ended empty. Two root causes under D36 found and fixed en route: (a) `pool.publish` never registered WRITE capability — the comment said "write relays join at publish time" but nothing joined them, and nostr-sdk 0.45 gates every send per relay ("write actions are disabled" on all five targets); the renderer migration's re-lock bumped the nostr stack and started enforcing the gate (notes worked Nov 1-2, silently could not leave the app after). publish() now registers new targets `add_relay(url).capabilities(WRITE)` (upsert-MERGE — a relay that is also a read keeps both), tracks them in `Pool.writes`, connects only when something new joins. (b) the bootstrap warm-start branch (every restart within 1h of a full relay phase) hydrated only the READ half of the own kind:10002 — `Active.write_relays` stayed empty, so commit AND drain fanned out to the indexers instead of the declared write relays (this is how the recovered notes went out); the warm branch now hydrates both halves ("write set from store: N relay(s)"). Queue writes now fsync before the rename (the write-ahead claim survives a power cut; a reverted rename at worst resurrects the previous queue — republish, relays dedupe by id). AUDIT, all closed or accepted: bounds hold (queue ≤500 items, backoff ≤600s, drainer serialized, idle Arc dropped before its sleep, shutdown-safe via Weak — no busy-spin: a logged-out reschedule makes the item not-due before the 5s poll); security holds (relay-supplied reasons sanitized before logs — control chars stripped, 120 cap, log-forging closed; queue file 0600 temp+rename create_new; no event content or keys in logs); funnel holds (exactly two `pool.publish` sites: commit() and the drainer). ACCEPTED: a commit() publish slower than backoff(1)=30s can overlap the drainer firing the same event — relays dedupe by event id; premium.primal.net (declared write relay) accepted none of the three notes (likely auth-walled) — write-dead learning drops it after 3 consecutive failures, revival stays possible via the all-dead fallback. KNOWN, no code change: gpui-pre 0.3.9's `AsyncApp::update` PANICS on a released app (`.expect("app was released before async operation completed")`) where 0.2.2 returned Err — shutdown-window-only risk in async completions that outlive the app; it can abort during exit, never during use. Live sample at boot+1m: RSS 549MB (HWM 575MB), threads 50, fds 85, 37 ESTAB — at/below the round-three baseline (733→820MB over browsing, threads 55-61, fds 150). | Owner: "run another performance/resource usage/memory leak/security review and audit and fix anything that needs to be fixed" (Nov 3 2026). |
| D38 | **The library scope: profile, bookmarks, drafts, lists, video posters (Nov 3 2026).** Owner asks after the D36/D37 outbox shipped. Inventory first — more existed than the asks implied: the profile view (banner, NIP-05 badge, Notes/Photos/Videos tabs, embedded feed) was built and pubkey-parameterized but own profile had NO rail spot; bookmarks had full core semantics (kind 10003 private-by-default, nip44-encrypted content, public-tag merge, feed toggles with optimistic rollback) but no surface; the composer stashed one session-scoped draft in memory (web app note.ts saveDraft); video tiles had the D32 poster ladder (publisher poster → blurhash → GStreamer first-frame sniff) but the app's OWN uploader embedded no poster in imeta (url/m/dim/x only), so app-posted videos always depended on the sniff — bare under strict Tor, failing on moov-at-end files — and degraded to the play-glyph-plus-filename row the owner called ugly. Owner decisions: profile = FULL (rail spot + kind 0 edit form + follow/mute on profiles); bookmarks keep private default AND gain a public option (NIP-51 kind 30001 d="bookmarks", display merges both, a settings pref governs what the star writes); drafts = SYNCED ENCRYPTED (one NIP-78 kind 30078 d="satori/drafts", content nip44-encrypted draft array — text + reply/quote context, attachments not stashed in v1); nav = avatar-as-profile-spot + ONE Library pane with Bookmarks/Drafts/Lists tabs; video posters = upload-time first-frame grab (GStreamer, local decode, second blossom blob, imeta `image` attr) + the bare tile restyled to a clean 16:9 panel with centered play mark, filename text gone. NO AUTOPLAY — tap still gates playback, unchanged. All publishes ride commit() (outbox coordinate-keys 10000-19999 and 30000-39999 already). Sequence: profile → library/bookmarks → drafts → lists → video posters. V1 clarifications made during the build (Nov 3 2026): drafts sync on composer CLOSE (any non-poll compose; the (reply, quote) id-pair is the slot, top-level = both None; an empty buffer DELETES the slot; a reopen spends it — a close re-saves), the session stash still covers the instant reopen; the Lists tab is READ-ONLY (the ask was "see their lists" — lists are made in other clients; own kind 30000s, member view, profile links); the poster grab runs over the FULL attachment (moov-at-end files poster too) on the background pool, degrading to the bare imeta on failure. V1.1 (Nov 3 2026, owner: "my drafts and lists are both empty"): nothing ever FETCHED own kinds 30000/30001/31234 (the bootstrap arm pulled only reactions/reposts/16/10003), and the web app's synced drafts are NIP-37 kind 31234 WRAPS (content = nip44-to-self of the inner draft event, d = draft id, empty content = deleted), not 30078 — the bootstrap arm now fetches own 30000/30001/31234; 31234 wraps hydrate into the draft array as id-marked entries (newest wrap per d, deduped, empty/expired skipped; note→content, article→title+body, poll→question); opening/deleting an import republishes the wrap empty (NIP-37's delete — the conversion into a Satori slot draft); save_draft's retain never clobbers id-marked entries; own_lists does a relay round before the store read, newest-per-d. V1.2 (same day, owner: drafts open/delete slow, lists still empty): save_draft/drop_draft go OPTIMISTIC — the in-memory array spends synchronously, the publishes ride behind, a failed publish RESTORES the spent entries (the "failed save leaves the slot intact" semantic survives, minus the wait); the composer open only waits on the local context reads. The empty Lists tab was a RACE: the one-shot fetch's SDK-side saves are async and can still be landing when the store read runs, and the first-open flag pinned the empty result — own_lists now saves every fetched event EXPLICITLY before the read, re-queries on EVERY tab visit, and delivers TWICE — a store-only snapshot the tab paints instantly, then the post-fetch merge — with [core] diagnostics at each stage (owner confirmation: the lists appeared on a second visit of the v1.2 build; the two-stage delivery makes even the first visit instant); and widens to kind 30001 (except d="bookmark" — that one is the Bookmarks tab's list); a private list's members (nip44-to-self tag-JSON content) decrypt for the detail view, and NostrList::display percent-decodes the d (Bloom's folder ds encode `/` as %2F); the merge sends ONLY on Ok (an error's empty-default never overwrites the painted snapshot). The prune (memory H-2) now SPARES the own bookmark set — its 90-day sweep was deleting the bookmarked notes the Bookmarks tab renders (349 gone at one cold start), and bookmarked_notes saves its indexer refetch explicitly. V1.3 (same day, owner: "lists work but it takes forever for them to load"): the store snapshot was decrypting EVERY private list's members at tab-open — 78 serial nip44 rounds (worse on a bunker signer, one network trip each) blocked the paint. Decryption goes LAZY: NostrList carries its source `event`, lists_from_store never decrypts (rows paint from plain tags alone), and a new core decrypt_list_members(ev) runs ONE decrypt per detail-open — the detail fills in the people as they resolve; row member-counts hide at 0 (a private list's size is unknown until opened). V1.4 (same day, owner picks the next pair: "nostr: links, draft attachments"): the URI half was already DONE — D27's seam verified end-to-end (parser/socket/routing/argv tests green, the flatpak install already owns the scheme, a second launch forwards its URI to the live app and exits; install-local.sh now also runs xdg-mime default to CLAIM the scheme after a local install) — so the work is the attachments half. Drafts could not carry attachments because the composer held RAW BYTES and uploaded only at post time (megabytes cannot ride a nip44-encrypted NIP-78 array — the v1.3 lesson at attach scale). The upload moves to ATTACH time: picking a file uploads it to Blossom immediately (sequential chain per batch, the poster grab riding its own upload exactly as D38 specced, a failed blob removing its tile with a word), the composer holds the finished NIP-92 imeta, and the synced DraftEntry gains `attachments` — the imeta arrays VERBATIM. Publish embeds them unchanged; a restore (session stash, synced reopen, Library prefill) rebuilds the tiles from the imetas and fetches the CDN copies through the media cache for the previews — no bytes, no re-upload; legacy entries deserialize to empty (the round-trip test pins both). A note pressed while an upload is still in flight is told to wait (the seal re-arms when the chain lands). | Owner: "we need users to be able to see their profile page, their drafts, their bookmarks, and their lists" + "preview of videos in timelines... no autoplay still" (Nov 3 2026). |
| D39 | **Relay hints + redundancy 3 + REQ chunk ceilings (WS1 of issue #1, Dark Wisp ports; Nov 7 2026).** Three outbox-model gaps closed, porting Dark Wisp (`barrydeen/dark-wisp-android`) semantics: (a) **Author-keyed relay hint store** (`crates/core/src/hints.rs`, `repo/RelayHintStore.kt`): per-AUTHOR relay index built from (a1) event provenance — which relay DELIVERED an event, recorded via `hints::observe` at every ingestion point next to `seen::record` (fetch_route, get_from, the live bell) — and (a2) `e`/`p`-tag relay urls at index 2, attributed to the REFERENCED pubkey (`extractHintsFromTags`). Caps as the Kotlin store: 5 hints/author, LRU 5000 in memory, 2000 persisted; indexers and non-routable urls never record (the `routable_relay` screen at record time, since hints REPLAY into future routed fetches). Persisted as the D33 sidecar discipline (`relay-hints.json`, 0600 temp+rename, 10s debounced flusher). Consumer: `hint_seeded_lists` in pool.rs seeds an author's routing list from their hints when their kind:10002 is missing/empty — in BOTH `get_routed` and `subscribe_routed` — so a listless author routes to relays we've actually seen them on before falling to the indexers. (b) **Redundancy ≥3 per followed author** (`repo/RelayScoreBoard.kt` `recompute()` MIN_REDUNDANCY = 3, ~line 88): `outbox::RELAYS_PER_AUTHOR_TARGET` raised 2 → 3, with the existing `want.min(n)` graceful degradation to however many relays the author actually has (no new behavior needed — the greedy already routed to min(target, list length)); MAX_RELAYS_PER_AUTHOR was already 3 considered, so no cap change. (c) **REQ chunk ceilings** (`repo/OutboxRouter.kt` ~lines 20–31: MAX_AUTHORS_PER_FILTER = 200, MAX_ETAGS_PER_FILTER = 150 — big relays reject "total filter items too large"): the 200-author ceiling already existed (`outbox::MAX_AUTHORS_PER_FILTER`, used by fetch_route/subscribe_routed/relay_lists); the missing **e-tag ceiling added** (`outbox::MAX_ETAGS_PER_FILTER = 150`) and enforced at the two `.ids(missing)` targeted-fetch sites in manager.rs (whisper parents, bookmarked notes) via `outbox::chunk` — note_by_id's single-id path needs no split. Unit tests: hint store (provenance caps/screens, tag-hint attribution, observe, LRU), redundancy target (3 relays → 3, 5 → 3, 2 → 2), chunk ceilings. | Owner: implement WS1 of GitHub issue #1 (Nov 2026), citing the Dark Wisp reference files. |
| D40 | **Viewport-scoped engagement subscriptions (WS2 of issue #1, Dark Wisp `FeedEngagementState.kt` port).** The feed's engagement is now LIVE for the notes under the viewport, without subscribing engagement for the whole timeline. New `crates/core/src/viewport.rs` is the pure half: the window math (`window(len, visible_start, visible_end, buffer=8, max=50)` — clamps to the feed, expands by an 8-row buffer each side, caps at 50 ids anchored at the range head), id selection (`ids_in_window`), filter chunking (`chunks_for_ids` — outbox::chunk at the D39 150-e-tag ceiling; 50 < 150 so in practice ONE filter) and the subscribe-replacement bookkeeping (`ViewportTracker::plan/commit` — a same-set request in any order plans `Keep`, empty plans `Drop`, anything else `Replace(shards)`; a FAILED arm commits empty so the next change re-arms — bookkeeping always matches the relay's view, no leaked REQs). The live half is ONE manager task per process (started on first `set_viewport`, like the outbox drainer): a `watch` channel carries the feed's settled id set; the task selects between `changed()` and the client's shared notification stream, closes the current subscription BEFORE arming the next (never two live viewport subs — D24's churn rule), and delivers matching events through the SAME ingestion discipline as the bell (seen::record, hints::observe, kind:5 tombstones / NIP-40 / NIP-09 gates) — the SDK has already saved the event, then `Live::Engagement` fires on the existing broadcast (a new variant; the shell ignores it, the feed re-seeds its own-engagement mirrors + touches the snapshot — no new rendering path, the store → notify chain repaints). Pool gains `subscribe_for_ids` (one subscription id over the `#e` filter shards; engagement authors are ARBITRARY so outbox author-routing does not apply — the filters ride the connected read set, same targets as the bell's filters). Feed side: the scroll stream (and every landing) bumps a generation counter; a 400 ms settle timer pushes the window only if still current — fast scrolling coalesces into one `set_viewport`; `Drop` for the feed entity sends the empty set so a closed pane closes its subscription; sign-out does the same. Distant rows keep the existing per-paint/store loads as the fallback — the sub only ADDS liveness. Unit tests: window clamping, id slicing, chunk ceiling, tracker keep/drop/replace bookkeeping (incl. failed-arm retry + cap/dedup). | Owner: implement WS2 of GitHub issue #1 (Nov 2026), citing `viewportRange`/`distantSubscriptions`. |
| D41 | **Trust posture: web of trust + duplicate-content spam filter (WS1+WS2 of issue #2; Nov 7 2026).** The 0.3 trust release lands the posture agreed in issue #2 — **inform in the feed, act only in the margins, audit every action** — with no ML, no NIP-85 trust providers, no telemetry, and NO feed-hiding switch (a feed never removes; annotation is the WS3 phase). (a) **The Verdict vocabulary** (`crates/core/src/wot.rs`, port of Amethyst's `TrustVerdicts.kt` shape minus the provider arms): `SelfUser / Follow / Trusted / NotInNetwork / Unknown` — the single word-list every current and future gate consumes. The load-bearing distinction is documented in the enum itself: **Unknown = the graph can't say yet, NOT a judgment; NotInNetwork = known, and outside.** "Unknown never reads as outside" is the rule that keeps feature-off a first-class state. (b) **The two-hop qualified set** (Dark Wisp `ExtendedNetworkRepository.kt`): an author is `Trusted` when ≥ `QUALIFICATION_THRESHOLD = 10` of the user's follows also follow them (`THRESHOLD = 10`, ExtendedNetworkRepository.kt:85). Built from kind-3 contact lists ALREADY flowing through the store — no crawl, no SQLite port (research §4: heavy for a client with LMDB). In-memory projection, rebuilt at bootstrap from the store's cached kind-3s (`store::contact_lists`, the `relay_lists` batching pattern) + incremental `wot::observe` on every new kind-3 (the pool chokepoint and the live bell — Amethyst `WoTService.applyKind3`: only own+follows' lists apply, newest-wins diff-based replace). Bounds (D24 discipline): follows cap `MAX_FOLLOWS = 2000` SHUTS THE FEATURE OFF (Amethyst WoTService.kt:302 — verdicts degrade to Unknown, never to a wrong "outside"), per-event contact cap 5000 (WoTService.kt:305), tracked second-degree targets capped at 50,000 (oldest-evicted; satori's own ceiling), source lists older than `STALE_SECS` = 24h contribute nothing (Dark Wisp `STALE_HOURS`), muted pubkeys never qualify (Dark Wisp filters them at qualification). (c) **Extended-network relay set-cover** (Dark Wisp `computeRelaySetCover`): a greedy weighted cover (`wot::relay_cover`, pure) over the qualified set's kind-10002 relay lists, computed by the manager at graph-rebuild time; `MAX_EXTENDED_RELAYS = 100` / `MAX_AUTHORS_PER_RELAY = 300` (ExtendedNetworkRepository.kt ~91). Consumed in pool.rs as a strictly ADDITIVE fallback tier — precedence unchanged: kind:10002 → D39 hints → **extended-network cover (new)** → indexers — and only for qualified authors with neither a list nor hints. (d) **Duplicate-content filter** (`crates/core/src/dedup.rs`, port of Amethyst's `AntiSpamFilter.kt`): duplicate key = content hashed WITH its tags (the Kotlin `31 * content.hashCode() + tags` — identical replies to different people don't collide); recent-key LRU 2000, spammer-record LRU 1000 (Amethyst's constants); a spammer's count accrues per DUPLICATE with per-relay attribution (`relayStats.newSpam` shape) and at `DUPLICATES_TO_HIDE = 5` (`Spammer.shouldHide`, Spammer.kt:38) their events stop passing the D34 `consume_and_filter` chokepoint in pool.rs — the one door every network read walks through — and never ping the bell. Exemptions (AntiSpamFilter.kt): the user themself and their follows, plus the short-content floor (60 chars — "GM" false positives; short `nostr:` bot commands; repeated long invoices/phishing urls are NOT exempt). Settings gate `dedup_enabled` in prefs.json, **default ON** (the issue's second allowed auto-action: objective + bounded), read through the config cache like `machine_notes_hidden`. Snapshots (`dedup::snapshot`) carry author + count + relays + last-seen — the receipts the SpammingUsers surface reads (WS3/WS4 UI lands next phase; core accessors `Core::verdict` / `Core::shared_follows` / `Core::spam_report` are in). One deviation, documented at the chokepoint: the nostr-sdk client auto-persists events before the pool sees them, so "stop being stored" is enforced as stop-being-RETURNED (filter + bell gate) — the same tombstone-shaped honesty as NIP-09 handling (D34), and strictly a subset of what the SDK already persists. Non-goals recorded: no ML scoring, no NIP-85 trust providers, no feed-hiding switch, no outgoing PoW (P5). Tests: verdict transitions (self/follow/threshold/not-in-network/unknown), shared-follow diff counting, threshold boundary (9→NotInNetwork, 10→Trusted), staleness, follows-cap shutoff, mute exclusion, target-cap eviction/refusal, greedy cover on small topologies, dedup hide-at-5th-duplicate / per-relay attribution / exemptions / receipts-before-hide. | Owner: implement WS1+WS2 of GitHub issue #2 (Nov 2026), citing `notes/wot-spam-research.md`. || D42 | **Four-lens audit of the 0.3 trust work (post-D41; Nov 7 2026).** Same methodology as D24 (performance / system usage / memory / security), read of every new file: `wot.rs`, `dedup.rs`, the `pool.rs` chokepoint + `extended_cover_seeded`, the `manager.rs` trust wiring / notification tier / receipts, and the app-gpui trust UI. **Fixed (bugs + quick wins):** (1) *bug, performance* — `wot::insert_target` tested target-order membership with `VecDeque::contains` (O(n), n ≤ MAX_TRACKED_TARGETS = 50k) per ingested tag; the bootstrap rebuild ingests every follow's kind-3 (potentially 10⁵–10⁶ tags), making it quadratic — seconds-to-minutes of CPU holding the graph mutex per list, ON the 2-worker relay runtime (bootstrap runs on `self.rt`). Fixed with an O(1) membership mirror (`Graph::in_order` HashSet); eviction and the drift-refusal path keep the map and the deque in step. (2) *quick win, performance* — `dedup::observe` computed the duplicate key (a serde_json serialization PER TAG) while HOLDING the global filter mutex, and AFTER a prefs read; the chokepoint sees every network event. Fixed: length floor → key computation → then the lock (exempt check inside, so exempt events still never join the LRU); `dup_key` also stopped cloning the whole tag list (`ev.tags.iter()`). (3) *quick win, performance* — `config::prefs()` deep-cloned the whole parsed JSON per call; the dedup chokepoint calls per event. It now returns the cached `Arc<serde_json::Value>` (all four call sites read through the deref unchanged). (4) *quick win* — `split_wot_tier` read the tier pref and locked the exempt set per EVENT; both hoisted to per PAGE (pref check first, verdict before the lock). (5) *quick win* — formatting scars (`manager::is_liked` brace-mangled line, `dedup::clear`). **Verified non-issues (the prompt's own questions):** the cover tier does NOT add REQ volume beyond D39's ceilings — it only fills authors with neither a kind:10002 nor hints, and `outbox::route` still caps at MAX_RELAYS_PER_AUTHOR = 3; the cover correctly rides `RelayList.write` (what `route` reads, outbox.rs ~222). The dedup recent LRU evicts correctly (order/map stay in sync; FIFO-by-first-insertion by design, the seen.rs convention). Receipts do NOT grow unbounded: `SUPPRESSED` is REPLACED (not appended) at every notifications build with the union of both phases' pulls, deduped by id — bounded by the notif cache + page sizes. Spammer records are LRU-capped at 1000. Prefs IO on the hot path is one metadata syscall between file changes (cache holds). **Accepted watch items (no fix):** the bootstrap rebuild still runs its CPU on the relay runtime (consistent with every other store-fed bootstrap; the rebuild's own elapsed log now makes regression visible — with fix (1) it's linear); `relay_cover` worst case is O(picks × relays × served) with picks ≤ 100 — bounded, only at rebuild; the dedup LRU is FIFO-by-first-insertion, so a flood spaced > 2000 distinct keys apart escapes counting (matches the seen.rs convention and Amethyst's own memory shape); `dup_key` uses a deterministic `DefaultHasher` — adversarial 64-bit collisions could false-positive a duplicate (same trust level as the Kotlin `hashCode` port); `collapse_plan` is recomputed per render over ALL replies (a graph-lock lookup per row) — cheap at thread scale, batch it if threads grow; client-level `get` attributes dedup receipts to `""` (no single relay) — per-relay attribution is blank for those rows. Gates after fixes: check clean, clippy silent, 157 tests green, `satori-gpui` builds. | Maintainer commissioned the same four-lens pass as D24 over the D41 commits (Nov 2026). |
|| D43 | **Anti-bot amendment to the notification tier (D41/D42; Nov 7 2026).** Maintainer-approved amendment of issue #2's posture for the one place bot economics actually bite: **bot replies are the cheapest notification attack** — a farm of fresh keys each replying once to your notes is invisible to the duplicate-content filter (D41c dedups per author's duplicates, not across authors) and invisible to the tier (each author reads Unknown — "the graph can't say yet", never a judgment). Two bounded paths, both landing in the existing SUPPRESSED receipts store so the "see why" surface stays honest (demote-not-delete). (1) **The tier extends to kind-1 replies**: where `split_wot_tier` gated only kinds 6/7 (engagement noise) from `NotInNetwork` authors, it now also gates **direct replies to the user's notes** — a kind-1/1111 event whose `e` tags reference one of the user's recent note ids (the same `my_notes` set the notifications pipeline already fetches); mentions (`#p` only) and zaps (9735) keep passing. Same receipts path, same phase-equality rule (cached phase wears the gate; the final union re-splits and writes the store). (2) **Reply-velocity verdict for Unknown authors** (`crates/core/src/rate.rs`): an Unknown author whose replies to the user's notes reach `REPLY_VELOCITY_LIMIT = 5` within `REPLY_WINDOW_SECS = 30 * 60` is treated as a mule — the 5th reply in the window is suppressed with reason "5 replies in 30 min", and every further reply inside the window joins it; 1–4 pass untouched. Discipline (D24 style): named consts; per-author timestamp ring capped at `AUTHOR_CAP = 1000` authors (oldest-inserted evicted, the `wot::insert_target` shape); the counter is fed ONLY by delivered replies (`admit_reply` runs in the final-union split alone — suppressed replies never join, so suppression is not self-justifying; the cached-phase split checks the gate via `reply_flooded` without counting); the window self-resets (stale stamps evict on every check); cleared at `teardown_session` alongside the verdicts and the receipts (per-session identity state — a different account never inherits a mule verdict). NotInNetwork replies are handled by rule 1 regardless of velocity; "show anyway" exemptions pass both paths (the exemption is a conscious per-session act). **Posture note:** this amends issue #2's "direct attention passes" rule — replies from NotInNetwork authors and velocity-flooded Unknowns no longer pass by default — justified by the receipts (demote-not-delete, click-through, per-item reason) and the threat model above; mentions/zaps and Unknown authors replying at human rates are untouched, and `notif_wot_tier = false` still disables everything. Tests: tier gates NotInNetwork replies + reactions (mentions/zaps pass), Unknown single reply passes, 5-in-window trip with the reason string, suppressed replies don't feed the counter, window reset, author cap. Gates: check clean, clippy silent, tests green, `satori-gpui` builds. | Maintainer-approved amendment to the D41 trust work (issue #2), Nov 2026. |
| D44 | **WoT bootstrap source fetch (D41 amendment; Nov 7 2026).** D41 built the two-hop graph from the store's cachedkind-3s — but nothing FETCHED those lists: they only entered the store if they happened to arrive with normal reads, so a fresh install read "253 follows · 0 qualified" forever and every verdict was Unknown (the trust system inert; confirmed by the maintainer's screenshot on issue #2). Fix: at the bootstrap graph rebuild (`manager::rebuild_wot_graph`), after the store-fed rebuild, the manager NOW proactively fetches the follows' contact lists — selection pure in `wot::fetch_targets(follows, cached, now, cap)`: a follow whose cached kind-3 is MISSING or older than `STALE_SECS` (24h; a fresh list is never re-fetched), first-come-first-served in follows order (deterministic shortfall). D24 ceiling `wot::MAX_FETCH_TARGETS = 400` authors per pass (two D39 outbox shards of `MAX_AUTHORS_PER_FILTER = 200`; the fetch rides `pool::get_routed` on `Kind::ContactList`, the existing read-tier routing — kind:10002 → D39 hints → cover → indexers, unchanged). ONE pass per launch, fire-and-forget on the shared runtime like the other bootstrap fetches; NO retry loop/timer — the next bootstrap retries what this one missed (bounded, honest). **Re-qualification path: NO second rebuild.** Results land through the pool chokepoint, whose `consume_and_filter` already runs `wot::observe` on every kind-3 any read returns — diff-based, newest-wins — and verdicts are computed from the graph's `scores` on demand at query time, so authors re-qualify incrementally as fetched lists arrive; the only alternative (one rebuild after the fetches settle) would re-pay the D42-noted bootstrap CPU for state `observe` already applied. Skipped when the graph is OverCap (`WotStatus`) — fetching sources feeds nothing. Tests (wot.rs): only-stale/missing selection (fresh list not re-fetched, exact-24h boundary fresh, non-kind-3 cache entries never count), cap truncation in follows order, ceiling composition with the outbox author ceiling (400 → exactly two 200-shards). Gates: check clean, clippy silent, tests green, `satori-gpui` builds. | Amendment to the D41/D42/D43 thread from the issue #2 maintainer report (Nov 2026). |
| D45 | **WoT: age excludes nothing; the applied sources persist (D44 amendment; Nov 7 2026).** Two defects probed from the maintainer's live logs (`fetching 251 → returned 98 → graph now: 100 applied, 4831 scored, 0 qualified`): (1) *the staleness rule was applied to the wrong thing* — `observe()` scored a follow's contact list only if it was <24h old, but kind-3s are not freshness-sensitive data (most users rewrite them every few months; the newest known list is the best known truth however old). Dark Wisp's `STALE_HOURS` is a crawl cadence, not a validity window. `STALE_SECS` is now **7 days and drives only the REFETCH decision** (`fetch_targets`); `observe()` scores every applied list regardless of age, and the old-contribution diff is unconditional (everything applied was counted when it landed). (2) *the SDK's one-shot fetch does not persist* — probed: 98 lists fetched one session, 2 in the store the next, so every restart refetched the whole graph from scratch and the graph lived and died inside one session. New sidecar `wot-sources.json` (config::files, 0600 temp-sibling + rename, 10s debounced `park_timeout` flusher — the hints.rs discipline): the applied sources (newest `MAX_FOLLOWS` slice) persist; `wot::apply_persisted` replays them at bootstrap BEFORE the store's cached kind-3s (which win on newer through observe's newest-wins diff), and `fetch_targets` merges sidecar + store timestamps so a restarted app refetches only genuinely missing/stale lists. Tests: `stale_sources_still_qualify` (the D45 law, replacing the old `stale_sources_do_not_qualify`), `persisted_sources_replay` (signed-out/not-follow refusals + application). | Maintainer: "shouldn't these persist app restarts so we're not having to rebuild from scratch every time? let's discuss" (Nov 2026); log evidence as above. |
| D46 | **Trust arms from local state before the first notifications paint (D45 amendment; Nov 7 2026).** Probed by the maintainer: spam bots showed in notifications for ~a minute after each app start, then vanished. Cause: bootstrap fetched the own contact list FRESH from relays (a round-trip with a read budget) BEFORE `set_identity`/`rebuild_wot_graph` — until the graph armed, every verdict read Unknown and the notification tier passed everything, so the cached phase painted ungated and the next refresh cleaned it up. Fix: bootstrap now arms the vocabulary (identity, mutes, dedup exemptions) + rebuilds the graph (sidecar + store's cached own list — local reads, ms) BEFORE the fresh fetch; the fresh `follows()` then re-arms with a newer list if it has one (rebuilds are idempotent, observe is newest-wins). First-paint gating is correct from launch; the cold start (no store list, no sidecar) is a first-launch-only event with nothing to gate anyway. | Maintainer: "on app start, they show up until everything runs. let's discuss because that's not a great user experience" (Nov 2026). |
| D47 | **The store door closes: cached notification builds consult the dedup ledger; velocity counts first contact only (D43/D46 amendment; Nov 7 2026).** Probed by the maintainer: spam replies kept ringing the bell until several pane visits. Two defects: (1) the dedup chokepoint only gates DELIVERIES — the SDK had already persisted the bots' earlier replies, and the cached notification phase read them straight back (D41's "unreachable through any read path" was false at the store door). Fix: `dedup::is_hidden` (false while the filter pref is off — the switch stops the deciding, not just the catching) is consulted in `split_wot_tier` for EVERY bell candidate, both phases — hidden authors' store-persisted events land in receipts with the reason "duplicate-content filter". (2) the velocity counter was fed per DELIVERY — every pane visit re-runs the relay phase over the same bot replies, so re-deliveries re-fed the ring and a bot tripped on visit three instead of being judged on first contact. Fix: `admit_reply` takes the event id; a counted-events LRU (`COUNTED_EVENT_CAP = 10_000`) makes re-deliveries no-ops — the ring only grows on first contact, and the flood verdict is a property of the bot's behavior, not the user's navigation. Tests: `redelivered_replies_count_once`; the receipts store already deduped by id. | Maintainer: "i still see spam replies. i have to go to and away from the notifications page a few times before they dont show up anymore" (Nov 2026). |
| D48 | **The anti-spam ledgers persist: verdicts outlive the session (D47 amendment; Nov 7 2026).** Probed by the maintainer after D47: "still spam replies first frame" — because every gate D47 consulted was per-session MEMORY. The dedup spammers map, the velocity stamps, and the receipts store all start empty at boot, so the first notifications frame gated from zero knowledge: `is_hidden` false for everyone, velocity untripped, and the store replayed last session's spam until relays re-taught the ledgers (about a minute of wrong state, every launch). Three sidecars in the hints/wot-sources discipline (0600 temp-sibling + rename, debounced `park_timeout` flusher, bounded): `dedup-state.json` (spammers + the recent duplicate-key LRU — identical content re-delivered after a restart still counts, no fresh streak), `rate-state.json` (stamps + counted event ids — a mule that tripped the gate before a restart stays flooded while the window lives; the window still self-resets), and `suppressed-notifs.json` (the receipts themselves, newest 200). The load-bearing rule: **once receipted, still receipted** — `split_wot_tier` re-suppresses any candidate whose id is in the ledger, so a verdict that EXPIRED (the 30-minute velocity window) while the event sat in the store cannot resurrect old spam at boot; the show-anyway exemption is the per-session override and now outranks every auto-gate INCLUDING the dedup store door (previously the door ignored it — the button lied for duplicate-content receipts). The manager loads all three at the bootstrap arm (guards make re-arms no-ops; sign-out `clear()` wipes memory only). Also: the shell's bell check fires once at STARTUP (not 15 minutes in) — the store warms so the pane's first paint is complete and the dot reads the truth from boot (the "notifications from actual people took a sec to load" half of the report). Tests: `persisted_ledger_reloads`, `persisted_stamps_reload`. | Maintainer: "still spam replies first frame … also, the notifications state is stale. there were notifications from actual people that took a sec to load" (Nov 2026). |

| D49 | **Attachment routing corrected: a note with media IS a note (issue #3 WS1; Nov 8 2026).** Preparing 0.4 surfaced that the GPUI composer collapsed the web app's routing: any attachment went out as a NIP-68 kind-20 picture event (`composer.rs`'s "pictures when there are imetas"), losing `signNote`'s imeta parameter (publish.ts:47 — the note composer's attachments rode the kind-1) and silently breaking reply-with-attachment (`post_picture` takes no `reply_to`, so an image on a reply published as a top-level kind 20 with the thread context gone). No decision ever recorded the collapse (the whole §8 log was checked); the feature table (§3, "Posting") already listed picture as its own surface. The correction, per the owner ("most clients do kind 1 + imeta … if it's just a note with an attachment it's kind 1 + imeta, but if the user chooses to post a photo post then we do it as a kind 20"): note attachments (photo or video) ride the kind-1 as NIP-92 imeta tags (`note_tags` gained the imeta parameter, in signNote's tag order); NIP-22 comments (kind 1111, the reply route for non-kind-1 roots) carry imetas too — NIP-92 is event-kind agnostic, and dropping them would silently lose attachments; kind 20 is reserved for the Photo tab (WS2, the web app's Picture composer restored: top-level only, images only). `schedule_note` and `save_draft`/`DraftEntry` carry an explicit `photo_post` flag (serde default false, skipped when false so the legacy shape stays byte-stable; web-app imports restore as notes) instead of inferring the kind from "imetas present". The kind-20 tag build is deduped into `picture_tags`, shared by the instant and scheduled paths. Hygiene: `store.rs`'s 21/22 comments were inverted vs NIP-71 (21 = short vertical video, 22 = video); the DraftEntry doc still said "attachments are not stashed in v1" (superseded by D38 v1.4). Tests: `note_tags_carry_imeta`, the `draft_entry_attachments_round_trip` photo_post asserts. | Owner: "most clients do kind 1 + imeta. what we should do is let the user choose" (Nov 8 2026); the unlogged-collapse finding: git-history comparison of signNote vs the GPUI routing + the §8 log sweep; issue #3. |

| D50 | **DNS-rebinding-safe media resolution: the guard moves to the address list (issue #3 WS4; Nov 8 2026).** The media SSRF guard checked HOST STRINGS — `is_safe_media_url` rejects literals and internal names, and F5 re-validates every redirect hop — but a public DNS name resolving to `127.0.0.1`/`169.254.169.254` sailed past both: the guard never saw an IP. Fix: the shared media client now runs a custom resolver (`media.rs`'s `SafeResolver`, reqwest's `dns_resolver` hook), and since reqwest dials WHATEVER its resolver returns, the check lives on the address list — only global-reachability addresses survive (`is_global_ip`, std's `is_global` still feature-gated: the RFC screen done explicitly — private/loopback/link-local/CGNAT/this-network/benchmark/documentation/reserved/multicast for v4; ULA/link-local/multicast/anything outside 2000::/3 for v6, with v4-mapped v6 screened through the v4 rules because dual-stack dials the embedded v4). Fail CLOSED: a name resolving to a public-and-private mix fails the WHOLE resolution, so no leg of it leaks onto the internal network. Every connection this client opens — first party, every redirect hop, every retry — re-walks the resolver, so a rebinding rotation between hops re-runs the check; TOCTOU-free by construction (the addresses that passed the filter are the addresses hyper dials). Scope note: the OTHER clients in core (upload.rs, nip11.rs, pool.rs) talk only to user-configured endpoints — deliberate choices, not third-party content — so the resolver rides the media client alone. Redirect re-validation was already in (F5), ticked with that note. Remaining SSRF set unchanged: Tor transport, NIP-B7 healing (M2/M4). Tests: `resolver_filter_keeps_only_global` (14 hostile spellings + mapped-v4 + global v6). | Issue #3 WS4; the D24 accepted-as-future-work item closed out. |
| D51 | **net_guard: paste-time screening of wallet-connect URI relays (issue #11; Nov 11 2026).** Every relay a `nostr+walletconnect://` or `bunker://` URI asks the client to dial is screened at paste time (`net_guard.rs`) by the same `routable_relay` rule the pool's routing applies: private/loopback/link-local addresses are refused before the transport ever dials. The hole: a pasted URI is untrusted input, and without the screen it could aim the wallet transport (or a bunker session) at an internal-network endpoint. One ruleset, deliberately NO user-exemption toggle — a dialog that asks "allow 192.168.1.10?" trains yes-through; the refusal verdict (`relay_refused`) names the offending relay URL so the connect dialog shows WHAT was refused, not just that something was. | Issue #11; D24's security lens flagged the class (untrusted endpoints reaching internal addresses). |
| D52 | **The publish door gates: NIP-40 refusal + own-kind:5 tombstone recording at the chokepoint (WS2, issue #4; Nov 11 2026).** `pool.rs`'s `gate_publish_and_tombstone` is the outgoing half of the D34 chokepoint: NIP-40-expired events are refused LOCALLY (the relays would reject them anyway, and the D36 outbox would otherwise retry a corpse forever; the report marks every target failed), and a kind:5 leaving through the door records its tombstone there, pinned (we signed it), so every delete path gets the gate for free (manager.rs's own pin stays: idempotent). | Issue #4 WS2; D34's inbound chokepoint's mirror. |
| D53 | **The video tab rides NIP-71 kind 22 only (issue #18; Nov 11 2026).** The composer's third tab publishes kind 22 (`video_tags`), instant and scheduled (the schedule rides the D49 `schedule_note` path with `PostKind::Video`); a note that merely carries a video attachment stays a kind-1 (D49's rule, unchanged). Kind 21 (short vertical video) is deliberately NOT built: there is no playback surface for the format yet; revisit with a vertical-video viewer. | Issue #18; D49's attachment-routing correction extended to video. |
| D54 | **One action row, worn everywhere a note renders (issue #16; Nov 11 2026).** The feed's action row is THE row (`actions.rs`: `ActionState` + the `ActionHost` contract): thread notes and notification reply/mention cards wear it; its state is optimistic mirrors reconciled by core's publish pumps and the D20 wallet broadcast, so the glyphs agree no matter which pane the zap started from. Receipt rows (reactions, zap receipts, poll votes) are not notes and get no row. | Issue #16 (maintainer report: "in a thread, the actions for notes are not available, only the Reply button"). |
| D55 | **The standing audit methodology gains a duplication lens (Nov 11 2026).** D24/D42's four-lens pass (performance / system usage / memory / security) is now five: every audit ALSO reads the change for duplicated code shapes (Fowler's Duplicated Code), because the 0.5.0 review found three copies of the same publish pump and a `(photo_post, video_post)` bool-pair API born within a single release cycle — extraction at birth is cheaper than at audit, and duplication is invisible to every other lens. Standing practice: when a review reports a shape repeated across hunks or files, extract it in the same cycle, not "for #21". | Owner directive ("i think we need to add a DRY audit to our list of things we audit"); the v0.5.0 two-axis review's three-pumps finding (Nov 11 2026). |
| D56 | **The rail's live dot is cut (Nov 11 2026).** The 5px pool-liveness dot at the rail's foot (0.2-design-spec §4.4: ink when any relay delivered/ACKed within CONNECTED_WINDOW_SECS = 300s) failed its own review: the maintainer daily-driving the app asked "what is that?", and the case for keeping it did not survive audit — the failure it guarded is self-healing (the §3.4 suspend watchdog recycles the sockets), or discovered at the point of action with more detail (the publish/zap failure whispers), the dot itself had no affordance (not clickable, no tooltip, doesn't route to the relay-health pane), and in normal use it read ink ~always (subscriptions deliver constantly), so it was noise, not signal. §1.3's rule applied: it didn't earn its place. Removed with the whole chain: the shell field + 30s poll, `Core::pool_connected`, `relay_latency::connected` + its window const; the write-ACK test reshaped to assert the per-relay facts (ok_at stamping, failure-doesn't-erase, write-dead at N) that outlive the dot. The durable connectivity surface is the settings pane's relay-health rows. | Maintainer: "i'm not sure the dot is necessary. even with the tooltip it seems useless" (Nov 11 2026); the tooltip had been offered and declined. |
| D57 | **One IP authority: `net_guard::is_public_ip` is the strict union (M5 WP1; Nov 12 2026).** The public-unicast ruleset existed twice — media.rs's `is_global_ip` (resolver-side, D50) and the paste screen's `is_public_ip` (D51) — and had already drifted: the paste copy lacked TEST-NET-1/2/3, 198.18/15, 192.0.0.0/24 and 2001:db8, so `wss://[64:ff9b::192.0.2.9]` (a NAT64 literal riding an embedded TEST-NET address) was routable on main. One authority now in `net_guard.rs`, the strict union: the classifier also sees through the transition mechanisms (NAT64 64:ff9b::/96, 6to4 2002::/16, Teredo 2001::/32 with both legs, client obfuscated per RFC 4380) to their embedded IPv4. media.rs's private resolver copy is deleted; the media client resolves through `net_guard::SafeResolver`, so the two screens cannot drift apart again. Riding along for WP1 (first consumers land with WP2's screening transport): `resolve_pinned_public` (fail closed on the WHOLE name — one internal answer poisons it, no partial dials), `NetGuardError` (`Resolve` is weather, `NonPublic` is the guard speaking), and the user-relay exemption ledger (`allow_direct_relay` / `user_chosen_relay` — hosts the USER typed dial direct; remote kind:10002 and hint hosts can never join the set). The paste-time screens deliberately do NOT consult the ledger (D51's no-user-toggle holds). Note the classifier tightening also means a transition-mechanism address with a PUBLIC embedded leg (`64:ff9b::8.8.8.8`) now passes where media's old outside-2000::/3 rule refused it wholesale — wanted: that is a genuinely reachable address. | PR #9 (authored without compilation on-device; container gate applied at merge: fmt/check/clippy clean, satori-core 154/154); two-axis review Nov 12 2026 (nits carried to WP2: dead test-only import in media.rs, 100/8-vs-100.64/10 comment mismatch, port-less-host normalization, Resolve/NonPublic test pinning, the fail-closed mix rule's two shapes). |
| D58 | **The paste screens MAY ask: D51's no-toggle rationale is superseded by an explicit, revocable, persisted consent (issue #11; Nov 12 2026).** D51 refused the "allow 192.168.1.10?" dialog because a yes-through dialog trains yes-through. The maintainer supersedes that stance (Nov 12), taking issue #11's belt-and-braces pass WITH the exemption: a screened URI relay may be allowed from the connect dialog, under four conditions that keep the training risk bounded: the consent PERSISTS (bunker path included, not just the NWC `Conn.direct` re-seed), it is REVOCABLE from the settings' relay surface, the dialog copy is honest that the ledger is GLOBAL (the allowed host is exempt for every relay dial, not just the URI that asked) and names the refused URL (D51's evidence requirement carries over), and refusals are never silent (the wallet resume path emits the same named refusal the connect path does). The no-second-ruleset rule stands: the consent flow consults the ONE ledger (`user_chosen_relay`), not a parallel set. Sentinel hosts ("(unparseable)") are never granted. The DNS-rebinding window between paste-time verdict and the transport's own resolution stays an accepted residual until WP2's screening transport dials the pinned addresses. | Maintainer decision superseding D51's rationale (Nov 12 2026, "Supersede D51, keep consent"); PR #25 carries the implementation, review conditions posted on the PR (persistence, revocation, honest copy, no silent refusals, the D55 extraction). |
| D59 | **0.7.0 is the interoperability pass: NIP-faithful surfaces where Satori is thin, tested against real-world fixtures (maintainer-approved plan, Nov 12 2026).** Amethyst's published event corpus is the fixture source and its NIP matrix the reference for scope, but the goal is spec conformance, not client mimicry. The spine: NIP-22 comments render + compose on every content kind (the biggest interop hole: kind-1111 threads are NIP-10-invisible here); NIP-68 picture posts (verified Nov 12: Satori already publishes kind 20 — manager.rs:4768, 5224, with NIP-71 title + content-warning tags — and renders kinds 20/21/22 as cards, so the scope reduced to a completeness check against fixtures and a picture-first feed-surface decision, both-directions publishing being already true); NIP-45 COUNT tallies (finishing #26's deferred counts strip); NIP-57 zap completeness (splits honored — paying only the author on a split note is a correctness bug; NIP-75 goals rendered; private-zap receipt parity verified); NIP-65 publish routing ("replies reach who they're for": own-write-relays-only publishing is Satori's worst interop bug — a reply to an author sharing no relay with us never reaches them; phase 1 adds parent-author inbox relays + seen-on relays + tagged users' inboxes, phase 2 adds deletion seen-on, profile everywhere, personal-kind opt-out; one-shot publish clients for foreign relays ride the net_guard screens; NIP-22 comments depend on this landing first; deliberately NOT copied: Amethyst's per-post relay-picker); NIP-36 sensitive content as a veil. Riders (render-only): NIP-84 highlights, NIP-30 custom emoji in content, NIP-24/14 subjects and titles, NIP-A0 voice / NIP-C0 snippets as attachments, an unknown-kind (NIP-31 spirit) sweep. Folded in: #25, #10 (when mptfire rebases), and issue #31's two structural extractions (engagement.rs, FeedSnap→thread) BEFORE the new surfaces build on the triplicated engine. A fixture corpus gate: each interop item lands with real published-event fixtures as satori-core tests; a verification pass (current split behavior, content-warning handling, custom-emoji state, unknown-kind fallthrough, relay-provenance tracking in the store, foreign-relay publish socketing) is the cycle's first PR. Explicitly deferred to their own releases: DMs (NIP-17 completion + NIP-04 read, security-sensitive enough for undivided attention), calendar/live/communities/Cashu, signer mode. The pre-0.7.0 clippy fix (nested-if memo eviction, CI's `-D warnings` form) is committed locally and rides the 0.7.0 push per maintainer instruction. | Maintainer: "let's come up with a list of things that should be in 0.7.0… good interoperability… they implement nostr specs pretty faithfully" (Nov 12 2026); scoping calls: DMs later, kind 20 both directions, fixtures first. |

| D60 | **The media discipline: window-scoped pins + no total-request timeout (0.7.0 click-through fixes; Nov 13 2026).** Three user reports traced the media pipeline end to end and produced two standing rules. (1) **Pins are window-scoped**: a pane pins exactly what is ON SCREEN (feed, profile strips, notifications — one `pin_window` called from render and the scroll stream), never its whole content set. #37's first pass pinned entire panes (profile's full notes list, notifications' 200 rows) and pinned entries were valve-immune: a photo-heavy profile held every decoded 24MP picture (~96MB RGBA each) past the caps forever while the render sweep queued the whole history through the six-slot gate. Bounded-content exceptions stay (thread: anchor + replies; the shell rail avatar). The desperate valve (2×96MB) gains a LAST-RESORT pinned victim — the oldest pinned entry dies when every over-budget candidate is pinned (a flash on screen beats unbounded memory; unpinned candidates always lose first). (2) **No total-request timeout on the media path**: the guarded client's hard 20s total killed exactly the biggest legitimate files (a 5.5MB cover rode past 20s under concurrent load, failed transient, cycled on backoff forever — the "some images never load" report). The client now bounds connect (10s) and each READ (15s idle: a stalled connection dies, a slow-but-flowing one finishes); one media attempt gets a generous 120s total (`ATTEMPT_BUDGET`); JSON surfaces (LNURL/oEmbed) keep the snappy 20s per request (`JSON_BUDGET`). Also en route: thread identity clicks (reply rows + anchor open the profile), and the NIP-78 login overlay defers to newer local pref edits (`{key}_at` recency stamps written at every toggle site — a stale relay slot was eating the curated hidden-kinds list at every login). Fixtures/probes: `crates/core/examples/dump_covers.rs` runs the real pipeline against live article covers; tests `prefs_overlay_defers_to_a_newer_local_edit`, `desperate_eviction_takes_pinned_only_as_last_resort`. | Maintainer click-through reports: "all kinds checked" / images disappearing / covers never loading / images breaking while scrolling profiles (Nov 12–13 2026); owner confirmed each fix ("looks good"). |

---

- **M0: Skeleton walks.** Workspace builds in container; window opens with sumi-e
  theme (both palettes); bunker login connects and subscribes; read-only following feed
  renders via outbox routing; tokio↔GLib bridge proven; LMDB store persists.
  *Accept: launch from container, log in with bunker, read feed, restart → resume.*
- **M1: Speak.** Note card final layout; composer (note type) with preview, CW, relay
  picker; optimistic actions (reply/like/bookmark/pin/mute) with reconcile; undo window
  with backstop; drafts (local); delete.
- **M2: Read deeply.** Threads, profiles (+strips), notifications, search, embeds,
  media proxy + avatars, picture/video cards, seen-state model (dots, caught-up
  boundary).
- **M3: Correspond.** NIP-17 DMs end-to-end (inbox, requests, threads, send, read
  state, legacy read-only), relay management panes, settings (appearance/content),
  profile edit, follow/mute management views.
  *Postponed by maintainer decision; DMs are the least-important part. Most non-DM scope
  (relays, settings, profile edit, follow/mute) already landed under M0/M4.*
- **M4: Show everything.** Manifest registry complete (all rich cards), polls + votes,
  calendar + RSVP + .ics, highlights, wiki, repo, classified, podcast, custom NIP,
  fallback + NIP-89; Blossom upload wired into composer; galleries + lightbox.
- **M5: Sovereign.** Bundled tor + privacy modes + warm-up; NIP-37 draft sync;
  scheduled posts; list backup; backup/restore; Flatpak manifest polished (metainfo,
  icons, screenshots); private-relay policy; reduced-motion & a11y pass.

---

## 9.1 Port milestones (GPUI, per D13)

The GTK milestones above describe *what* exists; the P-series below re-lands it on
GPUI. Feature scope is already built in `crates/core` + the GTK app; the port is a
re-expression of views, not a feature rebuild.

- **P0: Spike becomes the app (done).** `crates/app-gpui` scaffold: workspace dep +
  xattr patch, `theme.rs` (sumi-e tokens as constants), `icons.rs` (PathBuilder
  glyphs), feed view with real note cards. *Accept: real session, real feed, scroll,
  like posts a real reaction.*
- **P1: Shell & navigation (done).** `shell.rs`: nav stack of panes
  (feed/thread/profile/settings); content views emit `Nav` events, the shell
  owns the stack. Thread view (anchor + flat replies, reply
  click opens its own thread), profile view (name/about/follow seal + notes),
  settings view (relays + media servers, read-only). Escape-to-back via root
  key-down. *Later pass: the header became an icon rail (sumi-e sidebar):
  ensō home, feed, search, bell with dot, vermilion pen, gear at the foot;
  back+title live on the deep panes' own top row; re-clicking the active
  rail spot scrolls the feed to top, and a floating ↑ whisper appears when
  scrolled deep.*
  key-down + focused shell handle. *Accept: click note → thread, Escape →
  back, all four routes render.* Finding for P2: GPUI's Wayland CSD drag strip
  overlays our header; in fullscreen, clicks on the top ~30px start a window
  drag instead of hitting header buttons; interactive header elements need to
  clear the CSD inset (or `TitlebarOptions::appear` revisited).
- **P2: Speak & read deeply (done).** Composer (undo window as inline row,
  multi-image attach via `upload_chain`, session draft stashed/restored on
  close/reopen), thread view + NIP-10 nested comments (last-`e` parent, depth
  ≤ 3, flattened pre-order), note-card fidelity (imeta and standalone image
  links as picture cards, npub mentions, note/nevent quote embeds via
  `event_by_id_dech32`), optimistic bookmark/reply, avatars (core image fetch →
  gpui `img`). *Accept: composer/undo round-trip, bookmark glyph, avatars,
  rich bodies, quote embed, draft restore verified live; publish broadcast
  left to the user.* Mid-paragraph links/mentions stay plain text until P3
  (GPUI can't style spans in a wrapped block).
- **P3: Everything else.** Settings panes, notifications, search, rich cards from
  the manifest registry, media proxying. *Progress: notifications pane done
  (compact cards, mark-seen on open, New/Earlier high-water split, 90s bell-dot
  poll, `u` key opens; the header seals stay unclickable when CSD-tiled, so
  the keyboard is the reliable way in); non-text/empty-content events filtered
  to the slice-1 spec (reactions land with their milestones). Search pane done
  (single-line Input, Enter or the seal runs, compact-card results, stale
  answers dropped by generation counter, `/` key opens). GPUI gotcha found
  live: the Input handles characters via the IME path and lets key events
  bubble; shell keybindings fire for text typed in the search field (an "n"
  in "nostr" opened the composer); text surfaces must be guarded in the shell
  key handler. Login pane done (both doors, breathing ensō at the web app's
  3.4s period, `Root` window view swaps doors→shell on approval, Esc cancels
  a running flow; NIP-46 auth URLs are clickable chips; an improvement over
  the GTK TODO). Verified live via a sandboxed instance
  (`XDG_DATA_HOME=/tmp/… satori-desktop`; `data_dir()` already honors XDG, so
  isolated stores need no new env var; that's the dev-stories seam too).
  Login-layout gotchas recorded: gpui percent-height children collapse in
  content-sized chains (give input wrappers a definite height); an
  unwrapped single-line Input leaks its line width into taffy layout
  (`overflow_hidden` on the wrapper); Input gained single-line horizontal
  scroll so long bunker URIs stay on one row. Settings pane done: the relay
  editor (read/write toggles + add/remove, publish as NIP-65), media servers
  (normalize https, publish kind 10063), the undo window, the feed-content
  catalog and list backup (export/import via rfd's portal dialogs); `s` opens
  it (the header seal is unclickable CSD-tiled). Found and fixed a real
  data-loss bug the GTK original shared: `prefs::save` blind-serialized the
  Prefs struct, so saving any pref wiped `content_hidden` from prefs.json;
  save is now a read-modify-write merge in both crates. Verified live against
  the real store: catalog/undo toggles, seconds field, media add/remove;
  publish buttons left to the user (they broadcast). Rich cards are done;
  cards.rs ports `card_for`'s dispatch: video/podcast wear honest
  play/listen links (GPUI has no media pipeline; nothing fetched until
  asked), polls render NIP-68 tallies with a working ballot
  (core.poll_vote), calendar events get RSVP pills (core.rsvp, kind
  31925), articles get kicker/title/summary (the markdown reader is its
  own port), plus classified/repo/wiki/highlight. Found and fixed a
  feed-list wedge while testing: on_event called `list.reset()` on every
  FeedEvent, and gpui's List drops all scroll input while a reset is
  pending; a busy feed (profiles resolving) ate the wheel mid-scroll
  and left the pane permanently blank. Reset now only when the note
  count changes. Verified under stress: scroll-during-bootstrap-storm,
  60-tick deep scroll, rapid return to top.* UX polish pass (user
  request): avatars are circles: gpui images paint their own quad and
  ignore the parent's rounding, so the photo wears `rounded_full`
  itself; posts lose their card boxes (no panel bg, no hairline rules)
  and separate by whitespace alone, in feed, compact cards, thread
  replies and anchor note; type moved to P052 (Palatino) for body serif
  (Nimbus Roman read cramped) and Cantarell for UI chrome. Endless
  timeline (user decision; over the GTK app's deliberate "view older"
  batches): the feed projects through `store::feed_page`, which had
  been hardcoding kinds 1+6; bootstrap fetched the whole visible
  catalog into the store and the projection dropped it at the door, so
  polls and articles never reached a page; `core.older` now tops up
  from the relays when the store runs dry (the web app's
  until-pagination; `fetchRoutedPage` semantics, the "M2" TODO that
  never landed), and the feed orders pages from the list's scroll
  stream (near-bottom, one round trip at a time). gpui fact: list
  appends must go through `splice`, not `reset`; reset clears the
  scroll anchor and yanks the reader to the top; the anchor survives a
  non-overlapping splice. Live-verified: scrolled 36 minutes → 5 weeks
  deep through appended pages with the view holding still, images
  loading at depth, mentions hydrating to @names mid-scroll. Article
  reader done; markdown.rs ports the GTK blocks parser; the Pango
  inline half became styled spans the word flow renders (GPUI has no
  markup strings); the reader pane resolves entities like note bodies
  and the Read seal opens it (the shell quiets its keys while reading;
  a reading surface is a text surface). Wiki bodies render inline like
  the GTK card's AsciiDoc subset. Article comments (NIP-22) stay with
  the GTK original for now. Daily driver
  flipped to the GPUI binary (user decision, P2.5): `crates/app` stays only as
  the P3 porting reference (first-login no longer needs it); deletion remains
  the P4 formality.
- **P4: Cutover (done, minus packaging).** Binary named `satori-desktop`
  (D15 superseded the "named `satori`" goal); `crates/app` retired (deleted);
  both themes live-switchable; reduced-motion equivalent audited; the ensō
  breath (both login instances) is the app's only animation and honors
  `prefers-reduced-motion`; the hold toast's undone flash, the rail whisper
  and the bell dot are state appearances, not motion, nothing to gate. The
  TypeScript reference retired per D19 (deleted at `a4b09e8`, citations point
  into git history). P4 sweep extras: the bell's cached phase gained the
  engagement kinds (7/9735/1018 by `e` tag; a cold start's instant paint no
  longer waits a beat for the relays) with the relay phase's gates (dislikes,
  the reactionNotifs pref) applied to it, and reactionNotifs got its settings
  surface. **Open: Flatpak/packaging update** (held by the maintainer; the keyring
  decision rides with it).
- **P5: Renderer — blade out, wgpu in (planned, D35).** Swap the UI toolkit dep
  from `gpui` 0.2.2 to the `gpui-pre` crates.io snapshot (`=0.3.11`, zed@f816e1f,
  the wgpu renderer; NOT zed-official — D35 records the posture), then:
  (1) compile-and-fix `app-gpui` against the 2026 API churn (views/, theme.rs,
  icons.rs; core is gpui-free and untouched); (2) re-validate or retire the two
  vendored patches — `vendor/xattr` (D13) looks removable (no xattr/async-tar in
  the snapshot's graph) and `vendor/cosmic-text` (D29, emoji CBDT preference)
  must be RE-DONE against the new tree's cosmic-text version or the emoji
  regresses to invisible holes; (3) flatpak SDK rustc vs the snapshot's MSRV;
  (4) full gates + flatpak rebuild + md5 + **a real soak** (the blade crash took
  ~30 min to surface; accept = an overnight session with zero renderer panics
  and the emoji/render fidelity of the current build, spot-checked against the
  D29 probe evidence). *Fallback if the snapshot route fails: a pinned git dep
  on zed's repo (heavier, less reproducible — same accept criteria).*

- **P5: Renderer — blade out, wgpu in (built; overnight soak pending).** Done on
  the `gpui-pre` crates.io snapshots exactly as planned: `gpui` aliased to
  `package = "gpui-pre", "=0.3.9"` (the lib name stays `gpui`, so every `use
  gpui::` import survived), pinned to 0.3.9 to unify with `gpui-video-player`
  0.1.3's own `gpui-pre` feature (default-features off; its `VideoElement`
  implements THAT snapshot's traits — two snapshot versions would be two
  incompatible gpui copies), plus `gpui-pre-linux =0.3.9` for the platform:
  `Application::new()` became `Application::with_platform(gpui_linux::
  current_platform(false))`. blade is GONE from the graph (`cargo tree -i
  blade` matches nothing; wgpu 29.0.4 in), so the D13 xattr patch retired
  itself. API churn was small and mechanical: `window.focus(h)` gained a
  `&mut App` (9 sites), `Application::new()` → `with_platform`, `WrappedLine`
  lost `Clone` (input.rs `Para` holds an `Arc<WrappedLine>`), and
  `AsyncApp::update` returns `()` now (the `let _ =` unit-binds dropped;
  `WeakEntity::update` still returns Result). clippy silent, 111 tests green,
  SDK rustc 1.98.1 builds it, bundle 33M → 41M (the wgpu/naga stack). The two
  vendor dirs stay put but only `proc-macro-error2` remains patchable-in-
  principle — no `[patch.crates-io]` entries remain. **Accept criteria MET
  (Nov 3 2026):** the overnight soak passed — 15h53m uptime on the wgpu
  build, ZERO renderer panics (blade died at 30 min; RSS/sockets/threads
  sampled across the session in the round-three audit below) — and the
  D29 emoji spot-check is the only item still awaiting the owner's eyes
  (the cosmic-text patch (0.14) could not ride along (the graph wants
  0.19); if the duplicate-face regression is back (COLRv1 chosen over
  CBDT → invisible emoji), the D29 fix gets ported to 0.19 as a follow-up).

Port convention: each ported view cites its GTK/TS origin
(`// TODO(port): <origin> — P<n>`), same discipline as `crates/core`. Since
the retirement (D19) those citations point into git history.

---

## 10. Open questions / deferred

- Wallet milestone (un-deferred in D20): NIP-47 zaps via the `nwc` crate are
  the current Phase-1 work (public zaps through the bunker signer; the anon
  invoice mode stays the no-wallet fallback).
- Multi-account / profiles-per-window: deferred (D10).
- arti in-process: behind transport trait, evaluate post-M5.
- YouTube embeds in-app (webkitgtk view) vs external browser: **external browser for
  v1**; a webview is a code-execution surface and can be revisited with a hard argument.
- AsciiDoc depth for NIP-54: subset matching `src/nostr/asciidoc.ts` coverage.
- `engine.ts` wire-manifest idea: out of scope until external demand exists.
- M3 (NIP-17 DMs): **postponed by maintainer decision until explicitly re-prioritized**;
  least-important milestone. Land M5 + `TODO(port)` chokepoint cleanup first.
