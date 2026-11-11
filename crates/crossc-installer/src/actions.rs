//! Build / Install / Uninstall / Remove certificate, run on a worker thread. The driver steps use the pnputil
//! commands of tools/install-driver.ps1 (Install stages the new package before it removes the older ones, see
//! `Ctx::install_stopping`); Install and Uninstall also handle the `crossc` service and the tray app (`tray`).
//! Every child process's stdout and stderr are streamed line by line into the log callback as they arrive.
//!
//! "Download and install" is two calls with the user's confirmation between them: `fetch_release` downloads,
//! verifies and unpacks a release and reports its certificate; `install_release` then runs the same Install on
//! the unpacked folder.
//!
//! The admin-only steps cannot be unit-tested, so they are thin: what to look at and how to judge the result is
//! decided by the pure functions in `model`, `parse` and at the bottom of this file.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::cert::{Store, TEST_CERT_SUBJECT, TRUST_STORES, describe, parse_thumbprint, thumbprint};
use crate::layout::{
    BUNDLE_DRIVER_DIR, CLI_EXE, INSTALLER_EXE, Layout, Mode, SERVICE_EXE, TRAY_EXE,
    bundle_version_of, kept_release_dir,
};
use crate::model::{
    DRIVER_SERVICE, FetchedRelease, Flavor, INBOX_SERVICE, InstallVerdict, NodeInfo, PackageInfo,
    REQUIRED_FILES, ServiceState, Staged, USER_SERVICE, cert_record_plan, earlier_release_note,
    install_verdict, newer_installer_note, not_healthy_on, order_nodes, packages_to_remove,
    pending_before_trust, staged_keeper,
};
use crate::parse::{
    INF_NAME, LineKind, PnpExit, classify_output, hex_upper, pnputil_exit, problem_text, strip_ansi,
};
use crate::release::{LatestRelease, REPO_ENV, auth_text, releases_to_prune};
use crate::service;
use crate::system::{
    self, CREATE_NO_WINDOW, RECORD_CERT, RECORD_DRIVER_VER, RECORD_PENDING, RECORD_VERSION,
};
use crate::tray;
use crate::update;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Build,
    Install,
    BuildInstall,
    Uninstall,
    RemoveCert,
    /// First half of "Download and install" (`fetch_release`): download, verify, unpack.
    Download,
    /// Second half, after the confirmation (`install_release`): Install from the unpacked release.
    InstallRelease,
}

impl Action {
    pub fn label(self) -> &'static str {
        match self {
            Action::Build => "Build",
            Action::Install => "Install",
            Action::BuildInstall => "Build + Install",
            Action::Uninstall => "Uninstall",
            Action::RemoveCert => "Remove certificate",
            Action::Download => "Download",
            Action::InstallRelease => "Install release",
        }
    }

    pub fn needs_admin(self) -> bool {
        !matches!(self, Action::Build)
    }
}

/// The actions each layout offers as plain buttons, in button order. A bundle has nothing to build, and neither
/// has the installer on its own. "Download and install" is not in this list: it is offered in every mode when
/// the release check found something to install (`model::download_offer`).
pub fn actions_for(mode: Mode) -> &'static [Action] {
    match mode {
        Mode::Repo => &[
            Action::Build,
            Action::Install,
            Action::BuildInstall,
            Action::Uninstall,
            Action::RemoveCert,
        ],
        Mode::Bundle | Mode::Online => &[Action::Install, Action::Uninstall, Action::RemoveCert],
    }
}

/// The highlighted button (when "Download and install" is offered, that one is highlighted instead).
pub fn primary_action(mode: Mode) -> Action {
    match mode {
        Mode::Repo => Action::BuildInstall,
        Mode::Bundle | Mode::Online => Action::Install,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub kind: LineKind,
    pub text: String,
}

/// Runs `action` to completion on the calling thread (call it from a worker thread). `Err` carries the reason it
/// stopped; the details are already in the log.
pub fn run(
    action: Action,
    layout: &Layout,
    flavor: Flavor,
    log: &dyn Fn(LogLine),
) -> Result<(), String> {
    // Online mode: the release that is installed is the bundle (looked up now: a download changes it).
    let layout = &system::resolve_layout(layout);
    let mut cx = Ctx {
        layout,
        flavor,
        log,
        release: None,
    };
    let result = if !actions_for(layout.mode).contains(&action) {
        Err(match action {
            Action::Download | Action::InstallRelease => {
                "this step belongs to \"Download and install\"".to_string()
            }
            _ => "not available in a release bundle (there is nothing to build)".to_string(),
        })
    } else {
        match action {
            Action::Build => cx.build(),
            Action::Install => cx.install(),
            Action::BuildInstall => cx.build().and_then(|()| cx.install()),
            Action::Uninstall => cx.uninstall(),
            Action::RemoveCert => cx.remove_cert(),
            Action::Download | Action::InstallRelease => unreachable!("not in actions_for"),
        }
    };
    match &result {
        Ok(()) => cx.success(format!("{} finished", action.label())),
        Err(e) => cx.error(format!("{} stopped: {e}", action.label())),
    }
    result
}

/// First half of "Download and install": checks that Install could go on at all, downloads `release` from
/// GitHub, verifies it against its `SHA256SUMS.txt`, unpacks it to `%ProgramFiles%\crossc\releases\<tag>` and
/// looks at what it holds. Nothing is installed and no certificate is trusted: the result is what the user is
/// asked to confirm. Needs administrator rights, because that folder does.
pub fn fetch_release(
    release: &LatestRelease,
    log: &dyn Fn(LogLine),
) -> Result<FetchedRelease, String> {
    let say = |kind: LineKind, text: String| log(LogLine { kind, text });
    let result = fetch_steps(release, &say);
    match &result {
        Ok(f) => say(
            LineKind::Success,
            format!(
                "    Download finished: {} is unpacked and waits for your confirmation",
                f.tag
            ),
        ),
        Err(e) => say(LineKind::Error, format!("ERROR: Download stopped: {e}")),
    }
    result
}

fn fetch_steps(
    release: &LatestRelease,
    say: &dyn Fn(LineKind, String),
) -> Result<FetchedRelease, String> {
    require_admin()?;
    say(LineKind::Step, "==> Preflight".into());
    // The one Install prerequisite that does not depend on the package: no point in downloading without it.
    match system::test_signing() {
        Some(true) => say(LineKind::Success, "    test signing is on".into()),
        Some(false) => return Err(TEST_SIGNING_OFF.into()),
        None => return Err(TEST_SIGNING_UNKNOWN.into()),
    }
    let source = update::Source::github(std::env::var(REPO_ENV).ok().as_deref())?;
    let auth = update::find_token(true);
    say(
        LineKind::Info,
        format!("    {} on GitHub, {}", source.repo, auth_text(auth.source)),
    );
    if let Some(note) = &auth.note {
        say(LineKind::Warning, format!("    {note}"));
    }

    say(
        LineKind::Step,
        format!("==> Downloading release {}", release.tag),
    );
    let releases = system::releases_dir();
    let unpacked = update::download_release(&source, &auth, release, &releases, &|kind, text| {
        say(kind, format!("    {text}"))
    })?;

    say(LineKind::Step, "==> Checking the release".into());
    let fetched = inspect_release(&unpacked, release, &source.repo).inspect_err(|_| {
        // Not something to keep around: it cannot be installed.
        let _ = update::remove_path(&unpacked.dir);
    })?;
    say(
        LineKind::Info,
        format!(
            "    driver package and {SERVICE_EXE} are complete ({} files)",
            unpacked.files
        ),
    );
    say(
        LineKind::Info,
        format!(
            "    certificate: {} {}",
            fetched.cert_subject, fetched.cert_thumbprint
        ),
    );
    Ok(fetched)
}

/// What an unpacked release must hold before it is offered for confirmation: the four driver files, an INF with
/// at least one supported model, a complete `crossc-service.exe`, and a certificate with the subject of the WDK
/// test certificate. The subject is insisted on because "Remove certificate" and the replacement of an older
/// release's certificate both go by it: a certificate with another subject, once trusted, would be one this
/// installer could never remove again.
pub fn inspect_release(
    unpacked: &update::Unpacked,
    release: &LatestRelease,
    repo: &str,
) -> Result<FetchedRelease, String> {
    let tag = &release.tag;
    let driver = unpacked.dir.join(BUNDLE_DRIVER_DIR);
    let missing: Vec<&str> = REQUIRED_FILES
        .iter()
        .copied()
        .filter(|f| !driver.join(f).is_file())
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "release {tag} has no {BUNDLE_DRIVER_DIR}\\{}",
            missing.join(", ")
        ));
    }
    if system::package_hardware_ids(&driver).is_empty() {
        return Err(format!(
            "the INF of release {tag} lists no supported buds model"
        ));
    }
    let service_exe = unpacked.dir.join(SERVICE_EXE);
    if !service_exe.is_file() {
        return Err(format!("release {tag} has no {SERVICE_EXE}"));
    }
    if let Some(why) = system::damaged_exe(&service_exe) {
        return Err(format!("{SERVICE_EXE} of release {tag} is damaged: {why}"));
    }
    let der = std::fs::read(driver.join("WDRLocalTestCert.cer"))
        .map_err(|e| format!("cannot read the certificate of release {tag}: {e}"))?;
    let (subject, tp) = describe(&der)
        .map_err(|e| format!("WDRLocalTestCert.cer of release {tag} is not a certificate: {e}"))?;
    if subject != TEST_CERT_SUBJECT {
        return Err(format!(
            "the certificate of release {tag} is \"{subject}\", not {TEST_CERT_SUBJECT}: it is not offered for \
             trusting, because this installer could not remove it again"
        ));
    }
    Ok(FetchedRelease {
        repo: repo.to_string(),
        tag: tag.clone(),
        version: release.version.clone(),
        asset: unpacked.asset.clone(),
        sha256: unpacked.sha256.clone(),
        dir: unpacked.dir.clone(),
        cert_subject: subject,
        cert_thumbprint: hex_upper(&tp),
    })
}

/// Second half of "Download and install", after the user confirmed: the bundle-mode Install on the unpacked
/// release, trusting exactly the certificate that was shown. Afterwards older downloads are deleted.
pub fn install_release(
    fetched: &FetchedRelease,
    flavor: Flavor,
    log: &dyn Fn(LogLine),
) -> Result<(), String> {
    let layout = Layout {
        mode: Mode::Bundle,
        root: fetched.dir.clone(),
    };
    let mut cx = Ctx {
        layout: &layout,
        flavor,
        log,
        release: Some(fetched),
    };
    let result = cx.install_fetched(fetched);
    match &result {
        Ok(()) => cx.success(format!("Install of {} finished", fetched.tag)),
        Err(e) => cx.error(format!("Install of {} stopped: {e}", fetched.tag)),
    }
    result
}

/// The confirmation was declined: delete what was unpacked for it (only ever a folder below the releases
/// folder). Best effort; a leftover is never used for anything and goes with the next install or Uninstall.
pub fn discard_release(fetched: &FetchedRelease) {
    let releases = system::releases_dir();
    if fetched.dir.parent() == Some(releases.as_path()) {
        let _ = update::remove_path(&fetched.dir);
    }
}

/// `Ok` when the certificate file still is the certificate with this thumbprint (the one the user confirmed).
fn confirmed_cert(cer: &Path, confirmed_thumbprint: &str) -> Result<(), String> {
    let der = std::fs::read(cer).map_err(|e| format!("cannot read {}: {e}", cer.display()))?;
    let tp = thumbprint(&der).map_err(|e| format!("cannot parse {}: {e}", cer.display()))?;
    if hex_upper(&tp).eq_ignore_ascii_case(confirmed_thumbprint) {
        Ok(())
    } else {
        Err(format!(
            "{} is certificate {}, not the confirmed {confirmed_thumbprint}: it changed after the download. \
             Nothing was trusted; download the release again",
            cer.display(),
            hex_upper(&tp)
        ))
    }
}

/// Deletes everything in the releases folder except `keep` and the folder the running installer is in
/// (`release::releases_to_prune`); returns the names it removed and the ones it could not.
fn prune_releases(
    releases: &Path,
    keep: &str,
    running_exe: Option<&Path>,
) -> (Vec<String>, Vec<String>) {
    let entries: Vec<String> = std::fs::read_dir(releases)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let running_from = entries
        .iter()
        .find(|name| running_exe.is_some_and(|exe| is_inside(exe, &releases.join(name))))
        .cloned();
    let (mut removed, mut failed) = (Vec::new(), Vec::new());
    for name in releases_to_prune(&entries, keep, running_from.as_deref()) {
        match update::remove_path(&releases.join(&name)) {
            Ok(()) => removed.push(name),
            Err(_) => failed.push(name),
        }
    }
    (removed, failed)
}

const TEST_SIGNING_OFF: &str = "test signing is off. Run (admin): bcdedit /set testsigning on, then reboot. \
                                Secure Boot must be off (docs/SETUP.md section 1).";
const TEST_SIGNING_UNKNOWN: &str =
    "could not read the test signing state (code integrity query failed)";

/// Refuses to go on unelevated, so no code path can change drivers, certificates, services, the registry or
/// files under %ProgramFiles% without admin. First statement of every admin action.
fn require_admin() -> Result<(), String> {
    if system::is_admin() {
        Ok(())
    } else {
        Err("needs administrator rights: use \"Restart as administrator\"".into())
    }
}

/// What `Ctx::trust_cert` did, so that exactly that can be undone when Windows then refuses the package.
#[derive(Debug)]
struct Trusted {
    sha1: [u8; 20],
    /// The stores this run added the certificate to. A store that had it already is not listed: taking the
    /// certificate out again there would untrust a driver that is installed and working (every build in the
    /// repo carries the same certificate, and so does a reinstall of the same release).
    added: Vec<&'static str>,
    /// The `PendingCerts` value as it was before this run put the certificate on it; `None` when this run did
    /// not change the value.
    pending_before: Option<Option<String>>,
}

struct Ctx<'a> {
    layout: &'a Layout,
    flavor: Flavor,
    log: &'a dyn Fn(LogLine),
    /// Set for an install of a downloaded release: the certificate that was confirmed is the only one that is
    /// trusted, and its version is what gets recorded.
    release: Option<&'a FetchedRelease>,
}

