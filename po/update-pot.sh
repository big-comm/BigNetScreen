#!/usr/bin/env sh
# Refreshes po/bignetscreen.pot from the Rust sources.
#
# `xgettext --language=C` chokes on Rust lifetimes ('static becomes an
# "unterminated character constant"), so extraction is done by the Python script
# next door, which understands the tr!/tr_n! macros.
#
# LangForge owns the catalogues and re-extracts on its own when translating;
# this script is for working without it. Both produce the same strings, so only
# the source references need reconciling afterwards (see below).
#
# The `.desktop` and AppStream files are deliberately left out. They carry their
# translations inline (`Comment[pt_BR]`, `xml:lang="pt_BR"`), which is the normal
# AppStream convention; feeding them to xgettext would turn the translated
# variants into source strings.
set -eu
cd "$(dirname "$0")/.."

python3 po/extract.py
# LangForge records absolute paths in the `#:` references, which would put the
# developer's home directory into every catalogue committed to git.
for file in po/*.po po/*.pot; do
    [ -f "$file" ] || continue
    sed -i "s|#: $PWD/|#: |g; s| $PWD/| |g" "$file"
done
echo "po/bignetscreen.pot updated"

# Re-apply the template to the existing languages.
while read -r lang; do
    [ -f "po/$lang.po" ] || continue
    msgmerge --quiet --update --backup=none "po/$lang.po" po/bignetscreen.pot
    echo "  po/$lang.po updated"
done < po/LINGUAS
