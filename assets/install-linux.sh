#!/usr/bin/env bash
#
# Install rudder's icon + desktop entry on Linux so the GNOME/Ubuntu dock and
# the app launcher show the app icon.
#
# Why this is needed: the Windows build embeds the icon in the .exe, but on Linux
# the icon comes from a freedesktop ".desktop" entry plus an icon installed into
# the hicolor icon theme. On Wayland (Ubuntu's default) the shell matches a
# running window to its .desktop file via the window's app_id — rudder sets
# that to "rudder" (slint::set_xdg_app_id), and this script's StartupWMClass
# matches it.
#
# Usage:
#   ./install-linux.sh [--system] [/path/to/rudder-binary]
# You normally don't need an argument: when run from inside a release package
# (the `rudder` binary sits next to this script) it is picked up automatically.
# In the source tree it falls back to ./target/release/rudder.
#
# --system installs system-wide (/usr/local, requires sudo) so the GNOME/Ubuntu
# dock and app launcher use one canonical executable and desktop entry
# (upstream a5d3fc3). The default remains a per-user install (~/.local), which
# needs no root; pass --system first if you want the machine-wide layout.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# --system flag: install to /usr/local instead of ~/.local (needs sudo).
SYSTEM=0
if [ "${1:-}" = "--system" ]; then
    SYSTEM=1
    shift
fi

# Resolve the binary: explicit arg > sibling (release package) > source-tree build.
if [ -n "${1:-}" ]; then
    BIN="$1"
elif [ -x "$SCRIPT_DIR/rudder" ]; then
    BIN="$SCRIPT_DIR/rudder"
else
    BIN="$SCRIPT_DIR/../target/release/rudder"
fi
BIN="$(readlink -f "$BIN" 2>/dev/null || echo "$BIN")"

# Make sure the binary is executable (a downloaded tarball may have lost +x).
[ -f "$BIN" ] && chmod +x "$BIN" 2>/dev/null || true

if [ ! -x "$BIN" ]; then
    echo "error: rudder binary not found: $BIN" >&2
    echo "Run this script from the extracted release folder (it sits next to the" >&2
    echo "'rudder' binary), or pass the binary path as an argument." >&2
    exit 1
fi

ICON_SRC="$SCRIPT_DIR/icon@512.png"
if [ "$SYSTEM" = 1 ]; then
    PREFIX="/usr/local"
    ICON_DIR="$PREFIX/share/icons/hicolor/512x512/apps"
    APP_DIR="$PREFIX/share/applications"
    if ! command -v sudo >/dev/null 2>&1; then
        echo "error: sudo is required for a system-wide installation" >&2
        exit 1
    fi
    sudo -v
    sudo install -d "$ICON_DIR" "$APP_DIR"
    INSTALL="sudo install"
else
    ICON_DIR="$HOME/.local/share/icons/hicolor/512x512/apps"
    APP_DIR="$HOME/.local/share/applications"
    INSTALL="install"
    mkdir -p "$ICON_DIR" "$APP_DIR"
fi

if [ -f "$ICON_SRC" ]; then
    $INSTALL -m644 "$ICON_SRC" "$ICON_DIR/rudder.png"
else
    echo "warning: icon not found ($ICON_SRC); the desktop entry will use a generic icon" >&2
fi

DESKTOP_TMP="$(mktemp)"
trap 'rm -f "$DESKTOP_TMP"' EXIT
cat > "$DESKTOP_TMP" <<EOF
[Desktop Entry]
Type=Application
Name=rudder
GenericName=SSH Client
Comment=Lightweight Rust + Slint SSH/SFTP client
Comment[zh_CN]=轻量级 Rust + Slint SSH/SFTP 客户端
Exec=$BIN
Icon=rudder
Terminal=false
Categories=Network;TerminalEmulator;
Keywords=ssh;sftp;terminal;shell;
StartupNotify=true
StartupWMClass=rudder
EOF
$INSTALL -m644 "$DESKTOP_TMP" "$APP_DIR/rudder.desktop"

# System-wide installs replace a stale per-user launcher so the two can't
# disagree about Exec/Icon (upstream a5d3fc3).
if [ "$SYSTEM" = 1 ]; then
    OLD_USER_DESKTOP="$HOME/.local/share/applications/rudder.desktop"
    if [ -f "$OLD_USER_DESKTOP" ] && grep -q '^Exec=.*rudder' "$OLD_USER_DESKTOP"; then
        rm -f "$OLD_USER_DESKTOP"
        echo "Removed stale user launcher: $OLD_USER_DESKTOP"
    fi
fi

# Refresh the desktop + icon caches (best-effort; harmless if the tools are absent).
if [ "$SYSTEM" = 1 ]; then
    sudo update-desktop-database "$APP_DIR" 2>/dev/null || true
    sudo gtk-update-icon-cache -f -t "$PREFIX/share/icons/hicolor" 2>/dev/null || true
else
    update-desktop-database "$APP_DIR" 2>/dev/null || true
    gtk-update-icon-cache -f -t "$HOME/.local/share/icons/hicolor" 2>/dev/null || true
fi

echo "Installed:"
echo "  icon    -> $ICON_DIR/rudder.png"
echo "  desktop -> $APP_DIR/rudder.desktop"
echo "  exec    -> $BIN"
echo
echo "If the dock still shows the generic icon, log out/in (Wayland) or run"
echo "'killall -3 gnome-shell' (X11) to refresh the shell."