impl Ctx<'_> {
    fn emit(&self, kind: LineKind, text: impl Into<String>) {
        (self.log)(LogLine {
            kind,
            text: text.into(),
        });
    }
    fn step(&self, t: impl Into<String>) {
        self.emit(LineKind::Step, format!("==> {}", t.into()));
    }
    fn info(&self, t: impl Into<String>) {
        self.emit(LineKind::Info, format!("    {}", t.into()));
    }
    fn success(&self, t: impl Into<String>) {
        self.emit(LineKind::Success, format!("    {}", t.into()));
    }
    fn warn(&self, t: impl Into<String>) {
        self.emit(LineKind::Warning, format!("    {}", t.into()));
    }
    fn error(&self, t: impl Into<String>) {
        self.emit(LineKind::Error, format!("ERROR: {}", t.into()));
    }

    fn require_admin(&self) -> Result<(), String> {
        require_admin()
    }

    /// Install of a downloaded release, after the confirmation.
    fn install_fetched(&mut self, f: &FetchedRelease) -> Result<(), String> {
        self.require_admin()?;
        self.step(format!("Installing release {} (confirmed)", f.tag));
        self.info(format!("{}, SHA-256 {}", f.asset, f.sha256));
        self.info(format!(
            "certificate to trust: {} {}",
            f.cert_subject, f.cert_thumbprint
        ));
        // Before anything is stopped or changed; `trust_cert` checks again at the moment it trusts.
        confirmed_cert(
            &self
                .layout
                .package_dir(self.flavor)
                .join("WDRLocalTestCert.cer"),
            &f.cert_thumbprint,
        )?;
        if let Err(e) = self.install() {
            // The earlier release is only untrusted and pruned once this one is verified: say where it is.
            let record = system::read_record();
            let earlier = kept_release_dir(&system::releases_dir(), record.version.as_deref())
                .filter(|dir| {
                    system::release_cert_is_trusted(
                        &dir.join(BUNDLE_DRIVER_DIR).join("WDRLocalTestCert.cer"),
                    )
                });
            if let Some(note) = earlier_release_note(&f.dir, earlier.as_deref()) {
                self.warn(note);
            }
            return Err(e);
        }

        self.step("Cleaning up older downloads");
        let releases = system::releases_dir();
        if f.dir.parent() == Some(releases.as_path()) {
            let me = std::env::current_exe().ok();
            let (removed, failed) = prune_releases(&releases, &f.tag, me.as_deref());
            for name in &removed {
                self.info(format!("deleted {}", releases.join(name).display()));
            }
            for name in &failed {
                self.warn(format!(
                    "could not delete {}",
                    releases.join(name).display()
                ));
            }
            if removed.is_empty() && failed.is_empty() {
                self.info("nothing to delete");
            }
        }
        if let Some(note) = newer_installer_note(
            env!("CARGO_PKG_VERSION"),
            &f.version,
            &f.dir.join(INSTALLER_EXE),
        ) {
            self.warn(note);
        }
        Ok(())
    }

    // -----------------------------------------------------------------------------------------------------------
    // Build (as the calling user; repo only)
    // -----------------------------------------------------------------------------------------------------------

    fn build(&mut self) -> Result<(), String> {
        if system::is_admin() {
            // The script builds unelevated and elevates only the install; this process cannot drop its admin token.
            self.warn(
                "building elevated: new files under target\\ will be owned by Administrators. Prefer Build in a \
                 normal window, then Install here.",
            );
        }
        let mut env: Vec<(&str, OsString)> = Vec::new();
        if std::env::var_os("LIBCLANG_PATH").is_none() {
            let llvm = program_files().join("LLVM").join("bin");
            if !llvm.join("libclang.dll").is_file() {
                return Err(format!(
                    "LIBCLANG_PATH is not set and {} was not found (winget install LLVM.LLVM)",
                    llvm.join("libclang.dll").display()
                ));
            }
            self.info(format!("LIBCLANG_PATH={}", llvm.display()));
            env.push(("LIBCLANG_PATH", llvm.into_os_string()));
        }
        let cargo =
            find_cargo().ok_or("cargo not found on PATH or in %USERPROFILE%\\.cargo\\bin")?;

        self.step(format!("Building driver package ({})", self.flavor));
        let args: &[&str] = match self.flavor {
            Flavor::Debug => &["make"],
            Flavor::Release => &["make", "default", "--release"],
        };
        let rc = self.run_cmd(&cargo, args, &self.layout.driver_dir(), &env)?;
        if rc != 0 {
            return Err(format!("cargo make failed (exit {rc})"));
        }
        self.success(self.layout.package_dir(self.flavor).display().to_string());

        let mut args = vec!["build", "--release", "-p", "crossc-cli"];
        let mut built = vec![self.layout.cli_exe()];
        if self.layout.has_service_crate() {
            args.extend(["-p", "crossc-service"]);
            built.push(self.layout.service_exe());
        }
        self.step(build_step(&args));
        let rc = self.run_cmd(&cargo, &args, &self.layout.root, &env)?;
        if rc != 0 {
            return Err(format!("cargo {} failed (exit {rc})", args.join(" ")));
        }
        for exe in built {
            self.success(exe.display().to_string());
        }

        // The tray in a cargo run of its own: Windows does not let a running program be replaced, and a
        // developer's tray runs from this very file (docs/SETUP.md 16a). That must not take the service and
        // the CLI down with it; the tray is an extra here as it is in Install.
        if self.layout.has_tray_crate() {
            let args = ["build", "--release", "-p", "crossc-tray"];
            let exe = self.layout.tray_exe();
            self.step(build_step(&args));
            let rc = self.run_cmd(&cargo, &args, &self.layout.root, &env)?;
            match tray_build(rc, tray::running_count(&exe) > 0) {
                TrayBuild::Built => self.success(exe.display().to_string()),
                TrayBuild::InUse => self.warn(tray_in_use_note(rc, &exe)),
                TrayBuild::Failed => {
                    return Err(format!("cargo {} failed (exit {rc})", args.join(" ")));
                }
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------------------------------------------
    // Install (admin)
    // -----------------------------------------------------------------------------------------------------------

    fn install(&mut self) -> Result<(), String> {
        self.require_admin()?;
        let (kind, dir) = self.layout.packages(self.flavor).remove(0);
        let pkg = system::package_info(kind, dir);

        self.step("Preflight");
        match system::test_signing() {
            Some(true) => self.success("test signing is on"),
            Some(false) => return Err(TEST_SIGNING_OFF.into()),
            // The script stops when bcdedit fails; do the same rather than install a driver that may not load.
            None => return Err(TEST_SIGNING_UNKNOWN.into()),
        }
        if system::secure_boot() == Some(true) {
            self.warn("Secure Boot reports ON: test-signed drivers will not load until it is off");
        }
        if !pkg.complete() {
            return Err(format!(
                "{} missing in {} ({})",
                pkg.missing.join(", "),
                pkg.dir.display(),
                self.how_to_get_files()
            ));
        }
        self.success(match pkg.version() {
            Some(v) => format!("package: {} (version {v})", pkg.dir.display()),
            None => format!("package: {}", pkg.dir.display()),
        });
        let ids = system::package_hardware_ids(&pkg.dir);
        if ids.is_empty() {
            return Err(format!(
                "{} has no BTHENUM ..._VID&..._PID&... model line",
                pkg.dir.join(INF_NAME).display()
            ));
        }
        let nodes = system::find_buds_nodes(&ids)?;
        self.info(format!(
            "{} supported buds model(s) in the INF, {} paired here",
            ids.len(),
            nodes.len()
        ));
        for n in &nodes {
            self.success(format!("node: {} ({})", n.label(), n.instance_id));
        }
        if nodes.is_empty() {
            self.info(
                "no supported buds are paired: the package is staged anyway and they pick it up when paired",
            );
        }
        // Buds on crossc_bth that this package does not list: the package swap below takes their driver away.
        let unlisted = system::find_unlisted_nodes(&ids).unwrap_or_else(|e| {
            self.warn(e);
            Vec::new()
        });
        for n in &unlisted {
            self.warn(format!(
                "{} runs crossc_bth from {} but is not a model of this package: it goes back to the Windows \
                 driver ({})",
                n.label(),
                n.inf.as_deref().unwrap_or("another package"),
                n.instance_id
            ));
        }
        let service_exe = self.service_source()?;

        // From here on things change. The service is stopped first; if anything after that fails, say what state
        // the service is left in (the driver's state is in the log above; it is not guessed at here).
        let was_active = service::query(USER_SERVICE)
            .is_ok_and(|i| matches!(i.state, ServiceState::Running | ServiceState::StartPending));
        let result = self.install_stopping(&pkg, &nodes, &unlisted, service_exe.as_deref());
        if result.is_err() {
            let now = service::query(USER_SERVICE).ok().map(|i| i.state);
            if let Some(note) = service_left_note(was_active, now) {
                self.warn(note);
            }
        }
        result
    }

    /// Everything in Install that changes the machine, in an order that keeps a working install working for as
    /// long as possible:
    ///
    /// 1. the new certificate is put on record and trusted, and the new package is staged (not installed on
    ///    any device). If Windows refuses the package, this is undone and Install stops: the driver that was
    ///    installed, its certificate and the running service have not been touched;
    /// 2. only then the service is stopped, the older packages are removed and the new one is installed;
    /// 3. only once the driver is verified is the earlier certificate untrusted and the record moved on. If
    ///    the driver does not verify, the earlier release's certificate is still trusted and still the
    ///    recorded one, so that release can be installed again.
    fn install_stopping(
        &mut self,
        pkg: &PackageInfo,
        nodes: &[NodeInfo],
        unlisted: &[NodeInfo],
        service_exe: Option<&Path>,
    ) -> Result<(), String> {
        let inf = pkg.dir.join(INF_NAME);
        let cer = pkg.dir.join("WDRLocalTestCert.cer");

        self.step("Trusting the test certificate");
        let trusted = self.trust_cert(&cer)?;

        // Before anything is taken away: does Windows take the new package at all? (A catalog that is not signed
        // by the certificate just trusted is refused here.)
        self.step("Staging the new package");
        let keep = self.stage_package(pkg, &inf, &trusted)?;

        // The service keeps the buds' device open for its whole life. With that handle open the driver swap below
        // would end in "reboot required", so the service goes before the swap.
        self.step("Stopping the crossc service");
        if !service::stop(USER_SERVICE, &|m| self.info(m))? {
            self.info("the crossc service is not installed yet");
        }

        self.step("Removing older crossc_bth packages");
        self.remove_staged(keep.as_deref())?;

        self.step("Installing the new package");
        let inf_arg = inf.to_string_lossy().into_owned();
        let rc = self.run_cmd(
            Path::new("pnputil"),
            &["/add-driver", &inf_arg, "/install"],
            &tool_dir(),
            &[],
        )?;
        match pnputil_exit(rc) {
            PnpExit::Ok => {}
            PnpExit::NothingToDo => self.info(
                "pnputil found no device to install on right now (exit 259: no supported buds paired, or they \
                 already run this driver)",
            ),
            PnpExit::RebootRequired => self.warn("pnputil says a reboot is required to finish"),
            PnpExit::Failed(n) => self.warn(format!(
                "pnputil /add-driver exited {n} (checking the result anyway)"
            )),
        }
        if !unlisted.is_empty() {
            // Their package is gone and the new one does not match them: a rescan lets Windows give them its own
            // driver back. Not part of the verdict: these are not nodes this package installs on.
            match self.run_cmd(Path::new("pnputil"), &["/scan-devices"], &tool_dir(), &[]) {
                Ok(_) => {
                    self.info("buds that are not a model of this package:");
                    let after = wait_for_nodes(unlisted, |now| {
                        not_healthy_on(now, INBOX_SERVICE).is_empty()
                    });
                    self.show_states(unlisted, &after);
                }
                Err(e) => self.warn(e),
            }
        }

        self.step("Verifying the driver");
        self.verify_install(nodes, &cer)?;
        // The new driver works: now, and not before, the earlier certificate goes.
        self.record_cert(&cer);
        self.record_version(pkg);

        let registered = service::query(USER_SERVICE)?.state.installed();
        match service_plan(service_exe, registered) {
            ServicePlan::Install(exe) => self.install_service(exe)?,
            ServicePlan::StartExisting => {
                // Repo only: nothing to update the service with, but what Install stopped it starts again.
                self.step("Starting the crossc service");
                self.info(
                    "there is no crossc-service.exe to install, so the registered service is started as it is \
                     (not updated)",
                );
                self.start_service()?;
            }
            ServicePlan::DriverOnly => {
                self.step("Next");
                self.info(format!(
                    "No crossc service is installed and none was added (no crossc-service.exe to install). \
                     Diagnostics (admin): \"{}\" probe   (trace: DebugView, docs/SETUP.md section 6)",
                    self.layout.cli_exe().display()
                ));
                return Ok(());
            }
        }
        self.step("Next");
        self.info(if nodes.is_empty() {
            "Pair your Galaxy Buds (LE Audio off); they pick the driver up. Then select \"Headphones \
             (... Buds ...)\" as the output device in Windows sound settings."
        } else {
            "Select \"Headphones (... Buds ...)\" as the output device in Windows sound settings."
        });
        Ok(())
    }

    fn how_to_get_files(&self) -> &'static str {
        match self.layout.mode {
            Mode::Bundle => "unpack the whole release zip again",
            Mode::Online => "use \"Download and install\" to fetch the latest release",
            Mode::Repo => "build first",
        }
    }

    /// Puts the new package into the driver store (`pnputil /add-driver`, without `/install`: no device is
    /// touched) while the older packages are still there, and says which staged package it is
    /// (`model::staged_keeper`): `Some(oemNN.inf)` to keep when the others are removed, `None` when that cannot
    /// be told (the caller then removes all and adds again).
    ///
    /// `Err` when Windows refuses the package. The certificate trusted for it is taken out again
    /// (`untrust_new`), and nothing else has been changed by then.
    fn stage_package(
        &self,
        pkg: &PackageInfo,
        inf: &Path,
        trusted: &Trusted,
    ) -> Result<Option<String>, String> {
        let before = system::staged_packages()?;
        let inf_arg = inf.to_string_lossy().into_owned();
        let rc = self.run_cmd(
            Path::new("pnputil"),
            &["/add-driver", &inf_arg],
            &tool_dir(),
            &[],
        )?;
        let accepted = !matches!(pnputil_exit(rc), PnpExit::Failed(_));
        let after = system::staged_packages()?;
        match staged_keeper(&before, &after, pkg.version(), accepted) {
            Staged::Keep(name) => {
                if !accepted {
                    self.warn(format!(
                        "pnputil /add-driver exited {rc}, but the package is in the driver store"
                    ));
                }
                self.success(format!("the new package is staged as {name}"));
                Ok(Some(name))
            }
            Staged::Unclear => {
                self.warn(
                    "pnputil accepted the package, but which staged package it is cannot be told: every \
                     crossc_bth package is removed and the new one added again",
                );
                Ok(None)
            }
            Staged::Refused => {
                self.untrust_new(trusted);
                Err(format!(
                    "Windows did not accept the driver package (pnputil /add-driver exited {rc}, see the output \
                     above). Nothing was removed: the driver that was installed, its certificate and the crossc \
                     service are as they were"
                ))
            }
        }
    }

    /// Undoes `trust_cert` for a package that Windows then refused: the certificate is taken out of the stores
    /// this run added it to (by its thumbprint, and only with the subject CN=WDRLocalTestCert), and the pending
    /// list gets its earlier value back. If a removal fails, the certificate stays on the pending list, and the
    /// next install that verifies removes it.
    fn untrust_new(&self, trusted: &Trusted) {
        let mut ok = true;
        for name in &trusted.added {
            let store = match Store::local_machine(name, false) {
                Ok(s) => s,
                Err(e) => {
                    self.warn(format!("cannot open LocalMachine\\{name}: {e}"));
                    ok = false;
                    continue;
                }
            };
            let r = store.remove_exact(&trusted.sha1, TEST_CERT_SUBJECT);
            for tp in &r.removed {
                self.info(format!(
                    "certificate {tp} removed from LocalMachine\\{name} again"
                ));
            }
            for (tp, e) in &r.failed {
                self.warn(format!(
                    "could not remove certificate {tp} from LocalMachine\\{name} again: {e}"
                ));
            }
            // Nothing removed: its subject is not CN=WDRLocalTestCert, and this installer removes no other.
            ok &= r.failed.is_empty() && !r.removed.is_empty();
        }
        if !ok {
            self.warn(format!(
                "certificate {} is still trusted. It stays on record ({RECORD_PENDING}): the next install \
                 that succeeds removes it, and so does \"Remove certificate\"",
                hex_upper(&trusted.sha1)
            ));
        } else if let Some(before) = &trusted.pending_before
            && let Err(e) = system::write_record(RECORD_PENDING, before.as_deref())
        {
            self.warn(e);
        }
    }

    /// The driver signed with this certificate is verified: untrust the certificates it replaces (the one an
    /// earlier install recorded, and any that an install trusted without getting this far), and record this
    /// one (`model::cert_record_plan`). Run for every install (bundle, repo, downloaded release).
    ///
    /// Not before the driver is verified: until then the earlier certificate is what makes the earlier
    /// release installable again. A certificate is never trusted without being on record all the same,
    /// because `trust_cert` puts it on the pending list first.
    ///
    /// Nothing in here stops Install. A certificate that cannot be removed stays on the pending list and the
    /// next Install tries again.
    fn record_cert(&self, cer: &Path) {
        let record = system::read_record();
        let Some(tp) = std::fs::read(cer)
            .ok()
            .and_then(|der| thumbprint(&der).ok())
        else {
            self.warn(format!("cannot read {} to record it", cer.display()));
            return;
        };
        let now = hex_upper(&tp);
        let plan = cert_record_plan(
            record.cert_thumbprint.as_deref(),
            record.pending_certs.as_deref(),
            &now,
        );
        let failed: Vec<String> = plan
            .remove
            .iter()
            .filter(|old| !self.untrust_previous(old))
            .cloned()
            .collect();
        if plan.write {
            match system::write_record(RECORD_CERT, Some(&now)) {
                Ok(()) => self.info(format!("recorded the trusted certificate {now}")),
                Err(e) => {
                    // The pending list still names this certificate: leave it as it is.
                    self.warn(e);
                    return;
                }
            }
        }
        let pending = plan.pending_after(&failed);
        if (pending.is_some() || record.pending_certs.is_some())
            && let Err(e) = system::write_record(RECORD_PENDING, pending.as_deref())
        {
            self.warn(e);
        }
        if !failed.is_empty() {
            self.warn(format!(
                "still trusted and still on record ({RECORD_PENDING}): {}. The next Install tries to remove \
                 them again (\"Remove certificate\" removes every CN=WDRLocalTestCert)",
                failed.join(", ")
            ));
        }
    }

    /// The driver is verified: write down which version is installed now. Not before: until then the record
    /// describes the package that was there, and a failed install must not claim the new version.
    fn record_version(&self, pkg: &PackageInfo) {
        // The release version when it is known: the downloaded release's, else what the bundle says of itself.
        // An install from the repo has none, and must not leave an earlier release's version standing.
        let version = match (self.release, self.layout.mode) {
            (Some(f), _) => Some(f.version.to_string()),
            (None, Mode::Bundle | Mode::Online) => {
                bundle_version_of(&self.layout.root).map(|v| v.to_string())
            }
            (None, Mode::Repo) => None,
        };
        let written = system::write_record(RECORD_VERSION, version.as_deref())
            .and_then(|()| system::write_record(RECORD_DRIVER_VER, pkg.version()));
        match (written, version) {
            (Err(e), _) => self.warn(e),
            (Ok(()), Some(v)) => self.info(format!(
                "recorded version {v} (driver {})",
                pkg.version().unwrap_or("without a version")
            )),
            (Ok(()), None) => self.info(format!(
                "recorded driver {} (no release version: this is not a release bundle)",
                pkg.version().unwrap_or("without a version")
            )),
        }
    }

    /// Removes the certificate an earlier install trusted (`old`: its recorded thumbprint) from Root and
    /// TrustedPublisher: the one with exactly that thumbprint, and only if its subject is CN=WDRLocalTestCert.
    /// Nothing else is ever removed here. `false`: a store could not be opened or a delete failed.
    fn untrust_previous(&self, old: &str) -> bool {
        let Some(sha1) = parse_thumbprint(old) else {
            return true;
        };
        let mut ok = true;
        let mut removed = 0;
        for name in TRUST_STORES {
            let store = match Store::local_machine(name, false) {
                Ok(s) => s,
                Err(e) => {
                    self.warn(format!("cannot open LocalMachine\\{name}: {e}"));
                    ok = false;
                    continue;
                }
            };
            let r = store.remove_exact(&sha1, TEST_CERT_SUBJECT);
            for tp in &r.removed {
                removed += 1;
                self.success(format!(
                    "removed the earlier certificate {tp} from LocalMachine\\{name}"
                ));
            }
            for (tp, e) in &r.failed {
                self.warn(format!(
                    "could not remove the earlier certificate {tp} from LocalMachine\\{name}: {e}"
                ));
                ok = false;
            }
        }
        if ok && removed == 0 {
            self.info(format!(
                "the earlier certificate {old} is not in the stores any more: nothing to remove"
            ));
        }
        ok
    }

    /// Deletes values of the install record; a failure is a warning.
    fn clear_record(&self, names: &[&str]) {
        for name in names {
            if let Err(e) = system::write_record(name, None) {
                self.warn(e);
            }
        }
    }

    /// The `crossc-service.exe` to install, checked to be a complete executable (`system::damaged_exe`) so that
    /// a bad one stops Install here, before anything is changed. A bundle must have one; in the repo the service
    /// is optional.
    fn service_source(&self) -> Result<Option<PathBuf>, String> {
        let exe = self.layout.service_exe();
        let problem = if exe.is_file() {
            system::damaged_exe(&exe).map(|why| format!("is damaged: {why}"))
        } else {
            Some(match self.layout.mode {
                Mode::Bundle | Mode::Online => "is missing".into(),
                Mode::Repo => "is not built".into(),
            })
        };
        let Some(problem) = problem else {
            self.success(format!("service: {}", exe.display()));
            return Ok(Some(exe));
        };
        match self.layout.mode {
            Mode::Bundle | Mode::Online => Err(format!(
                "{} {problem} ({})",
                exe.display(),
                self.how_to_get_files()
            )),
            Mode::Repo => {
                self.warn(format!(
                    "{} {problem}: the crossc service is not installed or updated. Install does the driver, \
                     and starts the service if one is registered already (cargo build --release -p \
                     crossc-service, or Build)",
                    exe.display()
                ));
                Ok(None)
            }
        }
    }

    /// Judges the driver install on every supported node that was there before (`install_verdict`).
    fn verify_install(&self, nodes: &[NodeInfo], cer: &Path) -> Result<(), String> {
        let after = wait_for_nodes(nodes, |now| not_healthy_on(now, DRIVER_SERVICE).is_empty());
        self.show_states(nodes, &after);
        let staged = !system::staged_packages()?.is_empty();
        let trusted = system::cert_info(cer).is_ok_and(|c| c.trusted());
        match install_verdict(&after, staged, trusted) {
            InstallVerdict::Bound(n) => {
                self.success(format!("crossc_bth is running on {n} buds node(s)"));
                Ok(())
            }
            InstallVerdict::Waiting => {
                self.success("the package is in the driver store and its certificate is trusted");
                self.info("no supported buds are paired: pair your buds and they will pick it up");
                Ok(())
            }
            InstallVerdict::NotStaged => Err(
                "the package is not in the driver store, or its certificate is not trusted (see the \
                 output above)"
                    .into(),
            ),
            InstallVerdict::NotBound(failed) => {
                for n in failed.iter().filter(|n| !n.runs(DRIVER_SERVICE)) {
                    self.warn(format!(
                        "{} did not switch to crossc_bth. Ranking check:",
                        n.label()
                    ));
                    let _ = self.run_cmd(
                        Path::new("pnputil"),
                        &["/enum-devices", "/instanceid", &n.instance_id, "/drivers"],
                        &tool_dir(),
                        &[],
                    );
                }
                let names: Vec<&str> = failed.iter().map(|n| n.label()).collect();
                Err(format!(
                    "crossc_bth is not running on: {}",
                    names.join(", ")
                ))
            }
        }
    }

    /// Trusts the package's certificate in Root and TrustedPublisher. Before it goes into a store it is put on
    /// record (`model::pending_before_trust`), and if that cannot be written nothing is trusted: so no
    /// certificate this installer trusted is ever unknown to the record, whatever stops Install afterwards.
    fn trust_cert(&self, cer: &Path) -> Result<Trusted, String> {
        let der = std::fs::read(cer).map_err(|e| format!("cannot read {}: {e}", cer.display()))?;
        let tp = thumbprint(&der).map_err(|e| format!("cannot parse {}: {e}", cer.display()))?;
        // A downloaded release: the bytes read just now must be the certificate the user confirmed.
        if let Some(f) = self.release
            && !hex_upper(&tp).eq_ignore_ascii_case(&f.cert_thumbprint)
        {
            return Err(format!(
                "{} is certificate {}, not the confirmed {}: nothing was trusted",
                cer.display(),
                hex_upper(&tp),
                f.cert_thumbprint
            ));
        }
        let now = hex_upper(&tp);
        let record = system::read_record();
        let pending_before = match pending_before_trust(
            record.cert_thumbprint.as_deref(),
            record.pending_certs.as_deref(),
            &now,
        ) {
            Some(value) => {
                system::write_record(RECORD_PENDING, Some(&value)).map_err(|e| {
                    format!("{e}. The certificate was not trusted, because it could not be put on record first")
                })?;
                Some(record.pending_certs)
            }
            None => None,
        };
        let mut trusted = Trusted {
            sha1: tp,
            added: Vec::new(),
            pending_before,
        };
        for name in TRUST_STORES {
            match self.trust_in(name, &der, &tp) {
                Ok(true) => trusted.added.push(name),
                Ok(false) => {}
                Err(e) => {
                    // Not half trusted: out of the store it did get into.
                    self.untrust_new(&trusted);
                    return Err(e);
                }
            }
        }
        Ok(trusted)
    }

    /// Adds the certificate to one LocalMachine store; `Ok(false)` when it is there already.
    fn trust_in(&self, name: &str, der: &[u8], tp: &[u8; 20]) -> Result<bool, String> {
        let store = Store::local_machine(name, false)
            .map_err(|e| format!("cannot open LocalMachine\\{name}: {e}"))?;
        if store.contains_sha1(tp) {
            self.success(format!(
                "certificate already trusted in LocalMachine\\{name}"
            ));
            return Ok(false);
        }
        store
            .add_der(der)
            .map_err(|e| format!("cannot add the certificate to LocalMachine\\{name}: {e}"))?;
        self.success(format!(
            "certificate {} added to LocalMachine\\{name}",
            hex_upper(tp)
        ));
        Ok(true)
    }

    /// Copies the service (and the CLI) to %ProgramFiles%\crossc, registers it with `crossc-service.exe install`
    /// and starts it. The service is stopped by now.
    fn install_service(&self, source: &Path) -> Result<(), String> {
        self.step("Installing the crossc service");
        let dest_dir = system::install_dir();
        let dest = dest_dir.join(SERVICE_EXE);

        // A registration that points somewhere else (an earlier install from another folder) is replaced, so the
        // service never keeps running a stale copy.
        let before = service::query(USER_SERVICE)?;
        if before.state.installed() && !registered_with(before.exe.as_deref(), &dest) {
            self.info(format!(
                "the service is registered with {}: replacing that registration",
                before.exe.as_deref().unwrap_or("an unreadable path")
            ));
            self.remove_service()?;
        }

        let in_place = same_file(source, &dest);
        if in_place {
            self.info(format!("{} is already in place", dest.display()));
        } else {
            std::fs::create_dir_all(&dest_dir)
                .map_err(|e| format!("cannot create {}: {e}", dest_dir.display()))?;
            self.copy(source, &dest)?;
            self.copy_cli(&dest_dir, COPY_ATTEMPTS);
        }
        // The tray is replaced here, with the other files, and started last: whether or not the service
        // then registers and starts, a tray this Install ended comes back (it says what the service does).
        let tray = (!in_place).then(|| self.update_tray(&dest_dir, COPY_ATTEMPTS));
        let result = self.register_and_start(&dest, &dest_dir);
        if let Some(tray) = tray {
            self.finish_tray(&dest_dir, tray);
        }
        result
    }

    /// `crossc-service.exe install` in the install folder, then the start.
    fn register_and_start(&self, dest: &Path, dest_dir: &Path) -> Result<(), String> {
        // The contract of crossc-service.exe: `install` registers the service, exit code 0 on success. Whether it
        // minds an existing registration is not promised, so the service control manager has the last word.
        let rc = self.run_cmd(dest, &["install"], dest_dir, &[])?;
        let after = service::query(USER_SERVICE)?;
        if !after.state.installed() {
            return Err(format!(
                "crossc-service install exited {rc} and the crossc service is not registered"
            ));
        }
        let ours = registered_with(after.exe.as_deref(), dest);
        match (rc, ours) {
            (0, true) => self.success(format!("service registered: {}", dest.display())),
            (0, false) => self.warn(format!(
                "the service is registered with {}, not {}",
                after.exe.as_deref().unwrap_or("an unreadable path"),
                dest.display()
            )),
            (n, true) => self.warn(format!(
                "crossc-service install exited {n}, but the service is registered with {}: going on",
                dest.display()
            )),
            (n, false) => return Err(format!("crossc-service install failed (exit {n})")),
        }

        self.step("Starting the crossc service");
        self.start_service()
    }

    fn start_service(&self) -> Result<(), String> {
        service::start(USER_SERVICE, &|m| self.info(m)).map_err(|e| {
            format!("{e}; see %ProgramData%\\crossc\\service.log and the Windows event log")
        })?;
        self.success("the crossc service is running");
        Ok(())
    }

    /// Copies one file, retrying for a few seconds: right after a service stops, its exe can still be locked.
    fn copy(&self, from: &Path, to: &Path) -> Result<(), String> {
        self.copy_with(from, to, COPY_ATTEMPTS)
    }

    fn copy_with(&self, from: &Path, to: &Path, attempts: u32) -> Result<(), String> {
        retry(attempts, Duration::from_millis(500), || {
            std::fs::copy(from, to)
        })
        .map_err(|e| format!("cannot copy {} to {}: {e}", from.display(), to.display()))?;
        self.success(format!("{} -> {}", from.display(), to.display()));
        Ok(())
    }

    /// Puts `crossc-cli.exe` next to the installed service. The CLI is a diagnostic extra, so nothing about it
    /// stops Install: not a bundle without it, and not a copy that fails because an older crossc-cli.exe is
    /// running from the install folder (the service still has to be registered and started after this).
    fn copy_cli(&self, dest_dir: &Path, attempts: u32) {
        self.copy_extra(&self.layout.cli_exe(), &dest_dir.join(CLI_EXE), attempts);
    }

    /// Copies a program Install can do without (the CLI, the tray). A source that is not there and a copy that
    /// fails are warnings; `true` when the file was copied.
    fn copy_extra(&self, source: &Path, dest: &Path, attempts: u32) -> bool {
        if !source.is_file() {
            self.warn(format!("{} not found: not copied", source.display()));
            return false;
        }
        let Err(e) = self.copy_with(source, dest, attempts) else {
            return true;
        };
        self.warn(extra_not_copied(&e, dest));
        false
    }

    /// Puts `crossc-tray.exe` next to the installed service, ending a running tray first so its file can be
    /// replaced. An extra like the CLI: a bundle without it (a release from before there was a tray) and a copy
    /// that fails are warnings. A tray that is installed already is only ended when there is a new one to put
    /// in its place.
    ///
    /// A tray that cannot be ended (another account's that Windows does not let this process open or end) does
    /// not keep the old exe in place: the file in use is moved aside and the new one put where it was, so the
    /// tray is the new one the next time it starts and Install need not be run again.
    fn update_tray(&self, dest_dir: &Path, attempts: u32) -> TrayStep {
        let source = self.layout.tray_exe();
        let dest = dest_dir.join(TRAY_EXE);
        let mut step = TrayStep {
            first_install: !dest.is_file(),
            was_running: false,
            still_running: false,
            copied: false,
        };
        if !source.is_file() {
            // Says that it is not there.
            step.copied = self.copy_extra(&source, &dest, attempts);
            return step;
        }
        // What an earlier Install moved aside is free once that tray has exited.
        remove_set_aside(&dest);
        let stopped = tray::stop(&dest);
        // Only a tray of this desktop is one to start again: another user's, in another session, says nothing
        // about whether the user here had theirs open.
        step.was_running = stopped.ended_here > 0;
        step.still_running = stopped.left_here > 0;
        if stopped.ended_here > 0 {
            self.info("closed the running crossc-tray to replace it");
        }
        if stopped.ended_elsewhere > 0 {
            self.info(format!(
                "closed crossc-tray in {} other session(s) (another signed-in user's) to replace it; it starts \
                 again when that user signs in, or opens it",
                stopped.ended_elsewhere
            ));
        }
        if let Some(e) = &stopped.error {
            self.warn(e);
        }
        match self.copy_with(&source, &dest, attempts) {
            Ok(()) => step.copied = true,
            Err(e) => match replace_in_use(&source, &dest) {
                Ok(aside) => {
                    step.copied = true;
                    // A tray that could not be ended, or could not even be looked at, is what holds the old
                    // file: it is still there, and a new one next to it would exit at once. (With no such
                    // tray the file was held by something else, and a tray that was ended comes back.)
                    step.still_running |= stopped.left_elsewhere > 0 || stopped.unreadable > 0;
                    self.success(format!("{} -> {}", source.display(), dest.display()));
                    self.warn(format!(
                        "{} was in use, so it was moved to {} and the new one put in its place. The old file is \
                         deleted by the next Install or Uninstall",
                        dest.display(),
                        aside.display()
                    ));
                }
                Err(_) => {
                    self.warn(extra_not_copied(&e, &dest));
                    // Only worth saying when the file could not be replaced: then such a tray is the likely
                    // reason.
                    if let Some(note) = stopped.left_note(&dest) {
                        self.warn(note);
                    }
                }
            },
        }
        step
    }

    /// After the service: has the tray start with Windows for the signed-in user (first install only) and
    /// starts it in that user's session, not elevated (`tray::tray_plan`). Nothing here stops Install; what
    /// could not be done is said with what to do by hand.
    fn finish_tray(&self, dest_dir: &Path, step: TrayStep) {
        let exe = dest_dir.join(TRAY_EXE);
        // Nothing new and nothing ended (a release without a tray, or a copy that failed): nothing to do. A
        // tray that was ended and could not be replaced is started again as it was.
        if !exe.is_file() || !(step.copied || step.was_running) {
            return;
        }
        let plan = tray::tray_plan(step.first_install, step.was_running, step.still_running);
        if step.still_running {
            // Starting one now would start nothing: the new process finds the old one and exits.
            self.warn(format!(
                "a crossc-tray that could not be closed is still running, so none is started. Exit it (its \
                 menu, Exit) and open it again: {}",
                exe.display()
            ));
        }
        if !plan.register && !plan.start {
            if !step.still_running {
                self.info(format!(
                    "crossc-tray is updated; it was not running and is left closed (to open it: {})",
                    exe.display()
                ));
            }
            return;
        }
        self.step("Starting the tray app");
        let by_hand = format!("Open the tray yourself: {}", exe.display());
        // This process is elevated and may run as another account than the one at the desktop: the tray is
        // registered and started for the desktop's user, with that user's rights, never as a child of this
        // process.
        let Some(user) = tray::desktop_user() else {
            self.warn(format!(
                "cannot tell who is signed in at this desktop, so crossc-tray is neither started nor set to \
                 start with Windows. {by_hand} (its menu has \"Start with Windows\")"
            ));
            return;
        };
        let mut registered = false;
        if plan.register {
            match tray::enable_autostart(&user, &exe) {
                Ok(()) => {
                    registered = true;
                    self.success(
                        "crossc-tray starts with Windows for the signed-in user (to change that: \"Start with \
                         Windows\" in its menu)",
                    );
                }
                Err(e) => self.warn(format!(
                    "cannot register crossc-tray to start with Windows: {e}. Switch it on in the tray's menu"
                )),
            }
        }
        if plan.start {
            match tray::start_as(&user, &exe) {
                Ok(()) => self.success(
                    "crossc-tray is started (not elevated): its icon is in the notification area",
                ),
                Err(e) if registered => self.warn(format!(
                    "{e}. {by_hand}; it also starts the next time you sign in"
                )),
                Err(e) => self.warn(format!("{e}. {by_hand}")),
            }
        }
    }

    /// Uninstall: ends the tray, deletes its exe and takes its autostart away. Whatever became of the service:
    /// the driver goes either way, and a tray without it has nothing to show.
    fn remove_tray(&self) {
        let exe = system::install_dir().join(TRAY_EXE);
        self.remove_tray_at(&exe);
    }

    fn remove_tray_at(&self, exe: &Path) {
        let stopped = tray::stop(exe);
        if stopped.ended() > 0 {
            self.success("closed the running crossc-tray");
        }
        if let Some(e) = &stopped.error {
            self.warn(e);
        }
        let left = stopped.left_note(exe);
        // What an Install moved aside because it was in use.
        remove_set_aside(exe);
        // The exe goes here and not only with the install folder: that folder stays when the service could
        // not be removed, and when this installer runs from inside it.
        if exe.is_file() {
            match retry(6, Duration::from_millis(500), || std::fs::remove_file(exe)) {
                Ok(()) => self.success(format!("deleted {}", exe.display())),
                Err(e) => {
                    self.warn(format!("cannot delete {}: {e}", exe.display()));
                    if let Some(note) = left {
                        self.warn(note);
                    }
                }
            }
        }
        let (removed, failed) = tray::remove_autostart(exe);
        if removed > 0 {
            self.success("crossc-tray no longer starts with Windows");
        }
        for e in failed {
            self.warn(format!(
                "cannot remove the autostart of crossc-tray for {e}. By hand, as that user: reg delete \
                 \"HKCU\\{}\" /v {} /f",
                tray::RUN_KEY,
                tray::RUN_VALUE
            ));
        }
    }

    // -----------------------------------------------------------------------------------------------------------
    // Uninstall / remove certificate (admin)
    // -----------------------------------------------------------------------------------------------------------

    fn uninstall(&mut self) -> Result<(), String> {
        self.require_admin()?;
        // Any package's INF tells which nodes are buds; uninstall must work whichever one is there.
        let ids = self
            .layout
            .packages(self.flavor)
            .iter()
            .map(|(_, dir)| system::package_hardware_ids(dir))
            .find(|ids| !ids.is_empty())
            .unwrap_or_default();
        if ids.is_empty() {
            self.warn(
                "no crossc_bth.inf to read the supported models from: only the nodes that run crossc_bth now \
                 will be verified",
            );
        }
        // The nodes to see back on the Windows driver: the package's models, and whatever else runs crossc_bth
        // (bound by another version of the package). Taken now, while the service name still tells them apart.
        let mut nodes = Vec::new();
        for found in [
            system::find_buds_nodes(&ids),
            system::find_unlisted_nodes(&ids),
        ] {
            match found {
                Ok(more) => nodes.extend(more),
                Err(e) => self.warn(e),
            }
        }
        let nodes = order_nodes(nodes);

        // Service first: it holds the buds' device open, and the driver cannot be swapped under it.
        self.step("Removing the crossc service");
        let service_result = self.remove_service();
        self.remove_tray();
        match &service_result {
            Ok(()) => self.remove_install_dir(),
            // A service that is still registered must keep its exe; the driver is removed all the same.
            Err(e) => self.error(format!(
                "{e} (leaving %ProgramFiles%\\crossc in place, going on with the driver)"
            )),
        }

        self.step("Uninstalling crossc_bth");
        let removed = self.remove_staged(None)?;
        // Nothing is installed any more. The certificate stays trusted until "Remove certificate", and so
        // does its record.
        self.clear_record(&[RECORD_VERSION, RECORD_DRIVER_VER]);

        // A rescan lets Windows put its own driver back on the nodes the package was taken from, whether or not
        // this installer could tell which nodes those are.
        if needs_rescan(removed, nodes.len()) {
            let _ = self.run_cmd(Path::new("pnputil"), &["/scan-devices"], &tool_dir(), &[])?;
        }
        if nodes.is_empty() {
            self.info("no supported buds are paired and no node ran crossc_bth: no node to check");
        } else {
            self.step("Verifying the buds' nodes");
            let after = wait_for_nodes(&nodes, |now| not_healthy_on(now, INBOX_SERVICE).is_empty());
            self.show_states(&nodes, &after);
            let left = not_healthy_on(&after, INBOX_SERVICE);
            if left.is_empty() {
                self.success("every buds node is back on BthA2dp");
            } else {
                let names: Vec<&str> = left.iter().map(|n| n.label()).collect();
                self.warn(format!(
                    "not on BthA2dp yet: {}. Device Manager -> node -> Update driver -> pick \
                     \"Microsoft Bluetooth A2dp Source\", or re-pair the buds",
                    names.join(", ")
                ));
            }
        }
        service_result
    }

    /// Stops and unregisters the service: `crossc-service.exe uninstall` when the exe is still there, the service
    /// control manager when it is not or when that did not do it.
    fn remove_service(&self) -> Result<(), String> {
        let info = service::query(USER_SERVICE)?;
        if !info.state.installed() {
            self.success("the crossc service is not installed");
            return Ok(());
        }
        if let Err(e) = service::stop(USER_SERVICE, &|m| self.info(m)) {
            self.warn(e);
        }
        let installed_exe = system::install_dir().join(SERVICE_EXE);
        let exe = [info.exe.map(PathBuf::from), Some(installed_exe)]
            .into_iter()
            .flatten()
            .find(|p| p.is_file());
        match &exe {
            Some(exe) => {
                let fallback = tool_dir();
                let dir = exe.parent().unwrap_or(&fallback);
                match self.run_cmd(exe, &["uninstall"], dir, &[]) {
                    Ok(0) => {}
                    Ok(rc) => self.warn(format!("crossc-service uninstall exited {rc}")),
                    Err(e) => self.warn(e),
                }
            }
            None => self.info("crossc-service.exe is gone"),
        }
        if service::query(USER_SERVICE)?.state.installed() {
            self.info("removing the service through the service control manager");
            service::delete(USER_SERVICE, &|m| self.info(m))?;
        }
        if service::query(USER_SERVICE)?.state.installed() {
            self.warn(
                "the crossc service is marked for deletion; it goes away once nothing has it open (close the \
                 Services console) or after a reboot",
            );
        } else {
            self.success("the crossc service is removed");
        }
        Ok(())
    }

    /// Deletes %ProgramFiles%\crossc. Not fatal when it fails: the service is gone by then, the files are inert.
    fn remove_install_dir(&self) {
        let dir = system::install_dir();
        if !dir.is_dir() {
            self.info(format!("{} does not exist", dir.display()));
            return;
        }
        if std::env::current_exe().is_ok_and(|me| is_inside(&me, &dir)) {
            self.warn(format!(
                "this installer runs from {}: not deleting it. Delete the folder by hand when you are done",
                dir.display()
            ));
            return;
        }
        match retry(10, Duration::from_millis(500), || {
            std::fs::remove_dir_all(&dir)
        }) {
            Ok(()) => self.success(format!("deleted {}", dir.display())),
            Err(e) => self.warn(format!("cannot delete {}: {e}", dir.display())),
        }
    }

    fn remove_cert(&mut self) -> Result<(), String> {
        self.require_admin()?;
        self.step("Untrusting the test certificate");
        // Try every store even if one fails, so a stuck Root copy does not leave the TrustedPublisher one behind.
        let mut failed = Vec::new();
        for name in TRUST_STORES {
            let store = match Store::local_machine(name, false) {
                Ok(s) => s,
                Err(e) => {
                    self.error(format!("cannot open LocalMachine\\{name}: {e}"));
                    failed.push(name);
                    continue;
                }
            };
            let r = store.remove_by_subject(TEST_CERT_SUBJECT);
            if r.removed.is_empty() && r.failed.is_empty() {
                self.info(format!("no {TEST_CERT_SUBJECT} in LocalMachine\\{name}"));
            }
            for tp in &r.removed {
                self.success(format!("removed {tp} from LocalMachine\\{name}"));
            }
            for (tp, e) in &r.failed {
                self.error(format!(
                    "could not remove {tp} from LocalMachine\\{name}: {e}"
                ));
            }
            if !r.failed.is_empty() {
                failed.push(name);
            }
        }
        if self.layout.mode == Mode::Repo {
            self.warn("the signing copy (with private key) stays in your user store WDRTestCertStore; see docs/SETUP.md 8.3");
        }
        if !failed.is_empty() {
            return Err(format!(
                "could not untrust the certificate in: {}",
                failed.join(", ")
            ));
        }
        // No crossc certificate is trusted any more: nothing for a later install to replace.
        self.clear_record(&[RECORD_CERT, RECORD_PENDING]);
        Ok(())
    }

    // -----------------------------------------------------------------------------------------------------------
    // Shared steps
    // -----------------------------------------------------------------------------------------------------------

    /// Removes the staged crossc_bth packages from the driver store and from the devices that use them: all of
    /// them, or all but `keep` (the published name of the package Install has just staged). Returns how many
    /// there were to remove.
    fn remove_staged(&mut self, keep: Option<&str>) -> Result<usize, String> {
        let pkgs = packages_to_remove(system::staged_packages()?, keep);
        if pkgs.is_empty() {
            self.success(match keep {
                Some(_) => "no older crossc_bth package staged",
                None => "no crossc_bth package staged",
            });
            return Ok(0);
        }
        let count = pkgs.len();
        for p in pkgs {
            self.step(format!("Removing staged package {}", p.published));
            let rc = self.run_cmd(
                Path::new("pnputil"),
                &["/delete-driver", &p.published, "/uninstall", "/force"],
                &tool_dir(),
                &[],
            )?;
            if let PnpExit::Failed(n) = pnputil_exit(rc) {
                self.warn(format!("pnputil /delete-driver {} exited {n}", p.published));
            }
        }
        Ok(count)
    }

    /// One block per node that was there before the action: its state now, or that it is gone.
    fn show_states(&self, before: &[NodeInfo], after: &[NodeInfo]) {
        for old in before {
            match after.iter().find(|n| n.instance_id == old.instance_id) {
                Some(n) => self.show_state(n),
                None => self.warn(format!(
                    "{} is gone (buds unpaired meanwhile): {}",
                    old.label(),
                    old.instance_id
                )),
            }
        }
    }

    fn show_state(&self, n: &NodeInfo) {
        let none = || "-".to_string();
        self.info(format!("{}   {}", n.label(), n.instance_id));
        self.info(format!(
            "  Service : {}",
            n.service.clone().unwrap_or_else(none)
        ));
        self.info(format!(
            "  Problem : {}",
            n.problem.map_or_else(none, |p| p.to_string())
        ));
        self.info(format!(
            "  INF     : {} (version {})",
            n.inf.clone().unwrap_or_else(none),
            n.version.clone().unwrap_or_else(none)
        ));
        self.info(format!("  Stack   : {}", n.stack_text()));
        if !n.present {
            self.info("  not present (stale pairing): not checked");
        }
        if let Some(code) = n.problem.filter(|&p| p != 0) {
            self.warn(problem_text(code));
        }
    }

    /// Runs a child process, streaming both pipes into the log; returns its exit code.
    fn run_cmd(
        &self,
        program: &Path,
        args: &[&str],
        dir: &Path,
        env: &[(&str, OsString)],
    ) -> Result<i32, String> {
        let name = program.file_stem().map_or_else(
            || program.display().to_string(),
            |s| s.to_string_lossy().into_owned(),
        );
        self.emit(
            LineKind::Info,
            format!("> {name} {}   (in {})", args.join(" "), dir.display()),
        );
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW)
            .env("CARGO_TERM_COLOR", "never");
        // Variables `cargo run` sets for this binary would leak into nested cargo / cargo-make runs.
        for (k, _) in std::env::vars_os() {
            if let Some(k) = k.to_str()
                && is_cargo_run_var(k)
            {
                cmd.env_remove(k);
            }
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", program.display()))?;

        let (tx, rx) = mpsc::channel::<String>();
        let readers: Vec<_> = [
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
        ]
        .into_iter()
        .flatten()
        .map(|pipe| {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(pipe);
                let mut buf = Vec::new();
                while reader.read_until(b'\n', &mut buf).is_ok_and(|n| n > 0) {
                    if tx.send(clean_line(&buf)).is_err() {
                        break;
                    }
                    buf.clear();
                }
            })
        })
        .collect();
        drop(tx);
        for line in rx {
            if !line.trim().is_empty() {
                self.emit(classify_output(&line), line);
            }
        }
        for r in readers {
            let _ = r.join();
        }
        let status = child
            .wait()
            .map_err(|e| format!("waiting for {name}: {e}"))?;
        Ok(status.code().unwrap_or(-1))
    }
}

