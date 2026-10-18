# AGENTS.md

Satori is being rewritten from a server-rendered web app into a native Linux
desktop app in Rust. The UI toolkit is GPUI (decision D13); the GTK4/libadwaita
app remains the working implementation until the GPUI port reaches parity
(§9.1). The TypeScript app in `src/` + `public/` is the **frozen reference
implementation** from the original web app: consult it for product behavior,
not for architecture. The build spec is `DESIGN.md` — read it before working
in `crates/`.

## Layout

- `crates/core` — Nostr domain layer (relay routing, signing, storage, NIPs).
  **No UI dependency** (survived the GTK→GPUI decision untouched). Each
  module's doc comment names the TS file(s) it ports.
- `crates/app` — the GTK binary: views, widgets, sumi-e theme. Daily driver
  until P4 cutover; port from here, don't add new features.
- `crates/app-gpui` — the GPUI binary (`satori-gpui`): the port target.
  Tokens in `theme.rs`, stroke-drawn glyphs in `icons.rs`, views in `views/`.
- `vendor/xattr` — one-line Linux compile fix for a gpui 0.2.2 transitive dep
  (D13); wired via `[patch.crates-io]` in the root `Cargo.toml`.
- `packaging/flatpak` — Flatpak packaging (M5).

## Dev environment (host is bootc — no GTK/Vulkan dev headers on the host)

```
podman build -t satori-dev -f Containerfile.dev
podman run --rm -v "$PWD:/srv:Z" -w /srv satori-dev cargo check --workspace
podman run --rm -v "$PWD:/srv:Z" -w /srv satori-dev cargo clippy --workspace
```

Launching on the host: the bootc host lacks `libxkbcommon-x11` (GTK's Wayland
backend never needed it). A copy lives at `~/.local/lib/libxkbcommon-x11.so.0`;
launch with `LD_LIBRARY_PATH=~/.local/lib ./target/debug/satori-gpui`.

## Gates

- `cargo check --workspace` clean before any commit.
- Before asking anyone to test UI behavior: **`cargo build -p satori`** (GTK)
  or **`cargo build -p satori-gpui`** (GPUI) — clippy/test do NOT produce the
  binaries; a stale binary has caused a phantom "fixed but still broken"
  round-trip before.
- The GPUI binary and the GTK binary cannot run at the same time (LMDB store
  lock). Swap with care; restore the daily driver afterwards.
- Don't touch the frozen reference code (`src/` + `public/`) unless explicitly asked;
  its own gates (`npm run typecheck`) apply only there.

## Conventions

- Rust edition 2024, workspace deps in the root `Cargo.toml`.
- Ported semantics cite their origin (`// TODO(port): <ts-file> — <milestone>`).
- Decisions live in `DESIGN.md` §8 (decision log); add, don't bypass.
