# Build, test and install the PipeWire SSC codec plugin.
#
#   make             build target/release/libspa_codec_bluez5_ssc.so
#   make check       unit tests + load the plugin through PipeWire's loader and encode/decode
#   make install     install as $(SPA_DIR)/bluez5/libspa-codec-bluez5-$(SLOT).so (DESTDIR for packaging)
#   make dist PW_SRC=<PipeWire $(PW_VERSION) checkout>
#                    release tarball target/dist/crossc-linux-<version>-x86_64.tar.gz (+ .sha256) with the
#                    plugin for both slots, crossc-cli, install.sh; built against that PipeWire's headers

SPA_DIR   ?= $(shell pkg-config --variable=plugindir libspa-0.2 2>/dev/null || echo /usr/lib/spa-0.2)
SPA_AUDIO ?= $(shell pkg-config --variable=includedir libspa-0.2 2>/dev/null || echo /usr/include)/spa-0.2/spa/param/bluetooth/audio.h
# Patched PipeWire (patches/pipewire-ssc.patch) has its own SSC slot; stock PipeWire lends the LC3plus one.
SLOT      ?= $(shell grep -q SPA_BLUETOOTH_AUDIO_CODEC_SSC $(SPA_AUDIO) 2>/dev/null && echo ssc || echo lc3plus)
CARGO     ?= cargo
LIB        = target/release/libspa_codec_bluez5_ssc.so
PLUGIN     = libspa-codec-bluez5-$(SLOT).so
TESTDIR    = target/check
CFLAGS    += -O1 -g -Wall -Wno-unused-parameter
PW_VERSION = $(notdir $(wildcard pipewire/[0-9]*))
VERSION    = $(shell sed -n 's/^version = "\(.*\)"/\1/p' spa-codec-ssc/Cargo.toml | head -n1)
DIST_NAME  = crossc-linux-$(VERSION)-x86_64
DIST       = target/dist

.PHONY: all check install uninstall dist clean FORCE

all: $(LIB)

$(LIB): FORCE
	SSC_CODEC_SLOT=$(SLOT) $(CARGO) build --release -p spa-codec-bluez5-ssc

$(TESTDIR)/harness: tests/harness.c $(wildcard tests/ssc-decoder/*.c)
	mkdir -p $(TESTDIR)
	$(CC) $(CFLAGS) -DSLOT='"$(SLOT)"' -Ipipewire/1.6.11 -Itests/ssc-decoder \
		$(shell pkg-config --cflags libpipewire-0.3) -o $@ tests/harness.c tests/ssc-decoder/*.c \
		$(shell pkg-config --libs libpipewire-0.3) -lm

# A private SPA plugin dir: the system plugins plus this one in the codec slot.
check: $(LIB) $(TESTDIR)/harness
	SSC_CODEC_SLOT=$(SLOT) $(CARGO) test --release -p spa-codec-bluez5-ssc
	rm -rf $(TESTDIR)/spa
	mkdir -p $(TESTDIR)/spa/bluez5
	for f in $(SPA_DIR)/*; do [ "$${f##*/}" = bluez5 ] || ln -s "$$f" $(TESTDIR)/spa/; done
	for f in $(SPA_DIR)/bluez5/*; do ln -s "$$f" $(TESTDIR)/spa/bluez5/; done
	ln -sf $(CURDIR)/$(LIB) $(TESTDIR)/spa/bluez5/$(PLUGIN)
	SPA_PLUGIN_DIR=$(CURDIR)/$(TESTDIR)/spa $(TESTDIR)/harness ../crates/ssc-enc/tests/vectors

install: $(LIB)
	install -Dm755 $(LIB) $(DESTDIR)$(SPA_DIR)/bluez5/$(PLUGIN)

uninstall:
	rm -f $(DESTDIR)$(SPA_DIR)/bluez5/$(PLUGIN)

# The SPA headers come from PW_SRC, stock for the lc3plus slot and patched for the ssc slot, so a
# release does not depend on the build machine's PipeWire.
dist:
	test -f "$(PW_SRC)/spa/include/spa/param/bluetooth/audio.h" || { echo "PW_SRC must be a PipeWire $(PW_VERSION) checkout"; exit 1; }
	rm -rf $(DIST)
	mkdir -p $(DIST)/$(DIST_NAME) $(DIST)/pw-ssc
	cp -r "$(PW_SRC)/spa" $(DIST)/pw-ssc/
	patch -d $(DIST)/pw-ssc -p1 -s < patches/pipewire-ssc.patch
	SSC_CODEC_SLOT=lc3plus SSC_SPA_INCLUDE="$(abspath $(PW_SRC))/spa/include" $(CARGO) build --release --locked -p spa-codec-bluez5-ssc
	cp $(LIB) $(DIST)/$(DIST_NAME)/libspa-codec-bluez5-lc3plus.so
	SSC_CODEC_SLOT=ssc SSC_SPA_INCLUDE="$(abspath $(DIST))/pw-ssc/spa/include" $(CARGO) build --release --locked -p spa-codec-bluez5-ssc
	cp $(LIB) $(DIST)/$(DIST_NAME)/libspa-codec-bluez5-ssc.so
	$(CARGO) build --release --locked -p crossc-cli --manifest-path ../Cargo.toml
	cp ../target/release/crossc-cli install.sh README.md $(DIST)/$(DIST_NAME)/
	echo "$(VERSION)" > $(DIST)/$(DIST_NAME)/VERSION
	echo "$(PW_VERSION)" > $(DIST)/$(DIST_NAME)/PIPEWIRE_VERSION
	tar -C $(DIST) --owner=0 --group=0 -czf $(DIST)/$(DIST_NAME).tar.gz $(DIST_NAME)
	cd $(DIST) && sha256sum $(DIST_NAME).tar.gz > $(DIST_NAME).tar.gz.sha256
	@cat $(DIST)/$(DIST_NAME).tar.gz.sha256

clean:
	$(CARGO) clean