/// One raw output line -> display text: lossy UTF-8, last carriage-return segment, no ANSI, no line ending.
fn clean_line(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    let s = s.trim_end_matches(['\n', '\r']);
    let s = s.rsplit('\r').next().unwrap_or("");
    strip_ansi(s)
}

fn is_cargo_run_var(k: &str) -> bool {
    const PREFIXES: [&str; 3] = ["CARGO_PKG_", "CARGO_BIN_", "CARGO_CRATE_"];
    PREFIXES.iter().any(|p| k.starts_with(p))
        || matches!(
            k,
            "CARGO_MANIFEST_DIR" | "CARGO_MANIFEST_PATH" | "CARGO_PRIMARY_PACKAGE" | "CARGO"
        )
}

/// The step line of a user-mode build: "Building crossc-cli and crossc-service (release)", from the `-p`
/// arguments of the cargo command.
fn build_step(cargo_args: &[&str]) -> String {
    let packages: Vec<&str> = cargo_args
        .windows(2)
        .filter(|pair| pair[0] == "-p")
        .map(|pair| pair[1])
        .collect();
    let list = match packages.as_slice() {
        [] => String::new(),
        [one] => one.to_string(),
        [first @ .., last] => format!("{} and {last}", first.join(", ")),
    };
    format!("Building {list} (release)")
}

