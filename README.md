# crossc

Samsung's SSC codec for Galaxy Buds on Windows: **Seamless HiFi (48 kHz / 24-bit)
and UHQ (96 kHz / 24-bit)** over classic Bluetooth, as a normal Windows playback
device.

Windows' own Bluetooth audio driver speaks SBC, AAC and aptX and has no way to
add a codec. Galaxy Buds only reach 24-bit and 96 kHz through Samsung's SSC
codec, which Samsung ships on Galaxy phones only. crossc replaces the Windows
A2DP driver for the buds with one that carries SSC, and encodes the audio with
an open encoder.

> **Experimental.** This is a test-signed kernel driver. It needs Windows test
> signing mode with Secure Boot off, and a driver bug means a blue screen. Read
> [Before you install](#before-you-install) first. No warranty.

## Status

| | |
|---|---|
| Galaxy Buds4 Pro, Buds3 Pro | Tested: HiFi 24/48 plays correctly. UHQ 24/96 works from `crossc-cli play` but not yet through the Windows playback device (see [Known issues](#known-issues)) |
| Other Galaxy Buds | Listed by product ID, untested: see [docs/MODELS.md](docs/MODELS.md) |
| Windows | Windows 11 x64 (developed on build 26340). Not tested on Windows 10 or ARM64 |
| Bluetooth radio | Developed on a MediaTek RZ616 (MT7922) with the inbox Microsoft stack |

What works: the buds show up as *Headphones (your buds' name)* and any app
plays through them in HiFi (48 kHz, 24-bit).
The device shows as unplugged while the buds are away.

The Windows volume slider and the buds' own volume are one control (AVRCP
absolute volume through Windows' own AVRCP stack).

The bitrate adapts to the link: the service steps the SSC rate down while the
Bluetooth link is congested and back up when it is clean again, without
stopping the stream.

A tray icon (`crossc-tray`) shows what the service is sending to the buds,
with a details window. The installer installs it and has it start with
Windows; see [docs/SETUP.md](docs/SETUP.md) section 16. (An update started
from the 0.2.0 installer's own **Download and install** does not install the
tray: that installer does not know it. Run the new `crossc-installer.exe`
once and press **Install**.)

The tray icon has been checked with real buds. The adaptive bitrate's live test
under a congested link ([docs/SETUP.md](docs/SETUP.md) section 15) has not been
run yet.

## Known issues

* **UHQ (96 kHz) is not used by the Windows playback device.** Choosing
  *24 bit, 96000 Hz* in sound settings is saved, and the driver takes it as its
  default format, but Windows keeps rendering at 48 kHz, so the buds get HiFi.
  Restarting the device did not change it either. Under investigation; see
  [docs/SETUP.md](docs/SETUP.md) section 17. UHQ itself works: `crossc-cli play`
  streams 96 kHz to the buds directly.

What does not exist yet: SBC/AAC fallback through this driver, production code
signing.
Hands-free (call) audio and the microphone stay on Windows' own driver and are
unaffected.

## How it works

```
apps -> Windows audio engine -> crossc_bth.sys (ACX playback device + Bluetooth L2CAP pipe)
                                      |  PCM                      ^  SSC packets
                                      v                           |
                                 crossc-service (AVDTP signaling, SSC encoder, pacing)
```

* **`crossc_bth.sys`** (Rust, KMDF + ACX) is the function driver for the buds'
  A2DP service in place of `BthA2dp.sys`. It is deliberately thin: a playback
  endpoint, a tap that hands the rendered PCM to user mode, and a raw L2CAP
  channel to the buds. No codec or protocol logic runs in the kernel.
* **`crossc-service`** (Rust, runs as a Windows service) negotiates the stream
  with the buds, encodes with `ssc-enc`, and sends one SSC frame per packet.
  It follows the buds: it only talks to them once Windows reports them
  connected.
* **`crossc-tray`** is the icon in the notification area. It runs as the
  signed-in user, not elevated, and only reads the status the service
  publishes on a named pipe.
* **`ssc-enc`** is a Rust port of the BSD-licensed
  [samsung-ssc-encoder](https://github.com/WillyBilly06/samsung-ssc-encoder),
  byte-identical to it in every mode and bitrate.

More in [docs/DESIGN.md](docs/DESIGN.md).

## Before you install

1. **Test signing.** The driver is signed with a throwaway test certificate
   created by the build. Windows only loads it in test mode:
   Secure Boot **off** in the firmware setup, then, in an admin terminal,
   `bcdedit /set testsigning on` and reboot. If BitLocker is on, have the
   recovery key ready before touching Secure Boot. Some anti-cheat and DRM
   software refuses to run in test mode.
2. **It replaces Windows' A2DP driver for the listed buds.** While installed,
   those buds get stereo audio only through crossc. Uninstalling puts the
   Windows driver back.
3. **Recovery.** If Windows blue-screens at start-up after installing, boot to
   Safe Mode (the buds' driver is not started there) and uninstall, or set the
   kill switch described in [docs/SETUP.md](docs/SETUP.md) section 10.

## Install

1. Download `crossc-<version>-x64.zip` from the
   [newest release](https://github.com/mrquantumoff/crossc/releases) and
   unpack it.
2. Pair the buds with Windows as usual. Turn LE Audio off for them.
3. Run `crossc-installer.exe`, press **Restart as administrator**, then
   **Install**. It trusts the bundle's test certificate, installs the driver on
   the buds only, installs and starts the service, and starts the tray icon.
4. In Windows sound settings select *Headphones (your buds)*. Under the
   device's properties choose **24 bit, 48000 Hz** (HiFi) or **24 bit,
   96000 Hz** (UHQ).

The installer can also fetch and install the newest release itself, to update
or when it was downloaded on its own: **Check for update**, then **Download and
install**. It checks the download against the release's `SHA256SUMS.txt` and
shows the certificate before it asks to trust it. **Uninstall** removes the
service, the tray icon and the driver, **Remove certificate** the certificate.

## Build from source

Requirements: Rust (stable, MSVC), Visual Studio Build Tools, the Windows SDK
and WDK 10.0.26100, LLVM (`LIBCLANG_PATH`), `cargo install cargo-make`.

```powershell
cargo build --release                 # service, tray, CLI, installer, libraries
cd drivers\crossc-bth
cargo make default --release          # driver package (.sys/.inf/.cat + test certificate)
```

`tools\install-driver.ps1` or `cargo run -p crossc-installer --release` installs
a local build. `tools\package.ps1` assembles the release bundle; CI does the
same on every tag.

`crossc-cli` is the diagnostics tool: `probe` lists the buds' codecs, `play`
streams a tone, sweep or WAV file straight to the buds without the Windows
audio device.

## Repository layout

| Path | What |
|---|---|
| `drivers/crossc-bth` | Kernel driver (own cargo workspace) |
| `crates/crossc-ioctl` | IOCTL contract shared by driver and user mode |
| `crates/avdtp` | AVDTP signaling, capabilities, RTP header |
| `crates/ssc-enc` | SSC encoder |
| `crates/crossc-core` | Device access, stream session, packetiser, resampler |
| `crates/crossc-service` | The Windows service |
| `crates/crossc-status` | Status the service publishes on a named pipe: protocol, server, client |
| `crates/crossc-tray` | Tray icon and details window |
| `crates/crossc-cli` | `probe`, `media`, `play`, `raw` |
| `crates/crossc-installer` | Installer (GPUI) |
| `docs/` | Design, setup and test notes, supported models |

## License and provenance

crossc is licensed under the [BSD 2-Clause License](LICENSE).

* `crates/ssc-enc` is a port of samsung-ssc-encoder (BSD-3-Clause, © William
  Ho), which adapts the CELT layer of Opus 1.1 (BSD-3-Clause, © Xiph.Org and
  contributors). Their notices are in `crates/ssc-enc/NOTICE` and
  `LICENSE-THIRD-PARTY` and apply to that crate.
* The codec's format-specific tables and parameters in that encoder were
  obtained by its upstream author through reverse engineering of Samsung's
  encoder for interoperability. No Samsung code or binary is part of this
  repository.
* The driver's Bluetooth channel and audio endpoint code follow the structure
  of Microsoft's `bthecho` and ACX samples
  ([Windows-driver-samples](https://github.com/microsoft/Windows-driver-samples),
  MS-PL) and are built with
  [windows-drivers-rs](https://github.com/microsoft/windows-drivers-rs).
* Release bundles include a generated list of third-party dependency licenses.

**Patents.** Samsung describes SSC as patented technology. No patent search
has been done for this project, and the patent grants that come with Opus are
tied to Opus itself. Use and redistribution are at your own risk.

**Trademarks.** Samsung, Galaxy and Galaxy Buds are trademarks of Samsung
Electronics. Bluetooth is a trademark of Bluetooth SIG. This project is not
affiliated with or endorsed by either.
