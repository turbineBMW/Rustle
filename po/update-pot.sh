#!/bin/sh
# Regenerate po/rustle.pot: every string marked for translation in the Rust
# sources (gettext, plural), the Blueprints (_ and C_), the desktop file and
# the metainfo. A translation is po/<lang>.po, started with
#   msginit -i po/rustle.pot -o po/<lang>.po -l <lang>
# and refreshed after this with
#   msgmerge -U po/<lang>.po po/rustle.pot
# install.sh compiles them.
set -eu
cd "$(dirname "$0")/.."
out=po/rustle.pot
common="--from-code=UTF-8 --add-comments=TRANSLATORS --package-name=Rustle --msgid-bugs-address=https://github.com/turbineBMW/Rustle/issues"
git ls-files 'crates/*.rs' > po/.rust-files
xgettext $common --language=Rust --keyword=gettext --keyword=plural:1,2 \
  --files-from=po/.rust-files -o "$out"
git ls-files 'crates/rustle/ui/*.blp' > po/.blp-files
xgettext $common --language=C --keyword=_ --keyword=C_:1c,2 \
  --files-from=po/.blp-files --join-existing -o "$out"
xgettext $common --join-existing -o "$out" \
  data/io.github.turbinebmw.Rustle.desktop.in data/io.github.turbinebmw.Rustle.metainfo.xml.in
rm -f po/.rust-files po/.blp-files
echo "Wrote $out ($(grep -c '^msgid ' "$out") strings)"
