# Satori

A quiet Nostr client for the Linux desktop. Native GTK4/libadwaita, written in Rust.

Satori is built on three commitments:

- **Cypherpunk** — your key is never required (NIP-46 bunker first; a local key is an
  explicit opt-in, sealed in the Secret Service). Every event is verified locally:
  relays are untrusted pipes. No telemetry. Tor as a mode, not a bolt-on.
- **HATEOAS, natively** — the event stream is the state; the UI is a projection of it.
  Affordances are declared by data, not hardcoded into views.
- **The uncarved block** — feeds end when you're caught up (the ensō clearing, no
  infinite refill), the undo window is the breath before speech, reactions are off by
  default, dots not badges. Anything that can be left out, is. Sumi-e: ink on washi.

## Status

Early development. The design and build spec is [DESIGN.md](DESIGN.md).

- [x] M0 scaffold — workspace, sumi-e theme, shell chrome
- [ ] M0 — bunker login, following feed
- [ ] M1 speak · M2 read deeply · M3 correspond · M4 show everything · M5 sovereign

## Development

Needs GTK4 + libadwaita dev headers. On bootc/atomic hosts (no GTK headers), build
inside the dev container:

    podman build -t satori-dev -f Containerfile.dev
    podman run --rm -v "$PWD:/srv:Z" -w /srv satori-dev cargo check --workspace
    podman run --rm -v "$PWD:/srv:Z" -w /srv satori-dev cargo run -p satori

`AGENTS.md` covers layout, conventions, and gates. The TypeScript in `src/` + `public/`
is the frozen reference implementation consulted during the port — it will be removed
once the port is complete.

## License

[MIT](LICENSE) © 2026 Letdown2491
