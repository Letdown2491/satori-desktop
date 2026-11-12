#!/bin/sh
# Build the Alpine apk (musl). Runs INSIDE an alpine:3.22 container,
# never on the host — abuild refuses root, and the deps are alpine's:
#
#   podman run --rm -v "$PWD:/srv:Z" -w /srv alpine:3.22 sh packaging/build-apk.sh <version>
#
# release.yml's apk job calls the same script (its container: is this
# image), so CI and local builds share one code path.
#
# NOTES from the musl spike (issue #23):
# - alpine 3.22's system cargo (1.87) is below the lockfile's MSRV, so
#   the toolchain is rustup's stable, installed for the builder user.
# - There is no libxkbcommon-x11-dev: the x11 bits ship inside
#   libxkbcommon-dev itself (unlike debian/fedora's split).
# - One build at a time: concurrent containers mounting the same
#   volumes :Z fight over the SELinux labels.
set -eu

VERSION="${1:?usage: build-apk.sh <version> (e.g. 0.5.0)}"
REPO=/srv
WORK=/home/builder/pkg/satori-desktop

# Build-time deps. Runtime deps live in the APKBUILD's depends.
apk add -q abuild build-base sudo git curl pkgconfig fontconfig-dev \
	freetype-dev libxkbcommon-dev wayland-dev vulkan-headers \
	vulkan-loader-dev libxcb-dev gstreamer-dev gst-plugins-base-dev \
	clang

# abuild refuses root: the builder user owns the build. -r installs
# deps and -a installs the key through sudo, so passwordless sudo for
# the builder user (a build container, not a security boundary here).
adduser -D builder 2>/dev/null || true
addgroup builder abuild 2>/dev/null || true
echo 'builder ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/builder
chmod 0440 /etc/sudoers.d/builder
# The source-tarball cache: abuild fetches into SRCDEST (default
# /var/cache/distfiles), group-writable by the abuild group.
mkdir -p /var/cache/distfiles
chown root:abuild /var/cache/distfiles
chmod g+w /var/cache/distfiles
mkdir -p "$WORK" /srv/packaging/apk/out
cp packaging/apk/APKBUILD "$WORK"/APKBUILD
# The version is the caller's truth (CI: the tag; local: the argument),
# not the APKBUILD's memory of the last release.
sed -i "s/^pkgver=.*/pkgver=$VERSION/" "$WORK"/APKBUILD
chown -R builder:builder /home/builder

# The toolchain: rustup's stable for the BUILDER user (the APKBUILD puts
# $HOME/.cargo/bin on PATH; the system cargo's 1.87 is below the lock's
# MSRV — see the spike notes at the top of this script).
su builder -c "curl -fsSL https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal"

# Keypair: abuild signs, apk verifies. -a writes PACKAGER_PRIVKEY into
# the builder's abuild.conf AND installs the pub key into /etc/apk/keys
# (through the sudo above). The key is throwaway, per build; users
# sideload with --allow-untrusted or trust the shipped .pub.
su builder -c "abuild-keygen -a -n"
cp /home/builder/.abuild/*.pub /etc/apk/keys/ 2>/dev/null || true

# abuild fetches the tag tarball from GitHub (network), verifies
# checksums, builds, tests (check(): the core suite), and packages.
su builder -c "cd $WORK && abuild checksum && abuild -r"

find /home/builder -name '*.apk' -exec cp {} /srv/packaging/apk/out/ \; 2>/dev/null || true
cp /home/builder/.abuild/*.pub /srv/packaging/apk/out/ 2>/dev/null || true
ls -la /srv/packaging/apk/out/
