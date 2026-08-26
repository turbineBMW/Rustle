#!/usr/bin/env sh
# Install the Rustle for the current user (no Flatpak).
#   sh install.sh            -> ~/.local (binary in ~/.local/bin)
#   PREFIX=/usr sh install.sh -> system-wide (run with sudo)
set -eu
PREFIX="${PREFIX:-$HOME/.local}"
APP_ID=io.github.turbinebmw.Rustle
BIN="$PREFIX/bin"
SHARE="$PREFIX/share"

cargo build --release
install -Dm755 target/release/rustle "$BIN/rustle"
install -Dm644 data/$APP_ID.gschema.xml "$SHARE/glib-2.0/schemas/$APP_ID.gschema.xml"
glib-compile-schemas "$SHARE/glib-2.0/schemas"
sed "s|^Exec=rustle|Exec=$BIN/rustle|" data/$APP_ID.desktop.in > "$SHARE/applications/$APP_ID.desktop.tmp" 2>/dev/null \
  || { install -d "$SHARE/applications"; sed "s|^Exec=rustle|Exec=$BIN/rustle|" data/$APP_ID.desktop.in > "$SHARE/applications/$APP_ID.desktop.tmp"; }
mv "$SHARE/applications/$APP_ID.desktop.tmp" "$SHARE/applications/$APP_ID.desktop"
install -d "$SHARE/dbus-1/services"
sed "s|@bindir@|$BIN|" data/$APP_ID.service.in > "$SHARE/dbus-1/services/$APP_ID.service"
install -Dm644 data/$APP_ID.metainfo.xml.in "$SHARE/metainfo/$APP_ID.metainfo.xml"
for size in 48 64 128 256 512; do
  install -Dm644 "data/icons/hicolor/${size}x${size}/apps/$APP_ID.png" "$SHARE/icons/hicolor/${size}x${size}/apps/$APP_ID.png"
done
install -Dm644 "data/icons/hicolor/symbolic/apps/$APP_ID-symbolic.svg" "$SHARE/icons/hicolor/symbolic/apps/$APP_ID-symbolic.svg"
gtk-update-icon-cache -q -t -f "$SHARE/icons/hicolor" 2>/dev/null || true
update-desktop-database -q "$SHARE/applications" 2>/dev/null || true
echo "Installed to $PREFIX. Run: $BIN/rustle"
