# crossc for Linux

Samsung's SSC codec for Galaxy Buds on Linux: **Seamless HiFi (48 kHz / 24-bit)
and UHQ (96 kHz / 24-bit)** as an A2DP codec of PipeWire.

On Windows crossc has to replace the A2DP driver. On Linux, BlueZ and
PipeWire already handle AVDTP, pacing, volume and delay reporting, and
PipeWire loads A2DP codecs as plugins. So the Linux part is only that plugin:
`libspa-codec-bluez5-*.so`, written in Rust, exposing PipeWire's codec ABI
through `extern "C"` and encoding with [`ssc-enc`](../crates/ssc-enc), the
same encoder the Windows service uses.

> **Experimental.** Encoding, packetisation and the plugin ABI are tested
> offline (see [Tests](#tests)). Playback to real buds on Linux has not been
> tested yet.

## How PipeWire finds it

PipeWire's codec loader has a fixed list of plugin names and a fixed codec id
enum. There are two ways in:

| | Stock PipeWire | PipeWire with `patches/pipewire-ssc.patch` |
|---|---|---|
| Plugin file | `bluez5/libspa-codec-bluez5-lc3plus.so` | `bluez5/libspa-codec-bluez5-ssc.so` |
| Codec ids | `LC3PLUS_HR` and `MPEG` (borrowed; no PipeWire plugin uses them) | `SSC` and `SSC_UHQ` (new) |
| Profiles | `a2dp-sink-ssc` (HiFi), `a2dp-sink-ssc_uhq` (UHQ) | the same |

Stock PipeWire works because distributions build it without LC3plus
(`liblc3plus` is not packaged), which leaves that slot empty. If yours ships
`libspa-codec-bluez5-lc3plus.so`, use the patch instead. The build picks the
slot by itself: `ssc` when the installed SPA headers have
`SPA_BLUETOOTH_AUDIO_CODEC_SSC`, else `lc3plus`. Override with `SLOT=`.

The plugin targets PipeWire **1.6.11**: the codec ABI is private, so its
headers are vendored in `pipewire/1.6.11` and the build warns when the
installed PipeWire differs. For another version, point `SSC_PIPEWIRE_HEADERS`
at `spa/plugins/bluez5` of that version's source tree. The loader refuses a
plugin whose ABI version differs, it does not crash.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/mrquantumoff/crossc/master/linux/install.sh | bash
# or, with UHQ 96 kHz switched on automatically for audio above 48 kHz:
curl -fsSL https://raw.githubusercontent.com/mrquantumoff/crossc/master/linux/install.sh | bash -s -- --uhq auto
```

The script downloads the latest release's `crossc-linux-<version>-x86_64.tar.gz`,
checks it against its `.sha256`, and installs the plugin into PipeWire's SPA
plugin directory (in the slot that PipeWire has, see above) and `crossc-cli`
into `/usr/local/bin`. It asks for sudo only to copy those two files, then
restarts PipeWire and WirePlumber in your session and reconnects your
Bluetooth headphones (the restart drops their A2DP link). It refuses to replace a
real LC3plus plugin, and it warns when your PipeWire is not the version the
plugin was built for.

Then pick the *SSC HiFi* profile in your sound settings (or
`pactl set-card-profile bluez_card.XX_XX_XX_XX_XX_XX a2dp-sink-ssc`), and
choose how UHQ 96 kHz is used with `crossc-cli uhq auto`, `uhq on` or
`uhq off` ([UHQ on, off or automatic](#uhq-on-off-or-automatic)).

Options, also for a downloaded copy of the script (`install.sh --help`):

| | |
|---|---|
| `--version v0.4.0` | that release instead of the latest |
| `--from-source [DIR]` | build the crossc checkout in DIR (default: the one holding the script); needs Rust, clang and pkg-config |
| `--uhq auto\|on\|off` | afterwards run `crossc-cli uhq <mode>`: UHQ for audio above 48 kHz, always, or never |
| `--uninstall` | remove the plugin, `crossc-cli`, the UHQ auto service and its 96 kHz rule |
| `--no-restart` | leave PipeWire running; the plugin loads at its next start |

`PREFIX` (default `/usr/local`) and `SPA_DIR` (default: found automatically)
change where files go. `curl ... | bash -s -- --uninstall` passes options
through the pipe.

## Update

```sh
crossc-cli update            # install the latest release if it is newer
crossc-cli update --check    # only say whether there is one
crossc-cli update --version v0.4.0 | --force
```

`crossc-cli update` reads the version stamped into the installed plugin,
asks GitHub for the latest release, and when that is newer downloads its
Linux tarball, checks the `.sha256` (and GitHub's digest of the asset), and
runs the `install.sh` inside it. So the plugin and `crossc-cli` itself are
updated together, and a UHQ auto service keeps running on the new
`crossc-cli`. Downloads go through the system's `curl`. The `update`
command exists only in Linux builds of `crossc-cli`.

## Build from source

Needs Rust 1.89+, clang (for bindgen), and the PipeWire and SPA development
headers (`pipewire` on Arch, `libpipewire-0.3-dev` on Debian/Ubuntu).

```sh
make                 # target/release/libspa_codec_bluez5_ssc.so
make check           # unit tests + plugin loaded through PipeWire's loader
sudo make install    # into $(pkg-config --variable=plugindir libspa-0.2)/bluez5/
systemctl --user restart pipewire wireplumber
```

`sudo make uninstall` removes it. `install.sh --from-source` does the same
and also installs `crossc-cli`. `make dist PW_SRC=<PipeWire 1.6.11 checkout>`
builds the release tarball, with the plugin for both slots, as the release
workflow does.

## Modes and settings

The buds' SSC capability octet decides what is possible; the plugin sends
the same configuration octets as Samsung phones and the Windows service:

| Mode | Config | Stream | Bitrates (kb/s), highest first |
|---|---|---|---|
| UHQ | `0x0A` | 96 kHz, 24-bit | 584 (1 frame/packet), 291 and 152 (2 frames/packet) |
| HiFi | `0x0C` | 48 kHz, 24-bit | 229, 192, 128, 88 |
| SSC 48 / 44.1 kHz | `0x18` / `0x28` | 16-bit | 229, 128, 88 |

The plugin registers two codecs on the buds' one SSC endpoint, so PipeWire
lists two profiles: **SSC HiFi** (`a2dp-sink-ssc`: HiFi, or plain SSC on buds
without HiFi) and **SSC UHQ 96 kHz** (`a2dp-sink-ssc_uhq`, only on buds that
offer UHQ). Switching between them renegotiates the stream in about a second,
without restarting PipeWire, and WirePlumber remembers the last one per
device. Only rates the buds are known to decode are sent, and only those
whose packet fits the media MTU.

### UHQ on, off or automatic

```sh
crossc-cli uhq            # status: mode, stream rate, auto mode
crossc-cli uhq on         # UHQ 96 kHz now
crossc-cli uhq off        # HiFi 48 kHz now
crossc-cli uhq auto       # UHQ while audio above 48 kHz plays, HiFi otherwise
```

`on` and `off` switch the connected buds at once and turn auto mode off.
`auto` installs and starts a systemd user service, `crossc-uhq-auto.service`,
that runs `crossc-cli uhq watch`. It follows the PipeWire graph and switches
to UHQ once a stream above 48 kHz has played into the buds for a second, and
back to HiFi after 10 s without one, so pauses and track changes do not flap
it. Two switches are at least 5 s apart. It acts only while the buds are on
one of the SSC profiles, so choosing AAC by hand stays respected.

PipeWire changes its graph rate only while the output is idle, and a
browser usually keeps a 48 kHz stream open, so 96 kHz audio would be
resampled to 48 kHz and back. `uhq on` and `uhq auto` therefore install a
WirePlumber rule, `wireplumber.conf.d/51-crossc-uhq-rate.conf`, that gives
the UHQ output `node.force-rate = 96000`: while the buds play UHQ the graph
runs at 96 kHz and other streams are resampled up instead. The HiFi output
and other devices keep PipeWire's normal rate choice. Installing the rule
restarts WirePlumber once, which drops the buds, so the command reconnects
them. `default.clock.allowed-rates` should include 96000 as well
(`crossc-cli uhq` warns when it does not).

Note that most downloads are 44.1 kHz/16-bit (CD) or 48 kHz; only files
above 48 kHz switch auto mode to UHQ. `metaflac --show-sample-rate file.flac`
tells.

Settings go in the `monitor.bluez.properties` of a WirePlumber config, e.g.
`~/.config/wireplumber/wireplumber.conf.d/51-ssc.conf`:

```
monitor.bluez.properties = {
  # bluez5.a2dp.ssc.mode = ssc48     # SSC HiFi profile: hifi (default) | ssc48 | ssc44
  # bluez5.a2dp.ssc.max-bitrate = 192 # kb/s cap; the ladder starts below it
  # bluez5.a2dp.ssc.adaptive = false  # one fixed rate
  # bluez5.a2dp.ssc.uhq-alt-config = true  # UHQ profile: send 0x0E instead of 0x0A
}
```

**Adaptive bitrate.** PipeWire's media sink tells a codec when the socket is
full (one step down at once) and, about once a second, when nothing is
queued. The plugin steps up after 10 s of that. If the rate falls back within
15 s of a step up, the wait for that rung doubles, up to 80 s. A rate change
waits for the next packet, so a packet never mixes rates. This follows the
Windows service's controller (`crates/crossc-core/src/abr.rs`), reduced to the
signals PipeWire gives a codec.

## Handoff from a Samsung phone

[seuil](https://github.com/Eddie02345/seuil) is a separate daemon that uses
Samsung's Continuity protocol, so a Galaxy phone hands the buds to the
desktop the same way it hands them to another Galaxy device (Auto Switch).
It decides when the buds switch over; crossc decides which codec they use
once they are connected. When seuil brings the buds over, BlueZ connects
A2DP and WirePlumber restores the profile it saved for them. With no saved
profile, stock PipeWire ranks the borrowed LC3plus slot below AAC and picks
AAC, so choose *SSC* once by hand; with the patch, SSC already ranks above
AAC.

seuil is installed on its own (see its README) and is GPL-3.0; crossc does
not include or link any of its code. The two have not been tested together
with real buds yet.

## Tests

`make check` runs:

* Unit tests for negotiation (the octet table above, including the
  "sink disables UHQ" bit and capability elements with extra octets), packet
  sizes against `docs/DESIGN.md`, MTU fitting, adaptive bitrate, the RTP
  header and the pod layout.
* `tests/harness.c`, which loads the built `.so` through PipeWire's own plugin
  loader exactly as `codec-loader.c` does and drives it like `media-sink.c`:
  ABI version, capabilities, `select_config`, `validate_config`, a format
  pod that SPA parses, then encoding of real music in HiFi, UHQ, plain SSC,
  with a congestion step to two frames per packet and a tight MTU. Every
  frame must decode with the reference SSC decoder (`tests/ssc-decoder`, BSD,
  from [samsung-ssc-encoder](https://github.com/WillyBilly06/samsung-ssc-encoder)),
  every RTP header must match, and no packet may exceed the MTU. At 192 kb/s
  the frames' sizes and headers match Samsung's own encoder output.

What is not tested yet: playback to real buds over BlueZ, and the adaptive
bitrate on a congested link.

## Layout

| Path | What |
|---|---|
| `spa-codec-ssc/src/ssc.rs` | SSC over A2DP: capability octet, mode choice, ladders, adaptive bitrate, RTP header. No FFI. |
| `spa-codec-ssc/src/codec.rs` | The `struct media_codec` callbacks. |
| `spa-codec-ssc/src/plugin.rs` | The SPA handle factory and the exported log topic. |
| `spa-codec-ssc/src/pod.rs` | The SPA format pod (the C builder is `static inline`). |
| `spa-codec-ssc/build.rs` | bindgen over the vendored headers; slot selection. |
| `pipewire/` | PipeWire's private codec headers (MIT). |
| `patches/pipewire-ssc.patch` | Gives PipeWire its own SSC codec id and slot. |
| `install.sh` | Installer and uninstaller; also what `crossc-cli update` runs. |
| `../crates/crossc-cli/src/update.rs` | `crossc-cli update` (Linux builds only). |
| `../crates/crossc-cli/src/uhq.rs` | `crossc-cli uhq` and its auto mode (Linux builds only). |

This directory is its own Cargo workspace, so the Windows workspace at the
repository root never builds it.
