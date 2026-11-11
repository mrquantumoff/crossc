# crossc design

crossc makes Galaxy Buds a normal Windows playback device that streams Samsung's
SSC codec (Seamless HiFi 48 kHz/24-bit, UHQ 96 kHz/24-bit) over classic
Bluetooth A2DP. Windows' inbox A2DP driver cannot do that: it supports SBC, AAC
and aptX only and has no codec plug-in interface.

## Components

```
 apps -> Windows audio engine
            |  PCM (48k/24, 96k/24, 48k/16)
            v
 +------------------------------ crossc_bth.sys (KMDF, Rust) ----------------+
 |  ACX render endpoint "Headphones (Buds)"  ->  PCM tap (ring buffer)       |
 |  L2CAP pipe: OPEN / CLOSE / READ / WRITE on PSM 0x19 (AVDTP)              |
 +---------------^------------------------------------^----------------------+
                 | audio IOCTLs (attach, events, PCM)  | pipe IOCTLs
 +---------------+------------------------------------+----------------------+
 |  crossc-service.exe (LocalSystem, auto-start)                             |
 |  link manager -> AVDTP signaling -> SSC encoder (ssc-enc) -> RTP pacing   |
 +---------------------------------------------------------------------------+
                 | BTHENUM / bthport (inbox Bluetooth stack)
                 v
            Galaxy Buds (SSC SEP: vendor 0x0075, codec 0x0103)
```

| Piece | Path | Role |
|---|---|---|
| Driver | `drivers/crossc-bth` | Function driver for the buds' A2DP sink node, in place of `BthA2dp.sys`. Kernel part is thin: an L2CAP pipe, an ACX endpoint and a PCM tap. No codec or protocol logic. |
| Contract | `crates/crossc-ioctl` | IOCTL codes and wire structs shared by the driver and user mode (`no_std`, no `unsafe`). |
| Protocol | `crates/avdtp` | AVDTP signaling, capability parsing, RTP header (`no_std`). |
| Encoder | `crates/ssc-enc` | SSC encoder, byte-identical to the BSD reference encoder. |
| Core | `crates/crossc-core` | Device access, AVDTP session, stream setup, packetiser, pacing. Used by the service and the CLI. |
| Service | `crates/crossc-service` | Windows service that turns the endpoint's PCM into an SSC stream. |
| CLI | `crates/crossc-cli` | `probe`, `media`, `play`, `raw`: diagnostics and experiments. |
| Installer | `crates/crossc-installer` | GPUI app: install / uninstall the driver and the service, status. |

Encoding stays in user mode on purpose: the encoder is floating-point code, a
bug in it must not be able to crash the machine, and it can be updated without
touching the driver.

## Audio path contract (driver <-> service)

All additive to the pipe IOCTLs; `PROTOCOL_VERSION` 2.

| IOCTL | Direction | Behaviour |
|---|---|---|
| `AUDIO_ATTACH` | pends | Held by the service for its whole life. One holder at a time (`STATUS_DEVICE_BUSY` otherwise). When it completes or is cancelled (handle closed, process died) the driver detaches: jack unplugged, tap off, that handle's channels closed. |
| `AUDIO_SET_LINK` | in | `connected` flag = the buds have an open, configured SSC stream. Drives the endpoint's jack presence, so Windows shows the device as unplugged when the buds are away or the service is not running. |
| `AUDIO_GET_STATE` | out | Snapshot: sequence number, stream state (none / paused / running), stream format (rate, channels, valid bits, container bits), link flag, counters (frames rendered, frames dropped). |
| `AUDIO_WAIT_EVENT` | pends | Hanging get: completes when the state sequence differs from the caller's. |
| `AUDIO_READ_PCM` | pends | Completes with the next chunk of rendered PCM in the stream format: header (first frame index, frame count, flags: discontinuity) + interleaved samples. One reader. |

Rules the driver keeps regardless of the service:

* The audio engine never blocks on the service. The endpoint's clock is a
  driver timer; with nobody attached, or a reader that falls behind, PCM is
  dropped (counted, flagged as a discontinuity) and the stream keeps its pace.
* No memory is shared with user mode; PCM is copied through buffered IOCTLs.
* Everything the service can send is validated in the driver; a wrong or
  malicious caller gets an error, never a fault.
* Pipe and audio IOCTLs are for SYSTEM and Administrators only.

## Service behaviour

* **Follow, don't grab.** The service never pages the buds. It opens AVDTP only
  once Windows reports the buds connected (they connect by themselves when
  taken out of the case, or when the user presses Connect). The buds require the
  source to initiate AVDTP, so the service does that.
* **Link up:** DISCOVER, GET_ALL_CAPABILITIES, pick the SSC SEP,
  SET_CONFIGURATION for the mode matching the endpoint's current format
  (Delay Reporting included), OPEN, open the media channel, then
  `AUDIO_SET_LINK(connected)`.
* **Format -> mode**, limited by what the buds' SSC capability byte offers
  (the driver targets every Galaxy Buds model with an SSC endpoint, and older
  models have no HiFi or UHQ):

  | Endpoint format | Buds offer | Mode sent |
  |---|---|---|
  | 96 kHz | UHQ v2 (`0x02`, and not `0x01`) | UHQ, config `0x0A`, 584 kbps, one frame per packet |
  | 96 kHz | no UHQ | resampled to 48 kHz, then as the 48 kHz row |
  | 48 kHz | HiFi (`0x04`) | HiFi, config `0x0C`, 192 kbps, 24-bit (16-bit input widened) |
  | 48 kHz | 48 kHz SSC only (`0x10`) | SSC 48 kHz, config `0x10` (+`0x08` if offered), 16-bit (24-bit input dithered down) |
  | any | 44.1 kHz SSC only (`0x20`) | resampled to 44.1 kHz, SSC config `0x20` (+`0x08`) |

  The `0x08` bit is sent only when the buds offer it (every model with a
  reported capability byte does). Plain SSC runs at 229 kbps. Endpoint rates
  other than 48 and 96 kHz follow the 48 kHz rows with resampling.

  No SSC endpoint at all -> the link flag stays off and the reason is logged.
  A format change that needs another mode reconfigures (CLOSE,
  SET_CONFIGURATION, OPEN).
* **Stream running:** START, then one RTP packet per SSC frame (864 samples),
  paced by the PCM the endpoint produces. Idle for a few seconds after the
  stream pauses -> SUSPEND.
* **Volume:** the endpoint has no hardware volume, so Windows applies its own
  volume before the driver sees the audio.
* **The buds go away** (case, range, taken by the phone): channel closes or the
  sink sends CLOSE / ABORT -> `AUDIO_SET_LINK(disconnected)`, wait for the next
  connection. A sink SUSPEND is honoured and retried while audio is playing.
* **Settings** (`HKLM\SOFTWARE\crossc`): `UhqConfig` (default `0x0A`),
  `HifiBitrate` (192), `UhqBitrate` (584), `LogLevel`. Log:
  `%ProgramData%\crossc\service.log`.

## Out of scope for 0.1

AVRCP absolute volume (hardware volume on the buds), SBC/AAC fallback through
this driver, capture / hands-free (stays on the inbox driver), LE Audio,
multiple paired buds at once, production code signing.
