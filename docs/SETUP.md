# crossc spike: setup, first live test and rollback

This walks you through milestone 1: put `crossc_bth.sys` in place of the inbox
`BthA2dp.sys` on **the Galaxy Buds' A2DP node only**, open the AVDTP signaling
channel (L2CAP PSM 0x19) as initiator, and run DISCOVER / GET_ALL_CAPABILITIES
with `crossc-cli probe`.

Shell legend:

* **(user)**: a normal, non-elevated PowerShell is enough.
* **(admin)**: an elevated PowerShell ("Run as administrator").

Every command block is meant to be pasted as-is. Paths assume the repo is at
`C:\src\crossc`; change `$repo` if yours is somewhere else.

> **Risk.** This loads a kernel driver that has never run before. A bug in it
> means a bugcheck (BSOD). Before you start:
> * Have your BitLocker recovery key to hand (https://account.microsoft.com/devices/recoverykey).
> * Create a restore point.
> * Close anything you can't afford to lose.

> **Quick path.** Once test signing is on (section 1) and the buds are paired
> (section 3), `tools\install-driver.ps1` does sections 0, 2, 4, 5 and the 5c
> check in one go: build, trust the cert, replace any older staged package,
> install on the buds' node only, and verify. It asks for elevation once.
>
> ```powershell
> .\tools\install-driver.ps1              # debug build + install
> .\tools\install-driver.ps1 -Release     # release build
> .\tools\install-driver.ps1 -NoBuild     # reinstall the last build
> .\tools\install-driver.ps1 -Uninstall [-RemoveCert]   # back to BthA2dp
> ```
>
> Prefer a window? `crossc-installer` does the same steps with a live status
> checklist (admin, test signing, Secure Boot, Memory integrity, package,
> certificate, buds node, staged packages), a streaming log and a "next step"
> hint. Build works unelevated; for Install / Uninstall / Remove certificate
> click "Restart as administrator" (one UAC prompt). See
> `crates/crossc-installer/README.md`.
>
> ```powershell
> cargo run -p crossc-installer --release
> ```

---

## 0. Build (user)

```powershell
$repo = "C:\src\crossc"
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"

# Driver package: .sys + .inf + .cat, test-signed, plus WDRLocalTestCert.cer
cd "$repo\drivers\crossc-bth"
cargo make
# -> $repo\drivers\crossc-bth\target\debug\crossc_bth_package\
#    crossc_bth.sys  crossc_bth.inf  crossc_bth.cat  crossc_bth.pdb  WDRLocalTestCert.cer

# CLI + libraries
cd $repo
cargo build --release -p crossc-cli
# -> $repo\target\release\crossc-cli.exe
```

`cargo make` creates a self-signed code-signing cert `CN=WDRLocalTestCert` in
your *user* store `WDRTestCertStore` the first time, **including its private
key**. It then reuses it for every build. If you delete it (step 8.3), the next
build creates a new one, which must be trusted again (step 2).

The build fails on purpose if `crossc_bth.inx` ever gets a model line with the
generic A2DP ID `BTHENUM\{0000110b-...}` (see step 5 for why).

---

## 1. Allow test-signed drivers (admin, one-time, reboot)

1. **Secure Boot must be OFF.** With Secure Boot on, `testsigning` is refused
   ("The value is protected by Secure Boot policy"). Check it:

   ```powershell
   Confirm-SecureBootUEFI   # False = off (good). True = disable it in UEFI setup first.
   ```

2. **BitLocker.** Changing boot configuration or Secure Boot triggers
   BitLocker recovery unless you suspend protection first:

   ```powershell
   manage-bde -status C:
   Suspend-BitLocker -MountPoint "C:" -RebootCount 2
   ```

   Then turn off Secure Boot in UEFI setup if needed.

3. **Turn on test signing:**

   ```powershell
   bcdedit /set testsigning on
   bcdedit /enum "{current}" | Select-String testsigning   # testsigning  Yes
   Restart-Computer
   ```

   After the reboot the desktop shows a "Test Mode" watermark.

4. **Memory integrity (HVCI).** HVCI should accept test-signed drivers in test
   mode. If the driver later fails to load with Code 52 or
   `0xC0000428`/`STATUS_INVALID_IMAGE_HASH`, turn off Windows Security ->
   Device security -> Core isolation -> Memory integrity, and reboot.

---

## 2. Trust the test certificate (admin)

```powershell
$pkg = "C:\src\crossc\drivers\crossc-bth\target\debug\crossc_bth_package"
certutil -addstore -f Root             "$pkg\WDRLocalTestCert.cer"
certutil -addstore -f TrustedPublisher "$pkg\WDRLocalTestCert.cer"
```

---

## 3. Pair the buds, LE Audio off (user)

1. Settings -> Bluetooth & devices -> **Use LE Audio when available: Off**
   (on 24H2+ it is under *Devices* / *Bluetooth devices*). If the buds were
   paired with LE Audio on, remove them first.
2. Put the buds in pairing mode (both in the case, lid open, hold the touchpads
   until they chime, or use Galaxy Wearable). Pair them from
   Settings -> Add device -> Bluetooth.
3. Play some audio once with the inbox driver, so you know the baseline works.
4. **(admin)** Collect the diagnostics (the hardware IDs come from here):

   ```powershell
   & "C:\src\crossc\tools\diag\collect.ps1"
   ```

---

## 4. Find the buds' A2DP node (admin)

The node you want is the BTHENUM child for the buds' **A2DP Sink** SDP
record. Its UUID is 0x110B, its class is MEDIA, and its service is `BthA2dp`.

```powershell
Get-PnpDevice -PresentOnly | Where-Object InstanceId -like 'BTHENUM\{0000110B-0000-1000-8000-00805F9B34FB}*' |
  ForEach-Object {
    $p = Get-PnpDeviceProperty -InstanceId $_.InstanceId -KeyName DEVPKEY_Device_HardwareIds, DEVPKEY_Device_CompatibleIds, DEVPKEY_Device_Service, DEVPKEY_Device_DriverRank
    [pscustomobject]@{
      Name         = $_.FriendlyName
      Status       = $_.Status
      Service      = ($p | Where-Object KeyName -eq DEVPKEY_Device_Service).Data
      DriverRank   = '{0:X8}' -f ($p | Where-Object KeyName -eq DEVPKEY_Device_DriverRank).Data
      HardwareIds  = ($p | Where-Object KeyName -eq DEVPKEY_Device_HardwareIds).Data -join ' | '
      CompatibleIds = ($p | Where-Object KeyName -eq DEVPKEY_Device_CompatibleIds).Data -join ' | '
      InstanceId   = $_.InstanceId
    }
  } | Format-List
```

On this machine (measured) the buds' node is:

* InstanceId `BTHENUM\{0000110B-0000-1000-8000-00805F9B34FB}_VID&00010075_PID&A013\B&1A2B3C4D&0&AABBCCDDEEFF_C00000000`
* HardwareIds `..._VID&00010075_PID&a013 | ..._LOCALMFG&0046`
  (VID 0x0075 = Samsung, PID 0xA013 = Buds4 Pro)
* CompatibleIds `BTHENUM\{0000110b-...} | BTHENUM\{0000110b-...}_GENERIC`
* Service `BthA2dp`, DriverRank `00FF2000` (inbox INF matched the generic
  *compatible* ID)

The INF's model line is exactly that `_VID&00010075_PID&a013` hardware ID. If
yours differs (other buds, other firmware), put your node's
`..._VID&xxxxxxxx_PID&xxxx` hardware ID in `crossc_bth.inx` and rebuild.

```powershell
$node = 'BTHENUM\{0000110B-0000-1000-8000-00805F9B34FB}_VID&00010075_PID&A013\B&1A2B3C4D&0&AABBCCDDEEFF_C00000000'
```

---

## 5. Install crossc_bth on THAT node only (admin)

**How the ranking works, and why the INF has no generic line.** PnP picks the
lowest DriverRank, then the newest DriverVer. Once step 2 trusts the test
cert, its signature scores the same as Microsoft's (AllSigningEqual, default
since Windows 7, not overridden on this machine). A generic
`BTHENUM\{0000110b-...}` line would therefore tie with the inbox INF
(`00FF2000`) and win on date, taking over **every** A2DP headset on its next
install. The buds-specific hardware ID ranks `00FF0000` instead: it beats the
inbox driver **for the buds only**, and no other device matches it.

The flip side: **once the package is staged (and the cert trusted), the buds'
node gets crossc_bth automatically** whenever it is (re)installed: re-pairing,
"Update driver -> Search automatically", a reinstall, or step 5b below. Staging
is not a no-op for the buds. Undo with step 8.

### 5a. Stage the package and check the ranking

```powershell
pnputil /add-driver "C:\src\crossc\drivers\crossc-bth\target\debug\crossc_bth_package\crossc_bth.inf"
pnputil /enum-devices /instanceid "$node" /drivers
```

Expected in the `/drivers` list: **crossc_bth.inf** (oemNN.inf, provider
*crossc*) with `Matching Device ID: ..._VID&00010075_PID&a013`, `Driver Rank:
00FF0000`, status *Best Ranked*; `microsoft_bluetooth_a2dp_src.inf` at
`00FF2000` *Outranked* (still *Installed* until 5b). If crossc shows any rank other
than `00FF0000` (a non-zero high byte means a worse signature score), the
signature is probably not trusted: redo steps 1-2 and check again.

Check that nothing else can pick it: run the same command for every other
`BTHENUM\{0000110B-...}` node from step 4 (other headsets). crossc_bth.inf must
**not** appear in their *Matching Drivers*:

```powershell
Get-PnpDevice -PresentOnly | Where-Object { $_.InstanceId -like 'BTHENUM\{0000110B-*' -and $_.InstanceId -ne $node } |
  ForEach-Object { $_.InstanceId; pnputil /enum-devices /instanceid $_.InstanceId /drivers | Select-String 'crossc' }
# no "crossc" lines expected
```

### 5b. Install it on the buds' node

Either (simplest; with the specific ID it only touches matching devices):

```powershell
pnputil /add-driver "C:\src\crossc\drivers\crossc-bth\target\debug\crossc_bth_package\crossc_bth.inf" /install
```

or Device Manager: `devmgmt.msc` -> the node from step 4 (Properties ->
Details -> "Device instance path" must equal `$node`) -> Driver -> **Update
driver** -> **Browse my computer for drivers** -> **Let me pick from a list of
available drivers on my computer** -> **crossc A2DP sink (spike)** (provider
*crossc*) -> Next, accept any warning.

The buds' "Stereo"/A2DP audio endpoint disappears; that is expected. Hands-free
(HFP, UUID 0x111E) and AVRCP are separate nodes and stay on their inbox
drivers.

### 5c. Check it bound

```powershell
Get-PnpDeviceProperty -InstanceId $node -KeyName DEVPKEY_Device_Service, DEVPKEY_Device_DriverInfPath, DEVPKEY_Device_ProblemCode, DEVPKEY_Device_Stack |
  Format-Table KeyName, Data
# Service = crossc_bth, DriverInfPath = oemNN.inf, ProblemCode = 0
# Stack = \Driver\ksthunk, \Driver\crossc_bth, \Driver\BthEnum
sc.exe query crossc_bth
```

`ksthunk` is the MEDIA class upper filter; it sits above BthA2dp today and
will sit above crossc_bth too. Our IOCTLs are not kernel-streaming IOCTLs, so
it should pass them through. If `crossc-cli list/info` fails oddly (create
refused, IOCTLs answered with errors the driver trace never shows), suspect
ksthunk first.

Problem codes: **52** = signature not trusted (redo steps 1-2). **10** =
start failed (DeviceAdd failed, e.g. the profile interface query; see
DebugView). **31/39** = driver did not load (see DebugView or `C:\Windows\INF\setupapi.dev.log`).

---

## 6. Watch the driver's trace (admin)

The driver prints with `DbgPrintEx(DPFLTR_IHVDRIVER_ID, DPFLTR_ERROR_LEVEL)`,
which the default filter lets through, so no registry change is needed.

