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
| Core | `crates/crossc-core` | Device access, AVDTP session, stream setup, packetiser, pacing, adaptive bitrate. Used by the service and the CLI. |
| Service | `crates/crossc-service` | Windows service that turns the endpoint's PCM into an SSC stream, and publishes what it is doing on the status pipe. |
| Status | `crates/crossc-status` | The status feed between the service and its user interfaces: the snapshot, its wire format and the named pipe `\\.\pipe\crossc-status`. |
| Tray | `crates/crossc-tray` | Notification-area icon and details window of the logged-on user; reads the status pipe, never the driver. |
| CLI | `crates/crossc-cli` | `probe`, `media`, `play`, `raw`: diagnostics and experiments. |
| Installer | `crates/crossc-installer` | GPUI app: install / uninstall the driver and the service, status. |

Encoding stays in user mode on purpose: the encoder is floating-point code, a
bug in it must not be able to crash the machine, and it can be updated without
touching the driver.

## Audio path contract (driver <-> service)

All additive to the pipe IOCTLs; `PROTOCOL_VERSION` 4 (2 added the audio
IOCTLs, 3 the volume state in `AUDIO_GET_STATE`, see "Volume" below, 4
`AUDIO_SET_DEFAULT_FORMAT` and the default format in `AUDIO_GET_STATE`, see
"Format selection" below).

| IOCTL | Direction | Behaviour |
|---|---|---|
| `AUDIO_ATTACH` | pends | Held by the service for its whole life. One holder at a time (`STATUS_DEVICE_BUSY` otherwise). When it completes or is cancelled (handle closed, process died) the driver detaches: jack unplugged, tap off, that handle's channels closed. |
| `AUDIO_SET_LINK` | in | `connected` flag = the buds have an open, configured SSC stream. Drives the endpoint's jack presence, so Windows shows the device as unplugged when the buds are away or the service is not running. |
| `AUDIO_GET_STATE` | out | Snapshot: sequence number, stream state (none / paused / running), stream format (rate, channels, valid bits, container bits), link flag, counters (frames rendered, frames dropped). Protocol 3 appends the volume state (64 bytes in all); a 40-byte buffer still gets the v2 part, and a v3 reader takes a 40-byte reply from a v2 driver as "no hardware volume". |
| `AUDIO_WAIT_EVENT` | pends | Hanging get: completes when the state sequence differs from the caller's. |
| `AUDIO_READ_PCM` | pends | Completes with the next chunk of rendered PCM in the stream format: header (first frame index, frame count, flags: discontinuity) + interleaved samples. One reader. |
| `AUDIO_SET_DEFAULT_FORMAT` | in | Protocol 4. Makes one of the offered formats the endpoint's default, the one Windows opens its next stream in; stored in the device's hardware key. Any admin handle, attached or not. `AUDIO_GET_STATE` reports the default behind the v3 bytes (72 bytes in all; a 64-byte reply, from a v3 driver, reads as "unknown"). |

Rules the driver keeps regardless of the service:

* The audio engine never blocks on the service. The endpoint's clock is a
  driver timer; with nobody attached, or a reader that falls behind, PCM is
  dropped (counted, flagged as a discontinuity) and the stream keeps its pace.
* The endpoint's host pin offers its formats (48 kHz/24-in-32,
  96 kHz/24-in-32, 48 kHz/16; `OFFERED_FORMATS` in `crossc-ioctl`) in the
  RAW signal processing mode list only, as 0.2.0 did. Which one is the
  list's default is chosen per device ("Format selection" below); 48 kHz/24
  until something else is chosen. The driver can also give the pin a
  DEFAULT list with the same formats and default (`DEFAULT_MODE_LIST` in
  `acx.rs`), but that is off: it does not change the rate by itself (the
  default does), and with two modes the audio engine can run two streams at
  once, which the driver does not mix: only one would reach the buds.
* Several streams can exist at once (a format switch creates the new one
  before the old one is gone); the stream state and the tap describe one of
  them: a new stream takes the tap unless the current one is running, the
  last stream to start running takes it, and when the tap's stream pauses or
  ends while another runs, the tap moves there; when it ends and none runs,
  the tap moves to the newest stream left. Each move is a new stream to the
  service (sequence bump, empty ring, discontinuity).
* No memory is shared with user mode; PCM is copied through buffered IOCTLs.
* Everything the service can send is validated in the driver; a wrong or
  malicious caller gets an error, never a fault.
