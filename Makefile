# BigNetScreen — native build and installation.
#
# `cargo` handles the binary; this Makefile handles what Cargo does not install:
# the .desktop file, AppStream metainfo, the icon and the compiled translations.

PREFIX      ?= /usr
DESTDIR     ?=
BINDIR      := $(DESTDIR)$(PREFIX)/bin
DATADIR     := $(DESTDIR)$(PREFIX)/share
LOCALEDIR   := $(DATADIR)/locale
APPID       := br.com.biglinux.BigNetScreen
CARGO       ?= cargo
CARGO_FLAGS ?= --release --locked

LANGS := $(shell cat po/LINGUAS 2>/dev/null)
MOFILES := $(patsubst %,build/locale/%/LC_MESSAGES/bignetscreen.mo,$(LANGS))

.PHONY: all build locale install uninstall check clippy fmt test clean pot run

all: build locale

build:
	$(CARGO) build $(CARGO_FLAGS)

locale: $(MOFILES)

build/locale/%/LC_MESSAGES/bignetscreen.mo: po/%.po
	@mkdir -p $(dir $@)
	msgfmt $< -o $@

pot:
	./po/update-pot.sh

# Runs from the build tree, with the local translations.
run: locale
	BIGNETSCREEN_LOCALEDIR=$(CURDIR)/build/locale $(CARGO) run -p nd-gui

install: all
	install -Dm755 target/release/bignetscreen $(BINDIR)/bignetscreen
	install -Dm644 data/$(APPID).desktop $(DATADIR)/applications/$(APPID).desktop
	install -Dm644 data/$(APPID).metainfo.xml $(DATADIR)/metainfo/$(APPID).metainfo.xml
	install -Dm644 data/icons/$(APPID).svg \
		$(DATADIR)/icons/hicolor/scalable/apps/$(APPID).svg
	@for lang in $(LANGS); do \
		install -Dm644 build/locale/$$lang/LC_MESSAGES/bignetscreen.mo \
			$(LOCALEDIR)/$$lang/LC_MESSAGES/bignetscreen.mo; \
	done

uninstall:
	rm -f $(BINDIR)/bignetscreen
	rm -f $(DATADIR)/applications/$(APPID).desktop
	rm -f $(DATADIR)/metainfo/$(APPID).metainfo.xml
	rm -f $(DATADIR)/icons/hicolor/scalable/apps/$(APPID).svg
	@for lang in $(LANGS); do \
		rm -f $(LOCALEDIR)/$$lang/LC_MESSAGES/bignetscreen.mo; \
	done

check: fmt clippy test

fmt:
	$(CARGO) fmt --all -- --check

clippy:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

test:
	$(CARGO) test --workspace

clean:
	$(CARGO) clean
	rm -rf build