/// What `Ctx::update_tray` found and did, for `Ctx::finish_tray` to decide by (`tray::tray_plan`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TrayStep {
    /// There was no `crossc-tray.exe` in the install folder before this Install.
    first_install: bool,
    /// A tray that ran at this desktop (in the installer's session) was ended to replace its exe.
    was_running: bool,
    /// A tray is still there: one at this desktop that could not be ended, or whatever kept the old exe in
    /// use.
    still_running: bool,
    /// The new exe is in place.
    copied: bool,
}

/// How the cargo run that builds the tray ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrayBuild {
    Built,
    /// It failed while a tray runs from the file it builds: cargo cannot replace a running program.
    InUse,
    Failed,
}

/// `rc`: cargo's exit code. `running`: a tray runs from the exe the build writes (`target\release`).
///
/// A failure next to a running tray is taken for the file in use and does not stop Build. Cargo says what it
/// was in the lines above either way, and with the tray closed a real error stops Build as any other does.
fn tray_build(rc: i32, running: bool) -> TrayBuild {
    match (rc, running) {
        (0, _) => TrayBuild::Built,
        (_, true) => TrayBuild::InUse,
        (_, false) => TrayBuild::Failed,
    }
}

fn tray_in_use_note(rc: i32, exe: &Path) -> String {
    format!(
        "cargo build -p crossc-tray failed (exit {rc}) while crossc-tray runs from {}: a running program \
         cannot be replaced. Going on with {}. To rebuild it, exit that tray (its menu, Exit) and Build again",
        exe.display(),
        if exe.is_file() {
            "the tray as it was built before"
        } else {
            "no tray"
        }
    )
}