1. Get DebugView from Sysinternals: `winget install Microsoft.Sysinternals.DebugView`
   (or download it from https://learn.microsoft.com/sysinternals/downloads/debugview).
2. Run `Dbgview.exe` **as administrator** -> Capture -> tick **Capture Kernel**.
   Set Edit -> Filter/Highlight -> Include `crossc:` to hide other output.
3. Restart the node to see the start sequence:

   ```powershell
   pnputil /restart-device "$node"
   ```

   Expected lines:

   ```
   crossc: DriverEntry v0.1.0
   crossc: EvtDriverDeviceAdd
   crossc: device added
   crossc: D0Entry
   crossc: remote A0:B1:C2:D3:E4:F5 (flags 0x6) ...
   ```

   `flags 0x6` = address valid + came from `IOCTL_INTERNAL_BTHENUM_GET_DEVINFO`.
   `0x4` means that IOCTL failed and the address was parsed from the instance id.

---

## 7. Probe the buds (admin)

Take the buds out of the case (powered and in range). Opening PSM 0x19 pages
them if no ACL link is up.

```powershell
$cli = "C:\src\crossc\target\release\crossc-cli.exe"
& $cli list
& $cli info
& $cli probe -v
```

`probe` opens the signaling channel and sends DISCOVER, then
GET_ALL_CAPABILITIES for each SEP (it falls back to GET_CAPABILITIES if that
is rejected). It prints every SEP and capability, including the decoded
Samsung SSC flags byte:

```
SEP 0x03: Audio Sink  [GET_ALL_CAPABILITIES]
  Media Transport
  Content Protection: SCMS-T (0x0002)
  Media Codec (media type 0): vendor 0x00000075 codec 0x0103 value [16]
    Samsung SSC flags = 0x16 (00010110)
      [ ] 0x80 96 kHz (legacy)
      ...
    -> 48k/24 HiFi: true   96k/24 UHQv2 usable: true
  Delay Reporting
```

If the buds refuse without security, try `& $cli probe -v --secure`. That
requests an authenticated and encrypted link.

### 7a. Which reported MTU is which (unverified driver mapping)

The driver reports `out_mtu = OutResults.Params.Mtu` and `in_mtu =
InResults.Params.Mtu`, and puts our requested `in_mtu` into `ConfigOut`. That
mapping is a reading of `bthddi.h`, not verified. `probe` and `media` request
`in_mtu=1021` by default and print a `MTU check:` line. Make it conclusive by
opening twice with two different unusual values:

```powershell
& $cli raw "open 0x19 1013" "close 1"
& $cli raw "open 0x19 1021" "close 1"
```

Whichever reported field follows the request (1013, then 1021) is **our
receive MTU**. If it is `in_mtu`, the mapping is right. If `out_mtu` follows
it, the mapping is swapped: the driver's WRITE length check and the CLI's
fragmentation use the wrong number. Record both lines and the DebugView `open
done ... out_mtu= in_mtu=` lines.

### 7b. Media channel MTU (needs SET_CONFIGURATION + OPEN first)

A second L2CAP channel on PSM 0x19 is only the media transport channel after
the sink accepted SET_CONFIGURATION and OPEN for a SEP. `media` does exactly
that, with SBC (one value per field, Media Transport + Media Codec only, no
Delay Reporting), opens the second channel, prints its MTUs, then sends CLOSE
(ABORT if CLOSE is refused) and closes both channels. It never sends START, so
no audio flows.

```powershell
& $cli media -v                 # first free SBC sink SEP
& $cli media -v --seid 0x01     # a specific SEP
& $cli media -v --in-mtu 1013   # second data point for 7a
```

Record: the media channel's `out_mtu` / `in_mtu` (SSC wants >= 1005 for
`out_mtu`), whether the second open succeeds at all on this node, how long it
takes, and any SET_CONFIGURATION reject (`error_name` decodes the A2DP codes
0xC1-0xE5).

### 7c. Raw experiments

Raw experiments use one handle for the whole command list. Channels close when
the process exits.

```powershell
# DISCOVER by hand (label 0): header 00, signal 01
& $cli raw "open 0x19" "send 1 00 01" "recv 1 5000" "close 1"
# interactive:
& $cli raw
```

Save the DebugView log and the console output of the first run: the open
milestone-1 questions (does the node start, does OPEN succeed, do reads and
disconnect behave) are answered from those two logs.

A synchronous close (CLOSE ioctl, process exit, D0Exit) now gives up after
8 s; DebugView then shows `close ABANDONED ... (slot frees itself when bthport
drains)` and the CLI's `close` fails with Win32 error 121. That line answers
whether bthport honours cancel / flushes on CLOSE_CHANNEL.

---

## 8. Roll back to BthA2dp (admin)

1. **Put the inbox driver back on the node.** Either:
   * Device Manager -> the node -> Properties -> Driver -> **Roll Back Driver**, or
   * Update driver -> Browse -> Let me pick -> **Microsoft Bluetooth A2dp Source**.

   Check:

   ```powershell
   Get-PnpDeviceProperty -InstanceId $node -KeyName DEVPKEY_Device_Service | Format-Table KeyName, Data   # BthA2dp
   ```

2. **Remove the package from the driver store.** Do this before anything
   re-installs the node (re-pair, "search automatically"): while the package
   is staged and trusted it outranks BthA2dp for the buds.

   ```powershell
   pnputil /enum-drivers | Select-String -Context 1,6 'crossc_bth.inf'
   # note the "Published Name" (oemNN.inf), then:
   pnputil /delete-driver oemNN.inf /uninstall /force
   sc.exe query crossc_bth     # should report: service does not exist
   ```

3. **Remove the test certificate** from the machine trust stores **and** from
   your user store `WDRTestCertStore`, where cargo-make keeps it with its
   private key (anything running as you could sign code with it):

   ```powershell
   certutil -delstore Root             WDRLocalTestCert
   certutil -delstore TrustedPublisher WDRLocalTestCert
   certutil -user -delstore WDRTestCertStore WDRLocalTestCert
   ```

   The next `cargo make` creates a new cert; trust it again (step 2) if you
   come back.

4. **Turn test signing off:**

   ```powershell
   Suspend-BitLocker -MountPoint "C:" -RebootCount 2
   bcdedit /set testsigning off
   Restart-Computer
   ```

   Re-enable Secure Boot in UEFI setup and Memory integrity if you changed
   them. BitLocker resumes by itself after the reboot count; check with
   `manage-bde -status C:`.

5. **Last resort.** Remove the buds in Settings -> Bluetooth and pair them
   again. This re-creates the BTHENUM nodes, and **only because step 2
   deleted the package** they get BthA2dp. With the package still staged,
   re-pairing would install crossc_bth on the buds again. If the machine bugchecks in a loop because of
   the driver, boot to WinRE -> Troubleshoot -> Startup Settings -> **Safe
   Mode**, turn Bluetooth off (or disable the radio in Device Manager), then
   run step 2 from an elevated prompt.

## 9. Stream SSC (play) (admin)

`play` does the whole source side: DISCOVER, GET_ALL_CAPABILITIES, picks the
SSC SEP (vendor 0x75 / codec 0x0103) and checks that its capability octet
offers every bit of the chosen configuration, SET_CONFIGURATION (Media
Transport + SSC with the one-octet configuration + Delay Reporting if the buds
offer it), OPEN, opens the media channel, START, then streams SSC encoded live
on this machine. On the end of the source or **Ctrl+C** it sends CLOSE (ABORT
if CLOSE is refused) and closes both channels. Ctrl+C during setup takes
effect after the step in flight (up to 5 s) and START is never sent after it;
a second Ctrl+C ends the process at once. **The audio comes out of the
buds directly.** There is no Windows sound device yet, so nothing shows up in
the volume mixer and other apps cannot play through it.

Wear the buds and start with the volume low: the tone is -20 dBFS and the
sweep -12 dBFS, but a decoding fault can come out as full-scale noise. Keep
the phone's Bluetooth off (the buds take one A2DP source at a time). Rebuild
first (`cargo build --release -p crossc-cli`).

Without a device, `--dry-run` encodes and packetises as fast as possible and
prints the encoder speed. Use it to check a WAV file or a combination of
options before trying them on the buds:

```powershell
& $cli play --mode uhq --dry-run
```

Try these in order and stop at the first one that fails:

```powershell
# 1. HiFi 48 kHz/24, config 0x0C, 192 kbps, 1 kHz tone (20 s by default)
& $cli play -v
# 2. HiFi log sweep 20 Hz -> 21.6 kHz (should glide smoothly, no clicks or dropouts)
& $cli play -v --sweep --seconds 20
# 3. HiFi WAV: any WAV file (16/24/32-bit int or 32-bit float, mono or stereo, any sample rate)
& $cli play -v --wav C:\path\to\music-48k-24bit.wav
# 4. UHQ 96 kHz/24, 584 kbps, one 672-byte packet every 9 ms: both configuration octets
& $cli play -v --mode uhq --config 0x0A --sweep
& $cli play -v --mode uhq --config 0x0E --sweep
# 5. Bitrate probes (use the UHQ octet that worked in 4)
& $cli play -v --bitrate 229 --sweep
& $cli play -v --bitrate 328 --sweep
& $cli play -v --mode uhq --bitrate 291 --frames-per-packet 2 --sweep
& $cli play -v --mode uhq --bitrate 152 --frames-per-packet 2 --sweep
```

Other options:

* `--seconds S` sets the length. A WAV plays to its end by default.
* `--tone HZ --level DBFS` changes the tone.
* `--ts samples` makes the RTP timestamp count samples instead of packets
  (+1 per packet is what Samsung's stack is reported to send).
* `--lead N` sets how many packets we stay ahead of the wall clock (default 3).
* `--no-delay-report` leaves Delay Reporting out of SET_CONFIGURATION.
* `--force-config` sends a configuration octet the buds do not offer.
* `--mode ssc48` (0x18) and `--mode ssc44` (0x28) are plain SSC at 328 kbps.
  328 is a guess; try `--bitrate 229` too.

A WAV file at another sample rate is resampled to the mode's rate before
streaming (48000 Hz for HiFi and ssc48, 96000 Hz for UHQ, 44100 Hz for ssc44)
with a windowed-sinc filter, and fed to the encoder as 32-bit float. It takes
about 1.5-2.5 s per minute of stereo audio. So a 44.1 kHz file works in every
mode, including `--mode uhq`.

**MTU check.** Which driver field is the buds' MTU is still unverified (7a).
`play` therefore drops reported values of 0 or 65535 and takes the smallest
remaining one. If that value equals our own requested in_mtu, it may just be
our request echoed back, so it also caps the limit at 1005 (the media MTU the
phone used with Buds4 Pro). It refuses before START if a packet would be
larger, and prints the packet size, the limit and the reason. Every default
packet fits in 1005 bytes. `--force-mtu N` overrides the check.

**Report back**, for each command:

* Did you hear it? Was it clean, distorted, noisy, choppy or silent? Did the
  sweep reach the top or cut out early? Which UHQ octet (0x0A or 0x0E)
  sounded right?
* The `SET_CONFIGURATION` / `START` result. On a reject, the error name, and
  the full `-v` output with the signaling trace.
* The `media channel: out_mtu=... in_mtu=...; packet ... limit ...` line.
* Every `sink DELAYREPORT: ... ms` line.
* The `final:` statistics line: packets, average and maximum WRITE latency,
  `late` (sent more than 0.5 ms after the intended send time), `missed` (sent
  after the playout deadline; each one is also logged with `re-anchoring`),
  write errors.
* The `encoder: ...x realtime` line.
* How it ended: `finished`, `interrupted`, `sink sent CLOSE/ABORT/SUSPEND`,
  `SIGNALING CHANNEL LOST` or `MEDIA CHANNEL LOST`, with the elapsed time.
  A Buds3 Pro once dropped the link about 110 s into UHQ from Linux, so for
  UHQ also run a long one (`--seconds 300`) and note when, or whether, it
  drops. If it drops, save the driver trace from step 6.

---

## 10. ACX endpoint spike

**What this build is.** The same `crossc_bth.sys` is now also an ACX audio
driver. Besides the L2CAP pipe it exposes one render endpoint, a **null
sink**: Windows renders PCM into it, the driver consumes it in real time,
counts it and throws it away. Nothing is sent to the buds yet. The question
this answers is whether one KMDF driver on the buds' BTHENUM node can be the
Bluetooth profile client and an ACX device at the same time, built from Rust.

**Not tested live.** The package builds, is signed, passes InfVerif and the
link checks, but this driver has never been loaded. A mistake in the ACX path
can bugcheck the machine when the buds' node starts. Read "Recovery" below
before installing, and save your work.

### 10a. Before installing

* Keep `C:\crossc-known-good` (the last package that is known to
  work). Do not delete it.
* Start DebugView as in section 6 (Capture Kernel, filter `crossc:`), so the
  start sequence is on screen if something goes wrong.
* Optional link check, no elevation, loads nothing:

  ```powershell
  powershell -ExecutionPolicy Bypass -File drivers\crossc-bth\tools\acxcheck\check-image.ps1
  ```

  Expected: `[debug] OK: ACX bind info present, WdfMinimumVersionRequired from wdk-sys`.

### 10b. Install

```powershell
.\tools\install-driver.ps1            # debug build + install (one UAC prompt)
```

It rebuilds, removes the staged crossc_bth package, stages the new one and
installs it on the buds' node. The INF changed (KS/audio registration,
`AddInterface` entries, device DACL), so a plain driver-file copy is not
enough; use the script.

