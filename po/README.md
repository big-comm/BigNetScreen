# Translations

The `msgid`s are in **English**, the project's source language. The interface
currently ships in 29 languages; every catalogue lives in this directory and is
handled with standard gettext tooling.

## Refreshing the template (`.pot`)

```sh
./po/update-pot.sh
```

`xgettext --language=C` chokes on Rust lifetimes (`'static` reads as an
unterminated character constant), so the extraction is done by `extract.py`,
which understands the `tr!`/`tr_n!` macros. The script also normalises the
source references to relative paths — translation tooling tends to record
absolute ones, which would put a developer's home directory into every
committed catalogue.

The `.desktop` and AppStream files are deliberately left out of the template:
they carry their translations inline (`Comment[pt_BR]`, `xml:lang="pt_BR"`),
which is the usual AppStream convention.

## Adding a language

```sh
msginit --locale=fr --input=po/bignetscreen.pot --output=po/fr.po
echo fr >> po/LINGUAS
```

## Compiling and installing

The `Makefile` at the root does this as part of `make install`:

```sh
msgfmt po/<lang>.po -o <prefix>/share/locale/<lang>/LC_MESSAGES/bignetscreen.mo
```

To test without installing:

```sh
BIGNETSCREEN_LOCALEDIR=./build/locale cargo run -p nd-gui
```

## Two things worth checking after a bulk translation

1. **No `fuzzy` markers.** gettext ignores a message flagged `fuzzy`, and a
   `fuzzy` *header* makes it ignore the catalogue's metadata entirely — which
   is how a catalogue ends up looking as if it had no project name at all.
   `msgfmt --check --statistics` reports both.
2. **Relative source references.** Run `./po/update-pot.sh` once before
   committing; it strips absolute paths.

The archived C project shipped 28 languages, but **none of its strings could be
reused**: a `msgcomm` between the two catalogues finds zero msgids in common,
because the rewrite words its interface differently. New languages start from
`po/bignetscreen.pot`.
