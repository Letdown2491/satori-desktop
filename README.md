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

Daily-driver quality: the GPUI port is complete through P4 except packaging.

- Timeline — endless scroll to the bottom of your history, media, quotes,
  reposts (NIP-18), polls (NIP-68 vote + compose), articles (NIP-23 reader)
- Threads — NIP-10 roots, NIP-22 comments, reply-to-root routing
- Composer — undo window on every publish kind, draft restore, attachments
  (Blossom upload), @-mentions, quote cards
- Notifications — replies, comments, zaps, poll votes; reactions behind a pref
- Profiles, search (NIP-50), list backup (export/import), two live themes,
  reduced-motion honored

Open: Flatpak packaging (and with it the Secret Service keyring decision).
The design and build spec is [DESIGN.md](DESIGN.md).

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