### 10c. Expected result

1. DebugView, roughly in this order (the `acx:` lines are new):

   ```
   crossc: DriverEntry v0.1.0
   crossc: acx: driver initialized (ACX 1.1)
   crossc: EvtDriverDeviceAdd
   crossc: acx: device initialized
   crossc: device added
   crossc: acx: render circuit added
   crossc: D0Entry
   crossc: acx: circuit power up (from ...)
   crossc: remote A0:B1:C2:D3:E4:F5 (flags 0x6) ...
   ```

2. **Settings > System > Sound > Output** lists a new playback device,
   expected name **Headphones (Buds4 Pro)**. "Headphones" comes from the
   bridge pin category, "Buds4 Pro" from the INF interface `FriendlyName`.
   Which string Windows uses for the part in parentheses on a Bluetooth node
   is not verified; if the name differs, report what it shows.
3. Its properties (Sound settings > the device > Output settings > Format)
   offer **24 bit, 48000 Hz** (default), **24 bit, 96000 Hz** and
   **16 bit, 48000 Hz**. 24-bit is carried as 24 valid bits in a 32-bit
   container. Windows plays at the endpoint's default format, which the
   service sets to the one picked here (section 17); without the service
   it stays at what was last stored (48 kHz/24 on a fresh install).
4. Select it and play anything. It is **silent** (null sink), the player's
   progress bar advances at normal speed, and DebugView shows

   ```
   crossc: acx: stream created: RAW mode, 48000 Hz, 2 ch, 24 valid bits in 32-bit containers, block 8 bytes, tap yes, 1 of 4 streams
   crossc: acx: 2 RT packet(s) of ... bytes (... bytes allocated each)
   crossc: acx: stream prepared (tap ring 96000 bytes)
   crossc: acx: stream running (48000 Hz, feeds the tap: true)
   ```

   and, when playback stops (the audio engine closes the stream a few seconds
   after the last sound):

   ```
   crossc: acx: stream paused: 48000 Hz/32-bit, runs 1, packets N (M with signal), frames F
   crossc: acx: stream paused: late passes .. (max .. us), timeline shifts 0, SetRenderPacket late 0 / overrun 0
   crossc: acx: stream stopped: ...
   crossc: acx: RT packets freed
   ```

   `packets` should be about 100 per second of playback (10 ms packets;
   the audio engine chooses the size), `frames` about 48000 per second, and
   `with signal` greater than 0 while something audible was playing. Switch
   the format to 96 kHz and repeat: `frames` should be about 96000 per second
   (with the service running, section 17; without it the default stays at
   48 kHz and so does the stream).
5. The pipe still works next to it. From an elevated prompt, with `$cli` as
   in section 7:

   ```powershell
   & $cli probe
   ```

   must behave exactly as before (GET_INFO, AVDTP discovery, capabilities).
   Run it once while audio is playing to the endpoint and once while it is
   idle.
