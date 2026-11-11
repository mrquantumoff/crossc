# Wiring crossc-status into crossc-service

`crossc-status` and `crossc-tray` are complete and tested on their own. What is missing is the service's side:
nothing creates `\\.\pipe\crossc-status` yet, so the tray shows "Service not running". This file says exactly
what to add. Names are those of `crates/crossc-core/src/session.rs`, `abr.rs`, `plan.rs`, `volume.rs` and
`crates/crossc-service/src/{main,scm,host,supervisor}.rs` as they stood when this was written (adaptive bitrate
already wired into the session). Check them again before starting; where a name has moved, the meaning given
here is what counts.

The work is three pieces, in this order:

1. **crossc-core**: the session keeps a few numbers it only logs today, and gets one accessor, `status()`.
   No new dependency in crossc-core (no serde there).
2. **crossc-service**: one `Hub`, one `Server`, a conversion function, and a publish call in the session
   thread's loop.
3. **Packaging** (see `target/tray-followup.md`): ship `crossc-tray.exe`.

Nothing in the audio path waits for a client: the session thread calls `Hub::set_device_keeping_event`, which
takes an uncontended mutex, replaces one value and sets an event. The pipe is served by the `status-pipe`
thread of `crossc_status::pipe::Server`.

## 1. crossc-core: what the session has to expose

### 1a. What exists today and can be read as it is

| Snapshot field | Source today |
|---|---|
| `DeviceStatus::id` | the `name` passed to `Session::new` (`supervisor::short_name(path)`, the 12 hex digits of the address) |
| `phase` | `Session::phase()` (`Phase::{WaitDriver, WaitLink, Ready, Streaming, Stopped}`), plus `was_connected` and `retry_at` for the waiting / reconnecting split (1c) |
| `endpoint` | `self.audio.format` if `is_valid()`, else `self.last_format`; `PcmFormat { sample_rate, channels, valid_bits }` of `crossc_ioctl::audio` |
| `link.mode` | `link.plan.mode` (`SscMode::Uhq` -> `CodecMode::Uhq`, `Hifi` -> `Hifi`, `Ssc48` and `Ssc44` -> `Ssc`) |
| `link.config` | `link.plan.config` |
| `link.caps` | `link.sep.flags.0` |
| `link.stream_rate` | `link.plan.mode.sample_rate()` |
| `link.stream_bits` | 24 if `link.plan.mode.hires()`, else 16 |
| `link.bitrate_kbps` | `link.abr.rate().kbps()` (public: `Session::rate()`) |
| `link.top_kbps` | `link.plan.bitrate.bps() / 1000` (same as `link.abr.ladder().top().kbps()`) |
| `link.adaptive` | `link.abr.ladder().is_adaptive()` (false with `AdaptiveBitrate=0` and when the top is the only rung) |
| `link.ladder_kbps` | `link.abr.ladder().rungs().iter().map(Rate::kbps)` |
| `link.rung` | `link.abr.rung()` |
| `link.frames_per_packet` | `link.abr.rate().frames_per_packet` (the UHQ rungs below the top carry two) |
| `link.packet_bytes` | `link.abr.rate().packet_bytes(link.plan.encoder)` |
| `link.mtu_limit` | `link.mtu_limit` |
| `link.sink_delay_tenth_ms` | `self.last_delay` (`Session::sink_delay()`, already 1/10 ms) |
| `volume.mode` | `self.volume.mode` (`volume::VolumeMode::{Buds, Software, Windows}`, the same three as `crossc_status::VolumeMode`) |
| `volume.muted` | `self.audio.muted()` |
| `volume.level_percent` | from `self.audio.volume`: with hardware volume (`self.audio.hw_volume()`) and not muted, `avrcp * 100 / 127` rounded (`VolumeState::avrcp` is the 0..=127 AVRCP value the driver derives from the slider; it is 0 while muted, so send `None` then: the tray shows "muted" without a number); without hardware volume `None` |
| `counters.dropped_frames` | `self.totals.dropped_frames`: what the driver's counter rose by while a stream was started (between a START and the end of that stream), added up when a state arrives, with every statistics line and when the stream stops. Not `self.audio.frames_dropped`: the driver never resets that counter and also counts what it renders while nothing reads it (no session attached, a stream that is not started), so it shows large numbers that are no loss |

### 1b. What must be added to `Session` (all small, all in session.rs)

`Stats` is per statistics interval and private, so nothing cumulative exists. Add one struct next to it and one
field `totals: Totals` in `Session` (not in `Link`: the counters outlive a link).

