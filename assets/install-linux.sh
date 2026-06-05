#!/usr/bin/env bash
#
# Install meatshell's icon + desktop entry on Linux so the GNOME/Ubuntu dock and
# the app launcher show the app icon.
#
# Why this is needed: the Windows build embeds the icon in the .exe, but on Linux
# the icon comes from a freedesktop ".desktop" entry plus an icon installed into
# the hicolor icon theme. On Wayland (Ubuntu's default) the shell matches a
# running window to its .desktop file via the window's app_id — meatshell sets
# that to "meatshell" (slint::set_xdg_app_id), and this script's StartupWMClass
# matches it.
#
# Usage:
#   ./install-linux.sh [/path/to/meatshell-binary]
# If no path is given it defaults to ../target/release/meatshell relative to
# this script.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="${1:-$SCRIPT_DIR/../target/release/meatshell}"
BIN="$(readlink -f "$BIN" 2>/dev/null || echo "$BIN")"

if [ ! -x "$BIN" ]; then
    echo "error: meatshell binary not found or not executable: $BIN" >&2
    echo "build it first (cargo build --release) or pass the path as an argument." >&2
    exit 1
fi

ICON_SRC="$SCRIPT_DIR/icon@512.png"
ICON_DIR="$HOME/.local/share/icons/hicolor/512x512/apps"
APP_DIR="$HOME/.local/share/applications"

mkdir -p "$ICON_DIR" "$APP_DIR"
install -m644 "$ICON_SRC" "$ICON_DIR/meatshell.png"

cat > "$APP_DIR/meatshell.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=meatshell
GenericName=SSH Client
Comment=Lightweight Rust + Slint SSH/SFTP client
Comment[zh_CN]=轻量级 Rust + Slint SSH/SFTP 客户端
Exec=$BIN
Icon=meatshell
Terminal=false
Categories=Network;System;TerminalEmulator;Utility;
Keywords=ssh;sftp;terminal;shell;
StartupNotify=true
StartupWMClass=meatshell
EOF
chmod 644 "$APP_DIR/meatshell.desktop"

# Refresh the desktop + icon caches (best-effort; harmless if the tools are absent).
update-desktop-database "$APP_DIR" 2>/dev/null || true
gtk-update-icon-cache -f -t "$HOME/.local/share/icons/hicolor" 2>/dev/null || true

echo "Installed:"
echo "  icon    -> $ICON_DIR/meatshell.png"
echo "  desktop -> $APP_DIR/meatshell.desktop"
echo "  exec    -> $BIN"
echo
echo "If the dock still shows the generic icon, log out/in (Wayland) or run"
echo "'killall -3 gnome-shell' (X11) to refresh the shell."
