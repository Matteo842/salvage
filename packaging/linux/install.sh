#!/bin/sh
# Installs Salvage for the current user: the program in ~/.local/bin, plus a
# menu entry and its icon. `./install.sh --uninstall` takes them away again
# (your config and backups stay where they are).
set -eu

here=$(cd "$(dirname "$0")" && pwd)
bin="$HOME/.local/bin"
apps="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
icons="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/256x256/apps"

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$bin/salvage" "$apps/salvage.desktop" "$icons/salvage.png"
    echo "Salvage removed."
    exit 0
fi

mkdir -p "$bin" "$apps" "$icons"
install -m 755 "$here/salvage" "$bin/salvage"
install -m 644 "$here/salvage.png" "$icons/salvage.png"
# Absolute path: ~/.local/bin is not on every desktop's PATH.
sed "s|^Exec=salvage|Exec=$bin/salvage|" "$here/salvage.desktop" > "$apps/salvage.desktop"
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$apps" || true
echo "Salvage installed in $bin/salvage. It is in your app menu now."