```rust
/// Counters since the session started (the statistics interval's are in `Stats`).
#[derive(Clone, Copy, Debug, Default)]
pub struct Totals {
    pub packets: u64,          // += 1 where `link.stats.packets += 1` is (on_pcm, after a successful send)
    pub frames_in: u64,        // += where `link.stats.frames_in +=` is
    pub discontinuities: u64,  // += 1 where `link.stats.discontinuities += 1` is
    pub write_errors: u64,     // += 1 in on_pcm where `link.media.send(pkt)` returns Err
    pub late_packets: u64,     // += 1 in on_pcm when `late >= crate::abr::LATE` (the `late` handed to `Obs`)
    pub reconnects: u32,       // += 1 in `teardown` (every link that went down and is set up again)
    /// The current stream's WRITE latency (reset in `begin_streaming`).
    pub write_total: Duration, // += `write` next to `link.stats.write_total`
    pub write_max: Duration,   // max next to `link.stats.write_max`
    pub write_count: u64,      // packets of the current stream, for the average
}
```

Further fields in `Session`:

* `driver: Option<(u32, u32)>`: `(info.driver_version, info.protocol_version)`, set in `try_attach` where the
  "attached: driver ..." line is logged (today both are locals there). Cleared in `on_detached`.
* `mtu_why: String` in `Link`: the second element of what `media_mtu_limit` returns in `open_stream` (today only
  logged in "media channel open: ..."). Return it from `open_stream` next to the limit.
* `streaming_since: Option<Duration>` in `Link`: `Some(now)` in `begin_streaming`, `None` in `stream_stopped`.
* `reason: String` in `Session`: why the session is where it is. Set it in the four places that log it today:
  the `Err(msg)` arm of `step_wait_driver` ("driver not ready: ..."), `on_detached(why)`, the `Err(f)` arm of
  `step_wait_link` ("cannot set up the stream: ...") and `teardown(reason)` ("link down: ..."). Clear it when the
  link comes up (`Ok(link)` arm of `step_wait_link`) and in `try_attach`'s success path.

Then one public accessor, plain data, no I/O (it must be cheap: it is called from the audio thread):

```rust
pub struct SessionStatus {
    pub phase: Phase,
    pub connected: Option<bool>,          // self.was_connected
    pub retrying: bool,                   // phase == WaitLink && self.retry_at > self.clock.now()
    pub reason: String,
    pub endpoint: Option<PcmFormat>,
    pub link: Option<LinkView>,           // plan, rate, rung, ladder (kbps), mtu_limit, mtu_why, caps,
                                          // streaming_for: Option<Duration>
    pub sink_delay: Option<u16>,
    pub volume: Decision,                 // self.volume
    pub audio: AudioState,                // self.audio (flags, volume, frames_dropped)
    pub driver: Option<(u32, u32)>,
    pub totals: Totals,
}
impl<D: Driver, P: LinkProbe, K: Clock> Session<D, P, K> {
    pub fn status(&self) -> SessionStatus { ... }
}
```

### 1c. Phase mapping

| `crossc_status::LinkPhase` | Session |
|---|---|
| `WaitingDriver` | `Phase::WaitDriver` |
| `WaitingBuds` | `Phase::WaitLink` and `was_connected != Some(true)` |
| `Reconnecting` | `Phase::WaitLink` and `was_connected == Some(true)` (the buds are there, the stream is not: a setup that failed or a link that went down; `reason` says which, `retry_at` when the next attempt is) |
| `Ready` | `Phase::Ready` |
| `Streaming` | `Phase::Streaming` |
| `Stopped` | `Phase::Stopped` |

`phase_reason` is `reason` (empty for Ready / Streaming).

### 1d. The Bluetooth name

Not known anywhere in the service today. `crossc_core::real::device_info` already calls
`BluetoothGetDeviceInfo` for the connection probe and throws the rest away: `BLUETOOTH_DEVICE_INFO::szName`
(UTF-16, NUL-terminated) is the name. Add `pub fn device_name(addr: u64) -> Option<String>` next to
`BluetoothLink` in real.rs (same radio fallback as `connected`), and call it in the service (not in the
session) once per session thread start and again whenever the phase leaves `WaitingBuds`. An empty name is fine:
the tray falls back to the id.

## 2. crossc-service

### 2a. Cargo.toml

```toml
crossc-status = { path = "../crossc-status" }
```

This brings serde, serde_derive (build time) and serde_json into the service binary: MIT OR Apache-2.0, already
accepted in `.github/release/about.toml` and already in `Cargo.lock`.

### 2b. Create the hub and the server (both entry points)

