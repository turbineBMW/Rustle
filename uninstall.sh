#!/usr/bin/env sh
# Remove what install.sh put in place.
#   sh uninstall.sh                  -> from ~/.local
#   sh uninstall.sh --purge          -> and your mail cache, settings and saved data
#   PREFIX=/usr sh uninstall.sh      -> a system-wide install (run with sudo)
# Accounts live in Evolution Data Server and are left alone: remove the ones
# you added in Rustle from Manage Accounts first if you want them gone too.
set -eu
PREFIX="${PREFIX:-$HOME/.local}"
APP_ID=io.github.turbinebmw.Rustle
BIN="$PREFIX/bin"
SHARE="$PREFIX/share"

purge=false
for arg in "$@"; do
  case $arg in
    --purge) purge=true ;;
    -h|--help) sed -n '2,7p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $arg (try --help)" >&2; exit 2 ;;
  esac
done
is_root=false
if [ "$(id -u)" = 0 ]; then is_root=true; fi
if $purge && $is_root; then
  echo "--purge removes one user's data: run 'sh uninstall.sh --purge' as that user." >&2
  exit 2
fi

# A running Rustle (it may be in the background) quits the way Quit does, so
# a move still in its undo window is kept for the next launch, not lost.
if ! $is_root && pgrep -x rustle >/dev/null 2>&1; then
  gapplication action "$APP_ID" quit 2>/dev/null || true
fi

# The login unit: disabled, so nothing links to it, then removed.
if [ "$PREFIX" = "$HOME/.local" ]; then UNITDIR="$SHARE/systemd/user"; else UNITDIR="$PREFIX/lib/systemd/user"; fi
if [ -e "$UNITDIR/$APP_ID.service" ]; then
  if ! $is_root; then systemctl --user disable "$APP_ID.service" >/dev/null 2>&1 || true; fi
  rm -f "$UNITDIR/$APP_ID.service"
  if ! $is_root; then systemctl --user daemon-reload >/dev/null 2>&1 || true; fi
fi

# The D-Bus service goes first, so nothing can start Rustle again mid-way.
rm -f "$SHARE/dbus-1/services/$APP_ID.service"
rm -f "$BIN/rustle"
rm -f "$SHARE/applications/$APP_ID.desktop"
rm -f "$SHARE/metainfo/$APP_ID.metainfo.xml"
for size in 48 64 128 256 512; do
  rm -f "$SHARE/icons/hicolor/${size}x${size}/apps/$APP_ID.png"
done
rm -f "$SHARE/icons/hicolor/symbolic/apps/$APP_ID-symbolic.svg"
for mo in "$SHARE"/locale/*/LC_MESSAGES/rustle.mo; do
  if [ -e "$mo" ]; then rm -f "$mo"; fi
done
schemas="$SHARE/glib-2.0/schemas"
if [ -e "$schemas/$APP_ID.gschema.xml" ]; then
  rm -f "$schemas/$APP_ID.gschema.xml"
  # With no schema left to compile, glib-compile-schemas keeps the old
  # compiled file, which still lists Rustle's.
  if ls "$schemas"/*.gschema.xml >/dev/null 2>&1; then
    glib-compile-schemas "$schemas" >/dev/null 2>&1 || true
  else
    rm -f "$schemas/gschemas.compiled"
  fi
fi
gtk-update-icon-cache -q -t -f "$SHARE/icons/hicolor" 2>/dev/null || true
update-desktop-database -q "$SHARE/applications" 2>/dev/null || true

# "Start at Login" left an autostart entry that would now fail at every login.
if ! $is_root; then
  rm -f "${XDG_CONFIG_HOME:-$HOME/.config}/autostart/$APP_ID.desktop"
fi

if $purge; then
  rm -rf "${XDG_DATA_HOME:-$HOME/.local/share}/rustle"
  rm -rf "${XDG_CACHE_HOME:-$HOME/.cache}/rustle"
  if command -v dconf >/dev/null 2>&1; then
    dconf reset -f /io/github/turbinebmw/Rustle/ 2>/dev/null || true
  fi
  # Passwords from before accounts moved to EDS, if any are left.
  if command -v secret-tool >/dev/null 2>&1; then
    secret-tool clear xdg:schema "$APP_ID.Account" 2>/dev/null || true
  fi
  echo "Removed Rustle's mail cache, settings and saved data."
fi

# Accounts made in Rustle stay in EDS, where Evolution and others can use them.
sources="${XDG_CONFIG_HOME:-$HOME/.config}/evolution/sources"
if ! $is_root && ls "$sources"/rustle-*.source >/dev/null 2>&1; then
  echo "Accounts you added in Rustle are still set up on this desktop, in Evolution"
  echo "Data Server, for other apps to use:"
  for source in "$sources"/rustle-*.source; do
    sed -n 's/^DisplayName=/  /p' "$source" | head -n 1
  done
  echo "To remove them too, delete them in Rustle's Manage Accounts before"
  echo "uninstalling, or in Evolution."
fi
# The built-in Microsoft 365 bridge keeps its setup with graphmail-bridge's own.
if ! $is_root && [ -d "${XDG_CONFIG_HOME:-$HOME/.config}/graphmail-bridge" ]; then
  echo "graphmail-bridge's setup in ~/.config/graphmail-bridge is kept; its own"
  echo "uninstall script removes it."
fi

echo "Uninstalled Rustle from $PREFIX."
