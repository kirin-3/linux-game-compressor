#!/bin/sh
# Installs Linux Game Compressor for the current user (or system-wide with PREFIX=/usr/local).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
prefix=${PREFIX:-"$HOME/.local"}
install -Dm755 "$here/bin/compressor" "$prefix/bin/compressor"
install -Dm755 "$here/bin/compressor-gui" "$prefix/bin/compressor-gui"
install -Dm644 "$here/share/applications/linux-game-compressor.desktop" \
    "$prefix/share/applications/linux-game-compressor.desktop"
install -Dm644 "$here/share/icons/hicolor/256x256/apps/linux-game-compressor.png" \
    "$prefix/share/icons/hicolor/256x256/apps/linux-game-compressor.png"
echo "Installed to $prefix. Make sure $prefix/bin is on your PATH."