In `scm.rs::run_service`, right after the settings are read and before `Supervisor::new(...)`, and the same in
`main.rs::run_foreground` (there with `Security::CurrentUser`, so a console run does not need to be SYSTEM, and
`log_dir: String::new()`):

```rust
use crossc_status::{Hub, ServiceInfo};
use crossc_status::pipe::{Security, Server, ServerOptions};

let hub = Hub::new(ServiceInfo {
    version: env!("CARGO_PKG_VERSION").to_string(),
    started_unix: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()),
    log_dir: default_log_dir().display().to_string(),
    last_event: None,
});
let report_log = log.clone();
let _status = Server::start(hub.clone(), ServerOptions {
    security: Security::Service,
    report: Some(Arc::new(move |line: &str| report_log.log(Level::Warn, line))),
    ..ServerOptions::default()
});   // Err only if a thread or an event cannot be created: log it and go on without status
```

Keep `_status` alive until after `supervisor.stop_all()`; dropping it disconnects the clients and joins the
thread. Call `hub.clear_devices()` after `stop_all()` (and in the supervisor's `Command::Suspend` arm after its
`stop_all()`), so a tray that is still connected shows no stale device.

`Server::start` never fails because of the pipe: if the name cannot be created (another process holds it) it
reports once through `report` and retries every 5 s. Once it has the name it keeps it until it is stopped: it
always holds at least one instance of the pipe, so no other process can create the pipe while the service runs.

`Server::pipe_held()` says which of these it is (`Held::NotYet`, `Ours`, `Taken`, `Failed`). Ask it before
judging the pipe by opening it: while another process holds the name, what a client opens is that process's
pipe, and its owner says nothing about the service's descriptor (`Taken` is the log line "a pipe named ...
exists that is not this service's", not "not owned by SYSTEM or Administrators").

**`report` must write to the plain log, not to the wrapper of 2e.** The wrapper turns warnings into the state
the server sends; a report about sending must not become a change of that state. `report_log` above is the log
as it was before it was wrapped.

**The pipe's owner decides whether the tray believes it.** `crossc-tray` (and any client with the default
`Trust::Service`) reads the pipe only if its owner is SYSTEM or Administrators, because any program can create a
pipe of that name while the service is not running. A pipe created by a LocalSystem service has such an owner
by default (the owner of what LocalSystem creates is Administrators), and so has one created from an elevated
console. This could not be tested without elevation: it is the first thing to check after wiring (section 4).
If the tray says "Status not trusted" against the installed service, the owner is something else: then name it
in the descriptor, `pub const SERVICE_SDDL: &str = "O:SYD:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GR;;;IU)(A;;GR;;;AU)";`
in `crossc-status/src/pipe.rs` (LocalSystem may set itself as owner; a plain user may not, so the crate's test
`the_service_descriptor_lets_users_read_and_nothing_else`, which creates that pipe as a plain user, then needs
its creation to expect ERROR_INVALID_OWNER instead).

### 2c. Hand the hub to the session threads

`RealHost::new(settings, log)` gets a third argument `hub: Hub` (it is `Clone`); `Host::spawn` passes a clone to
`session_thread`. The fake host of the supervisor tests does not change.

### 2d. Publish from the session thread (`host.rs::session_thread`)

Replace `session.run()` by the loop it stands for, with a publish step:

```rust
// Once before the first step: the device must be in the hub before the first warning is logged
// ("driver not ready: ..." comes out of the very first step), or `set_device_event` has no device to
// attach it to and drops it.
hub.set_device_keeping_event(to_device_status(&name, &bt_name, &session.status()));
let mut last = Instant::now();
let mut last_phase = Some(session.phase());
while session.phase() != Phase::Stopped {
    session.step();
    // At once when the phase changed, else at most every 250 ms: `step` returns for every PCM chunk.
    if last_phase != Some(session.phase()) || last.elapsed() >= PUBLISH_EVERY {
        last_phase = Some(session.phase());
        last = Instant::now();
        hub.set_device_keeping_event(to_device_status(&name, &bt_name, &session.status()));
    }
}
hub.remove_device(&name);
```

`PUBLISH_EVERY = 250 ms`. `step` waits at most `MAX_WAIT` (5 s) when nothing happens, so an idle session
publishes at least that often; the hub ignores a status equal to the last one, and the server sends its own
keepalive every 2 s. Also call `hub.remove_device(&name)` on the panic path (after `catch_unwind`).

`to_device_status` is the conversion of section 1 (new function in host.rs, pure, unit-testable with a
hand-made `SessionStatus`). The remaining fields:

| Field | Value |
|---|---|
| `name` | the Bluetooth name of 1d |
| `link.mtu_reason` | `LinkView::mtu_why` |
| `link.streaming_ms` | `streaming_for.map(as_millis)` |
| `counters.packets / pcm_frames / discontinuities / write_errors / late_packets / reconnects` | `Totals` |
| `counters.write_avg_us` | `write_total.as_micros() / write_count.max(1)` |
| `counters.write_max_us` | `write_max.as_micros()` |
| `driver` | `Some(DriverInfo { version: format!("{}.{}.{}", v >> 24, (v >> 16) & 0xFF, v & 0xFFFF), protocol })` |
| `last_event` | leave `None`: see 2e |

### 2e. Last warning / error

The session logs through `Arc<dyn Log>` with the prefix `[<short name>] `. Wrap the service's log once, where it
is created (scm.rs `service_main`, main.rs `run_foreground`), in a `Log` that forwards every line and, for
`Level::Warn` and `Level::Error`, also does:

```rust
let event = Event { level, time_unix: now, text };
match line.strip_prefix('[').and_then(|l| l.split_once("] ")) {
    Some((id, text)) if hub.set_device_event(id, event_with(text)) => {}
    // No prefix, or a device the hub does not have (`false`): the service's, with the whole line.
    _ => hub.set_service_event(Some(event_with(line))),
}
```

A line about a device that is not in the hub is not dropped: "the session thread panicked" is logged after the
thread's device was taken out, and "cannot start a session thread" before there is one. Those are the lines a
user most needs to see, so they become the service's last event, with the `[<id>] ` prefix kept.

The text may be any length and contain anything: the hub cuts every string to 300 characters and takes out
control and invisible formatting characters, exactly as a reader does, so no log line can make the state too
large to send.

The wrapper must forward `enabled()` to the wrapped log too (the session skips building debug lines when it
says no). `set_device_keeping_event` (2d) keeps that event across the statuses the session thread sets. The hub needs to
exist before the wrapper, so create the hub first, then the log wrapper, then everything else.

## 3. What the protocol guarantees (so the service needs no care)

* One snapshot per message, at most 64 KiB, at most 8 devices (`Hub::set_device*` refuses a ninth). Whatever
  is put into the hub is cut to the format's caps on the way in (strings: 300 characters and 600 bytes), and
  eight devices with every field at its largest still fit a message (tested).
* The server holds the pipe's name from its first instance until it stops (an instance whose client left is
  used again, never closed while it is the only one). Clients that connect and leave at once, however fast,
  cost it nothing but the time to notice.
* At most 4 of the 8 clients come from one Windows session (`ServerOptions::max_per_session`), so one logged-on
  user's programs cannot take every place. Within a session the places are first come, first served.
* `Hub::on_change` returns an id and `Hub::remove_waker` takes the waker out; `Server` does that when it stops,
  so a server may be stopped and started again on the same hub (around suspend and resume, say) without
  leaving anything behind.
* A change of substance goes out within 100 ms, running numbers every 500 ms while a device streams, every 2 s
  otherwise; with no change a snapshot still goes out every 2 s (clients treat 6 s of silence as "gone").
* At most 8 clients (`ServerOptions::max_clients`). A tray with its flyout open is two.
* The pipe is outbound only; the DACL is `D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GR;;;IU)(A;;GR;;;AU)`; remote clients
  are rejected; the server never reads and never impersonates.
* Adding a field to a struct of `model.rs` (with its `Default`) is compatible in both directions and needs no
  version change. `PROTOCOL_VERSION` changes only when an old reader would misread a new snapshot.

## 4. Checking it

```powershell
cargo test -p crossc-status -p crossc-service
# service running (as a service, or `crossc-service run` in an elevated console with Security::CurrentUser):
.\target\release\crossc-tray.exe --print-once      # prints the snapshot as text; exit code 0
.\target\release\crossc-tray.exe                   # icon; left click for the flyout
```

The first check, from a normal (not elevated) PowerShell with the installed service running:
`crossc-tray.exe --print-once` must print the devices and exit 0. "Status not trusted" means the pipe's owner
is not SYSTEM or Administrators (see 2b for the fix); "Access denied" means the DACL did not let the user in;
"Service not running" means the pipe does not exist (look for a "status pipe ... cannot be created" line in
`service.log`).

A service run from an elevated console creates the pipe with that elevated token's default DACL
(`Security::CurrentUser`), which may not let an unelevated tray in ("Access denied" in the flyout): run
`crossc-tray --print-once` from the same elevated console for that check. The installed service
(`Security::Service`) has no such limit.