* Pipe and audio IOCTLs are for SYSTEM and Administrators only.

## Format selection

Windows does not open the endpoint's stream in the format picked in the
sound settings. ACX answers `KSPROPERTY_PIN_PROPOSEDATAFORMAT2` with the
default format of the host pin's mode list, and "if driver supports this
property, OS will always use this format for the specific signal
processing mode" (WDK, `ksproperty-pin-proposedataformat2`). With a fixed
48 kHz default that meant: the user picks 96000 Hz, the endpoint's
`PKEY_AudioEngine_DeviceFormat` says 96000, `GetMixFormat` and the stream
the driver renders say 48000, and UHQ is never used. So the default follows
the choice:

* **Driver.** The default is a per-device index into the offered formats.
  It is read from `DefaultSampleRate` / `DefaultBits` (REG_DWORD) in the
  device's hardware key whenever the circuit is built (every
  PrepareHardware); missing or unknown means 48 kHz/24.
  `AUDIO_SET_DEFAULT_FORMAT` changes it at run time:
  `AcxDataFormatListAssignDefaultDataFormat` on the live list(s),
  `AcxPinNotifyDataFormatChange` on the host pin
  (`KSEVENT_PINCAPS_FORMATCHANGE`), then the two values are written so the
  choice survives a restart. The handles this needs are kept under a
  passive wait lock from AcxDeviceAddCircuit to ReleaseHardware, and the
  IOCTL reports and stores the new default under it too, so two requests at
  once cannot leave the lists, the reported default and the stored values
  apart. Nothing of it can stop the device from starting (without the lock
  the IOCTL is refused and the stored value still applies).
* **Service.** For each crossc device a helper thread finds the device's
  render endpoint under
  `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render`
  (the registry, not the MMDevice API: the service is LocalSystem in
  session 0): the endpoint whose `{233164c8-1b2c-4c7d-bc68-b671687a2567},1`
  (the KS filter path) has the crossc interface's device instance and the
  reference string `headphones0`, with `DeviceState` active or unplugged.
  The instance id alone (`{b3f8fa53-0004-438e-9003-51a46e139bfc},2`) is not
  enough: the endpoint the inbox A2DP driver left on the same node has it
  too. It reads `PKEY_AudioEngine_DeviceFormat`
  (`{f19f064d-082c-4e27-bc73-6882a1bb8e4c},0`: `VT_BLOB` header, then the
  `WAVEFORMATEX(TENSIBLE)` from byte 8) and waits for
  `RegNotifyChangeKeyValue` on the Render key (no polling). A changed format
  wakes the session, which maps it to an offered format (same rate; 24-bit
  for 24 bits or more; a rate not offered is left alone) and sends
  `AUDIO_SET_DEFAULT_FORMAT` when it differs from the default the driver
  reports: once per change, not again after a failure until the choice or
  the driver changes, and compared afresh after every attach. One info line
  per change (`Windows device format for <id> is 96000 Hz/24: driver
  default set`). A driver older than protocol 4 is still used; when the
  choice is anything but 48 kHz/24 (what such a driver plays), the service
  says once per attach that it cannot follow it.
* **Streams already open.** A stream keeps its format; the new default is
  what the engine's next stream gets. If the engine keeps rendering the old
  format for 5 s after the default was set, the service warns that the
  change applies when playback restarts (pause and resume). Whether the
  engine reopens its stream by itself on the format-change event has not
  been seen on a device; restarting the device to force it is not done.

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
  | 48 kHz | HiFi (`0x04`) | HiFi, config `0x0C`, 229 kbps, 24-bit (16-bit input widened) |
  | 48 kHz | 48 kHz SSC only (`0x10`) | SSC 48 kHz, config `0x10` (+`0x08` if offered), 16-bit (24-bit input dithered down) |
  | any | 44.1 kHz SSC only (`0x20`) | resampled to 44.1 kHz, SSC config `0x20` (+`0x08`) |

  The `0x08` bit is sent only when the buds offer it (every model with a
  reported capability byte does). Plain SSC runs at 229 kbps. Endpoint rates
  other than 48 and 96 kHz follow the 48 kHz rows with resampling. The
  bitrates of the table are the highest the stream is sent at; see
  "Adaptive bitrate" below.

  No SSC endpoint at all -> the link flag stays off and the reason is logged.
  A format change that needs another mode reconfigures (CLOSE,
  SET_CONFIGURATION, OPEN).
