#!/bin/sh
# Satori launcher (installed to ~/.local/bin/satori by install-local.sh).
# Supplies libxkbcommon-x11 on hosts that lack it — bootc base images never
# needed it before GPUI (GTK's Wayland backend didn't touch it). A no-op
# wherever the lib is system-installed. Replaced by the flatpak at M5.
#
# MALLOC_ARENA_MAX=2: glibc spins one heap per thread (37 threads on 12
# cores → dozens of arenas), and free() almost never returns arena pages —
# decode-sized transients (a 4000x3000 photo is ~48MB RGBA) scatter across
# arenas and stay resident. Capping arenas makes freed memory get REUSED.
LIBDIR="$HOME/.local/lib"
if [ -f "$LIBDIR/libxkbcommon-x11.so.0" ]; then
    export LD_LIBRARY_PATH="$LIBDIR${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
fi
export MALLOC_ARENA_MAX=2
exec "$HOME/.local/libexec/satori-desktop" "$@"
