# AGENTS.md

Satori is a native Linux desktop app in Rust. The UI toolkit is GPUI (decision
D13); the old GTK4/libadwaita app and the original TypeScript web app are both
retired; their code lives in git history only (the TS reference's final state
is commit `a4b09e8`: `git show a4b09e8:src/<file>`). The build spec is
`DESIGN.md`: read it before working in `crates/`.

## Layout

- `crates/core`: Nostr domain layer (relay routing, signing, storage, NIPs).
  **No UI dependency.** Each module's doc comment names the TS file(s) it ported.
- `crates/app-gpui`: the GPUI binary (`satori-desktop`; package `satori-gpui`):
  the app (the P4 cutover pulled forward: `crates/app`, the old GTK binary,
  was deleted; its `TODO(port)` citations now point into git history). Tokens in
  `theme.rs`, stroke-drawn glyphs in `icons.rs`, views in `views/`.
- `vendor/xattr`: one-line Linux compile fix for a gpui 0.2.2 transitive dep
  (D13); wired via `[patch.crates-io]` in the root `Cargo.toml`.
- `packaging/`: `install-local.sh` ships the release (bin + wrapper + desktop
  file + hicolor icons into `~/.local`). `self/` +
  `.github/workflows/release.yml`: tag-driven deb/rpm/flatpak builds and
  the Orchard publish (D31).

## Dev environment (the bootc host has no GTK/Vulkan dev headers)

```
podman build -t satori-dev -f Containerfile.dev
podman run --rm -v satori-cargo:/usr/local/cargo -v "$PWD:/srv:Z" -w /srv satori-dev cargo check --workspace
podman run --rm -v satori-cargo:/usr/local/cargo -v "$PWD:/srv:Z" -w /srv satori-dev cargo clippy --workspace
```

(The cargo cache lives in the `satori-cargo` volume, so builds stay warm across
container runs.)

Launching on the host: the bootc host lacks `libxkbcommon-x11` (GTK's Wayland
backend never needed it). A copy lives at `~/.local/lib/libxkbcommon-x11.so.0`;
launch with `LD_LIBRARY_PATH=~/.local/lib ./target/debug/satori-desktop`.
Daily-driver install (release): `cargo build --release` in the container, then
`./packaging/install-local.sh` (bin + wrapper + desktop file + hicolor icons
into `~/.local`; `target/debug` can be deleted after).

## Gates

- `cargo check --workspace` clean before any commit.
- Before asking anyone to test UI behavior: **`cargo build -p satori-gpui`**:
  clippy/test do NOT produce the binary; a stale binary has caused a phantom
  "fixed but still broken" round-trip before.

## Conventions

- Rust edition 2024, workspace deps in the root `Cargo.toml`.
- No em dashes in user-facing text (UI copy, metainfo, desktop entries) or
  documents: rephrase with commas, periods, or middots (`·`). Code comments
  may use them, but prefer not to.
- Ported semantics cite their origin (`// TODO(port): <ts-file> — <milestone>`);
  the cited files live in git history (`git show a4b09e8:<path>`).
- Decisions live in `DESIGN.md` §8 (decision log); add, don't bypass.
