#!/usr/bin/env bash
# Assemble satori's Orchard app tree: the built binary into files/bin,
# beside the committed metadata and the export/ files (desktop entry,
# AppStream metainfo, icons) that orchard-publish turns into a
# flatpak-shaped OCI image. The freedesktop runtime supplies the native
# libraries the binary links (wayland, xkbcommon-x11, fontconfig) and,
# via the runtime's GStreamer, the demuxers and codecs the deb/rpm name
# as Recommends — the sandbox sees no host plugins either way.
#
# GLIBC CONTRACT: the binary must be built against a glibc no newer
# than the runtime's (org.freedesktop.Platform 25.08: 2.42). The
# release CI builds on ubuntu-24.04 (2.39), which clears it; the local
# satori-dev container is Fedora 44 (2.43) and its binary dies in the
# sandbox with "version `GLIBC_2.43' not found". Local flatpak builds:
#
#   podman run --rm -v satori-cargo:/usr/local/cargo \
#     -v "$HOME/.rustup:/mnt/rustup:ro,Z" -v "$HOME/.cargo/bin:/opt/rustbin:ro,Z" \
#     -v "$PWD:/srv:Z" -w /srv ubuntu:24.04 bash -c '
#       export RUSTUP_HOME=/mnt/rustup PATH=/opt/rustbin:$PATH
#       apt-get update -qq && apt-get install -y -qq pkg-config \
#         libfontconfig-dev libfreetype-dev libxkbcommon-dev \
#         libxkbcommon-x11-dev libwayland-dev libxcb1-dev libvulkan-dev \
#         libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev clang git
#       cargo build --release --locked -p satori-gpui'
#   ./self/assemble.sh
#   flatpak build-export repo self/tree stable
#   flatpak build-bundle repo bundles/satori-<ver>-$(uname -m).flatpak \
#     io.github.Letdown2491.satori stable
set -euo pipefail
cd "$(dirname "$0")/.."

binary=target/release/satori-desktop
if [[ ! -x $binary ]]; then
    echo "missing $binary; build it first (cargo build --release -p satori-gpui)" >&2
    exit 1
fi

mkdir -p self/tree/files/bin
install -m 0755 "$binary" self/tree/files/bin/satori-desktop

# The metadata's runtime ref carries the arch, and the export's commit
# ref comes from it (app/<id>/<arch>/stable). The tree is assembled
# per-host (CI legs: x86_64 and aarch64; flatpak's arch names match
# uname -m), so stamp the host arch into the runtime ref. On the arch
# the metadata already pins this is a no-op rewrite.
arch=$(uname -m)
sed -i "s|^runtime=org\.freedesktop\.Platform/.*/25\.08$|runtime=org.freedesktop.Platform/$arch/25.08|" \
    self/tree/metadata

# The desktop tree: entry, AppStream metainfo, icons (the flatpak id is
# the icon name the desktop entry already references).
mkdir -p self/tree/export/share/applications
mkdir -p self/tree/export/share/metainfo
mkdir -p self/tree/export/share/icons/hicolor/scalable/apps
mkdir -p self/tree/export/share/icons/hicolor/512x512/apps
install -m 0644 packaging/io.github.Letdown2491.satori.desktop \
    self/tree/export/share/applications/io.github.Letdown2491.satori.desktop
install -m 0644 packaging/io.github.Letdown2491.satori.metainfo.xml \
    self/tree/export/share/metainfo/io.github.Letdown2491.satori.metainfo.xml
install -m 0644 packaging/icons/io.github.Letdown2491.satori.svg \
    self/tree/export/share/icons/hicolor/scalable/apps/io.github.Letdown2491.satori.svg
install -m 0644 packaging/icons/512x512/apps/io.github.Letdown2491.satori.png \
    self/tree/export/share/icons/hicolor/512x512/apps/io.github.Letdown2491.satori.png

echo "assembled self/tree: $(du -sh self/tree/files | cut -f1)"
