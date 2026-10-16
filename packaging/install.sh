#!/bin/sh
# Install only application files; never change lan-workspace settings or trust.
set -eu
if [ "$(uname -s)" != Linux ] || [ "$(uname -m)" != x86_64 ]; then
    echo 'Bifrost requires x86_64 Linux.' >&2
    exit 1
fi
case "${1-}" in
    ''|--uninstall) ;;
    *) echo 'Usage: ./install.sh [--uninstall]' >&2; exit 1 ;;
esac
: "${HOME:?HOME must be set}"
bin="$HOME/.local/bin/bifrost"
applications="$HOME/.local/share/applications"
icons="$HOME/.local/share/icons/hicolor"
sizes='16 20 24 32 40 48 64 256'
if [ "${1-}" = --uninstall ]; then
    rm -f "$bin" "$applications/bifrost.desktop" "$icons/scalable/apps/bifrost.svg"
    for size in $sizes; do
        rm -f "$icons/${size}x${size}/apps/bifrost.png"
    done
else
    source_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
    mkdir -p "$HOME/.local/bin" "$applications" "$icons/scalable/apps"
    install -m 0755 "$source_dir/bifrost" "$bin"
    # Desktop Entry quoted arguments escape backslash, quote, dollar and backtick;
    # literal percent signs must be doubled to avoid desktop field-code expansion.
    escaped=$(printf '%s' "$bin" | sed 's/\\/\\\\\\\\/g; s/"/\\\\"/g; s/\$/\\\\$/g; s/`/\\\\`/g; s/%/%%/g')
    {
        printf '%s\n' '[Desktop Entry]' 'Type=Application' 'Name=Bifrost' 'Comment=Share keyboard, mouse and clipboard across your LAN'
        printf 'Exec="%s"\n' "$escaped"
        printf '%s\n' 'Icon=bifrost' 'Terminal=false' 'Categories=Utility;RemoteAccess;' 'StartupWMClass=bifrost'
    } > "$applications/bifrost.desktop"
    chmod 0644 "$applications/bifrost.desktop"
    install -m 0644 "$source_dir/share/icons/hicolor/scalable/apps/bifrost.svg" "$icons/scalable/apps/bifrost.svg"
    for size in $sizes; do
        mkdir -p "$icons/${size}x${size}/apps"
        install -m 0644 "$source_dir/share/icons/hicolor/${size}x${size}/apps/bifrost.png" "$icons/${size}x${size}/apps/bifrost.png"
    done
fi
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$applications"
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1 && [ -f "$icons/index.theme" ]; then
    gtk-update-icon-cache -f -t "$icons"
fi
if [ "${1-}" = --uninstall ]; then
    echo 'Bifrost removed; your lan-workspace settings and pairing are unchanged.'
else
    echo 'Open Bifrost from your applications menu (or ~/.local/bin/bifrost), then pair your computers.'
fi