6. Channels of a killed client are still closed. With ACX on the device,
   Windows' driver framework no longer tells the driver when a pipe handle is
   closed (found by reading the framework's code, not by a test), so the
   driver now closes a dead handle's channels when the handle's file object
   is destroyed instead. Check that this works:

   ```powershell
   & $cli play -v                            # window 1: wait until it streams
   Stop-Process -Name crossc-cli -Force      # window 2 (elevated)
   & $cli info                               # window 2
   ```

   Expected: `info` prints `channels   0/...`, and DebugView shows, right
   after the kill, two `chN closing` lines and
   `crossc: file close: closed 2 channel(s) left open by a handle`.
   If `info` still counts channels, the hook did not fire: restart the node
   (`pnputil /restart-device "$node"`) to free them, and report it.

**Report back:** the DebugView log from node start to the end of one
playback, the endpoint name, which formats are offered, the stats lines, the
`probe` output and the result of step 6. If the endpoint does not appear,
also the output of

```powershell
pnputil /enum-devices /instanceid "$node" /stack /interfaces
```

### 10d. Kill switch: `DisableAcx`

The value `DisableAcx` (REG_DWORD, non-zero) turns the endpoint off and
leaves the pipe. It is read in two places, and they are not equivalent:

* **Whole driver** (service key): the driver makes no ACX call at all, and
  the code that runs is the pre-ACX driver's. Use this one for recovery.
* **Per device** (hardware key): the driver has already called
  `AcxDriverInitialize` when it loaded; only the calls for that device are
  skipped. A driver that is initialised for ACX but whose device is not is a
  combination no Microsoft sample uses, so it is itself untested.

Per device (the device's hardware key; `$node` from section 4):

```powershell
reg add "HKLM\SYSTEM\CurrentControlSet\Enum\$node\Device Parameters" /v DisableAcx /t REG_DWORD /d 1 /f
pnputil /restart-device "$node"
```

Written out, the key is
`HKLM\SYSTEM\CurrentControlSet\Enum\BTHENUM\{0000110b-0000-1000-8000-00805f9b34fb}_VID&00010075_PID&a013\<instance>\Device Parameters`,
where `<instance>` is the only subkey there (it contains the buds' address).
With it set, DebugView shows `acx: DisableAcx set in the device key - ACX off for this device`.

Whole driver (fixed path, easiest to type in Safe Mode; also skips
`AcxDriverInitialize` in DriverEntry). It is read when the driver loads, so
restart the node afterwards:

```powershell
reg add "HKLM\SYSTEM\CurrentControlSet\Services\crossc_bth\Parameters" /v DisableAcx /t REG_DWORD /d 1 /f
pnputil /restart-device "$node"
```

With it set, DebugView shows `acx: DisableAcx set in the service Parameters key - ACX off`.

To turn ACX back on, delete the value (`reg delete "<key>" /v DisableAcx /f`)
and restart the node.

Limits of both switches: neither can skip the class-extension *binding* the
WDF loader does before DriverEntry (the image asks for ACX 1.1 at load), and
the INF changes stay in place (the wider device DACL of 10g and the
`AddInterface` entries). If the driver does not even load, go back to the
known-good package.

### 10e. Back to the known-good package

```powershell
.\tools\install-driver.ps1 -Uninstall
pnputil /add-driver C:\crossc-known-good\debug-pkg\crossc_bth.inf /install
```

(elevated for the `pnputil` line). `-Uninstall` removes the spike package and
the buds fall back to BthA2dp; the second line installs the known-good
pipe-only driver again. Check with section 5c and `crossc-cli probe`.

### 10f. Recovery if it bugchecks

`crossc_bth` is demand-start: it loads when the buds' A2DP node starts, that
is at boot or sign-in if the buds are paired and the node exists, and again
whenever the node restarts. If the machine bugchecks in a loop:

1. Take the buds out of range or keep them in the closed case, if that
   lets Windows boot. It may not: the node of a paired device exists without
   the buds being connected.
2. Boot into **Safe Mode**: hold Shift while clicking Restart, or let two
   boots fail to get the recovery menu, then Troubleshoot > Advanced options >
   Startup Settings > Restart > 4. Safe Mode loads a minimal driver set
   without the Bluetooth stack, so the node should not start and the driver
   should stay unloaded (not verified on this machine).
3. In Safe Mode, from an elevated prompt, do one of:

   * Disable ACX and keep the driver:

     ```powershell
     reg add "HKLM\SYSTEM\CurrentControlSet\Services\crossc_bth\Parameters" /v DisableAcx /t REG_DWORD /d 1 /f
     ```

   * Or remove the package, which puts the buds back on BthA2dp:

     ```powershell
     pnputil /enum-drivers | Select-String -Context 1,6 crossc_bth.inf   # find its oemNN.inf
     pnputil /delete-driver oemNN.inf /uninstall /force
     ```

4. Reboot normally. Then reinstall the known-good package (10e) if you
   removed the driver.
5. Keep `C:\Windows\MEMORY.DMP` or the newest file in `C:\Windows\Minidump`
   and report the bugcheck code and the faulting module.

If Safe Mode is not reachable, use the recovery environment's Command Prompt:
load the offline SYSTEM hive (`reg load HKLM\OFF C:\Windows\System32\config\SYSTEM`),
set `HKLM\OFF\ControlSet001\Services\crossc_bth\Start` to `4` (disabled),
`reg unload HKLM\OFF`, reboot, then remove the package as above.

### 10g. Known limits of the spike

* **Access to the pipe is wider than before.** The audio stack must be able
  to open the device, so the INF now gives the device object ACX's own DACL
  (SYSTEM and Administrators full, LOCAL SERVICE and interactive users
  read/write). The pipe interface is on the same device object, so a
  non-elevated process of the logged-on user can now open L2CAP channels to
  the buds. Accepted for the spike; to be closed afterwards.
* **A dead client's channels are closed a little later than before.** With
  ACX on they are closed when the handle's file object is destroyed, which
  is after its last pending request has ended, not at the moment the handle
  is closed. When a process dies Windows cancels its pending requests, so
  this follows at once (step 6 of 10c checks it). A program that closes its
  handle while one of its own READs is still waiting keeps its channels open
  until that READ ends. `crossc-cli` is not affected: it sends CLOSE_CHANNEL
  itself.
* No hardware volume or mute: Windows applies the volume in software, which
  is what the later encoder path wants.
* The endpoint is always "connected", whether or not the buds are in range.
* Windows refuses to disable, update or remove the device while a stream to
  the endpoint is running. Stop playback first.

---

## 11. The service

`crossc-service.exe` is the user-mode half of the audio path: it takes the PCM
Windows renders into the driver's endpoint, encodes it as SSC and streams it
to the buds. It needs a driver that speaks protocol v2 (the audio IOCTLs).
With an older driver it logs that and keeps retrying every 10 s.

```powershell
cd C:\src\crossc
cargo build --release -p crossc-service
$svc = "C:\src\crossc\target\release\crossc-service.exe"
```

**Not tested against the real driver.** The session logic is tested against a
fake driver and a fake sink only. Try it in the foreground (11a) before
installing it as a service.

What it does, per crossc device (one per paired pair of buds):

1. Waits for the driver (protocol v2, ACX on) and attaches to it.
2. Waits until Windows reports the buds connected. It never connects to them
   by itself; take them out of the case or press Connect.
3. Opens AVDTP, configures the SSC stream for the endpoint's format and
   reports the link to the driver. Windows then shows the endpoint as plugged
   in.
4. Streams while audio plays. 3 s after the audio stops it suspends the
   stream.
5. When the buds go away it reports the link down (the endpoint shows as
   unplugged) and waits for them to come back.

### 11a. Foreground run (admin)

```powershell
& $svc run        # logs to this console; Ctrl+C stops it cleanly
& $svc run -v     # the same with the debug log (every AVDTP packet)
```

Stop the installed service first if there is one: the driver accepts one
session at a time, and the second one logs that `AUDIO_ATTACH` failed and
keeps retrying. `--for SECONDS` makes it stop by itself. A typical start:

```
... INFO  [AABBCCDDEEFF] attached: driver x.y.z (protocol v2), buds AA:BB:CC:DD:EE:FF
... INFO  [AABBCCDDEEFF] state wait-driver -> wait-link
... INFO  [AABBCCDDEEFF] the buds are connected
... INFO  [AABBCCDDEEFF] SSC SEP 0x03: caps 0x3e [...], Delay Reporting
... INFO  [AABBCCDDEEFF] link up: HiFi 48 kHz/24: config 0x0c, 229 kbps, ...
... INFO  [AABBCCDDEEFF] state wait-link -> ready
... INFO  [AABBCCDDEEFF] state ready -> streaming
```

The foreground run serves the status pipe too (section 16), with the
console's own security instead of the service's: its log says
`status pipe \\.\pipe\crossc-status is up` once the tray app would believe
the pipe. An unelevated tray may still be refused by that pipe ("Access
denied"); for a look at the status of a foreground run use
`crossc-tray.exe --print-once` from the same elevated console.

### 11b. Install, status, uninstall (admin)

```powershell
& $svc install            # register the service "crossc" (auto-start, LocalSystem)
& $svc install --start    # the same, and start it now
& $svc status             # no admin needed for the service line
& $svc uninstall          # stop and remove it
```

`install` registers `"<path>\crossc-service.exe" service` under the name
`crossc` ("crossc SSC audio service"), restarted by Windows 5 s, 30 s and then
60 s after a failure. Run it again after moving or rebuilding the executable:
it updates the existing registration. A running service keeps its old
executable until it is restarted (`Restart-Service crossc`). `uninstall`
succeeds when the service is not installed.

`status` prints whether the service is installed and running, and for every
crossc device the driver version, the endpoint and stream state, and whether
Windows reports the buds connected. The device lines need an elevated prompt.

The service stops its sessions when the machine goes to sleep and starts them
again on resume.

### 11c. Log

`C:\ProgramData\crossc\service.log`, rotated at 1 MB (`service.log.1`,
`service.log.2`; three files at most). It has every state change, the
configuration chosen, the buds' delay reports, statistics once a minute while
streaming (packets, frames the driver dropped, discontinuities, WRITE
latency), and each error once a minute at most. One line after the start
says whether the status pipe for the tray app is up (16f).

```powershell
Get-Content C:\ProgramData\crossc\service.log -Tail 40 -Wait
```

### 11d. Settings

Optional values under `HKLM\SOFTWARE\crossc`, read when the service starts:

| Value | Type | Default | Meaning |
|---|---|---|---|
| `UhqConfig` | DWORD | `0x0A` | SSC configuration octet sent for UHQ 96 kHz. `0x0E` is what another implementation sends. |
| `HifiBitrate` | DWORD | `229` | Highest HiFi 48 kHz bitrate in kbps: 88, 96, 128, 192 or 229. |
| `UhqBitrate` | DWORD | `584` | Highest UHQ bitrate in kbps: 152, 291 or 584. |
| `AdaptiveBitrate` | DWORD | `1` | 1: the bitrate follows the link, never above the two values before. 0: always those. |
| `LogLevel` | DWORD | `2` | 0 error, 1 warn, 2 info, 3 debug. |

Section 15 has the bitrate values in full (what happens to a rate that is
not in the list, and the ladders under each top rate).

```powershell
reg add "HKLM\SOFTWARE\crossc" /v LogLevel /t REG_DWORD /d 3 /f
Restart-Service crossc
```

A value that is not valid is ignored with a warning in the log. Which mode is
sent for which endpoint format is the table in `docs/DESIGN.md`; buds that
offer neither HiFi nor UHQ get plain SSC at 229 kbps.

---

## 12. Audio path live test

**What this build adds to the driver.** The endpoint of section 10 is no
longer a null sink: the driver copies what Windows renders into a 250 ms
buffer (the PCM tap) and hands it to `crossc-service` through the audio
IOCTLs of protocol v2. The endpoint has a jack that follows the service's
link flag, so it shows as unplugged until the service has an SSC stream to
the buds. Every crossc IOCTL except `GET_INFO` now requires an elevated
caller. The playback device is named after the buds' Bluetooth name.

Where this section and an earlier one disagree, this one is current:

* **Section 10:** the device name (10c step 2), "the endpoint is always
  connected", and the pipe being open to non-elevated processes (10g).
* **5b:** the driver to pick in Device Manager is now called
  **Galaxy Buds (crossc SSC audio)**, not "crossc A2DP sink (spike)".
* **7a and the "MTU check" paragraph of section 9:** the MTU mapping is no
  longer the one described there. The live runs showed the two fields
  swapped, and the driver now reports `out_mtu = InResults.Params.Mtu` (what
  the buds accept) and `in_mtu = OutResults.Params.Mtu`. That changes the two
  numbers in the `OPEN_CHANNEL` reply and the length limit of `WRITE`, which
  is checked against `out_mtu`. The two-opens experiment of 7a is still the
  way to confirm it, and has not been re-run on this build: `in_mtu` must now
  follow the value that was requested.

**Not tested live.** Like the ACX part of section 10, none of this has been
loaded. It builds, is signed, passes InfVerif, clippy and the image check, and
the buffer and session logic passes its unit tests on the host
(`cd drivers\crossc-bth\host-tests; cargo test`). A mistake in the kernel part
can still bugcheck the machine. Read 10f (recovery) first, keep
`C:\crossc-known-good`, save your work.

Both `DisableAcx` switches of 10d still work. With ACX off, `GET_STATE` still
succeeds and returns a state without the ACX flag; every other audio IOCTL
fails with "not supported". `crossc-service` reads that state, logs "the
driver's audio endpoint is off (ACX disabled)" and waits for a driver with
ACX. The pipe works as before.

### 12a. Turn Driver Verifier on for this driver (admin, reboot)

Driver Verifier makes Windows check every call the driver makes (IRQL, pool,
locks, I/O, the WDF rules). A violation becomes an immediate bugcheck that
names the rule, instead of a corruption that shows up later somewhere else.
Use it for this whole test.

```powershell
verifier /standard /driver crossc_bth.sys
shutdown /r /t 0
```

After the reboot, check:

```powershell
verifier /querysettings      # lists crossc_bth.sys and the standard flags
```

While it is on, the driver runs slower; short audio glitches under Verifier
are not a finding by themselves.

If the machine bugchecks at boot with Verifier on: Safe Mode (10f step 2),
then `verifier /reset` and reboot. Verifier settings can also be told to
drop themselves after a failed boot; if `verifier /?` on this build lists
`/bootmode`, run `verifier /bootmode resetonbootfail` after the first command
and before the reboot.

Turn it off when the test is done (12d).

### 12b. Install

```powershell
.\tools\install-driver.ps1            # debug build + install (one UAC prompt)
```

The INF changed again (device description `Galaxy Buds (crossc SSC audio)`),
so use the script, not a file copy. Have DebugView running as in section 6.
New lines at node start, compared with 10c:

```
crossc: acx: render circuit added (jack: unplugged until a service sets the link)
crossc: remote A0:B1:C2:D3:E4:F5 (flags 0x6) devinfo flags=0x.. cod=0x......
crossc: friendly name set from the Bluetooth name (N UTF-16 units): 0x0
```

Build the service and the CLI as well:

```powershell
cargo build --release -p crossc-service -p crossc-cli
$svc = "C:\src\crossc\target\release\crossc-service.exe"
$cli = "C:\src\crossc\target\release\crossc-cli.exe"
```

### 12c. Tests, in this order

Stop after the first step that fails and report it; later steps build on the
earlier ones. Keep the service **stopped** until step 3
(`Stop-Service crossc` if it is installed).

1. **The endpoint exists and is unplugged.** No service is running, so the
   jack reports "not connected".
   * Settings > System > Sound > Output does **not** offer the buds.
   * `mmsys.cpl` > Playback > right-click > tick **Show Disconnected
     Devices**: there is **Headphones (*the buds' Bluetooth name*)**, marked
     **Not plugged in**. Write down the exact name. If it reads
     `Headphones (Galaxy Buds (crossc SSC audio))` or anything else, restart
     the node once (`pnputil /restart-device "$node"`) and look again: the
     name is stored at the first start and may only be picked up at the
     second. Report what both starts showed.

2. **The admin gate.** From a PowerShell that is **not** elevated:

   ```powershell
   & $cli info        # works: GET_INFO needs no elevation
   & $cli probe       # must fail with "Access is denied" (error 5)
   ```

   From an elevated one, `& $cli probe` must work as in section 7. Run it now:
   it also shows that the pipe survived the changes. If the non-elevated
   `probe` is *not* denied, stop and report it.

3. **The service attaches and the endpoint becomes available** (elevated;
   buds out of the case and connected to this PC):

   ```powershell
   & $svc run -v
   ```

   Expected in its log: `attached: driver ... (protocol v2)`, then
   `link up: ...`. DebugView:

   ```
   crossc: audio: session attached
   crossc: chN open ...                 (signaling, then media)
   crossc: audio: link up
   ```

   Within a second or two the device of step 1 loses "Not plugged in" and
   appears in Settings > Sound > Output. If the service logs `link up` but
   Windows still shows "Not plugged in", report that (it would mean the jack
   notification did not reach Windows).

4. **Sound at 48 kHz / 24 bit.** Select the device as output; its format
   (device > Output settings > Format) should be **24 bit, 48000 Hz**. Play
   music. Expected: sound in the buds. DebugView:

   ```
   crossc: acx: stream created: RAW mode, 48000 Hz, 2 ch, 24 valid bits in 32-bit containers, block 8 bytes, tap yes, 1 of 4 streams
   crossc: acx: stream prepared (tap ring 96000 bytes)
   crossc: acx: stream running (48000 Hz, feeds the tap: true)
   ```

   Play for a minute, pause the player and wait until the stream pauses
   (a few seconds). DebugView then prints the statistics, among them

   ```
   crossc: acx: tap (device totals): rendered R frames, dropped D, silence for stale packets S (P packets of this stream), attached true, link true
   ```

   `R` grows by about 48000 per second of playback. `D` should not grow
   while the service is streaming (frames rendered before the service
   attached count as dropped, so note `D` before and after). `S` and `P`
   should be 0; a few after a visible stall are harmless. Note the service's
   own statistics line too (frames the driver dropped, discontinuities).
   Listen for clicks, repeats or drift over a few minutes.

5. **Switch to 96 kHz / 24 bit.** Stop playback, set Format to **24 bit,
   96000 Hz**, play again. Expected: the service log has `Windows device
   format for ... is 96000 Hz/24: driver default set` and shows it
   reconfigure to UHQ (if the buds offer it; otherwise it resamples), sound
   continues, DebugView shows a new stream with `96000 Hz` and `tap ring
   192000 bytes`. If the stream stays at 48 kHz, pause and resume playback
   (section 17). Switch back to 48 kHz once and play again.

6. **Stopping the service unplugs the device.** While music plays, press
   Ctrl+C in the service window. Expected, DebugView:

   ```
   crossc: chN closing ...
   crossc: audio: session detached (ATTACH cancelled or its handle closed): link was true, N channel(s) closed
   ```

   (`N` is 0 if the service closed its channels itself first, and `link
   was false` if it also reported the link down first; both are fine.) The
   device shows **Not plugged in** again and Windows moves the sound to
   another output. `& $cli info` (elevated) shows no open channels.

7. **A killed service is cleaned up too.** Start `& $svc run -v` again, play
   music, then from a second elevated prompt:

   ```powershell
   Stop-Process -Name crossc-service -Force
   & $cli info
   ```

   Expected: the same `session detached` line, the device unplugged,
   `info` reporting no open channels. If the device stays available after
   the service is gone, report it (then `pnputil /restart-device "$node"`
   puts it right).

8. **Buds in the case.** Start the service again and wait for `link up`. Put
   both buds into the case and close it. Expected: the service logs the link
   going down, DebugView shows `crossc: audio: link down`, the device shows
   **Not plugged in**. Take the buds out: once Windows reports them
   connected, the service brings the link up and the device is available
   again.

9. **Installed service.** `& $svc install --start`, then repeat steps 4 and 8
   once with the service running as LocalSystem. Reboot once with the buds in
   the case, sign in, take them out: the device must become available
   without touching anything.

10. **Sleep.** With music playing, put the PC to sleep and wake it. Expected
    before sleep, in DebugView, one `session detached` line. Which one
    depends on who is faster, and both are correct:

    ```
    crossc: audio: session detached (ATTACH cancelled or its handle closed): ...
    crossc: audio: session detached (D0Exit): ...
    ```

    The installed service (step 9) is told about the suspend and closes its
    session itself, which gives the first line; the driver then has nothing
    left to detach when it powers down. If the driver powers down first, or
    the service runs in the foreground (`run` does not listen for power
    events), the driver ends the session, which gives the second line. After
    resume: a new `session attached`, and sound again once the buds have
    reconnected. A failure is: no `session detached` line at all, or no new
    `session attached` after resume.

### 12d. Afterwards

Turn Driver Verifier off:

```powershell
verifier /reset
shutdown /r /t 0
```

**What to send back:**

* the DebugView log from node start to the end of the tests (File > Save);
* `C:\ProgramData\crossc\service.log` (or the console output of `run -v`);
* the device name Windows showed, at the first and at the second start;
* for each of the ten steps: passed, or what happened instead;
* the statistics lines of step 4 at 48 kHz and of step 5 at 96 kHz;
* the output of the non-elevated `& $cli info` and `& $cli probe`;
* if anything bugchecked: the stop code and its four parameters (for
  Verifier stops `0xC4` / `0xC9` / `0x10D` the first parameter names the
  rule), the faulting module, and the newest file in `C:\Windows\Minidump`
  or `C:\Windows\MEMORY.DMP`, and whether Driver Verifier was on.

### 12e. What the driver does that the contract text does not say

The contract (`crates\crossc-ioctl`) is frozen for this build. The driver
keeps every rule in it; these points go beyond its wording, and a client
written from the text alone would not expect them:

* **Who may open the device.** The contract says the INF limits the device to
  SYSTEM and Administrators. ACX needs the device to be openable by Local
  Service and by interactive users, so the INF allows that, and any
  logged-on user can open it and call `GET_INFO`. Every other IOCTL is
  refused by the driver itself unless the caller is elevated (step 2 of 12c).
* **"Not supported".** While the endpoint does not exist (ACX off), every
  audio IOCTL except `GET_STATE` fails with `STATUS_NOT_SUPPORTED`.
* **ATTACH also ends on sleep and removal.** The pending `ATTACH` completes
  with `STATUS_CANCELLED` when the device leaves D0 or is removed, not only
  when it is cancelled or its handle is closed. The session is over then; the
  client attaches again after resume.
* **`READ_PCM` can fail late.** A `READ_PCM` accepted while no stream existed
  is checked against the smallest possible frame. If the stream that then
  starts has larger frames than the buffer holds, the pending request
  completes with `STATUS_BUFFER_TOO_SMALL`. A buffer of `PCM_READ_BUFFER`
  bytes never runs into this.
* **`frames_rendered`** (and `frames_dropped`) count since the driver's
  device object for the node was created, not since the driver was loaded.
  They keep counting across sleep and resume; restarting the node
  (`pnputil /restart-device`) or pairing the buds again sets them back to 0.
* **"Busy"** is `STATUS_DEVICE_BUSY`, which user mode sees as
  `ERROR_BUSY` (170).

---

## 13. Changes after the crash review

A review of the whole driver for crash paths confirmed three findings. All
three are fixed in this build; none of them was seen on hardware. Host
tests: `cd drivers\crossc-bth\host-tests; cargo test`.

**What changed**

* **Sleep or removal with an OPEN in flight (F1).** When the device leaves
  D0 or is removed, WDF waits until every request of the device's I/O queue
  that the driver still owns is completed. bthport does not cancel an
  `OPEN_CHANNEL` it is working on, so that wait could last until bthport's
  own L2CAP timers ran out (15 s per step, 60 s after "connect pending"),
  and under Driver Verifier a wait of more than 60 s breaks into the
  debugger. The caller's `OPEN_CHANNEL` request now waits in a small queue
  of its own that does not take part in power changes, and the BRB goes to
  bthport on a request the driver creates for it. Leaving D0 or removal
  therefore no longer waits for an OPEN; the close of the slot still waits
  up to 8 s and then gives the slot up (`close ABANDONED ... state=Opening`),
  and the caller gets `STATUS_CANCELLED` when bthport ends the OPEN (a
  channel that still opens is closed again). What the caller sees otherwise
  is unchanged: a cancelled OPEN (crossc-core cancels after its timeout and
  then waits for the result) still returns whatever bthport makes of it,
  a late success included, so no retry OPEN can overlap the old one. READ
  and WRITE are cancelled as before.
* **Disconnect callback after the device is gone (F2).** bthport may call the
  driver's disconnect callback for as long as it has a channel. The memory
  the callback works on is now a small block of its own that lives until the
  device and bthport have both let go of it, and the callback can no longer
  reach a deleted device. A `CLOSE_CHANNEL` that could not be handed to
  bthport is no longer treated as a finished close: the slot stays reserved,
  the close is tried again by the next close attempt, and when bthport
  reports the disconnect it is told to close the channel itself
  (`CloseNow`). The close request is formatted once at device start, so the
  close path no longer allocates.
* **Audio buffer size and stream count (F7).** The size of the audio
  packets is chosen by the client. It is now limited to between 1 ms and 5 s
  of audio in the stream's format (192 to 960000 bytes at 48 kHz/16-bit,
  768 to 3840000 bytes at 96 kHz), and at most 4 streams can exist on the
  endpoint at once. Before, one unprivileged process could take 2 GiB of
  nonpaged memory per stream, with no limit on streams. The audio engine's
  10 ms packets (1920 / 3840 / 7680 bytes) and every exclusive-mode buffer
  WASAPI accepts (up to 5 s event driven, 2 s timer driven) are inside the
  window.

**What to watch in the live test (section 12)**

* **Node start.** The device must start as before. New at start are one
  small allocation and one extra call per channel slot; a failure shows as
  `no memory for the indication callback block` (the node does not start) or
  `close request could not be pre-formatted` (it starts anyway).
* **Stream start, step 4 and 5 of 12c.** The line
  `acx: N RT packet(s) of M bytes` must appear and sound must play. A line
  `AllocateRtPackets(..) rejected: ...` or
  `stream refused: 4 streams exist already` while Windows itself is the only
  client means the limits are too tight: send the line, it carries the sizes.
  The `stream created` line now ends in `K of 4 streams`. K is normally 1
  and must not grow from one `stream created` line to the next over repeated
  play/stop or format changes.
* **Closing and disconnecting.** Every `ch{n} closing` is still followed by
  `close BRB done`. Lines that should never appear in normal use:
  `CLOSE NOT SENT`, `close ABANDONED`, and a `remote disconnect` line with
  `ours=false` or `close_now=true`. Turning the buds off while connected
  must still give `remote disconnect ... ours=true close_now=false`.
* **Opening channels (probe, play, service connect).** This path changed
  inside the driver; it must behave exactly as before. Each `ch{n} open`
  line is followed by `open done 0x0 ...` and the tool works as before.
  The line `open request cancelled while parked: kept for the OPEN's
  result` appears only when a tool timed out on an OPEN; it must be
  followed by `open done` and the tool must report what that line says.
* **Sleep and resume, step 10 of 12c.** Expected as before. If the PC is put
  to sleep in the moment a channel is being opened, the new line is
  `close ABANDONED ... state=Opening`, at most 8 s after D0Exit; D0Exit
  takes up to those 8 s. After resume the channel must be free again once
  `open done` appears.
* **Removal** (`pnputil /remove-device`, unpairing, Bluetooth off) with a
  channel open: `device cleanup` must appear. The line
  `bthport still holds N channel registration(s)` means the device went away
  before bthport dropped a channel; it must be followed, when the buds
  disconnect, by `left to bthport (CloseNow)` and `callback block freed`.
  If it is not, send the log.
* **Driver Verifier (12a).** An OPEN in flight no longer holds up the I/O
  queue, so the Verifier break after 60 s of waiting there should not
  happen. Removal of the node while an OPEN is in flight still waits, when
  the device object is deleted, until bthport ends that OPEN (the same wait
  as for an abandoned close), which is bounded by bthport's timers and has
  no Verifier break.

## 14. Unified volume live test

**What this build adds.** The Windows volume slider and the buds' own volume
become one control, as with Windows' inbox A2DP driver. The driver's render
circuit now also carries the private interface the inbox AVRCP stack
(`BthAvctpSvc` with `BthAvrcp.dll`) uses to do absolute volume for
`BthA2dp.sys`: interface class `{e630310c-1733-42bb-9847-7e45d1a27490}`, KS
property and event `{401e566b-b47e-4e34-b0fc-94826e9e2a1a}` id 1, KS method
`{0e62540f-bac4-4b85-b344-0d82ed5008ac}` id 6, plus a hardware volume and mute
element with BthA2dp's range (-63.5..0 dB). AVRCP itself (play / pause / next)
stays on the inbox nodes and is not touched. The service speaks protocol v3:
while the buds apply the volume it streams unscaled, and while nobody does
(no AVRCP subscriber, kill switch, a Windows update that changed the private
interface) it applies the slider and mute in software, so the slider always
works. Design: `docs/DESIGN.md`, "Volume".

**Not tested live.** Nothing of it has been loaded. It builds, is signed,
passes InfVerif, clippy, the image check and the layout check against cl.exe,
and the volume logic passes its host tests (a closed-loop simulation of
Windows + BthAvrcp + buds included). Whether the inbox AVRCP stack accepts a
non-Microsoft driver as its volume partner is what this test finds out. Keep
`C:\crossc-known-good` and read 10f first.

**Install both halves together.** A v0.1.1 service with this driver leaves
the slider and mute without effect whenever AVRCP is not active: the
endpoint claims hardware volume and mute, so Windows stops scaling and
muting the stream itself, and the old service applies neither.

**Hearing first.** Before installing, set the Windows volume of the buds low
(20 % or less) and keep the buds out of your ears until 14c is done. From
this build on the Windows level is sent to the buds as their own volume. The
buds' level is expected to win when they connect, but if Windows restores its
stored level after AVRCP has connected, that level goes to the buds instead,
and with the old two-control setup it may well be 100 %.

### 14a. Install (admin)

```powershell
cd C:\src\crossc
.\tools\install-driver.ps1                  # driver (debug build), one UAC prompt
cargo build --release -p crossc-service -p crossc-cli
$svc = "C:\src\crossc\target\release\crossc-service.exe"
$cli = "C:\src\crossc\target\release\crossc-cli.exe"
& $svc uninstall; & $svc install --start    # service, protocol v3
```

DebugView (section 6) running, filter `crossc:`. New lines at node start:

```
crossc: acx: device initialized (unified volume on)
crossc: avrcp: circuit has the volume contract (category {e630310c}, property/event {401e566b}/1, method {0e62540f}/6), volume element true, mute true, event true
crossc: acx: render circuit added (jack: unplugged until a service sets the link; hardware volume true, mute true, AVRCP event true)
```

Something else instead:

* `avrcp: circuit with the volume contract failed 0x... - unified volume
  off, building it without`, after a line saying what failed (for example
  `AcxDeviceAddCircuit failed 0x...`, `volume element or AVRCP event
  missing - discarding the circuit`, or an `AcxCircuitInitAssign...`
  failure): ACX refused something the feature adds. The half-built circuit
  is deleted and the endpoint is built as in 0.1.1 (Windows scales the
  volume); the next `render circuit added` line says `hardware volume
  false`. The feature stays off for the device until it is removed and added
  again. Send the lines.
* `mute false` with `volume element true`: only the mute element could not
  be created; Windows mutes in software, the rest works.

If Windows still scales the volume after the install (the Levels tab of 14e
shows no dB range), remove the device in Device Manager and reconnect the
buds once so that the endpoint is rebuilt.

### 14b. The interface exists (user)

```powershell
pnputil /enum-interfaces /class "{e630310c-1733-42bb-9847-7e45d1a27490}"
```

Expected: an entry on the buds' node (`...{0000110b-...}...AABBCCDDEEFF...`)
whose path ends in `\Headphones0`, **Enabled**. The old `\CONTROL` entry from
when BthA2dp drove the node may still be listed as Disabled; that is fine.

### 14c. The inbox stack finds it: reconnect the buds (the deciding step)

Put both buds in the case, close it, wait 10 s, take them out. Expected, in
this order (values and process ids vary; here the buds were at AVRCP 38):

```
crossc: avrcp: method {0e62540f}/6 (MethodServiceReady) - answered with success [pid N]
crossc: avrcp: volume event enabled (AVRCP subscribed) [pid N]
crossc: avrcp: property SET -17.80 dB (0xffee... , AVRCP 38) from the buds: slider moved, mute off, event false, connect window closed [pid N]
crossc: avrcp: Windows volume ch 0 = -18.00 dB (AVRCP 37)
crossc: avrcp: Windows volume ch 1 = -18.00 dB (AVRCP 37)
```

or, if BthAvrcp turned on its response processing before the buds' INTERIM
answer arrived, two SETs within about a second: first its built-in default,
then the buds' level:

```
crossc: avrcp: property SET -10.19 dB (0xfff5... , AVRCP 64) from the buds: slider moved, mute off, event false, connect window open [pid N]
crossc: avrcp: Windows volume ch 0 = -10.00 dB (AVRCP 65)
crossc: avrcp: Windows volume ch 1 = -10.00 dB (AVRCP 65)
crossc: avrcp: property SET -17.80 dB (0xffee... , AVRCP 38) from the buds: slider moved, mute off, event false, connect window closed [pid N]
crossc: avrcp: Windows volume ch 0 = -18.00 dB (AVRCP 37)
crossc: avrcp: Windows volume ch 1 = -18.00 dB (AVRCP 37)
```

* The method line comes from BthAvctpSvc, the event and SET lines from
  BthAvrcp inside the same service. `tasklist /svc /fi "PID eq N"` should
  show the `svchost.exe` that hosts `BthAvctpSvc`. The pid is the process the
  callback ran in: ACX runs the event enable in the caller's process, the
  property and method callbacks may show 4 (System).
* The (last) SET carries the buds' current level: the buds' level wins at
  connect, and the Windows slider jumps to it. In the second sequence the
  slider shows about 50 % for a moment first. Note which sequence you got.
  If the buds were at 50 % (AVRCP 64) there is only the first SET in either
  case.
* The `Windows volume` lines after a SET are audiosrv writing the level back
  on its 0.5 dB grid, which is why their AVRCP value can differ from the
  SET's by one or two. They must **not** end in `, event` (echo rule).
* If Windows was muted at connect, each connect SET says `mute on, event
  true` and a `property GET -> -63.50 dB (AVRCP 0)` follows; Windows stays
  muted and the buds go to 0.

`& $cli info` (elevated) now has a volume line:

```
volume     hardware, the buds apply it (AVRCP absolute volume active); level -17.8 dB = AVRCP 38; left -18.0 / right -18.0 dB; AVRCP subscriber: yes; AVRCP requests: 0 get, 1 set, 0 events
```

and `C:\ProgramData\crossc\service.log` has
`volume: the buds apply the volume (AVRCP absolute volume active); level ...`.

(`level` is the buds' exact level, `left` / `right` what audiosrv wrote
back on its grid; the request counts say 2 sets after the second sequence.)

**Go / no-go.** Go: the method line, `volume event enabled` and the connect
`property SET` appear. No-go, none of them: the inbox stack does not use the
interface (send the `pnputil` output of 14b and the log). The endpoint keeps
working with software volume (the service log says `software gain`).

### 14d. Buds -> Windows

Change the volume on the buds (gesture) a few steps up and down. Expected per
step: at most one `property SET ... from the buds: slider moved`, the
Windows slider follows, then the `Windows volume` write-back lines
**without** `, event`. No `volume event generated` or `property GET` follows
a buds gesture. The loudness changes once per step, not twice. A step can
have no SET at all: BthAvrcp ignores a new buds level whose whole percent
(`level * 100 / 127`, rounded down) equals the previous one's, e.g. 0 -> 1,
4 -> 5, 63 -> 64 (inbox behaviour, not a crossc fault); the slider then
catches up on the next step.

### 14e. Windows -> buds

Move the slider (Settings > Sound, or the volume flyout). Expected per change:

```
crossc: avrcp: Windows volume ch 0 = -18.00 dB (AVRCP 37), event
crossc: avrcp: volume event generated
crossc: avrcp: property GET -> -18.00 dB (AVRCP 37) [pid N]
```

The buds get louder or quieter. Slider 50 % should be about AVRCP 64
(-10.2 dB; 63 or 65 if audiosrv writes on the 0.5 dB grid, which the
values above assume), 100 % AVRCP 127 (0 dB), 0 % AVRCP 0. In general
slider p reaches the buds as `round(127 p)`, or one or two away from it.
When you stop moving the slider the lines stop within a second: no
ping-pong between the slider and the buds, and the slider stays where you
left it.

Control Panel > Sound > Headphones > Properties > Levels shows the hardware
range: right-click the value to switch it to dB; the slider runs from
-63.5 dB to 0 dB.

### 14f. Mute

Mute in the volume flyout. Expected: `Windows mute ch ... = true, event`, then
`property GET -> -63.50 dB (AVRCP 0)`. No sound (the service silences the
stream as well). Unmute: `= false, event`, a `property GET` with the old
level, sound at the old level. Mute again, then press volume up on the buds:
it unmutes (`mute cleared` in the SET line) and the flyout shows unmuted.

### 14g. AVRCP still works

Play / pause / next track from the buds still work, and

```powershell
sc query BthAvctpSvc        # STATE: RUNNING
```

### 14h. Software fallback (optional, admin)

Turn absolute volume off in Windows' own AVRCP stack and reconnect the buds:

```powershell
reg add "HKLM\SYSTEM\CurrentControlSet\Control\Bluetooth\Audio\AVRCP\CT" /v DisableAbsoluteVolume /t REG_DWORD /d 1 /f
```

Expected after the reconnect: no `volume event enabled`; the service logs
`volume: software gain (hardware volume without an AVRCP subscriber); level
...`; the slider changes the loudness (smoothly, no clicks) and the buds' own
volume is separate again. Put the value back afterwards (it was 0 on this
machine):

```powershell
reg add "HKLM\SYSTEM\CurrentControlSet\Control\Bluetooth\Audio\AVRCP\CT" /v DisableAbsoluteVolume /t REG_DWORD /d 0 /f
```

### 14i. Reconnect, sleep, phone, second buds

* **Buds into the case, out of range, or taken by the phone:**
  `avrcp: volume event disabled (no AVRCP subscriber)` when BthAvrcp lets go
  (the service then logs `software gain`, harmless while the link is down).
  On the next connect 14c repeats (method 6, enable, SET).
* **Connect while muted:** mute Windows, buds into the case and out again.
  Windows must stay muted and the buds silent (each connect SET line says
  `mute on, event true`, followed by a GET of AVRCP 0). Unmute: the buds play
  at the level they had before. Exception: if the buds' INTERIM answer was
  taken after they had already applied the mute (it reads 0), or their level
  was 0 or 1 and the INTERIM arrives after the mute went out, BthAvrcp drops
  it (same whole percent as the 0 it just sent) and the level after unmute
  is its default, about 50 % (AVRCP 64). Note which one you got.
* **Sleep and resume:** after resume and reconnect, the same sequence as 14c.
  Level and mute survive (they belong to the device, not to the circuit).
* **A second pair of buds** with crossc on its node: each node has its own
  circuit, interface and state; the lines of both appear, each after its
  own reconnect.
* A subscriber that stops answering (an event without a GET for 2 s when the
  next change comes) is treated as gone: `crossc-cli info` then shows
  `AVRCP subscriber: yes` together with `software gain`, and the service
  applies the slider until the subscriber reads again.

### 14j. Kill switch: `DisableAvrcpVolume` (admin)

Same two keys as `DisableAcx` (10d). Per device:

```powershell
reg add "HKLM\SYSTEM\CurrentControlSet\Enum\$node\Device Parameters" /v DisableAvrcpVolume /t REG_DWORD /d 1 /f
pnputil /restart-device "$node"
```

Whole driver:

```powershell
reg add "HKLM\SYSTEM\CurrentControlSet\Services\crossc_bth\Parameters" /v DisableAvrcpVolume /t REG_DWORD /d 1 /f
pnputil /restart-device "$node"
```

Expected: `avrcp: DisableAvrcpVolume set in the device key - unified volume
off for this device` (or `... in the service Parameters key ...`),
`acx: device initialized (unified volume off)`, no `avrcp: circuit has the
volume contract` line, the `\Headphones0` entry of 14b gone or Disabled,
`crossc-cli info` showing `volume     no hardware volume (Windows scales the
PCM)`, and the service logging `volume: Windows applies the volume`.
Everything else is exactly 0.1.1. Delete the value
(`reg delete "<key>" /v DisableAvrcpVolume /f`) and restart the node to turn
it back on.

### 14k. Rollback

1. The kill switch (14j): the endpoint and the stream stay.
2. If the driver misbehaves with the switch set too:
   `.\tools\install-driver.ps1 -Uninstall` (back to BthA2dp), or the
   known-good package and the recovery of 10f.

### 14l. What to send

The DebugView log from node start through 14c-14g, `service.log`, the output
of `& $cli info`, and the `pnputil` output of 14b.

## 15. Adaptive bitrate live test

**What this build adds.** The service no longer sends one fixed bitrate. It
starts at the configured rate and steps down a ladder while the Bluetooth
link is congested, then back up when the link is clean again:

| Mode | Rates, kbps | Packets |
|---|---|---|
| HiFi 48 kHz/24 | 229 / 192 / 128 / 88 | 588 / 496 / 336 / 236 bytes, one every 18 ms |
| UHQ 96 kHz/24 | 584 / 291 / 152 | 672 bytes every 9 ms, then two frames per packet: 672 / 360 bytes every 18 ms |
| plain SSC (older buds) | 229 / 128 / 88 | 588 / 336 / 236 bytes |

HiFi starts at 229 kbps: Buds4 Pro and Buds3 Pro were measured to decode
both 229 and 192 kbps in HiFi (config `0x0C`, 48 kHz). 192 kbps, the top
before this change and the highest rate the phones send, is now the first
step down; `HifiBitrate = 192` gives the old ladder, 192 / 128 / 88.

The rate is in every SSC frame, so a change needs no reconfiguration and
the stream does not stop. Design, signals and constants: `docs/DESIGN.md`,
"Adaptive bitrate". **Only the service and `crossc-cli` changed. The driver
is not touched: keep the 0.2.0 driver that is installed.**

**Not tested live.** The controller and the session pass their tests against
a simulated link (a fake driver whose WRITEs block when the link carries less
than the stream). Three things only real buds can show, and this section is
there to find them out:

1. Whether a congested link shows up as slow WRITEs at all. The controller's
   one congestion signal is the time a media WRITE takes; the assumption is
   that the Bluetooth stack holds a WRITE while the radio has no buffer free.
   If it does not, no rate change is ever made (one rate, as 0.2.0 sent)
   and 15d says how to tell.
2. Whether the buds take two frames per packet at UHQ 291 / 152 kbps from
   this stack, and switch rates without a click.
3. Whether the thresholds fit: no step down on a good link, a step down
   before the audio breaks up on a bad one.
4. Whether the service can tell catching up from congestion. After one
   stalled WRITE, or at START, it sends what has piled up back to back;
   WRITEs that then take up to a packet time are the link doing what it
   can and must not step the rate down. It recognises that by not having
   had to wait for the audio (less than 1/8 of a packet time, 1.1 ms in
   UHQ). 15b and 15d say what to look for.

Settings, replacing the table in 11d (`HKLM\SOFTWARE\crossc`, read when the
service starts):

| Value | Type | Default | Meaning |
|---|---|---|---|
| `AdaptiveBitrate` | DWORD | `1` | `1`: the bitrate follows the link. `0`: one fixed rate, the configured one (after the two rules below). |
| `HifiBitrate` | DWORD | `229` | Highest HiFi rate in kbps: 88, 96, 128, 192 or 229. The ladder is the rates of 229 / 192 / 128 / 88 that lie below it. 256 and 328 are no longer sent (the buds' decoder takes 229 at most at 48 kHz): they give 229. |
| `UhqBitrate` | DWORD | `584` | Highest UHQ rate in kbps: 152, 291 or 584. 250, 308 and 442 are no longer sent (not known to be decoded): they give the next rate below, 152 or 291. |
| `UhqConfig`, `LogLevel` | | | as in 11d |

A rate that is lowered this way is named in a warning in the log
(`... is not a rate the buds are known to decode; using 291 kbps, the next
one below`). Anything else that is not valid is ignored with a warning, and
the default is used, as before. The same list of rates is used when a
packet does not fit the media MTU: UHQ then falls back from 584 to 291 kbps
(0.2.0 took 442). Both rules apply with `AdaptiveBitrate = 0` too.

### 15a. Install the service (admin)

```powershell
cd C:\src\crossc
cargo build --release -p crossc-service -p crossc-cli
$svc = "C:\src\crossc\target\release\crossc-service.exe"
$cli = "C:\src\crossc\target\release\crossc-cli.exe"
& $svc uninstall; & $svc install --start
Get-Content C:\ProgramData\crossc\service.log -Tail 40 -Wait
```

Take the buds out of the case. New lines at start and at link up:

```
... INFO  crossc-service x.y.z starting as a service; settings: UhqConfig=0x0a HifiBitrate=229 UhqBitrate=584 AdaptiveBitrate=1 LogLevel=info
... INFO  [AABBCCDDEEFF] adaptive bitrate: 229 / 192 / 128 / 88 kbps
... INFO  [AABBCCDDEEFF] link up: HiFi 48 kHz/24: config 0x0c, 229 kbps, 1 frame(s)/packet, 588-byte packets, input I24
```

### 15b. Baseline: a good link must stay at the top (user)

Play music for three minutes with the buds in your ears, sitting at the PC.
The statistics line (once a minute) now ends with the bitrate fields:

```
... stats: packets=3333 frames_in=2880000 dropped=0 discontinuities=0 write avg=180us max=950us sink_delay=250.0ms rate=229kbps at[229k=60.0s 192k=0.0s 128k=0.0s 88k=0.0s] rate_changes=0 slow_writes=0 late_max=3ms
```

| Field | Meaning | Expected here |
|---|---|---|
| `rate` | the bitrate at the time of the line | `229kbps` |
| `at[...]` | seconds of audio sent at each rate since the last line | all at the top rate |
| `rate_changes` | changes since the last line | `0` |
| `slow_writes` | WRITEs that took a quarter of a packet's audio time or more (4.5 ms for HiFi, 2.25 ms for UHQ at 584 kbps), whether or not they counted as congestion | `0`, or a handful |
| `late_max` | the furthest the service was behind the endpoint's clock while it rendered (a pause does not count) | a few ms |

No `bitrate` line may appear, also not in the first seconds after playback
starts or resumes, and not after pausing and resuming the player a few
times (pauses under 3 s do not suspend the stream). Note `write avg` and
`max`: they are the healthy figures the rest is compared with. If `slow_writes` is in the dozens
per minute on a link that sounds perfect, the threshold is too tight for
this PC: send the log (15g) and use 15f meanwhile.

Repeat at 96 kHz (Settings > Sound > the buds > Format **24 bit, 96000 Hz**,
as in 12 step 5; pause and resume if the stream does not switch, section 17). Link up then logs
`adaptive bitrate: 584 / 291 (2 frames/packet) / 152 (2 frames/packet) kbps`
and the statistics `rate=584kbps`.

### 15c. Provoke congestion (user)

Keep the log tail of 15a open and music playing. Use something with steady
high-frequency content (cymbals, applause): a lower rate is easiest to hear
there. Try these in order, each for a minute or two, and note the time when
you start and stop:

1. **Walk away.** Leave the room with the buds in, close the door, go on
   until the audio starts to stutter, stay a few steps short of that point
   for a minute, then walk back.
2. **Cover the buds.** Close both hands over your ears, or put the buds into
   a closed fist or a pocket on the far side of your body from the PC. The
   body absorbs 2.4 GHz well; this is a weak link without leaving the desk.
3. **2.4 GHz Wi-Fi load.** Connect the PC to a 2.4 GHz Wi-Fi network (not
   5 or 6 GHz) and run a large download or a speed test while the music
   plays. A USB 3 disk or hub in use next to the Bluetooth adapter does the
   same to it.
4. **All of it at 96 kHz** (UHQ needs three times the air time of HiFi and
   gives way first).

What the log shows when it works, here walking away at 14:02:10 and coming
back at 14:02:40:

```
... 14:02:11.297 INFO  [AABBCCDDEEFF] bitrate 229 -> 192 kbps: link congested (WRITE blocked 123 ms in 5 slow packets); 496-byte packets
... 14:02:11.610 INFO  [AABBCCDDEEFF] bitrate 192 -> 128 kbps: link congested (WRITE blocked 106 ms in 4 slow packets); 336-byte packets
... 14:02:22.590 INFO  [AABBCCDDEEFF] bitrate 128 -> 192 kbps: link clean for 10 s; 496-byte packets
... 14:02:23.261 INFO  [AABBCCDDEEFF] bitrate 192 -> 128 kbps: link congested (WRITE blocked 112 ms in 5 slow packets); 336-byte packets
... 14:02:43.510 INFO  [AABBCCDDEEFF] bitrate 128 -> 192 kbps: link clean for 20 s; 496-byte packets
... 14:02:53.520 INFO  [AABBCCDDEEFF] bitrate 192 -> 229 kbps: link clean for 10 s; 588-byte packets
... 14:03:00.000 INFO  [AABBCCDDEEFF] stats: packets=3333 ... write avg=1742us max=33066us ... rate=229kbps at[229k=17.8s 192k=11.0s 128k=31.2s 88k=0.0s] rate_changes=6 slow_writes=101 late_max=131ms
```

* Down is fast: within about 0.2 s of the WRITEs starting to block, one
  rung at a time, the second no sooner than 0.3 s after the first and only
  if the link is slower than the lower rate as well (WRITEs as long as a
  packet plays).
* Up is slow: one rung after 10 s of clean link. The third and fourth line
  are a probe that failed (192 kbps was still too much); the next try for
  that rate then waits 20 s, then 40, at most 80. Each rate keeps its own
  wait (229 kbps was never refused here, so it follows after 10 s), and a
  wait halves again for every 15 s the stream holds that rate. So a link
  that stays bad settles at a lower rate, with a probe now and then.
* At 96 kHz the first step reads
  `bitrate 584 -> 291 kbps, 2 frame(s)/packet: link congested (...); 672-byte packets`.
* Other reasons that can appear: `audio was dropped while WRITEs blocked`
  (a stall that cost audio), and after
  `link down: media write failed: timed out` the next link says
  `a media write timed out on the last link: starting at 88 kbps`. Any
  other `media write failed` (the buds went into the case or out of range
  for good) leaves the next link at the top rate.

Listen for: fewer and shorter dropouts at the edge of the range than with
`AdaptiveBitrate = 0` (15f; do the same walk both ways), no click or gap at
the moment of a `bitrate` line, and at 96 kHz normal sound after the step to
two frames per packet.

### 15d. Reading the result

| What you see | What it means |
|---|---|
| `bitrate` lines while the link is bad, the rate back at the top some 10 to 30 s after it is good again | It works. |
| Dropouts, but no `bitrate` line, `slow_writes=0` and `write max` as in 15b | The Bluetooth stack does not hold WRITEs when the link is congested: the signal is not there, the service behaves like 0.2.0, and a driver change is needed to see the link. Send the log. |
| Dropouts, `slow_writes` well above 0, but no or late `bitrate` lines | WRITEs do block, the thresholds are too lax for this link. Send the log; the `stats` lines have what is needed. |
| `bitrate` lines on a link that sounds perfect, or the rate going up and down every few seconds | Thresholds too tight, or something else on the PC stalls the WRITEs. Send the log, use 15f. |
| A `bitrate ... link congested` line within a second or two after playback starts, or right after a single hiccup, on a link that is otherwise fine | Catching up was taken for congestion (point 4 above): reading and encoding a packet takes longer on this PC than assumed. Send the log with `LogLevel` set to `debug`. |
| The rate goes down one rung and never comes back although the link sounds perfect, `slow_writes` in the hundreds per minute and `write avg` of several ms | WRITEs are slow for a reason that does not depend on the bitrate. The lower rate buys nothing here: use 15f and send the log. |
| At 96 kHz the audio is garbled, silent or stutters right after `584 -> 291 kbps, 2 frame(s)/packet`, and is fine again after `291 -> 584` | The buds do not take two frames per packet from this stack. Set `AdaptiveBitrate` to `0` (15f) and report it. |
| A click or a short gap exactly at a `bitrate` line | The buds do not switch rates seamlessly. Report at which step (which two rates). |
| `link down: media write failed` | The WRITE itself failed (5 s without completing, or the link or channel is gone). The link is rebuilt as in 0.2.0; the new one starts at the lowest rate only after `timed out`. |

### 15e. The same controller in `crossc-cli play` (admin)

For a controlled signal instead of music. Stop the service first (one
source at a time):

```powershell
Stop-Service crossc
& $cli play --adaptive --seconds 120                 # HiFi, 192 / 128 / 88
& $cli play --mode uhq --adaptive --seconds 120      # UHQ, 584 / 291 / 152
& $cli play --bitrate 192 --seconds 120              # fixed rate, for comparison
Start-Service crossc
```

`--adaptive` prints the ladder at the start, every change as it happens and a
summary at the end:

```
adaptive bitrate: 192 / 128 / 88 kbps (down while WRITEs block, up after 10 s of clean link; changes are printed)
[  41.310] bitrate 192 -> 128 kbps: link congested (WRITE blocked 118 ms in 6 slow packets); 336-byte packets
[  52.140] bitrate 128 -> 192 kbps: link clean for 10 s; 496-byte packets
bitrate: 2 change(s); audio sent at 192k=109.2s 128k=10.8s 88k=0.0s
```

Differences from the service: `play` still takes 192 kbps as its HiFi rate
when `--bitrate` is not given, so its HiFi ladder is the one under 192, not
the service's 229 / 192 / 128 / 88; it keeps one frame per packet at every
rate (also in UHQ, unless `--frames-per-packet` says otherwise), and the few
packets it has already encoded still go out at the old rate. `--bitrate`
and `--adaptive` exclude each other: `--bitrate` is exactly that rate.
`-v` adds `rate=` to its statistics lines.

### 15f. Turn it off, or lower the top (admin)

```powershell
reg add "HKLM\SOFTWARE\crossc" /v AdaptiveBitrate /t REG_DWORD /d 0 /f
Restart-Service crossc
```

The log then has `AdaptiveBitrate=0` in the settings line and, at link up,
`adaptive bitrate is off (AdaptiveBitrate=0): always 229 kbps`: one fixed
rate, as 0.2.0 sent. The stream is 0.2.0's byte for byte, with three
exceptions that do not depend on this switch (see the settings table
above): HiFi is sent at 229 kbps unless `HifiBitrate` says otherwise (0.2.0
sent 192; set `HifiBitrate` to 192 for that), a `HifiBitrate` or
`UhqBitrate` the buds are not known to decode is lowered, and on a media
MTU too small for 584 kbps UHQ falls back to 291 instead of 442 kbps. Back
on:

```powershell
reg delete "HKLM\SOFTWARE\crossc" /v AdaptiveBitrate /f
Restart-Service crossc
```

To keep it on but start lower (for a place where the top rate never holds),
set the top of the ladder instead, for example HiFi 128 / 88 (or 192 for
192 / 128 / 88):

```powershell
reg add "HKLM\SOFTWARE\crossc" /v HifiBitrate /t REG_DWORD /d 128 /f
Restart-Service crossc
```

### 15g. What to send

`C:\ProgramData\crossc\service.log` covering 15b and 15c, with the times you
started and stopped each of the four ways of spoiling the link, what you
heard at each (`bitrate` line or not), the buds' model, and whether the PC's
Bluetooth adapter is built in or a USB dongle. For 15e the console output.

## 16. Tray app

`crossc-tray.exe` is an icon in the notification area that shows what the
service is sending to the buds, with a details window. It runs as the
logged-on user, not elevated, and only reads: everything it shows comes from
the service over the named pipe `\\.\pipe\crossc-status` (the driver's IOCTLs
are admin-only, so the tray never talks to the driver). It changes nothing
about the stream.

The tray believes that pipe only if it is the service's: owned by SYSTEM or
by Administrators, which is what a pipe created by the service (or by
`crossc-service run` in an elevated console) is, and what no program of a
plain user can create. While the service is not running any program could
create a pipe of that name; the tray then shows "Status not trusted" instead
of what that program says.

> The service publishes its status from the version that includes the status
> pipe. With an older service (0.2.0 and before) the tray runs and says
> "Service not running". The service's side has been tested against a fake
> driver and a pipe of the test's own; the installed service with real buds
> has not been looked at through the tray yet: 16f is that first check.

The installer's Install also adds a Start menu entry for all users,
"crossc tray" in a `crossc` folder, to open the installed tray; Uninstall
removes it.

### 16a. Run it

```powershell
cd $repo
cargo build --release -p crossc-tray
& .\target\release\crossc-tray.exe                 # the icon; starting it twice does nothing
& .\target\release\crossc-tray.exe --print-once    # one status as text, then exit (0: got one)
& .\target\release\crossc-tray.exe --demo          # made-up states, no service needed
```

While a tray runs from `target\release`, cargo cannot rebuild it (Windows does
not let a running program be replaced: `failed to remove file ...
crossc-tray.exe`, access denied). Exit it from its menu first. The installer's
Build builds the tray in a step of its own for that reason: with a tray
running from the checkout that step is a warning, and the service and the CLI
are built all the same.

`--demo` runs a script (no service, waiting, HiFi, UHQ, adaptive step down,
reconnecting, two pairs of buds, version mismatch) so the icon and the window
can be looked at without buds. `--demo-state <name>` holds one of `off`,
`waiting`, `hifi`, `uhq`, `adaptive`, `reconnect`, `two`, `mismatch`,
`nobuds`, `untrusted`.

`--pipe \\.\pipe\<name>` reads another pipe than the service's (for trying the
tray against `cargo run -p crossc-status --example serve`); such a pipe may be
your own.

### 16b. The icon

Three ascending bars, a quality meter. New icons start in the overflow area
(the `^` next to the clock); drag it onto the taskbar to keep it visible.

| Icon | Meaning |
|---|---|
| bars dim, red cross | the service is not running (or has no status pipe) |
| bars dim, amber dot | a problem that waiting does not fix: service and tray are of different releases, access denied, unreadable status, a status pipe that is not the service's |
| bars dim | the service runs, no stream is open: buds not connected, driver not ready, reconnecting |
| 1 / 2 / 3 bars lit in the taskbar's text colour | a stream is open and idle: plain SSC / Seamless HiFi / UHQ |
| 1 / 2 / 3 bars lit green | audio is streaming in that mode |

With two pairs of buds the one that plays gives the icon. The tooltip has one
line per pair with mode, sample rate and depth, bitrate and media MTU limit,
for example `Buds4 Pro — UHQ 96 kHz/24 · 584 kbps · MTU 1005`, or says
what is wrong (`crossc — service not running`). The icon follows the taskbar's
light or dark mode and its DPI, and comes back by itself after Explorer
restarts.

### 16c. The details window

Left click (or Enter on the icon, or "Open details"): a window next to the
notification area, one card per pair of buds, updated live (twice a second
while audio streams). It closes when you click elsewhere, press Esc, or click
the icon again. It has no taskbar button.

| Field | Meaning |
|---|---|
| Title, badge | the buds' Bluetooth name (their address-based id if the name is unknown) and the link state: waiting for the driver / buds not connected / ready / streaming / reconnecting |
| Line under the title | with a stream: the mode by its full name and the bit depth, e.g. `UHQ (Ultra High Quality) · 24-bit`. Without one: what is being waited for |
| The four boxes | the stream at a glance: **Mode** (UHQ, HiFi = Seamless HiFi, SSC = plain 16-bit), **Sample rate** of the audio as sent, **Bitrate** in use now (amber while stepped down), **Media MTU** in bytes: the largest media packet this link takes. Only while a stream is open |
| Windows format | the format of the Windows endpoint: what you chose under Sound > Properties > Format. "nothing played yet" until the first stream |
| Sent to the buds | rate and depth of the audio as sent. Underneath, if they differ from the Windows format: "resampled from ...", "16-bit input widened", "24-bit input dithered down" |
| Bitrate | the bitrate in use now. Underneath: `fixed` (`AdaptiveBitrate=0`), `adaptive · ladder 584 / 291 / 152` (at the top; these are the steps), or in amber `stepped down · step 2 of 3 · top 584` while the link carries less |
| Delay | the playback delay the buds reported for the current stream (AVDTP delay report); "not reported" if they have not, also after a reconnect or a switch of mode until they report again |
| Volume | the level in percent, who applies the Windows slider, and "muted". `set on the buds`: the buds themselves (hardware volume over AVRCP). `applied by the service`: the service scales the audio because the buds did not take the volume. `applied by Windows`: Windows scales the audio (`DisableAvrcpVolume`, or an older driver) |
| State | only when there is a reason to give: the state again, with the reason underneath (what the driver lacks, why the link went down, when the next attempt is). No reason is given while the buds are simply not connected |
| Packet | size of a media packet in bytes and the SSC frames in it |
| Media MTU | the media MTU limit in use, in bytes ("Copy details" also has how the limit was derived) |
| Codec options | two octets in hex, `0x0a of 0x3e`: the SSC configuration octet sent to the buds, and the SSC capability octet the buds offered (`0x3e` on Buds4 Pro) |
| Driver | version of `crossc_bth`; underneath the version of the IOCTL interface it speaks |
| Activity | shown once the device has had a stream (or a counter is not zero) |
| Packets sent, Frames read | media packets sent, and PCM frames taken from the endpoint, since the service started this device's session |
| Dropped frames | PCM frames the driver dropped from a running stream because the service did not read them in time, since the service started this device's session. Should stay 0. What the driver discards while no stream is started (the buds away, a stream the buds suspended) is not counted. The service asks the driver when a stream stops, when the endpoint's state changes and once a minute while a stream runs, so the number can be a minute old |
| Stream gaps | discontinuities in the PCM the driver reported (a stream that stopped and started) |
| Write errors | media writes that failed (each one takes the link down) |
| Late packets | packets sent later than their schedule allows (40 ms or more behind the endpoint) |
| Write latency | average and longest time a media write took in the current stream; shown while audio streams. Normally well under a millisecond; tens of milliseconds mean the link is congested |
| Streaming for | how long the current stream has been running |
| Reconnects | how often the link was set up again after a failure |
| Amber / red line | the last warning or error of this device from the service's log, and how long ago. Left out when it only repeats the reason under State ("Copy details" always has it) |
| Amber / red line of the window | the service's own last warning or error, and how long ago ("Copy details": `last service warning`). That includes one about a device that has no card to show it on: `[<id>] the session thread panicked; it will be restarted`, `[<id>] cannot start a session thread` |
| Footer | the service's version and how long it has been running |

With two pairs of buds the cards stand side by side, equally high.

When there is no card the window says why: service not running (open the
crossc installer and choose Install), version mismatch (use the
`crossc-tray.exe` that came with the service), access denied, status not
trusted (another program holds the pipe's name; the service itself is probably
not running), or no buds with the crossc driver present.

### 16d. The menu

Right click:

* **Open details**: the window of 16c.
* **Copy details**: everything of 16c as text on the clipboard (plus the
  bitrate ladder, how the MTU limit was derived, the two SSC octets by name and
  the status protocol version). Paste it into a report.
* **Open service log folder**: `%ProgramData%\crossc` in Explorer (the folder
  the service names; only a plain local folder is ever opened).
* **Start with Windows**: toggles the value `crossc-tray` under
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, pointing at this exe.
  Per user, no elevation, nothing under HKLM. Ticked when the value points at
  the exe that is running; if you move the exe, tick it again. To check or
  remove it by hand:

  ```powershell
  reg query  "HKCU\Software\Microsoft\Windows\CurrentVersion\Run" /v crossc-tray
  reg delete "HKCU\Software\Microsoft\Windows\CurrentVersion\Run" /v crossc-tray /f
  ```

* **Exit**: removes the icon and ends the tray (the service and the stream
  are not touched).

### 16e. What to expect

* Idle, the tray is one small Win32 process (about 2 MB private memory) that
  wakes when the service sends a status: every 2 s when nothing changes,
  every 500 ms while audio streams, at once when the state changes. Without
  the service it looks for the pipe every 5 s.
* The details window is a second process of the same exe (`--flyout`) that
  exists only while the window is open (about 65 MB working set).
* The service serves at most 8 status readers at once, 4 per Windows session
  (the tray is one, its open details window a second). One more is turned
  away and tries again; the window then says "Service busy" or
  "Reconnecting" for a moment.
* Stop the service (`sc stop crossc`, admin): within a second the icon gets
  the red cross and the tooltip says `crossc — service not running`. Start it
  again: the icon follows within 5 s, without restarting the tray.

### 16f. First run against the installed service (not verified yet)

With the service of this build installed and running (15a), look at its log
first. Shortly after the start it has one of:

```
... INFO  status pipe \\.\pipe\crossc-status is up
... WARN  the status pipe \\.\pipe\crossc-status is not owned by SYSTEM or Administrators: the tray app will not trust it ("Status not trusted")
... WARN  status pipe \\.\pipe\crossc-status cannot be created (another process holds that name); user interfaces get no status. Retrying every 5 s
... WARN  a pipe named \\.\pipe\crossc-status exists that is not this service's: another program created it before the service could. ...
```

The first is what is expected: the service has opened its own pipe the way
the tray does, owner check included. The second is about the service's own
pipe. The last two come together and are about somebody else's: a program
created a pipe of that name while the service was not running (during an
update, say), and the service cannot have the name until that program is
closed. It retries every 5 s and logs `status pipe ... is available now` when
it has it. Then, from a normal (not elevated) PowerShell:

```powershell
& .\target\release\crossc-tray.exe --print-once ; $LASTEXITCODE
```

It must print one block per pair of buds (state, mode, bitrate, counters) and
exit with 0. Compare the values with `service.log`: the mode and bitrate of
`link up`, the ladder of `adaptive bitrate:`, the media MTU limit of
`media channel open`, and after a `bitrate ... kbps` line the new rate.

| What it prints | What it means |
|---|---|
| The devices | It works. |
| Service not running | The pipe does not exist: the service is an older one or is stopped, or it logged `status pipe ... cannot be created` with another reason than a held name. |
| Status not trusted | The pipe's owner is neither SYSTEM nor Administrators. Look at the service's log to see which of two things it is. **`a pipe named ... exists that is not this service's`** (with `cannot be created (another process holds that name)`): another program has the name, and the tray is looking at that program's pipe. Find and close that program (`Get-ChildItem \\.\pipe\ | Where-Object Name -eq crossc-status` shows that the pipe exists; a reboot clears it if the program cannot be found); the service takes the name within 5 s and logs `status pipe ... is available now`. Nothing in crossc needs changing. **`the status pipe ... is not owned by SYSTEM or Administrators`**: the service's own pipe has the wrong owner. Not expected of a LocalSystem service (what LocalSystem creates is owned by Administrators); if it happens, report it: the fix is to name the owner in the pipe's security descriptor (`SERVICE_SDDL` in `crates/crossc-status/src/pipe.rs`, described in `crates/crossc-status/INTEGRATION.md` 2b). |
| Access denied | The pipe's access list did not let this user in. Not expected of the installed service; expected of `crossc-service run` in an elevated console when the tray is not elevated (11a). |

Audio does not depend on any of this: a service without a working status
pipe streams exactly as before, and says so once in its log.

---

## 17. Choosing the format: HiFi or UHQ

The format picked for the buds in **Settings > System > Sound > the buds >
Output settings > Format** decides the rate Windows plays at, and with it
the mode sent to the buds:

| Format in the sound settings | Windows plays at | Sent to the buds (if they offer it) |
|---|---|---|
| **24 bit, 96000 Hz** | 96 kHz / 24 | UHQ |
| **24 bit, 48000 Hz** | 48 kHz / 24 | Seamless HiFi |
| **16 bit, 48000 Hz** | 48 kHz / 16 | Seamless HiFi (16-bit widened) |

Why the service is involved at all: Windows does not open the endpoint's
stream in the format picked there. It asks the endpoint for its *default*
format and uses that (`docs/DESIGN.md`, "Format selection"). The service
therefore watches the format picked in the sound settings and makes it the
endpoint's default (driver protocol 4; an older driver always plays at
48 kHz and the service says so once in its log).

To pick UHQ: choose **24 bit, 96000 Hz**. The service log then has

```
... INFO  [AABBCCDDEEFF] Windows device format for AABBCCDDEEFF is 96000 Hz/24: driver default set
```

and DebugView `crossc: acx: default format 96000 Hz, 24-bit (lists updated; stored 0x0)`.
The next stream Windows opens is at 96 kHz: the service reconfigures to UHQ
(`configuring SEP ... for endpoint format 96000 Hz, ...: UHQ ...`).

**If it does not switch** (the music keeps playing at 48 kHz), pause and
resume playback. A stream that is already open keeps its format; whether
Windows reopens it by itself on the change has not been seen yet. The
service warns when this is the case:

```
... WARN  [AABBCCDDEEFF] Windows still renders at 48000 Hz/24 although the driver default is now 96000 Hz/24: the change applies when playback restarts (pause and resume playback)
```

If even a new stream comes at 48 kHz, wait until Windows closes the stream
(a few seconds of silence; DebugView `stream stopped`) and play again. If it
still does not switch, report it with the service log and the DebugView
trace.

The choice is also stored in the buds' device key (`DefaultSampleRate`,
`DefaultBits` under `HKLM\SYSTEM\CurrentControlSet\Enum\<instance>\Device
Parameters`), so the endpoint starts with it after a reboot or a reconnect,
before the service runs. The service needs no setting for any of this.

### 17a. Live test (user, service of this build installed)

1. Pick **24 bit, 48000 Hz**, play music. Expected: HiFi, as before. The log
   has `Windows device format for ... is 48000 Hz/24: driver default already
   set` (or `... set`, the first time).
2. Pick **24 bit, 96000 Hz** while the music plays. Note whether the stream
   switches by itself (DebugView: a new `stream created: RAW mode, 96000 Hz`;
   service: `UHQ`), or only after pause and resume, or the warning above
   appears. All three are expected outcomes for now; which one it is, is
   what this test is for.
3. Pick **16 bit, 48000 Hz**, then **24 bit, 48000 Hz** again; each time a
   `driver default set` line, and after a pause and resume the stream in
   that format.
4. Reboot (or disconnect and reconnect the buds) with 96000 Hz picked, and
   play before anything else: the first stream should be at 96 kHz.
5. `reg query "HKLM\SYSTEM\CurrentControlSet\Enum\<instance>\Device Parameters" /v DefaultSampleRate`
   (admin) shows the last choice.

Send: the service log lines of each step, and the DebugView lines with
`default format` and `stream created`.