* **Stream running:** START, then one RTP packet per SSC frame (864 samples;
  two frames per packet at the lower UHQ rates), paced by the PCM the
  endpoint produces, at the bitrate the link carries ("Adaptive bitrate").
  Idle for a few seconds after the stream pauses -> SUSPEND.
* **Volume:** see "Volume" below. While the buds apply the volume the PCM is
  streamed unscaled; while nobody does, the service applies the endpoint's
  level and mute itself.
* **The buds go away** (case, range, taken by the phone): channel closes or the
  sink sends CLOSE / ABORT -> `AUDIO_SET_LINK(disconnected)`, wait for the next
  connection. A sink SUSPEND is honoured and retried while audio is playing.
* **Settings** (`HKLM\SOFTWARE\crossc`): `UhqConfig` (default `0x0A`),
  `HifiBitrate` (229; 88, 96, 128, 192 or 229), `UhqBitrate` (584; 152, 291
  or 584), `AdaptiveBitrate` (1), `LogLevel`. A bitrate the buds are not
  known to decode (HiFi 256 or 328, UHQ 250, 308, 442 or 886) is lowered to
  the next one they are, with a warning. Log:
  `%ProgramData%\crossc\service.log`.
* **Status feed.** The driver's IOCTLs are for SYSTEM and Administrators, so
  a program of the logged-on user can show nothing it does not get from the
  service. The service therefore keeps one snapshot of its state (per
  device: phase and the reason for it, endpoint format, mode, stream rate
  and depth, bitrate with its ladder and rung, media MTU limit, counters,
  volume, driver version, last warning or error) and sends it on the named
  pipe `\\.\pipe\crossc-status` (`crates/crossc-status`; the tray app is its
  reader). The pipe is outbound only: the service never reads from a client
  and never impersonates one. Users may read the service's pipe and only
  SYSTEM and Administrators may add an instance to it. That protects the
  pipe while the service runs; while it does not, any program could create
  a pipe of that name, so a client believes only a pipe whose owner is
  SYSTEM or Administrators.

  The feed must never cost audio. A session thread only replaces its entry
  in a shared value (a short lock, no I/O, no client involved): at once when
  its phase, bitrate or stream configuration changes, otherwise at most
  every 250 ms while audio streams. The entry is built from what the session
  already holds in memory; no driver call is made for it. Sending is the
  work of the pipe's own thread, which paces what goes out (a change of
  substance within 100 ms, running numbers every 500 ms while streaming,
  every 2 s otherwise). A pipe that cannot be created is retried in the
  background and said once in the log; the service streams with or without
  it. Dropped frames are the exception in how they are kept: the driver's
  counter is never reset and also counts what is rendered while nothing
  reads it, so the session adds up only what that counter rose by while a
  stream was started. It reads the counter when the stream starts (the
  base), with each statistics interval, with every state the driver
  announces and when the stream stops; the counter itself raises no state
  event, so that one number can be up to a minute old.

## Adaptive bitrate

The SSC frame header carries the rate code, so the encoder may change the
bitrate on any frame; the AVDTP configuration, the encoder state and the RTP
sequence stay. Samsung phones do this from a link "busy level". The service
does it from what it can see of the link in user mode, with the driver that
is in the field (0.2.0): no IOCTL was added.

**Ladders** (`crates/crossc-core/src/abr.rs`). The top is the configured
rate (`HifiBitrate`, `UhqBitrate`, 229 kbps for plain SSC), lowered first if
a packet does not fit the media MTU. Below it:

| Mode | Rates, kbps | Packet bytes | Audio per packet |
|---|---|---|---|
| HiFi 48 kHz/24 | 229 / 192 / 128 / 88 | 588 / 496 / 336 / 236 | 18 ms |
| SSC 48 or 44.1 kHz/16 | 229 / 128 / 88 | 588 / 336 / 236 | 18 / 19.6 ms |
| UHQ 96 kHz/24 | 584 / 291 / 152 | 672 / 672 / 360 | 9 / 18 / 18 ms |

The two lower UHQ rates carry two frames per packet, as the phones do: half
as many packets, each as long as a HiFi one, and at 291 kbps exactly the
size of a 584 kbps packet. A rate change takes effect at a packet boundary,
so a packet never mixes rates. Only rates the buds are known to decode are
ever sent (up to 229 kbps at 44.1 / 48 kHz; 152, 291 and 584 kbps in UHQ);
the settings and the MTU fallback use the same list.

