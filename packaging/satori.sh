#!/bin/sh
# Satori launcher (installed to ~/.local/bin/satori by install-local.sh).
# Supplies libxkbcommon-x11 on hosts that lack it — bootc base images never
# needed it before GPUI (GTK's Wayland backend didn't touch it). A no-op
# wherever the lib is system-installed. Replaced by the flatpak at M5.
LIBDIR="$HOME/.local/lib"
if [ -f "$LIBDIR/libxkbcommon-x11.so.0" ]; then
    export LD_LIBRARY_PATH="$LIBDIR${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
fi
exec "$HOME/.local/libexec/satori-desktop" "$@"
