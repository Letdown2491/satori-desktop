# Satori

A quiet Nostr client for the Linux desktop. Native Rust on GPUI — the Zed
toolkit — one window, sumi-e ink on washi.

Satori is built on three commitments:

- **Cypherpunk** — the app never handles the master key (D17): you sign in
  through a remote signer, `bunker://` or `nostrconnect://` (NIP-46). Every
  event is verified locally: relays are untrusted pipes. No telemetry. Tor
  as a mode, not a bolt-on.
- **HATEOAS, natively** — the event stream is the state; the UI is a
  projection of it. Kind affordances come from a manifest registry
  (`KindHandler`), not hardcoded view logic.
- **The uncarved block** — the undo window is the breath before speech
  (every publish kind is held, undoable, restart-safe). Quiet by default:
  reactions in notifications off, no like/repost counters, dots not badges.
  Sumi-e: whitespace over boxes, vermilion only for seal moments. Anything
  that can be left out, is.

## Status

Daily-driver quality — the owner runs it as their main client. The GPUI port
is complete through P4, plus the follow-on polish phases:

- Timeline — endless scroll to the bottom of your history, media, quotes,
  reposts (NIP-18), polls (NIP-68 vote + compose), articles (NIP-23 reader),
  hashtag timelines; a new-notes pill when the live subscription lands
  updates while you're reading
- Threads — NIP-10 roots, NIP-22 comments, reply-to-root routing
- Composer — undo window on every publish kind, draft restore, scheduled
  posts, attachments (Blossom upload), @-mentions, quote cards
- Zaps — NIP-57 send with verified receipts (D18), paid through a
  connected NWC wallet (NIP-47)
- Notifications — replies, comments, zaps, poll votes; reactions behind a pref
- Profiles — banner + avatar header, NIP-05 verification badge,
  click-to-copy npub/Lightning rows, notes rendered exactly like the
  timeline (D30), photos/videos media strips that page endlessly
- Inline video — poster tiles, click-to-play, one GStreamer player per
  process (D21)
- Search (NIP-50), list backup (export/import), NIP-78 preferences sync,
  two live themes, reduced-motion honored

Open: NIP-37 draft sync, the owner metrics pane, the private-relay routing
policy, Tor-routed media fetch (D24), and Flatpak packaging — the release
ships today via `packaging/install-local.sh` (bin + desktop file + icons
into `~/.local`). The design and build spec is [DESIGN.md](DESIGN.md).

## Development

The host is bootc/atomic (no GTK or Vulkan dev headers), so builds run in a
container — the cargo cache lives in a named volume, so tooling stays warm:

    podman build -t satori-dev -f Containerfile.dev
    podman run --rm -v satori-cargo:/usr/local/cargo -v "$PWD:/srv:Z" -w /srv satori-dev cargo check --workspace
    podman run --rm -v satori-cargo:/usr/local/cargo -v "$PWD:/srv:Z" -w /srv satori-dev cargo clippy --workspace
    podman run --rm -v satori-cargo:/usr/local/cargo -v "$PWD:/srv:Z" -w /srv satori-dev cargo test -p satori-core

Run it on the host (GPUI needs `libxkbcommon-x11`, which the GTK-era host
never shipped):

    cargo build -p satori-gpui
    LD_LIBRARY_PATH=~/.local/lib ./target/debug/satori-desktop

Layout: `crates/core` is the Nostr domain layer (no UI dependency);
`crates/app-gpui` is the binary it feeds. `AGENTS.md` covers conventions and
gates. The original TypeScript web app was retired at P4 parity — its final
state lives in git history at commit `a4b09e8`
(`git show a4b09e8:src/<file>`).

## License

[MIT](LICENSE) © 2026 Letdown2491