HiFi's top is 229 kbps by default: Buds4 Pro and Buds3 Pro were measured to
decode both 229 and 192 kbps in HiFi (config `0x0C`, 48 kHz), and 192 kbps,
the highest rate Samsung phones send, is the first rung below it.
`HifiBitrate = 192` gives the phones' ladder, 192 / 128 / 88.

**Signals.** What user mode can observe, and what it is worth:

| Signal | Where it comes from | Verdict |
|---|---|---|
| Time a media WRITE takes | The driver forwards each WRITE as one outbound `BRB_L2CA_ACL_TRANSFER` and completes the IOCTL in that BRB's completion routine (`ioctl_transfer` / `transfer_complete` in `drivers/crossc-bth/src/lib.rs`); nothing is queued in the driver. So a WRITE takes as long as bthport holds the transfer. | **The congestion signal.** Healthy: 100 to 250 us, far less than a packet's air time (several ms), so bthport completes the transfer when it has handed the data on, not when the buds acknowledged it. The controller has a few ACL buffers and returns them (HCI Number Of Completed Packets) as packets are delivered; with a link that delivers less than the stream offers the buffers stay full and the transfer has to wait. That bthport then holds the BRB, rather than queueing without bound or flushing, is the working hypothesis and the first thing the live test must show. If WRITE latency never moves, the controller never acts and the service sends one rate, like 0.2.0. |
| Time the session waited for a packet's audio | The time it spent blocked with nothing to send since the packet before. | **Tells a paced packet from a burst.** Next to nothing means the audio was there already: the session is behind and sends back to back, and the WRITE time then measures how fast the link takes data, not how busy it is (see the rules). |
| Lateness of the PCM | Each PCM chunk carries its frame position; against the clock that gives how far the sender is behind the endpoint (`abr::Lateness`, self-calibrating against clock drift, and started anew whenever the endpoint stops rendering). | **Secondary.** The session is one thread and WRITE is its only blocking call on the packet path, so a backlog is either blocked WRITEs (seen directly, sooner) or a sender that does not get to run, which a lower bitrate does not cure. A late packet therefore never steps down; it only postpones stepping up, and the worst lateness is in the statistics. |
| PCM discontinuity | The driver flags the chunk after it dropped audio a slow reader did not fetch (its ring holds 250 ms). | **Corroborating.** Also raised by a new stream or a format change. Next to WRITEs that blocked for 100 ms it means the link cost audio: one step down at once. Alone, or next to one short WRITE, it changes nothing. |
| WRITE error | The IOCTL fails: the service's 5 s timeout, or an error from bthport. | **Reliable but late, and only the timeout counts.** The link is rebuilt as before. After a WRITE that timed out the next link of the same mode within 60 s starts at the lowest rate. Any other error (not connected, aborted, a stale channel) is the link or the channel going away, as when the buds go into the case, and the next link starts at the top. |
| Delay reports | AVDTP DELAYREPORT from the buds (200 to 340 ms seen). | **Logged only.** Sent when the buds' buffer target changes, not per packet; whether it moves with link quality is unknown. |
| RSSI, link quality, flush occurred, the controller's buffer state | HCI. | **Not available**: the driver exposes none of it, and reading it needs a driver change. |

**Rules** (constants in `abr.rs`, each with its rationale):

* A WRITE is *busy* when it took at least a quarter of the audio time its
  packet carries, and at least 2 ms (HiFi: 4.5 ms; UHQ at 584: 2.25 ms).
