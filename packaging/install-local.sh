#!/bin/sh
# Install the release build into ~/.local — the pre-flatpak shape (M5's
# flatpak replaces it: sandbox runtime carries its own libs and icons).
#
# Build first, in the dev container (DESIGN.md D13):
#   podman run --rm -v satori-cargo:/usr/local/cargo -v "$PWD:/srv:Z" \
#     -w /srv satori-dev cargo build --release
# Then:
#   ./packaging/install-local.sh
set -e
cd "$(dirname "$0")/.."

APP=io.github.Letdown2491.satori
BIN=target/release/satori-desktop

if [ ! -x "$BIN" ]; then
    echo "no release binary — build it first:"
    echo "  podman run --rm -v satori-cargo:/usr/local/cargo -v \"\$PWD:/srv:Z\" \\"
    echo "    -w /srv satori-dev cargo build --release"
    exit 1
fi

install -Dm755 "$BIN" "$HOME/.local/libexec/satori-desktop"
install -Dm755 packaging/satori.sh "$HOME/.local/bin/satori"
install -Dm644 "packaging/$APP.desktop" "$HOME/.local/share/applications/$APP.desktop"
install -Dm644 "packaging/icons/$APP.svg" "$HOME/.local/share/icons/hicolor/scalable/apps/$APP.svg"
for s in 512 256 128 64 48 32; do
    install -Dm644 "packaging/icons/${s}x${s}/apps/$APP.png" \
        "$HOME/.local/share/icons/hicolor/${s}x${s}/apps/$APP.png"
done

# Best-effort cache refresh (GNOME picks the changes up regardless).
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -qf "$HOME/.local/share/icons/hicolor" || true
fi
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q "$HOME/.local/share/applications" || true
fi

echo "installed: $HOME/.local/bin/satori (+ desktop file, hicolor icons)"
