# AGENTS.md

Satori is being rewritten from a server-rendered web app into a native Linux
GTK4/libadwaita desktop app in Rust. The TypeScript app in `src/` + `public/`
is the **frozen reference implementation** from the original web app: consult
it for product behavior, not for architecture. The build spec is `DESIGN.md`
— read it before working in `crates/`.

## Layout

- `crates/core` — Nostr domain layer (relay routing, signing, storage, NIPs).
  **No GTK dependency.** Each module's doc comment names the TS file(s) it ports.
- `crates/app` — the GTK binary: views, widgets, sumi-e theme.
- `packaging/flatpak` — Flatpak packaging (M5).

## Dev environment (host is bootc — no GTK headers on the host)

```
podman build -t satori-dev -f Containerfile.dev
podman run --rm -v "$PWD:/srv:Z" -w /srv satori-dev cargo check --workspace
podman run --rm -v "$PWD:/srv:Z" -w /srv satori-dev cargo clippy --workspace
```

## Gates

- `cargo check --workspace` clean before any commit.
- Don't touch the frozen reference code (`src/` + `public/`) unless explicitly asked;
  its own gates (`npm run typecheck`) apply only there.

## Conventions

- Rust edition 2024, workspace deps in the root `Cargo.toml`.
- Ported semantics cite their origin (`// TODO(port): <ts-file> — <milestone>`).
- Decisions live in `DESIGN.md` §8 (decision log); add, don't bypass.