* A busy WRITE is *slow*, evidence of a link that carries too little, unless
  a queue is being worked off. The measure "a quarter of the packet time"
  assumes a sender that offers one packet per packet time. A sender that is
  behind sends back to back, and a link with room to spare but not without
  limit then takes each packet as fast as it can, which is most of a packet
  time on anything below four times the stream. So: after two packets in a
  row that the session did not have to wait for (it is behind: after one
  stalled WRITE, at START with audio waiting in the driver's ring), and
  after every step down (packets of the old rate still fill the link's
  buffer), a WRITE is slow only if it took at least as long as its packet
  plays, which is a link slower than the stream. The usual measure is back
  after four packets that the session waited for, with no busy WRITE in
  between.
* *Down, fast.* The time slow WRITEs took goes into a bucket that leaks at
  a quarter of real time, so only a link that blocks the sender for more
  than a quarter of the time fills it. 100 ms in the bucket from at least 3
  slow packets: one rung down. With every WRITE as long as its packet that
  is about 140 ms after the first slow one, inside the 250 ms the driver
  keeps, so the step normally comes before audio is lost. One stalled
  WRITE, however long, is one slow packet and changes nothing (the bucket
  holds at most 200 ms and has forgotten it after 0.8 s), and neither does
  the catching up after it. After a change the bucket starts empty and
  collects nothing for 200 ms; so two rungs take at least 0.3 s. A second
  rung follows only if the link is slower than the lower rate too.
* *Dropped audio.* A discontinuity steps down at once if WRITEs (busy ones,
  slow or not) held the sender for 100 ms just before. One WRITE of 20 or
  30 ms that overflows a ring that was full already, as at START, does not.
* *Up, slow.* One rung up after 10 s of clean streamed audio. A busy WRITE
  takes 50 times its duration off the clean time: a link that holds the
  sender for 2 % of the time or more does not step up, while one stray
  WRITE of 5 ms costs a quarter of a second, the same WRITE that would
  never move a stream at the top. A late packet (40 ms behind the endpoint)
  or a step down starts the count again. A step up is a probe: if the rate
  leaves that rung downwards within 15 s, the hold for the rung doubles
  (20, 40, at most 80 s). Every rung has its own hold, so coming back to a
  rung that holds does not shorten the wait for the one above it that did
  not; and a rung's hold halves again for every 15 s the stream stays at
  that rung or above. A link that is bad more often than every 10 s
  therefore stays at the lower rate, and one that just cannot carry the
  next rung is tried ever more rarely: after 10, 20, 40 s and then once in
  80 s, at most 6 changes in any minute (tested on simulated links between
  the middle and the top rate, with link buffers of 2 to 16 kB).
* Lateness is counted against the endpoint's clock only while the endpoint
  renders: a pause too short for a SUSPEND (under 3 s) is the endpoint
  standing still, not the sender being behind.
* Hold and probe times count streamed audio, not wall time: a suspended
  stream says nothing about the link, and the rate survives SUSPEND / START.
  A new stream configuration (HiFi <-> UHQ) starts a new controller at the
  top; a format change within a mode (24 <-> 16 bit) keeps it.
* Never above the configured rate; `AdaptiveBitrate = 0` gives a one-rung
  ladder: one fixed rate, as 0.2.0 sent. (Which rate can differ from 0.2.0
  in three cases, adaptive or not: HiFi without a `HifiBitrate` setting is
  229 kbps where 0.2.0 sent 192; a configured rate the buds are not known
  to decode is lowered to the next one that is; and so is the fallback
  when a packet does not fit the media MTU, 584 -> 291 kbps where 0.2.0
  took 442.)

**Known limits of these rules.** WRITE latency that does not depend on the
rate (every WRITE taking a quarter of a packet time or more for a reason
other than the link's capacity) looks like a busy link from here: the
stream goes one rung down and stays there, although the lower rate buys
nothing. And between 7/8 of a packet time and a whole one per WRITE the
sender is taken to be catching up, so a link that busy is acted on only
when WRITEs reach the packet time. Whether either occurs only real buds
can show; the statistics (`slow_writes` with `write avg`) would show both.

On the packet path this is two clock readings that were already there and a
few integer operations per packet; the packet buffers are sized for the
largest rung up front, so a rate change allocates nothing and adds no
delay. Every change is logged (at most 8 lines a minute at info level, the
rest at debug) and the statistics line carries the current rate, the audio
sent at each rate, the number of changes, of busy WRITEs (`slow_writes`) and the worst
lateness. `crossc-cli play --adaptive` runs the same controller (one frame
per packet throughout, and the few packets its producer thread has encoded
ahead still go out at the old rate); `--bitrate` keeps the rate fixed.

**What only real buds can show:** whether WRITE blocks at all under
congestion (see the table); whether the buds take two frames per packet
from this stack and switch rates without a click; whether the thresholds
fit real latency tails; and whether reading and encoding a packet stays
well under an eighth of a packet time (1.1 ms in UHQ), which is what tells
a burst from paced packets. `docs/SETUP.md` section 15 is that test.

## Volume

One control: the Windows slider and the buds' own volume (gestures, the
phone-style volume of the buds) are the same level, as with Windows' inbox
A2DP driver. AVRCP (play / pause / next, absolute volume) stays with the
inbox stack (`BthAvctpSvc`, `BthAvrcp.dll`) on the buds' AVRCP nodes;
crossc does not open AVCTP (the buds refuse a second AVCTP channel anyway).
Instead the driver provides, on its own A2DP node, the private contract the
inbox AVRCP volume controller drives for `BthA2dp.sys`, recovered from the
binaries (`target/avrcp-research/PLAN.md`, `inbox-contract.md`):

```
Windows slider <-> audiosrv <-> ACX volume + mute elements (render circuit "Headphones0")
                                   |  Windows writes: level stored, KS event {401e566b}/1
                                   v
            BthAvrcp VolumeController: KS GET {401e566b}/1 -> SetAbsoluteVolume -> buds
buds gesture -> EVENT_VOLUME_CHANGED -> BthAvrcp: KS SET {401e566b}/1 -> slider moves
```

| Piece | Value | Evidence |
|---|---|---|
| Interface class | `{e630310c-1733-42bb-9847-7e45d1a27490}` on the circuit, next to ACX's own categories | BthA2dp's controller filter categories; BthAvrcp / BthAvctpSvc watch instance id + this class + enabled |
| Property | set `{401e566b-b47e-4e34-b0fc-94826e9e2a1a}` id 1, GET / SET, one LONG, 1/65536 dB, -63.5..0 dB | BthA2dp `GetVolumeLevelCache` / `SetVolumeLevelCache`; BthAvrcp sends 0x2F0003 with flags 1 / 2 |
| Event | same set, id 1, `KSEVENTF_EVENT_HANDLE` | BthAvrcp sends 0x2F0007 with a 0x20-byte KSEVENTDATA, type 1 |
| Method | set `{0e62540f-bac4-4b85-b344-0d82ed5008ac}` id 6, answered with success at once | BthAvctpSvc sends it on interface arrival and only traces the result |
| Volume element | 2 channels, -63.5..0 dB, step 0.5 dB | BthA2dp's volume node range |
| Mute element | 2 channels; muted = AVRCP 0 to the buds | BthA2dp reports the minimum to AVRCP while muted |

Rules (driver, `drivers/crossc-bth/src/avrcp_vol.rs`, host-tested): the
level for AVRCP is the louder channel; the taper between dB and AVRCP 0..127
is BthAvrcp's (`35 * log10(lo + v/127 (1 - lo))`), so slider position p,
written exactly, reaches the buds as `round(127 p)` (one or two AVRCP steps
off for about half the positions if audiosrv writes on the 0.5 dB grid).
The buds' level wins when AVRCP connects. BthAvrcp's first SET is its
cached byte, which is its built-in default (AVRCP 64) when it enabled
response processing before the buds' INTERIM; so after a first SET of
exactly 64 the next SET within 3 s is taken as the connect level too, and a
Windows mute survives either order. An echo rule keeps audiosrv's 0.5 dB
write-back of a buds level from being sent back as a different AVRCP value,
and a buds confirmation of a Windows level does not move the slider.

If anything of the feature fails while the circuit is built or added
(including `AcxDeviceAddCircuit`, where ACX checks the category list and
the elements), the half-built circuit is deleted and the 0.1.1 circuit is
built instead; the feature then stays off for that device until it is added
again. Only a missing mute element is tolerated (Windows mutes in software).

The service (`crates/crossc-core/src/volume.rs`) decides per state:

| Driver reports | PCM |
|---|---|
| no hardware volume (v2 driver, kill switch) | as rendered: Windows has scaled it |
| hardware volume, AVRCP active (event enabled and answered) | unscaled (only balance and mute applied) |
| hardware volume, AVRCP not active | level and mute applied in software, 20 ms ramps |

So the slider keeps working when AVRCP is not connected, absolute volume is
disabled, or a Windows update changes the private contract. Kill switch:
`DisableAvrcpVolume` (REG_DWORD, service `Parameters` key or the device key,
like `DisableAcx`) registers none of the above; the endpoint is then the
0.1.1 one.

## Out of scope for 0.1

SBC/AAC fallback through this driver, capture / hands-free (stays on the
inbox driver), LE Audio, multiple paired buds at once, production code
signing.
