# crossc-installer

A desktop window (GPUI, Zed's UI framework) that installs and uninstalls crossc: the `crossc_bth` driver on
every paired pair of supported Galaxy Buds, and the `crossc` Windows service that streams to them. It checks that
the driver bound, and rolls everything back to Windows' own `BthA2dp`. In every mode it can also look up the
latest release on GitHub, download it, verify it and install it (see
[Installing the latest release from GitHub](#installing-the-latest-release-from-github)).

It runs in one of three modes, decided at start from where the exe is:

| Mode | When | Package it installs | What it offers |
|---|---|---|---|
| **bundle** | `<exe folder>\driver\crossc_bth.inf` exists (an unpacked release zip) | `<exe folder>\driver\` | Install, Uninstall, Remove certificate |
| **repo** | otherwise: the exe is inside a checkout (or was compiled in one that still exists) | `drivers\crossc-bth\target\<debug or release>\crossc_bth_package` | the same, plus Build, Build + Install and the Debug/Release choice |
| **online** | neither: the exe was copied or downloaded on its own | the release it downloaded and installed, `%ProgramFiles%\crossc\releases\<tag>\driver\`, once there is one | Download and install (the main path), Install (again, from that release), Uninstall, Remove certificate |

All three also have **Check for update** and, when the check finds a release newer than what is here,
**Download and install \<tag\>**.

A release zip holds `crossc-installer.exe`, `crossc-service.exe`, `crossc-cli.exe` and
`driver\{crossc_bth.sys, crossc_bth.inf, crossc_bth.cat, WDRLocalTestCert.cer}` side by side. Bundle mode needs no
cargo and no repo; the header shows `bundle <version>` (the version in the INF's `DriverVer`).

Online mode is what used to be reported as an "incomplete bundle": an exe that is neither next to a `driver\`
folder nor in a checkout. It has no package of its own, and says so ("this installer runs on its own"); the footer
leads through Check for update, Restart as administrator, Download and install. After that the release it
installed is its bundle: the status shows it like a bundle's, and Install reinstalls it. Only a release that went
through the confirmation counts: the folder must be the one the install record names, and its certificate must
still be trusted. So plain Install in online mode never trusts a certificate; that only happens through the
confirmation. A download that was cancelled is not the recorded release, and after "Remove certificate" the
release has to be downloaded and confirmed again.

In repo mode it does what `tools\install-driver.ps1` does for the driver (the script stays in the repo; both use
the same pnputil commands and read exit codes the same way; the installer stages the new package before it removes
the older ones, see [Actions](#actions)):

```powershell
cargo run -p crossc-installer --release            # window, debug driver flavour selected
cargo run -p crossc-installer --release -- --release   # window, release flavour selected
```

## Which devices it touches

The INF lists one model line per supported Galaxy Buds model
(`BTHENUM\{0000110b-...}_VID&0001xxxx_PID&yyyy`). The installer reads **all** of them and finds **every** A2DP
node whose hardware IDs contain one of them, exactly: none, one or several paired buds. Of a node that matches
no INF ID only the hardware IDs and the driver service name are read, and nothing is ever done to a node
directly: the driver is staged and removed with `pnputil`, which binds it only where the INF matches. The generic
A2DP compatible ID is never accepted as an ID (comment lines are skipped, and an ID needs the `_VID&..._PID&...`
suffix).

Install works with no supported buds paired: the package is staged and the certificate trusted, and buds pick the
driver up when they are paired.

A node that already runs `crossc_bth` without being a model of this package (it was bound by another version of
the package) is not one Install can put the driver on, but it is affected: Install removes the staged package it
runs, and so does Uninstall. Such nodes get their own amber **Buds** row, Install warns about each one and
rescans devices afterwards so Windows gives them its own driver back, and Uninstall verifies them like the
others. The same lookup lets Uninstall verify the nodes when there is no INF to read at all.

## Installing the latest release from GitHub

Two clicks, with the facts to check between them:

1. **Check for update** (works unelevated, changes nothing) asks
   `https://api.github.com/repos/<owner>/<name>/releases?per_page=20` and fills the **Latest release** row:
   unknown, checking, up to date, update available (with the installed version), no release yet, or why the check
   failed. Nothing is checked on its own: the row stays "unknown" until the button is pressed. (The one exception:
   "Restart as administrator" repeats a check that was done in the unelevated window, so the elevated one does
   not start blank.)
2. **Download and install \<tag\>** (administrator; shown when the release is newer than what is installed and
   than the bundle next to the installer) downloads, verifies and unpacks the release. Nothing is installed yet.
3. A panel then shows what was downloaded: release tag and repository, asset name, its SHA-256, the subject and
   SHA-1 thumbprint of the certificate that would be trusted, where it was unpacked, and the statement that this
   installs a test-signed kernel driver downloaded from GitHub. It is part of the window, not a dialog.
   **Trust certificate and install \<tag\>** is the second click; **Cancel** deletes what was unpacked.
4. The install is the bundle-mode Install on the unpacked folder (driver, service, CLI). Afterwards older
   downloads are deleted, and if the release's `crossc-installer.exe` is newer than the running one the log says
   where it is (the installer does not replace itself).

### Which release

* Repository: `mrquantumoff/crossc`; the environment variable `CROSSC_REPO=owner/name` names another (a fork).
* Drafts never count. Of the published releases the one with the highest version is taken (semantic versioning
  order of the tags `vMAJOR.MINOR.PATCH[-suffix]`; other tags are ignored).
* Prereleases: a release is a prerelease when its tag has a `-suffix` (`v0.3.0-rc.1`). GitHub's own "prerelease"
  flag is not used, because the release workflow sets it on every 0.x release. Prereleases are only considered
  when no other release exists, or when the installed version is a prerelease itself.
* The release must have the assets `crossc-<version>-x64.zip` and `SHA256SUMS.txt`, by exactly these names. If
  the newest release lacks one, the row says so and nothing is offered (an older release is not picked instead).
* A tag counts only in its one canonical spelling: no leading zeros, no empty part (`v0.2.0-rc.1`, not
  `v0.2.0-rc.01`, `v01.2.3`, `v0.2.0-rc..1` or `v0.2.0-`). The release workflow's tag rule
  (`^v\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$`) is looser and lets those through. A published release with such a tag is
  not offered, and the Latest release row and the log say so ("ignored v0.2.0-rc.01: not a release tag ..."), so
  it is not passed over in silence. Tags that are no attempt at a version (`nightly`) are ignored without a word.

### Token

Needed only while the repository is private (and to get past the anonymous rate limit of 60 requests an hour):
`GITHUB_TOKEN`, then `GH_TOKEN`, then the output of `gh auth token` if `gh.exe` is in a folder on `PATH` (started
without a window, given 5 s), else anonymous. A private repository asked anonymously answers 404; the row then
says how to provide a token. The token is only ever sent to `api.github.com`, in the `Authorization` header, and
is never written to the log: the log says where it came from, nothing more. In an elevated window `gh.exe` is
only run when it really is below Program Files (either one, as the system reports them), so a `gh.exe` planted in
a user-writable folder on `PATH` is never run as administrator. "Really" means the path is resolved by the file
system first: `C:\Program Files\..\Users\x\gh.exe` and a junction below Program Files that leads elsewhere do not
pass, and what is run is the resolved path. The Windows folder does not count, because parts of it (`Tasks`,
`tracing`, `System32\spool\drivers\color`) take new files from ordinary users. A `gh.exe` anywhere else is passed
over and the log says so; set `GITHUB_TOKEN` then.

### Download and verification

* HTTPS only, through WinHTTP (the system's TLS and certificate store, TLS 1.2 or newer, the system's proxy
  settings). Each step of a request has its own timeout: 10 s for the check, 30 s for downloads.
* Hosts: `api.github.com`, `github.com`, `release-assets.githubusercontent.com` and
  `objects.githubusercontent.com`, on port 443. Automatic redirects are off; the installer follows a redirect
  itself, at most 5 times, and only to an HTTPS URL on that list. Assets are requested from the API
  (`.../releases/assets/<id>` with `Accept: application/octet-stream`), which works for public and private
  repositories alike; GitHub answers with a redirect to a signed URL on `release-assets.githubusercontent.com`,
  which is requested without the token. The query of that URL is a signature and is never logged.
* Size caps, enforced on the bytes that actually arrive: release list 4 MB, `SHA256SUMS.txt` 64 KB, zip 200 MB.
* `SHA256SUMS.txt` (`sha256sum` format) must list the zip under its exact file name; a file with any line that
  is not `<64 hex digits>  <name>` is refused. The zip is hashed (SHA-256, Windows BCrypt) while it is written,
  and the result must equal that entry and, when GitHub reports a digest for the asset, that digest too. On any
  mismatch the download is deleted and nothing is unpacked.
* The zip is written to `%ProgramFiles%\crossc\releases\<tag>.download`, a folder only administrators can write
  to, through a handle that denies writing to everyone else; it is unpacked from that same handle, so what is
  unpacked is what was hashed. This is why the download needs administrator rights. "`%ProgramFiles%`" here and
  everywhere below is the Program Files folder as the system reports it (`SHGetKnownFolderPath`), **not** the
  environment variable of that name: the variable is whatever the process that started the installer set, and a
  value pointing at a folder ordinary users can write to would undo the whole argument.
* Unpacking (`zip` crate, stored and deflate only, no decryption): every entry must lie in the one folder
  `crossc-<version>-x64/`, which is dropped. Refused: absolute paths, `..`, backslashes, colons (drives,
  alternate data streams), other characters Windows forbids, non-ASCII, names ending in a dot or space, device
  names (`NUL`, `COM1`, ...), symbolic links, more than 2000 entries, more than 512 MB unpacked (counted while
  unpacking, not taken from the zip's headers). Files are only created, never overwritten. It goes into
  `<tag>.partial`, which becomes `%ProgramFiles%\crossc\releases\<tag>\`; `releases` and its parent must be real
  folders, not links.
* The unpacked release must have the four driver files, an INF with at least one supported model, a complete
  `crossc-service.exe`, and a certificate whose subject is `CN=WDRLocalTestCert`. A certificate with another
  subject is not offered for trusting: nothing in this installer could remove it again.

What this does and does not prove: the zip is the file the release's `SHA256SUMS.txt` describes, it came from
GitHub over TLS, and nothing changed it between the check and the install. The checksum file comes from the same
release as the zip, so this is no protection against someone who can publish a release in the repository. That is
what the confirmation step is for: the certificate's thumbprint can be compared with the one in the release notes
or in `BUILD-INFO.txt`.

### The install record

Install (from a bundle, the repo or a download) writes to `HKLM\SOFTWARE\crossc`:

| Value | Meaning |
|---|---|
| `CertThumbprint` | SHA-1 thumbprint of the certificate of the last install whose driver was verified |
| `PendingCerts` | thumbprints (separated by spaces) of certificates that are trusted without being that one: trusted by an install that did not get as far as a verified driver, or replaced but not removable at the time |
| `Version` | version of the release that was installed (`0.2.0`); absent after an install from the repo |
| `DriverVer` | version number of the driver package it staged (`8.50.53.934`) |

Every release is signed with its own throwaway certificate, so an update trusts a new one. The order is chosen
so that a release that cannot be installed does not take the working install with it:

1. The new certificate's thumbprint is written to `PendingCerts`, then the certificate is trusted. If the record
   cannot be written, nothing is trusted. So a certificate this installer trusted is always named by the
   record, whatever stops the install afterwards.
2. The new package is staged (`pnputil /add-driver`, without `/install`) while the older packages, the earlier
   certificate and the running service are untouched. Whether that worked is read from the driver store, not
   from pnputil's (localised) words. If Windows refuses the package (a catalog that is not signed by the
   certificate in the bundle, a broken INF), the certificate is taken out again, from the stores this run added
   it to only, and Install stops: nothing else has changed.
3. Only then: stop the service, remove the older packages (all but the one just staged), install the new one,
   verify it.
4. Only once the driver is verified: the previously recorded certificate and every pending one other than the
   new one are removed from LocalMachine Root and TrustedPublisher, `CertThumbprint` names the new one,
   `PendingCerts` is emptied, and `Version` and `DriverVer` are written.

If the driver does not verify in step 3, the older packages are gone, but the earlier release's certificate is
still trusted and still the recorded one, `Version` still names the earlier release and its folder in `releases`
has not been pruned: it can be installed again (in online mode with plain Install; the log says where it is).
The new certificate stays in `PendingCerts` and is removed by the next install that verifies.

A removal is by exact thumbprint **and** the subject `CN=WDRLocalTestCert`: no other certificate is ever
touched. One that fails stays in `PendingCerts` and the next Install tries again. Uninstall deletes `Version` and
`DriverVer` and keeps `CertThumbprint` and `PendingCerts` (the certificates are still trusted); "Remove
certificate" deletes both. A certificate trusted by an installer older than this record is only removed by
"Remove certificate", which removes every `CN=WDRLocalTestCert`.

**Installed version**: the recorded `Version`, as long as the recorded `DriverVer` is still the version of a staged
`crossc_bth` package (otherwise something else installed a driver since, and the version is "not known"). The
driver's `DriverVer` itself cannot be compared with a release: it is the build time (stampinf's default), not the
release version, and `crossc-service.exe` carries no version resource.

**Version of a bundle**, in this order: `version.txt` in the bundle folder (first line, `0.2.0` or `v0.2.0`), the
first line of `BUILD-INFO.txt` (`crossc 0.2.0 (x64)`), the folder name (`crossc-0.2.0-x64`, or `v0.2.0` for a
downloaded release). `tools/package.ps1` writes `BUILD-INFO.txt` today; a `version.txt` holding just the version
would be the machine-readable form and is what packaging should add.

## What the window shows

* **Status**: Administrator, Test signing, Secure Boot, Memory integrity, Driver package, Certificate trusted, one
  **Buds** row per paired node (the buds' Bluetooth name, service, problem code, instance id; driver stack, INF and
  version on a second line), Staged packages, **Service** (not installed / installed, stopped / running, and the
  exe it is registered with), **Latest release** (see above). Green means OK, amber means worth a look, red means
  it blocks the install, grey means unknown. Every query runs unelevated and uses Win32 directly:
  * test signing and HVCI: `NtQuerySystemInformation(SystemCodeIntegrityInformation)` (no bcdedit needed)
  * Secure Boot: `HKLM\SYSTEM\CurrentControlSet\Control\SecureBoot\State\UEFISecureBootEnabled`
  * Memory integrity configured: `...\DeviceGuard\Scenarios\HypervisorEnforcedCodeIntegrity\Enabled`
  * buds nodes: cfgmgr32 (`CM_Get_Device_ID_List` on BTHENUM, matched on the hardware IDs from the INF; the name
    comes from the `BTHENUM\DEV_<address>` node of the same buds)
  * certificate: CryptoAPI, read-only LocalMachine `Root` / `TrustedPublisher`, matched by the `.cer`'s SHA-1
  * staged packages: `pnputil /enum-drivers`, parsed like the script does
  * service: the service control manager (`OpenService` with query rights only)
  * install record: `HKLM\SOFTWARE\crossc`, read-only
  * latest release: only when asked (Check for update), over HTTPS
* **Warning** (bundle and online mode, until crossc is installed): what a test-signed kernel driver means, that
  it is experimental, and what Uninstall undoes. In a short window the status gives up its height first, then the
  warning (both scroll), so the log always keeps its minimum height.
* **Confirmation** (in place of the warning, after a download): what was downloaded and which certificate the
  next click trusts. Its two buttons are in the button row, where they stay visible when the panel scrolls.
* **Buttons**: Download and install \<tag\> (when offered; it is then the highlighted one), Install, Uninstall,
  Remove certificate, Check for update, Refresh; in repo mode also Debug/Release, Build and Build + Install. All
  of them are disabled while an action runs. The admin-only actions stay disabled until the
  app is elevated, and they also check elevation themselves before doing anything. "Restart as administrator"
  relaunches the exe with `runas` and closes this window once the elevated one has started. After that the whole
  app is elevated, so a Build run there writes admin-owned files under `target\`: build in a normal window first,
  then Install elevated.
* **Closing while an action runs**: the first close is refused with a warning in the log (stopping Install part
  way can leave a node with no package); closing again quits anyway. Actions cannot be cancelled.
* **Log**: stdout and stderr of every child process (cargo, cargo-make, pnputil, crossc-service), streamed line by
  line, plus the service control steps. Errors are red, warnings amber, success green. It follows new output only
  while scrolled to the bottom; when you have scrolled up, "Jump to end" appears. Copy log / Clear.
* **Next step**: one line worked out from the status. Examples: "Test signing is off: ...", "Ready: click
  Install", "Installed and running: select "Headphones (... Buds ...)" as the output device in Windows sound
  settings", "The driver is installed, but the crossc service is not running ...: click Install to start it",
  "This installer runs on its own, without a driver package next to it: click "Check for update" ...", "Ready:
  click "Download and install v0.2.0"". When a newer release is on offer and nothing is in the way, the line ends
  with "A newer release is available: ...".
  Install is only suggested when its preflight would pass; otherwise the line says what is in the way ("... and
  Install cannot start it yet: crossc-service.exe is missing from the bundle or damaged: ...").

## Actions

| Action | Elevation | Steps |
|---|---|---|
| Build (repo) | no | `cargo make` (or `cargo make default --release`) in `drivers\crossc-bth`, then `cargo build --release -p crossc-cli` (and `-p crossc-service` when the workspace has that crate). Sets `LIBCLANG_PATH` to `%ProgramFiles%\LLVM\bin` if unset |
| Install | yes | preflight (test signing on; stops if it cannot be read, Secure Boot warning, package complete, supported models and paired nodes listed, service exe found and a complete Windows executable), put the `.cer` on record (`PendingCerts`) and trust it in Root + TrustedPublisher if not trusted yet, `pnputil /add-driver crossc_bth.inf` to stage the new package (refused by Windows: the certificate is taken out again and Install stops with nothing else changed), stop the `crossc` service if it runs (it holds the buds' device open; waits up to 45 s, the service's own stop allowance plus a margin), `pnputil /delete-driver oemNN.inf /uninstall /force` for the older crossc_bth packages (all but the one just staged), `pnputil /add-driver crossc_bth.inf /install` (3010 = reboot needed, 259 = no device to install on right now), verify **every** present node runs `crossc_bth` with problem 0 (on failure, prints the `pnputil /enum-devices /drivers` ranking of the node that did not switch); with no node present, verify the package is staged and the certificate trusted. Then the install record: remove the previously recorded and the pending certificates if this package has another one, write `CertThumbprint`, `Version` and `DriverVer`. Then the service: copy `crossc-service.exe` and `crossc-cli.exe` to `%ProgramFiles%\crossc\` (a `crossc-cli.exe` that cannot be replaced because it is running is a warning, not a stop), run `crossc-service.exe install` there, start the service and wait until it runs. If any step after the service stop fails, the log ends with a warning that the service ran before and does not now |
| Build + Install (repo) | yes | both of the above |
| Download and install, first click | yes | preflight (test signing on: no download without it), find a token, download `SHA256SUMS.txt` and the zip with progress lines in the log, compare the SHA-256, unpack to `%ProgramFiles%\crossc\releases\<tag>`, check the release (driver files, INF models, service exe, certificate subject). Installs nothing, trusts nothing |
| Download and install, second click | yes | check that the certificate file still is the one that was shown (again at the moment it is trusted), then Install as above on the unpacked release, then delete every other folder and leftover in `releases` (not the one the running installer is in), and say so if the release has a newer installer. If the install fails the confirmation stays, to try again without downloading; the earlier release is still unpacked then, and the log says where |
| Uninstall | yes | stop and remove the service (`crossc-service.exe uninstall`; through the service control manager if the exe is gone or that did not remove it), delete `%ProgramFiles%\crossc` (with the downloaded releases in it), delete staged crossc_bth packages, delete `Version` and `DriverVer` from the install record, `pnputil /scan-devices` (whenever a package was removed or there is a node to check), verify **every** present node is back on `BthA2dp`: the package's models and any other node that ran `crossc_bth` |
| Remove certificate | yes | delete `CN=WDRLocalTestCert` from LocalMachine Root and TrustedPublisher, then delete `CertThumbprint` and `PendingCerts` from the install record |
| Check for update | no | one GET for the release list; reads only |

The service exe comes from the bundle, or from `target\release\crossc-service.exe` in repo mode. It has to be
there and be a complete Windows executable: the preflight checks the DOS and PE headers and that every section's
data lies inside the file, which catches an empty placeholder and a download or unzip that was cut short (it does
not run or otherwise validate the program). In a bundle a missing or damaged service exe stops the install before
anything is changed, and the status shows it in red. In repo mode it is a warning: Install does the driver and
then starts the `crossc` service if one is registered already (it stopped it for the driver swap), without
updating it; with no service registered it does the driver only. What the installer expects of
`crossc-service.exe`: the subcommands `install`, `uninstall` and `status`, exit code 0 on success, service name
`crossc`.

`%ProgramFiles%\crossc` is `<Program Files as the system reports it>\crossc`; the `ProgramFiles` environment
variable is never read. The reported folder is only used when it is an absolute path with a drive and without
`.` or `..` (otherwise `C:\Program Files` is used), and the folder is never deleted when the installer itself
runs from it.

## Hidden command-line modes

```powershell
crossc-installer --status [--release]   # print the mode, status (with the latest release on GitHub: one request,
                                        # 10 s timeout per step), checklist and next step, then exit
crossc-installer --build  [--release]   # run Build headless, log to stdout (repo mode)
crossc-installer --autostart-build      # open the window and start Build at once (repo mode, to watch the log stream)
crossc-installer --show-warning         # open the window with the first-install warning shown whatever the status
crossc-installer --check-release        # open the window and check for the latest release at once
crossc-installer --show-confirm         # open the window with the confirmation step of a made-up release (to look
                                        # at it; confirming it stops at once, there is no such release)
```

## Code layout

* `layout.rs`: bundle, repo or online, and where the package and the companion executables are
* `parse.rs`: pure parsers (pnputil output, INF hardware IDs and DriverVer, Bluetooth address, service image
  path, problem codes, exit codes, ANSI, line colouring)
* `release.rs`: the pure rules of the GitHub path: versions and their order, the release list, which release and
  assets, `SHA256SUMS.txt`, URLs, the host allow-list, redirects, request headers, the token and where it is
  looked for, zip entry paths, what to prune
* `model.rs`: `Status`, node matching, the install / uninstall verdicts, the checklist (with the Latest release
  row), the download offer, the certificate record plan, the next-step logic, the warning and confirmation texts,
  all as pure functions
* `system.rs`, `cert.rs`, `service.rs`: Win32 queries that fill `Status`, and the install record; `service.rs`
  also stops, starts and deletes the service for the actions
* `http.rs`: one GET through WinHTTP, redirects off
* `update.rs`: token lookup, the release check, download + SHA-256 + unpack
* `actions.rs`: the action steps and the child-process runner
* `ui.rs`, `main.rs`: the window

`cargo test -p crossc-installer` runs the parser, layout, matching, verdict and guidance tests, and for the GitHub
path: release JSON in the shape the API returns (drafts, prereleases, missing and unfinished assets, error
bodies), version order, asset selection, `SHA256SUMS.txt` parsing and mismatch, the host allow-list and redirect
rules, token lookup order (environment and `gh` injected), zip-slip and Windows path tricks, size caps, the
certificate record plan (what goes on record before a certificate is trusted, what is removed once a driver
verified), which staged package is the new one and when a package counts as refused, tags that are skipped, the
`gh.exe` guard on real folders (`..`, a junction), and an end-to-end check + download + verify + unpack against a local HTTP server on
loopback. Plain HTTP exists only in test builds (`Policy::local_test` is `#[cfg(test)]`). One test is ignored by
default because it talks to the real `api.github.com` (read-only, a public repository):
`cargo test -p crossc-installer -- --ignored real_github`.

It also runs a few smoke tests against the real machine, all read-only and unelevated: the code integrity query,
the device list, the service query, reading the install record, and certificate add / find / remove in an
in-memory store with test certificates from `testdata\` (self-signed, made for the tests, private keys deleted).
The registry code is exercised on a throwaway key under `HKEY_CURRENT_USER`. The elevated steps themselves
(pnputil, certificate stores, service control, the install record in `HKLM`, copying and unpacking to
`%ProgramFiles%`) are not tested; they are thin wrappers around the tested functions, and every one of them sits
behind the elevation check, which is tested.

## Dependencies

Beyond `gpui` and the `windows` crate (WinHTTP for HTTPS, BCrypt for SHA-256, so no HTTP or crypto crate):

| Crate | License | Why |
|---|---|---|
| `zip` 7 (`default-features = false`, `deflate-flate2`) | MIT | unpacking the release zip; brings `typed-path` (MIT OR Apache-2.0) |
| `flate2` (`rust_backend`) | MIT OR Apache-2.0 | inflate for `zip`; already a dependency of gpui |
| `serde_json` | MIT OR Apache-2.0 | the release list; already a dependency of gpui |

Note: the workspace is `members = ["crates/*"]`, so a plain workspace `cargo build` / `cargo test` now compiles
gpui as well (slow the first time). Use `-p <crate>` to skip it.
