# BigNetScreen — native build and installation.
#
# `cargo` handles the binaries; this Makefile handles what Cargo does not
# install: the .desktop file, the D-Bus activation file, AppStream metainfo,
# the icon and the compiled translations.

PREFIX      ?= /usr
DESTDIR     ?=
BINDIR      := $(DESTDIR)$(PREFIX)/bin
DATADIR     := $(DESTDIR)$(PREFIX)/share
LOCALEDIR   := $(DATADIR)/locale
APPID       := br.com.biglinux.BigNetScreen
CARGO       ?= cargo
CARGO_FLAGS ?= --release --locked
CARGO_TARGET_DIR ?= target
BINARY  ?= $(CARGO_TARGET_DIR)/release/bignetscreen
DAEMON  ?= $(CARGO_TARGET_DIR)/release/bignetscreend
CTL     ?= $(CARGO_TARGET_DIR)/release/bignetscreenctl

LANGS := $(shell cat po/LINGUAS 2>/dev/null)
MOFILES := $(patsubst %,build/locale/%/LC_MESSAGES/bignetscreen.mo,$(LANGS))

.PHONY: all build locale install uninstall check clippy fmt test clean pot run

all: build locale

build:
	$(CARGO) build $(CARGO_FLAGS)

locale: $(MOFILES)

build/locale/%/LC_MESSAGES/bignetscreen.mo: po/%.po
	@mkdir -p $(dir $@)
	msgfmt --check "$<" -o "$@"

pot:
	./po/update-pot.sh

# Runs from the build tree, with the local translations.
run: locale
	BIGNETSCREEN_LOCALEDIR=$(CURDIR)/build/locale $(CARGO) run -p nd-gui

install: all
	install -Dm755 "$(BINARY)" "$(BINDIR)/bignetscreen"
	install -Dm755 "$(DAEMON)" "$(BINDIR)/bignetscreend"
	install -Dm755 "$(CTL)" "$(BINDIR)/bignetscreenctl"
	# What makes the service start on demand: a client calls the name, the
	# bus starts this, and nothing runs while nothing is being shared.
	install -Dm644 data/$(APPID).Service.service \
		$(DATADIR)/dbus-1/services/$(APPID).Service.service
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
	rm -f $(BINDIR)/bignetscreend
	rm -f $(BINDIR)/bignetscreenctl
	rm -f $(DATADIR)/dbus-1/services/$(APPID).Service.service
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
	$(CARGO) clippy --locked --workspace --all-targets -- -D warnings

test:
	$(CARGO) test --locked --workspace

clean:
	$(CARGO) clean
	rm -rf build