/// The warning for a program Install can do without that could not be copied to `dest`; `e` is why.
fn extra_not_copied(e: &str, dest: &Path) -> String {
    let name = dest.file_stem().map_or_else(
        || dest.display().to_string(),
        |s| s.to_string_lossy().into_owned(),
    );
    format!(
        "{e}. Going on without it: {} is {}. Close any running {name} and run Install again to update it",
        dest.display(),
        if dest.is_file() {
            "left as it was"
        } else {
            "not installed"
        }
    )
}

/// How many set-aside names are tried for one file (each stays until the process that runs it has exited).
const SET_ASIDE_NAMES: u32 = 8;

/// The `n`th name a file in use is moved to: `crossc-tray.exe.old`, `crossc-tray.exe.old2`, ...
fn set_aside_path(file: &Path, n: u32) -> PathBuf {
    let mut name = file.file_name().unwrap_or_default().to_os_string();
    name.push(if n <= 1 {
        ".old".to_string()
    } else {
        format!(".old{n}")
    });
    file.with_file_name(name)
}

/// Deletes what `replace_in_use` moved aside for `file`, as far as it is free by now. Best effort: one that is
/// still running stays for the next time.
fn remove_set_aside(file: &Path) {
    for n in 1..=SET_ASIDE_NAMES {
        let _ = std::fs::remove_file(set_aside_path(file, n));
    }
}

/// Replaces `dest` with a copy of `source` although a process runs `dest`: Windows lets the file of a running
/// program be renamed, not overwritten or deleted. The old file is moved aside (the running process keeps
/// running it from there), the new one is copied to `dest`. Returns where the old one went. When the copy then
/// fails the old file is put back, so `dest` is never left missing.
fn replace_in_use(source: &Path, dest: &Path) -> Result<PathBuf, String> {
    if !dest.is_file() {
        return Err(format!("{} is not there", dest.display()));
    }
    let mut last = String::new();
    for n in 1..=SET_ASIDE_NAMES {
        let aside = set_aside_path(dest, n);
        // An older one that is free by now makes room; one still in use keeps its name.
        let _ = std::fs::remove_file(&aside);
        if aside.exists() {
            continue;
        }
        if let Err(e) = std::fs::rename(dest, &aside) {
            last = format!("cannot move {} aside: {e}", dest.display());
            break;
        }
        return match std::fs::copy(source, dest) {
            Ok(_) => Ok(aside),
            Err(e) => {
                let _ = std::fs::remove_file(dest);
                let _ = std::fs::rename(&aside, dest);
                Err(format!(
                    "cannot copy {} to {}: {e}",
                    source.display(),
                    dest.display()
                ))
            }
        };
    }
    if last.is_empty() {
        last = format!("no free name to move {} aside to", dest.display());
    }
    Err(last)
}

/// How often a copy into %ProgramFiles%\crossc is tried, half a second apart.
const COPY_ATTEMPTS: u32 = 10;

/// What Install does about the service once the driver is in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServicePlan<'a> {
    /// Copy this `crossc-service.exe`, register it, start it.
    Install(&'a Path),
    /// No exe to install (repo, not built) but a service is registered: start that one, so that Install never
    /// ends with a service it stopped itself, and "click Install to start it" is true.
    StartExisting,
    /// No exe and nothing registered: the driver is all there is.
    DriverOnly,
}

fn service_plan(exe: Option<&Path>, registered: bool) -> ServicePlan<'_> {
    match (exe, registered) {
        (Some(exe), _) => ServicePlan::Install(exe),
        (None, true) => ServicePlan::StartExisting,
        (None, false) => ServicePlan::DriverOnly,
    }
}

/// The line to log when Install stops with an error after it has stopped the service: `None` when the service
/// was not running before (nothing was taken away) or runs now. `now`: its state after the failure, when it can
/// be read.
fn service_left_note(was_active: bool, now: Option<ServiceState>) -> Option<String> {
    if !was_active || now == Some(ServiceState::Running) {
        return None;
    }
    let again = "Run Install again once the problem above is fixed";
    Some(match now {
        Some(ServiceState::NotInstalled) => format!(
            "the crossc service was running before this install and is not registered any more. {again}"
        ),
        Some(state) => format!(
            "the crossc service was running before this install and is not running now ({state}): nothing \
             streams to the buds. {again}, or start it by hand (admin): sc start {USER_SERVICE}"
        ),
        None => format!(
            "the crossc service was running before this install and may be left stopped (its state cannot be \
             read). {again}"
        ),
    })
}

/// Uninstall rescans the device tree when a package was removed (nodes lost their driver) or when it has nodes
/// to verify; only with nothing removed and nothing to look at is there no point.
fn needs_rescan(packages_removed: usize, nodes_to_verify: usize) -> bool {
    packages_removed > 0 || nodes_to_verify > 0
}

/// Polls the nodes for up to 6 s in all (PnP finishes binding asynchronously) until `done` holds for the whole
/// set; returns their last state. Nodes that are gone are left out.
fn wait_for_nodes(nodes: &[NodeInfo], done: impl Fn(&[NodeInfo]) -> bool) -> Vec<NodeInfo> {
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        let now = system::refresh_nodes(nodes);
        if done(&now) || Instant::now() >= deadline {
            return now;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// The working directory for tools that do not care about one (pnputil): the folder this installer is in. It
/// is there for as long as the installer runs, which the bundle folder is not: in online mode the bundle is the
/// installed release below `%ProgramFiles%\crossc`, and Uninstall deletes that folder before it removes the
/// driver package. A child process cannot be started in a folder that does not exist.
fn tool_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .filter(|dir| dir.is_dir())
        .unwrap_or_else(std::env::temp_dir)
}

/// Runs `op` up to `attempts` times, `pause` apart, until it succeeds; returns the last error otherwise.
fn retry<T, E>(
    attempts: u32,
    pause: Duration,
    mut op: impl FnMut() -> Result<T, E>,
) -> Result<T, E> {
    let mut tries = 1;
    loop {
        match op() {
            Err(_) if tries < attempts => {
                tries += 1;
                std::thread::sleep(pause);
            }
            result => return result,
        }
    }
}

/// Program Files as the system reports it (for finding LLVM; Build runs as the calling user).
fn program_files() -> PathBuf {
    system::program_files().unwrap_or_else(|| PathBuf::from(r"C:\Program Files"))
}

/// Path text for comparing: lower case, backslashes, no `\\?\` prefix, no trailing or doubled separator, and
/// `.` and `..` resolved as text (`..` never climbs above the first part, the drive).
pub fn path_key(p: &Path) -> String {
    let s = p.to_string_lossy().replace('/', "\\").to_lowercase();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    let mut parts: Vec<&str> = Vec::new();
    for part in s.split('\\') {
        match part {
            "." => {}
            ".." => {
                if parts.len() > 1 {
                    parts.pop();
                }
            }
            "" if !parts.is_empty() => {}
            part => parts.push(part),
        }
    }
    parts.join("\\")
}

/// `path` is `dir` or lies under it (compared as text, ignoring case; neither has to exist). `..` is resolved
/// first, so `C:\Program Files\..\Users\x` is not inside `C:\Program Files`. Links are not followed: where
/// that matters (`update::gh_to_run`) the paths are resolved by the file system before they get here.
pub fn is_inside(path: &Path, dir: &Path) -> bool {
    let (path, dir) = (path_key(path), path_key(dir));
    !dir.is_empty()
        && path
            .strip_prefix(&dir)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('\\'))
}

