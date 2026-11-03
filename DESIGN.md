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
- **Leave things out**: no wallet in v1, no NIP-07 (browser-only), no multi-user daemon,
  no HTTP API, no SPA. Every feature must earn its place.

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
  Cargo.toml                # workspace
  crates/
    core/                   # satori-core: nostr domain. NO UI dependency.
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
    app-gpui/               # satori-desktop (package satori-gpui): the GPUI binary.
      src/
        main.rs             # Application, the Root view (login doors → shell)
        theme.rs            # sumi-e tokens (P052/Cantarell, night/washi palettes)
        icons.rs            # PathBuilder stroke glyphs
        views/              # shell, feed, thread, profile, reader, search,
                            # notifications, composer, settings, login, lightbox
  packaging/                # .desktop file; Flatpak manifest is the open P4 item
  Containerfile.dev         # podman dev image (host is bootc; no GTK/Vulkan headers)
  vendor/xattr              # gpui 0.2.2 transitive-dep compile fix ([patch.crates-io])
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
- `*.json`: app state, 0600, debounced flush + flush-on-exit (ports `json-store.ts`):
  `prefs.json`, `drafts.json`, `holds.json`, `scheduled.json`, `filters.json`,
  `read-state.json`, `dm-read.json`, `relays-cache.json`, `seen-relays.json`,
  `relay-latency.json`, `relay-info.json`, `relay-favorites.json`, `engagement.json`,
  `deletions.json`, `profiles.json`.
- `media-cache/`: avatar/media blob cache (LRU, size-capped), keyed by sha256.
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

### 5.3 Shell

- Single 720px reading column, centered, on the washi background.
- Custom header bar (`AdwToolbarView` + `GtkHeaderBar` with custom center widget):
  home+bell left, feed switcher center (popover: Following, timelines, Browse a relay),
  search + avatar menu right. Avatar shows a privacy shield when Tor mode is on.
- Compose FAB bottom-right; scroll-top button above it (revealed past ~800px).
- Views map: feed/thread/profile/article → navigation stack (`AdwNavigationView`);
  modals (compose, zap placeholder, edit profile, new message) → `AdwDialog`;
  settings → `AdwPreferencesWindow`-style custom page with the 5 tabs
  (General, Backup, Content, Privacy, Relays; Relays keeps 4 sub-panes).

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
| D31 | **Distribution: deb, rpm, and an Orchard-published flatpak-shaped bundle (the gertie stack), tag-driven CI.** `cargo deb` / `cargo generate-rpm` metadata blocks on the bin crate (assets from `packaging/`, crate-relative climbs; both Provide/Conflict the app id; the GStreamer plugins are Recommends — dlopen'd at pipeline build, never linked). No flatpak-builder: `self/assemble.sh` builds an app tree (binary + desktop entry + metainfo + icons + `metadata`), `flatpak build-export`/`build-bundle` produce the `.flatpak` single-file per arch (x86_64 + aarch64 legs); the freedesktop Platform 25.08 runtime supplies every linked library AND the GStreamer demuxers/codecs (verified in-sandbox: `qtdemux` + `avdec_h264` present with no addon and no host packages — D21's addon expectation retired). The sandbox needs `sockets=x11;wayland`, `devices=dri`, `shared=network`, and session-bus talk to `org.freedesktop.secrets` for the keyring; the XDG data dir maps to `~/.var/app/<id>` with no filesystem grants. Orchard: `self/satori.publish.toml` (icon must be a PNG at an absolute URL; `relay.orchard.rocks` required), `orchard-publish` pinned v0.3.0, prepare per arch + one signing run gated on `ORCHARD_NSEC_SET`; `orchard.txt` is written by `--prove` at publish time. Two traps found and dodged (both bitten gertie v1.0.1): cargo-deb reads ONLY `[package.metadata.deb]` — a `cargo-deb`-named table silently vanishes, taking the desktop file/metainfo/icon with it; and `-p` for `cargo generate-rpm` takes the package directory path. | Maintainer: "should we go ahead and do deb/rpm/flatpak of satori" + "publish to orchard once we have everything ready" (Nov 2026); first tag v0.1.0; both arches; the owner adds `ORCHARD_NSEC` to the repo when ready. |

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