/// The service's registered executable is `exe` (compared as text, ignoring case).
fn registered_with(registered: Option<&str>, exe: &Path) -> bool {
    registered.is_some_and(|r| path_key(Path::new(r)) == path_key(exe))
}

/// Both paths name the same existing file (so a copy from one to the other would be pointless or destructive).
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => path_key(&a) == path_key(&b),
        _ => false,
    }
}

/// cargo.exe from PATH, else `%CARGO_HOME%\bin`, else `%USERPROFILE%\.cargo\bin` (an elevated relaunch may have a
/// different PATH).
fn find_cargo() -> Option<PathBuf> {
    let on_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|d| d.join("cargo.exe"));
    let homes = [
        std::env::var_os("CARGO_HOME").map(|h| PathBuf::from(h).join("bin").join("cargo.exe")),
        std::env::var_os("USERPROFILE").map(|h| {
            PathBuf::from(h)
                .join(".cargo")
                .join("bin")
                .join("cargo.exe")
        }),
    ];
    on_path
        .chain(homes.into_iter().flatten())
        .find(|p| p.is_file())
}

/// Relaunches this exe elevated ("runas") with `args`: one UAC prompt. Returns Ok when the new process was
/// started; the caller then quits. Never called automatically; call it off the UI thread.
pub fn restart_as_admin(layout: &Layout, args: &str) -> Result<(), String> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{HSTRING, w};

    // Runs on its own short-lived thread (it blocks on the UAC prompt); ShellExecute wants COM initialised there.
    unsafe {
        use windows::Win32::System::Com::{
            COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx,
        };
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let h = unsafe {
        ShellExecuteW(
            None,
            w!("runas"),
            &HSTRING::from(exe.as_os_str()),
            &HSTRING::from(args),
            &HSTRING::from(layout.root.as_os_str()),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecute returns a value > 32 on success (an HINSTANCE for compatibility only).
    if h.0 as usize > 32 {
        Ok(())
    } else {
        Err(format!(
            "elevation was cancelled or failed (code {})",
            h.0 as usize
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::tests::TempDir;
    use std::cell::{Cell, RefCell};
    use std::ffi::OsStr;

    #[test]
    fn clean_lines() {
        assert_eq!(clean_line(b"hello\r\n"), "hello");
        assert_eq!(
            clean_line(b"progress 10%\rprogress 100%\n"),
            "progress 100%"
        );
        assert_eq!(clean_line(b"\x1b[32mok\x1b[0m\n"), "ok");
        assert_eq!(clean_line(&[0x66, 0xff, 0x6f, b'\n']), "f\u{fffd}o");
    }

    #[test]
    fn cargo_run_vars() {
        assert!(is_cargo_run_var("CARGO_PKG_NAME"));
        assert!(is_cargo_run_var("CARGO_MANIFEST_DIR"));
        assert!(is_cargo_run_var("CARGO"));
        assert!(!is_cargo_run_var("CARGO_HOME"));
        assert!(!is_cargo_run_var("CARGO_TARGET_DIR"));
        assert!(!is_cargo_run_var("PATH"));
    }

    #[test]
    fn admin_actions() {
        assert!(!Action::Build.needs_admin());
        for a in [
            Action::Install,
            Action::BuildInstall,
            Action::Uninstall,
            Action::RemoveCert,
        ] {
            assert!(a.needs_admin());
        }
    }

    #[test]
    fn a_bundle_offers_no_build() {
        assert_eq!(
            actions_for(Mode::Repo),
            [
                Action::Build,
                Action::Install,
                Action::BuildInstall,
                Action::Uninstall,
                Action::RemoveCert
            ]
        );
        assert_eq!(
            actions_for(Mode::Bundle),
            [Action::Install, Action::Uninstall, Action::RemoveCert]
        );
        assert_eq!(actions_for(Mode::Online), actions_for(Mode::Bundle));
        assert_eq!(primary_action(Mode::Repo), Action::BuildInstall);
        assert_eq!(primary_action(Mode::Bundle), Action::Install);
        assert_eq!(primary_action(Mode::Online), Action::Install);
        for mode in [Mode::Repo, Mode::Bundle, Mode::Online] {
            assert!(actions_for(mode).contains(&primary_action(mode)));
            // The two halves of "Download and install" are never plain buttons.
            assert!(!actions_for(mode).contains(&Action::Download));
            assert!(!actions_for(mode).contains(&Action::InstallRelease));
        }
        // Every action button of a bundle and of the lone installer needs admin, and so do both halves of
        // "Download and install": nothing that changes the machine can run before the elevation check. (The
        // release check is not an action: it only reads, and works unelevated.)
        for mode in [Mode::Bundle, Mode::Online] {
            assert!(actions_for(mode).iter().all(|a| a.needs_admin()));
        }
        assert!(Action::Download.needs_admin() && Action::InstallRelease.needs_admin());
    }

    #[test]
    fn build_actions_are_refused_in_a_bundle() {
        let t = TempDir::new("bundle-build");
        let layout = Layout {
            mode: Mode::Bundle,
            root: t.0.clone(),
        };
        let lines = RefCell::new(Vec::<LogLine>::new());
        let log = |l: LogLine| lines.borrow_mut().push(l);
        for a in [Action::Build, Action::BuildInstall] {
            let err = run(a, &layout, Flavor::Debug, &log).unwrap_err();
            assert!(err.contains("release bundle"), "{err}");
        }
        // The halves of "Download and install" have their own entry points, in every mode.
        for layout in [&layout, &Layout::locate()] {
            for a in [Action::Download, Action::InstallRelease] {
                let err = run(a, layout, Flavor::Debug, &log).unwrap_err();
                assert!(err.contains("Download and install"), "{err}");
            }
        }
        // No cargo, no pnputil, no step.
        assert!(
            lines.borrow().iter().all(|l| l.kind == LineKind::Error),
            "{:?}",
            lines.borrow()
        );
    }

    #[test]
    fn install_dir_is_always_an_absolute_crossc_folder() {
        use crate::layout::install_dir;
        let dir = |v: &str| install_dir(Some(OsStr::new(v)));
        assert_eq!(
            dir(r"C:\Program Files"),
            PathBuf::from(r"C:\Program Files\crossc")
        );
        assert_eq!(dir(r"D:\Programs\"), PathBuf::from(r"D:\Programs\crossc"));
        // Unset, empty, relative, drive-relative, a bare root or a bare drive: never trusted.
        let fallback = PathBuf::from(r"C:\Program Files\crossc");
        assert_eq!(install_dir(None), fallback);
        for bad in [
            "",
            "Program Files",
            r"\Program Files",
            "C:",
            r"C:\",
            "C:Program Files",
            ".",
            // Nothing with `.` or `..` in it: such a value is not what the system reports, and where it leads
            // is not what it says.
            r"C:\Program Files\..\Users\Public",
            r"C:\Program Files\.\x",
            r"C:\..\Users\Public",
        ] {
            assert_eq!(dir(bad), fallback, "{bad:?}");
        }
        for v in [r"C:\Program Files", "", r"C:\", "x"] {
            let d = dir(v);
            assert!(d.is_absolute() && d.ends_with("crossc"), "{}", d.display());
            assert!(d.components().count() >= 4, "{}", d.display());
        }
    }

    #[test]
    fn inside_the_install_dir() {
        let dir = Path::new(r"C:\Program Files\crossc");
        assert!(is_inside(
            Path::new(r"C:\Program Files\crossc\crossc-installer.exe"),
            dir
        ));
        assert!(is_inside(
            Path::new(r"c:\program files\CROSSC\sub\x.exe"),
            dir
        ));
        assert!(is_inside(
            Path::new(r"\\?\C:\Program Files\crossc\x.exe"),
            dir
        ));
        assert!(is_inside(Path::new(r"C:/Program Files/crossc/x.exe"), dir));
        assert!(is_inside(dir, Path::new(r"C:\Program Files\crossc\")));
        // A sibling whose name merely starts the same, the parent, another drive.
        assert!(!is_inside(
            Path::new(r"C:\Program Files\crossc-old\x.exe"),
            dir
        ));
        assert!(!is_inside(Path::new(r"C:\Program Files\x.exe"), dir));
        assert!(!is_inside(Path::new(r"D:\Program Files\crossc\x.exe"), dir));
        assert!(!is_inside(Path::new(r"C:\x.exe"), Path::new("")));
        // `..` is resolved before comparing: a path that only starts with the folder's name is not inside it,
        // and one that comes back into it is.
        let program_files = Path::new(r"C:\Program Files");
        for outside in [
            r"C:\Program Files\..\Users\x\bin\gh.exe",
            r"C:\Program Files\crossc\..\..\Users\x\gh.exe",
            r"C:\Program Files\.\..\Windows\Tasks\gh.exe",
            r"C:/Program Files/../Users/x/gh.exe",
            r"\\?\C:\Program Files\..\Users\x\gh.exe",
        ] {
            assert!(!is_inside(Path::new(outside), program_files), "{outside}");
        }
        assert!(!is_inside(
            Path::new(r"C:\Program Files\crossc\..\other\x.exe"),
            dir
        ));
        for inside in [
            r"C:\Program Files\crossc\sub\..\x.exe",
            r"C:\Program Files\.\crossc\\x.exe",
            r"C:\Users\..\Program Files\crossc\x.exe",
            r"C:\..\..\Program Files\crossc\x.exe",
        ] {
            assert!(is_inside(Path::new(inside), dir), "{inside}");
        }
        assert!(is_inside(
            Path::new(r"C:\Program Files\crossc\x.exe"),
            Path::new(r"C:\Program Files\other\..\crossc")
        ));
    }

    #[test]
    fn service_registration_is_compared_ignoring_case() {
        let dest = Path::new(r"C:\Program Files\crossc\crossc-service.exe");
        assert!(registered_with(
            Some(r"c:\program files\crossc\CROSSC-SERVICE.EXE"),
            dest
        ));
        assert!(!registered_with(
            Some(r"C:\repo\target\release\crossc-service.exe"),
            dest
        ));
        assert!(!registered_with(None, dest));
    }

    #[test]
    fn same_file_needs_both_to_exist() {
        let t = TempDir::new("same-file");
        let a = t.file("a.exe", "x");
        let b = t.file(r"sub\b.exe", "x");
        assert!(same_file(&a, &a));
        assert!(same_file(&a, &t.0.join("sub").join("..").join("a.exe")));
        assert!(!same_file(&a, &b));
        assert!(!same_file(&a, &t.0.join("missing.exe")));
    }

    #[test]
    fn install_starts_a_registered_service_even_without_an_exe_to_install() {
        let exe = Path::new(r"C:\x\crossc-service.exe");
        assert_eq!(service_plan(Some(exe), false), ServicePlan::Install(exe));
        assert_eq!(service_plan(Some(exe), true), ServicePlan::Install(exe));
        // Repo, crossc-service.exe not built, a service registered by an earlier install: Install stopped it for
        // the driver swap, so Install starts it again instead of ending with "No service was installed".
        assert_eq!(service_plan(None, true), ServicePlan::StartExisting);
        assert_eq!(service_plan(None, false), ServicePlan::DriverOnly);
    }

    #[test]
    fn a_failed_install_says_when_it_leaves_the_service_stopped() {
        use ServiceState::*;
        // Whatever step failed after the stop: the service ran before and does not now.
        for now in [Stopped, StopPending, StartPending, Other(7)] {
            let note = service_left_note(true, Some(now)).unwrap();
            assert!(note.contains("was running before this install"), "{note}");
            assert!(note.contains(&now.to_string()), "{note}");
            assert!(note.contains("Run Install again"), "{note}");
            assert!(note.contains("sc start crossc"), "{note}");
        }
        let note = service_left_note(true, Some(NotInstalled)).unwrap();
        assert!(note.contains("not registered any more"), "{note}");
        assert!(!note.contains("sc start"), "{note}");
        let note = service_left_note(true, None).unwrap();
        assert!(note.contains("may be left stopped"), "{note}");
        // Nothing to say: it was not running before, or the stop itself failed and it still runs.
        for now in [None, Some(Stopped), Some(NotInstalled), Some(Running)] {
            assert_eq!(service_left_note(false, now), None);
        }
        assert_eq!(service_left_note(true, Some(Running)), None);
    }

    #[test]
    fn uninstall_rescans_whenever_a_package_was_removed() {
        // No node this installer could identify (no INF, or its models changed), but a package was removed.
        assert!(needs_rescan(1, 0));
        assert!(needs_rescan(0, 2));
        assert!(needs_rescan(2, 2));
        assert!(!needs_rescan(0, 0));
    }

    /// A `Ctx` over `layout` that collects the log.
    fn with_ctx<T>(layout: &Layout, f: impl FnOnce(&Ctx) -> T) -> (T, Vec<LogLine>) {
        let lines = RefCell::new(Vec::<LogLine>::new());
        let log = |l: LogLine| lines.borrow_mut().push(l);
        let out = f(&Ctx {
            layout,
            flavor: Flavor::Debug,
            log: &log,
            release: None,
        });
        (out, lines.into_inner())
    }

    fn kinds(lines: &[LogLine]) -> Vec<LineKind> {
        lines.iter().map(|l| l.kind).collect()
    }

    #[test]
    fn a_locked_cli_does_not_stop_the_install() {
        use std::os::windows::fs::OpenOptionsExt;
        let t = TempDir::new("cli-copy");
        let layout = Layout {
            mode: Mode::Bundle,
            root: t.0.join("bundle"),
        };
        let dest_dir = t.0.join("installed");
        std::fs::create_dir_all(&dest_dir).unwrap();
        let dest = t.file(r"installed\crossc-cli.exe", "old");

        // A bundle without the CLI: a warning, nothing copied.
        let ((), lines) = with_ctx(&layout, |cx| cx.copy_cli(&dest_dir, 1));
        assert_eq!(kinds(&lines), [LineKind::Warning], "{lines:?}");
        assert!(lines[0].text.contains("not found"), "{lines:?}");

        // The installed CLI is in use (no sharing, like a running exe refuses writers): a warning, the old file
        // stays, and the caller goes on to register and start the service.
        t.file(r"bundle\crossc-cli.exe", "new");
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&dest)
            .unwrap();
        let ((), lines) = with_ctx(&layout, |cx| cx.copy_cli(&dest_dir, 2));
        assert_eq!(kinds(&lines), [LineKind::Warning], "{lines:?}");
        assert!(lines[0].text.contains("cannot copy"), "{lines:?}");
        assert!(
            lines[0].text.contains("Going on without it")
                && lines[0].text.contains("left as it was"),
            "{lines:?}"
        );
        drop(lock);
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "old");

        // Free again: copied. The service exe, by contrast, must copy or Install stops.
        let ((), lines) = with_ctx(&layout, |cx| cx.copy_cli(&dest_dir, 1));
        assert_eq!(kinds(&lines), [LineKind::Success], "{lines:?}");
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new");
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&dest)
            .unwrap();
        let (r, _) = with_ctx(&layout, |cx| cx.copy_with(&layout.cli_exe(), &dest, 1));
        assert!(r.unwrap_err().starts_with("cannot copy"));
        drop(lock);
    }

    #[test]
    fn the_tray_is_an_extra_and_install_knows_a_first_install_from_an_update() {
        let t = TempDir::new("tray-copy");
        let layout = Layout {
            mode: Mode::Bundle,
            root: t.0.join("bundle"),
        };
        let dest_dir = t.0.join("installed");
        std::fs::create_dir_all(&dest_dir).unwrap();
        let dest = dest_dir.join("crossc-tray.exe");

        // A release from before there was a tray: a warning, nothing installed, nothing to start.
        let (step, lines) = with_ctx(&layout, |cx| cx.update_tray(&dest_dir, 1));
        assert_eq!(kinds(&lines), [LineKind::Warning], "{lines:?}");
        assert!(lines[0].text.contains("not found"), "{lines:?}");
        assert!(step.first_install && !step.was_running && !step.copied);
        let ((), lines) = with_ctx(&layout, |cx| cx.finish_tray(&dest_dir, step));
        assert!(lines.is_empty(), "{lines:?}");

        // The first install copies it; no tray runs from the install folder, so none is ended.
        t.file(r"bundle\crossc-tray.exe", "new");
        let (step, lines) = with_ctx(&layout, |cx| cx.update_tray(&dest_dir, 1));
        assert_eq!(kinds(&lines), [LineKind::Success], "{lines:?}");
        assert_eq!(
            step,
            TrayStep {
                first_install: true,
                was_running: false,
                still_running: false,
                copied: true
            }
        );
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new");

        // An update: the exe is replaced, and Install knows not to touch the user's autostart setting. With
        // no tray running before, it is not started either: one line, no registry, no process.
        t.file(r"bundle\crossc-tray.exe", "newer");
        let (step, _) = with_ctx(&layout, |cx| cx.update_tray(&dest_dir, 1));
        assert_eq!(
            step,
            TrayStep {
                first_install: false,
                was_running: false,
                still_running: false,
                copied: true
            }
        );
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "newer");
        let ((), lines) = with_ctx(&layout, |cx| cx.finish_tray(&dest_dir, step));
        assert_eq!(kinds(&lines), [LineKind::Info], "{lines:?}");
        assert!(lines[0].text.contains("left closed"), "{lines:?}");

        // The installed tray cannot be replaced: a warning, the old one stays, Install goes on.
        use std::os::windows::fs::OpenOptionsExt;
        t.file(r"bundle\crossc-tray.exe", "newest");
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&dest)
            .unwrap();
        let (step, lines) = with_ctx(&layout, |cx| cx.update_tray(&dest_dir, 1));
        assert_eq!(kinds(&lines), [LineKind::Warning], "{lines:?}");
        assert!(
            lines[0].text.contains("Close any running crossc-tray"),
            "{lines:?}"
        );
        assert!(!step.copied && !step.was_running && !step.still_running);
        drop(lock);
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "newer");
        assert!(!set_aside_path(&dest, 1).exists());
        let ((), lines) = with_ctx(&layout, |cx| cx.finish_tray(&dest_dir, step));
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn nothing_is_started_and_nothing_called_started_next_to_a_tray_that_would_not_go() {
        // The stop failed: the old tray is still there. A new one would exit at once on finding it, so none
        // is started and the log does not say that one was. (Reaches no registry and no process: the plan
        // has nothing to register or start.)
        let t = TempDir::new("tray-stuck");
        let layout = Layout {
            mode: Mode::Bundle,
            root: t.0.join("bundle"),
        };
        let dest_dir = t.0.join("installed");
        t.file(r"installed\crossc-tray.exe", "old");
        for copied in [false, true] {
            let step = TrayStep {
                first_install: false,
                was_running: true,
                still_running: true,
                copied,
            };
            let ((), lines) = with_ctx(&layout, |cx| cx.finish_tray(&dest_dir, step));
            assert_eq!(kinds(&lines), [LineKind::Warning], "{lines:?}");
            assert!(
                lines[0].text.contains("still running")
                    && lines[0].text.contains("none is started"),
                "{lines:?}"
            );
            assert!(
                !lines.iter().any(|l| l.text.contains("is started (")),
                "{lines:?}"
            );
        }
    }

    #[test]
    fn a_tray_exe_that_is_in_use_is_moved_aside_and_replaced() {
        // A copy of this test program stands in for a tray that cannot be ended (another account's): it runs
        // from the install folder, so its file can be neither overwritten nor deleted, only renamed.
        let t = TempDir::new("tray-aside");
        let layout = Layout {
            mode: Mode::Bundle,
            root: t.0.join("bundle"),
        };
        let dest_dir = t.0.join("installed");
        std::fs::create_dir_all(&dest_dir).unwrap();
        let dest = dest_dir.join("crossc-tray-stand-in.exe");
        std::fs::copy(std::env::current_exe().unwrap(), &dest).unwrap();
        let source = t.file(r"bundle\crossc-tray-stand-in.exe", "new");
        let mut child = std::process::Command::new(&dest)
            .args(["--exact", "tray::tests::sleeper_for_the_stop_test"])
            .env("CROSSC_TEST_SLEEP", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while tray::running_count(&dest) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(tray::running_count(&dest), 1);
        // What Install could do before: nothing. The copy fails for as long as the process lives.
        let (r, _) = with_ctx(&layout, |cx| cx.copy_with(&source, &dest, 1));
        assert!(r.unwrap_err().starts_with("cannot copy"));

        let aside = replace_in_use(&source, &dest).unwrap();
        assert_eq!(aside, set_aside_path(&dest, 1));
        assert_eq!(aside, dest_dir.join("crossc-tray-stand-in.exe.old"));
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new");
        assert!(aside.is_file());
        // The process goes on, from the file that was moved.
        assert!(child.try_wait().unwrap().is_none());
        // While it runs the old file stays, also across another Install; a second replacement takes the
        // next name.
        remove_set_aside(&dest);
        assert!(aside.is_file());
        std::fs::write(&source, "newer").unwrap();
        // (The new file is not in use, so the plain copy does it.)
        let (r, _) = with_ctx(&layout, |cx| cx.copy_with(&source, &dest, 1));
        r.unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "newer");
        assert_eq!(
            set_aside_path(&dest, 2),
            dest_dir.join("crossc-tray-stand-in.exe.old2")
        );

        // Once that tray has exited, the next Install or Uninstall takes the old file away.
        child.kill().unwrap();
        child.wait().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while aside.exists() && std::time::Instant::now() < deadline {
            remove_set_aside(&dest);
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!aside.exists());
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "newer");

        // A file that is locked outright (no process: a scanner, say) cannot be renamed either: an error,
        // and the old file is where it was.
        use std::os::windows::fs::OpenOptionsExt;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&dest)
            .unwrap();
        let e = replace_in_use(&source, &dest).unwrap_err();
        assert!(e.contains("aside"), "{e}");
        drop(lock);
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "newer");
        assert!(!aside.exists());
        // Nothing there to replace: not this function's case.
        assert!(replace_in_use(&source, &dest_dir.join("absent.exe")).is_err());
        // A source that cannot be copied: the old file is put back.
        let e = replace_in_use(&t.0.join("no-such-source.exe"), &dest).unwrap_err();
        assert!(e.starts_with("cannot copy"), "{e}");
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "newer");
        assert!(!aside.exists());
    }

    #[test]
    fn a_tray_that_runs_from_the_checkout_does_not_stop_the_build() {
        // Built, whatever runs.
        assert_eq!(tray_build(0, false), TrayBuild::Built);
        assert_eq!(tray_build(0, true), TrayBuild::Built);
        // Cargo cannot replace the exe of a running tray (exit 101): a warning, and Build goes on to what
        // it built before; the service and the CLI are built by then.
        assert_eq!(tray_build(101, true), TrayBuild::InUse);
        // No tray in the way: a compile error is one.
        assert_eq!(tray_build(101, false), TrayBuild::Failed);
        assert_eq!(tray_build(1, false), TrayBuild::Failed);

        let t = TempDir::new("tray-build");
        let exe = t.file(r"target\release\crossc-tray.exe", "built before");
        let note = tray_in_use_note(101, &exe);
        assert!(
            note.contains("(exit 101)")
                && note.contains("a running program cannot be replaced")
                && note.contains("the tray as it was built before")
                && note.contains("Build again"),
            "{note}"
        );
        assert!(tray_in_use_note(101, &t.0.join("none.exe")).contains("Going on with no tray"));
        // No tray runs from a folder that was just made up.
        assert_eq!(tray::running_count(&exe), 0);
    }

    #[test]
    fn uninstall_deletes_the_tray_exe_itself() {
        // Not left to the removal of the install folder, which is skipped when the installer runs from it.
        // (No tray runs from this folder and no Run value names it: nothing is ended, no registry is written.)
        let t = TempDir::new("tray-remove");
        let layout = Layout {
            mode: Mode::Bundle,
            root: t.0.clone(),
        };
        let exe = t.file(r"installed\crossc-tray.exe", "tray");
        let ((), lines) = with_ctx(&layout, |cx| cx.remove_tray_at(&exe));
        assert_eq!(kinds(&lines), [LineKind::Success], "{lines:?}");
        assert!(lines[0].text.contains("deleted"), "{lines:?}");
        assert!(!exe.exists());
        // Nothing there: nothing to say.
        let ((), lines) = with_ctx(&layout, |cx| cx.remove_tray_at(&exe));
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn the_build_step_names_what_is_built() {
        assert_eq!(
            build_step(&["build", "--release", "-p", "crossc-cli"]),
            "Building crossc-cli (release)"
        );
        assert_eq!(
            build_step(&[
                "build",
                "--release",
                "-p",
                "crossc-cli",
                "-p",
                "crossc-service"
            ]),
            "Building crossc-cli and crossc-service (release)"
        );
        assert_eq!(
            build_step(&[
                "build",
                "--release",
                "-p",
                "crossc-cli",
                "-p",
                "crossc-service",
                "-p",
                "crossc-tray"
            ]),
            "Building crossc-cli, crossc-service and crossc-tray (release)"
        );
        // The tray is built in a cargo run of its own.
        assert_eq!(
            build_step(&["build", "--release", "-p", "crossc-tray"]),
            "Building crossc-tray (release)"
        );
    }

    #[test]
    fn a_damaged_service_exe_stops_a_bundle_install_in_preflight() {
        let t = TempDir::new("service-source");
        let bundle = Layout {
            mode: Mode::Bundle,
            root: t.0.join("bundle"),
        };
        // Missing.
        let (r, lines) = with_ctx(&bundle, |cx| cx.service_source());
        let err = r.unwrap_err();
        assert!(
            err.contains("crossc-service.exe is missing")
                && err.contains("unpack the whole release zip"),
            "{err}"
        );
        assert!(lines.is_empty(), "{lines:?}");
        // The zero-byte placeholder, then something that is not a program.
        t.file(r"bundle\crossc-service.exe", "");
        let (r, _) = with_ctx(&bundle, |cx| cx.service_source());
        let err = r.unwrap_err();
        assert!(
            err.contains("crossc-service.exe is damaged: the file is empty")
                && err.contains("unpack the whole release zip"),
            "{err}"
        );
        t.file(r"bundle\crossc-service.exe", "<html>404</html>");
        let (r, _) = with_ctx(&bundle, |cx| cx.service_source());
        assert!(
            r.unwrap_err()
                .contains("is damaged: it is not a Windows executable")
        );
        // A real executable passes.
        let exe = t.0.join("bundle").join("crossc-service.exe");
        std::fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
        let (r, lines) = with_ctx(&bundle, |cx| cx.service_source());
        assert_eq!(r, Ok(Some(exe)));
        assert_eq!(kinds(&lines), [LineKind::Success], "{lines:?}");

        // In the repo neither is fatal: a warning, and Install goes on without an exe to install.
        let repo = Layout {
            mode: Mode::Repo,
            root: t.0.join("repo"),
        };
        let (r, lines) = with_ctx(&repo, |cx| cx.service_source());
        assert_eq!(r, Ok(None));
        assert_eq!(kinds(&lines), [LineKind::Warning], "{lines:?}");
        assert!(lines[0].text.contains("is not built"), "{lines:?}");
        assert!(
            lines[0]
                .text
                .contains("starts the service if one is registered already"),
            "{lines:?}"
        );
        t.file(r"repo\target\release\crossc-service.exe", "");
        let (r, lines) = with_ctx(&repo, |cx| cx.service_source());
        assert_eq!(r, Ok(None));
        assert!(
            lines[0].text.contains("is damaged: the file is empty"),
            "{lines:?}"
        );
    }

    #[test]
    fn retry_stops_at_the_first_success_or_after_the_last_attempt() {
        let calls = Cell::new(0);
        let r: Result<u32, &str> = retry(5, Duration::ZERO, || {
            calls.set(calls.get() + 1);
            if calls.get() < 3 { Err("busy") } else { Ok(7) }
        });
        assert_eq!((r, calls.get()), (Ok(7), 3));

        calls.set(0);
        let r: Result<u32, u32> = retry(4, Duration::ZERO, || {
            calls.set(calls.get() + 1);
            Err(calls.get())
        });
        assert_eq!((r, calls.get()), (Err(4), 4));
    }

    #[test]
    fn tools_never_run_in_a_folder_that_uninstall_deletes() {
        // Online mode after an install: the bundle is the installed release below %ProgramFiles%\crossc, and
        // Uninstall deletes that folder before it removes the driver package. Stand-in: a folder that is gone.
        let t = TempDir::new("tool-dir");
        let layout = Layout {
            mode: Mode::Online,
            root: t.0.join("releases").join("v0.2.0"),
        };
        assert!(!layout.root.exists());
        let cmd = Path::new(r"C:\Windows\System32\cmd.exe");
        // A child process cannot be started there...
        let (r, _) = with_ctx(&layout, |cx| {
            cx.run_cmd(cmd, &["/d", "/c", "echo started"], &layout.root, &[])
        });
        assert!(r.unwrap_err().starts_with("cannot start"));
        // ...so pnputil is started in the installer's own folder, which exists whatever the layout says.
        let dir = tool_dir();
        assert!(dir.is_dir(), "{}", dir.display());
        assert!(!is_inside(&dir, &layout.root));
        let (r, lines) = with_ctx(&layout, |cx| {
            cx.run_cmd(cmd, &["/d", "/c", "echo started"], &tool_dir(), &[])
        });
        assert_eq!(r, Ok(0));
        assert!(lines.iter().any(|l| l.text == "started"), "{lines:?}");
    }

    #[test]
    fn run_cmd_streams_both_pipes_and_exit_code() {
        let lines = RefCell::new(Vec::<LogLine>::new());
        let log = |l: LogLine| lines.borrow_mut().push(l);
        let layout = Layout::locate();
        let cx = Ctx {
            layout: &layout,
            flavor: Flavor::Debug,
            log: &log,
            release: None,
        };
        let rc = cx
            .run_cmd(
                Path::new("cmd"),
                &[
                    "/d",
                    "/c",
                    "echo out-line& echo error: err-line 1>&2& exit /b 3",
                ],
                &layout.root,
                &[],
            )
            .unwrap();
        assert_eq!(rc, 3);
        let lines = lines.into_inner();
        assert!(
            lines
                .iter()
                .any(|l| l.text == "out-line" && l.kind == LineKind::Output),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.text.starts_with("error: err-line") && l.kind == LineKind::Error),
            "{lines:?}"
        );
    }

    #[test]
    fn admin_only_actions_refuse_unelevated() {
        if system::is_admin() {
            return; // only meaningful in the normal, unelevated test run
        }
        let t = TempDir::new("refuse");
        let layouts = [
            Layout::locate(),
            Layout {
                mode: Mode::Bundle,
                root: t.0.clone(),
            },
        ];
        for layout in &layouts {
            let lines = RefCell::new(Vec::<LogLine>::new());
            let log = |l: LogLine| lines.borrow_mut().push(l);
            for a in [Action::Install, Action::Uninstall, Action::RemoveCert] {
                let err = run(a, layout, Flavor::Debug, &log).unwrap_err();
                assert!(err.contains("administrator"), "{err}");
            }
            // Nothing was started and no step began: no "> pnputil" / "> crossc-service" line, no "==>" line
            // (service stop, copy, delete and the driver steps all sit behind one), only the three refusals.
            let lines = lines.into_inner();
            assert_eq!(lines.len(), 3, "{lines:?}");
            assert!(lines.iter().all(|l| l.kind == LineKind::Error), "{lines:?}");
        }
    }

    fn latest(version: &str) -> LatestRelease {
        let v = crate::release::Version::parse(version).unwrap();
        let asset = |name: &str| crate::release::Asset {
            name: name.to_string(),
            api_url: "https://api.github.com/repos/mrquantumoff/crossc/releases/assets/1".into(),
            size: 1,
            sha256: None,
        };
        LatestRelease {
            tag: v.tag(),
            assets: Ok(crate::release::ReleaseAssets {
                zip: asset(&crate::release::zip_asset_name(&v)),
                sums: asset(crate::release::SUMS_NAME),
            }),
            version: v,
        }
    }

    #[test]
    fn download_and_install_refuse_unelevated_before_any_request() {
        if system::is_admin() {
            return; // only meaningful in the normal, unelevated test run
        }
        let lines = RefCell::new(Vec::<LogLine>::new());
        let log = |l: LogLine| lines.borrow_mut().push(l);
        let err = fetch_release(&latest("0.2.0"), &log).unwrap_err();
        assert!(err.contains("administrator"), "{err}");
        let t = TempDir::new("refuse-release");
        let fetched = FetchedRelease {
            repo: "mrquantumoff/crossc".into(),
            tag: "v0.2.0".into(),
            version: crate::release::Version::parse("0.2.0").unwrap(),
            asset: "crossc-0.2.0-x64.zip".into(),
            sha256: "ab".repeat(32),
            dir: t.0.clone(),
            cert_subject: TEST_CERT_SUBJECT.into(),
            cert_thumbprint: crate::cert::tests::WDR_A_THUMBPRINT.into(),
        };
        let err = install_release(&fetched, Flavor::Debug, &log).unwrap_err();
        assert!(err.contains("administrator"), "{err}");
        // No step began: no download, no folder created, nothing trusted. Only the two refusals.
        let lines = lines.into_inner();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines.iter().all(|l| l.kind == LineKind::Error), "{lines:?}");
        assert!(std::fs::read_dir(&t.0).unwrap().next().is_none());
    }

    /// The test bundle of `update::tests`, unpacked into `<dir>\v0.2.0`.
    fn unpacked_release(t: &TempDir) -> update::Unpacked {
        let dir = t.0.join("releases").join("v0.2.0");
        std::fs::create_dir_all(&dir).unwrap();
        let zip = update::tests::bundle_zip("crossc-0.2.0-x64");
        let (files, _) = update::extract_bundle(
            std::io::Cursor::new(zip),
            &dir,
            "crossc-0.2.0-x64",
            u64::MAX / 2,
            100,
        )
        .unwrap();
        update::Unpacked {
            dir,
            asset: "crossc-0.2.0-x64.zip".into(),
            sha256: "ab".repeat(32),
            files,
        }
    }

    #[test]
    fn an_unpacked_release_is_inspected_before_it_is_offered() {
        use crate::cert::tests::{OTHER, WDR_A_THUMBPRINT};
        let t = TempDir::new("inspect");
        let unpacked = unpacked_release(&t);
        let release = latest("0.2.0");
        let inspect = || inspect_release(&unpacked, &release, "mrquantumoff/crossc");
        // Everything there: what the confirmation shows.
        assert_eq!(
            inspect(),
            Ok(FetchedRelease {
                repo: "mrquantumoff/crossc".into(),
                tag: "v0.2.0".into(),
                version: crate::release::Version::parse("0.2.0").unwrap(),
                asset: "crossc-0.2.0-x64.zip".into(),
                sha256: "ab".repeat(32),
                dir: unpacked.dir.clone(),
                cert_subject: "CN=WDRLocalTestCert".into(),
                cert_thumbprint: WDR_A_THUMBPRINT.into(),
            })
        );

        let driver = unpacked.dir.join("driver");
        let service = unpacked.dir.join("crossc-service.exe");
        let restore = |path: &Path, bytes: &[u8]| std::fs::write(path, bytes).unwrap();
        // A certificate with another subject is never offered.
        let cer = driver.join("WDRLocalTestCert.cer");
        let good_cer = std::fs::read(&cer).unwrap();
        restore(&cer, OTHER);
        let err = inspect().unwrap_err();
        assert!(
            err.contains("\"CN=SomebodyElse\", not CN=WDRLocalTestCert"),
            "{err}"
        );
        assert!(err.contains("could not remove it again"), "{err}");
        restore(&cer, b"junk");
        assert!(inspect().unwrap_err().contains("is not a certificate"));
        restore(&cer, &good_cer);

        // A service exe that is cut short or missing.
        let good_exe = std::fs::read(&service).unwrap();
        restore(&service, &good_exe[..good_exe.len() / 2]);
        assert!(
            inspect()
                .unwrap_err()
                .contains("crossc-service.exe of release v0.2.0 is damaged")
        );
        std::fs::remove_file(&service).unwrap();
        assert_eq!(
            inspect().unwrap_err(),
            "release v0.2.0 has no crossc-service.exe"
        );
        restore(&service, &good_exe);

        // An INF without a supported model, then driver files missing.
        let inf = driver.join("crossc_bth.inf");
        let good_inf = std::fs::read(&inf).unwrap();
        restore(&inf, b"[Version]\r\nDriverVer = 11/11/2026,1.2.3.4\r\n");
        assert!(
            inspect()
                .unwrap_err()
                .contains("lists no supported buds model")
        );
        restore(&inf, &good_inf);
        std::fs::remove_file(driver.join("crossc_bth.sys")).unwrap();
        std::fs::remove_file(driver.join("crossc_bth.cat")).unwrap();
        assert_eq!(
            inspect().unwrap_err(),
            r"release v0.2.0 has no driver\crossc_bth.sys, crossc_bth.cat"
        );
    }

    #[test]
    fn only_the_confirmed_certificate_is_accepted() {
        use crate::cert::tests::{WDR_A, WDR_A_THUMBPRINT, WDR_B, WDR_B_THUMBPRINT};
        let t = TempDir::new("confirmed");
        let cer = t.0.join("WDRLocalTestCert.cer");
        std::fs::write(&cer, WDR_A).unwrap();
        assert_eq!(confirmed_cert(&cer, WDR_A_THUMBPRINT), Ok(()));
        assert_eq!(
            confirmed_cert(&cer, &WDR_A_THUMBPRINT.to_lowercase()),
            Ok(())
        );
        // The file was swapped after the confirmation (same subject, another certificate).
        std::fs::write(&cer, WDR_B).unwrap();
        let err = confirmed_cert(&cer, WDR_A_THUMBPRINT).unwrap_err();
        assert!(
            err.contains(WDR_B_THUMBPRINT) && err.contains(WDR_A_THUMBPRINT),
            "{err}"
        );
        assert!(err.contains("Nothing was trusted"), "{err}");
        std::fs::write(&cer, b"junk").unwrap();
        assert!(
            confirmed_cert(&cer, WDR_A_THUMBPRINT)
                .unwrap_err()
                .starts_with("cannot parse")
        );
        assert!(
            confirmed_cert(&t.0.join("gone.cer"), WDR_A_THUMBPRINT)
                .unwrap_err()
                .starts_with("cannot read")
        );

        // The same check sits in the trust step itself: with a release to install, a certificate file that is
        // not the confirmed one stops it before any store is opened (so this runs unelevated).
        std::fs::write(&cer, WDR_B).unwrap();
        let fetched = FetchedRelease {
            repo: "mrquantumoff/crossc".into(),
            tag: "v0.2.0".into(),
            version: crate::release::Version::parse("0.2.0").unwrap(),
            asset: "crossc-0.2.0-x64.zip".into(),
            sha256: "ab".repeat(32),
            dir: t.0.clone(),
            cert_subject: TEST_CERT_SUBJECT.into(),
            cert_thumbprint: WDR_A_THUMBPRINT.into(),
        };
        let layout = Layout {
            mode: Mode::Bundle,
            root: t.0.clone(),
        };
        let lines = RefCell::new(Vec::<LogLine>::new());
        let log = |l: LogLine| lines.borrow_mut().push(l);
        let cx = Ctx {
            layout: &layout,
            flavor: Flavor::Debug,
            log: &log,
            release: Some(&fetched),
        };
        let err = cx.trust_cert(&cer).unwrap_err();
        assert!(
            err.contains("not the confirmed") && err.contains("nothing was trusted"),
            "{err}"
        );
        assert!(lines.borrow().is_empty());
    }

    #[test]
    fn older_downloads_are_deleted_after_an_install() {
        let t = TempDir::new("prune");
        let releases = t.0.join("releases");
        t.file(r"releases\v0.1.0\crossc-installer.exe", "old");
        t.file(r"releases\v0.1.0\driver\crossc_bth.inf", "old");
        t.file(r"releases\v0.2.0\crossc-installer.exe", "current");
        t.file(r"releases\v0.3.0.partial\half.txt", "half");
        t.file(r"releases\v0.3.0.download", "half a zip");
        t.file(r"keep-me.txt", "not in the releases folder");
        let (mut removed, failed) = prune_releases(&releases, "v0.2.0", None);
        removed.sort();
        assert_eq!(removed, ["v0.1.0", "v0.3.0.download", "v0.3.0.partial"]);
        assert!(failed.is_empty(), "{failed:?}");
        assert!(
            releases
                .join("v0.2.0")
                .join("crossc-installer.exe")
                .is_file()
        );
        assert!(t.0.join("keep-me.txt").is_file());
        assert_eq!(std::fs::read_dir(&releases).unwrap().count(), 1);
        // Again: nothing left to delete. A releases folder that does not exist: nothing either.
        assert_eq!(prune_releases(&releases, "v0.2.0", None), (vec![], vec![]));
        assert_eq!(
            prune_releases(&t.0.join("none"), "v0.2.0", None),
            (vec![], vec![])
        );

        // The release the installer itself was started from is left alone, although it is not the current one.
        t.file(r"releases\v0.1.0\crossc-installer.exe", "running");
        t.file(r"releases\v0.0.9\crossc-installer.exe", "older");
        let me = releases.join("v0.1.0").join("crossc-installer.exe");
        let (removed, failed) = prune_releases(&releases, "v0.2.0", Some(&me));
        assert_eq!((removed, failed), (vec!["v0.0.9".to_string()], vec![]));
        assert!(me.is_file());

        // Discarding an unconfirmed download only ever deletes a folder directly below the real releases
        // folder: anything else (here: a temp folder) is not touched.
        let fetched = FetchedRelease {
            repo: "mrquantumoff/crossc".into(),
            tag: "v0.2.0".into(),
            version: crate::release::Version::parse("0.2.0").unwrap(),
            asset: "crossc-0.2.0-x64.zip".into(),
            sha256: "ab".repeat(32),
            dir: releases.join("v0.2.0"),
            cert_subject: TEST_CERT_SUBJECT.into(),
            cert_thumbprint: crate::cert::tests::WDR_A_THUMBPRINT.into(),
        };
        discard_release(&fetched);
        assert!(
            releases
                .join("v0.2.0")
                .join("crossc-installer.exe")
                .is_file()
        );
    }
}
