//! The machine state the installer shows (`Status`), and the pure functions that turn it into the checklist, the
//! "next step" footer and the install / uninstall verdicts. Nothing here touches Windows, so all of it is
//! unit-tested.

use std::fmt;
use std::path::PathBuf;

use crate::parse::{StagedPackage, problem_text, version_number};
use crate::release::{LatestRelease, Version};

/// Driver service name once crossc_bth is bound to a buds node.
pub const DRIVER_SERVICE: &str = "crossc_bth";
/// Inbox A2DP driver service the node runs before install / after uninstall.
pub const INBOX_SERVICE: &str = "BthA2dp";
/// Name of the user-mode Windows service (`crossc-service.exe`) that turns the endpoint's PCM into an SSC stream.
pub const USER_SERVICE: &str = "crossc";
/// Files the install needs in the package directory (the script's preflight list).
pub const REQUIRED_FILES: [&str; 4] = [
    "crossc_bth.inf",
    "crossc_bth.sys",
    "crossc_bth.cat",
    "WDRLocalTestCert.cer",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Debug,
    Release,
}

impl Flavor {
    pub fn dir_name(self) -> &'static str {
        match self {
            Flavor::Debug => "debug",
            Flavor::Release => "release",
        }
    }

    pub fn other(self) -> Flavor {
        match self {
            Flavor::Debug => Flavor::Release,
            Flavor::Release => Flavor::Debug,
        }
    }
}

impl fmt::Display for Flavor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.dir_name())
    }
}

/// Where a driver package comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageKind {
    /// Built in the repo (`drivers\crossc-bth\target\<flavor>\crossc_bth_package`).
    Built(Flavor),
    /// Shipped in a release bundle: `driver\` next to the installer, or in the release this installer downloaded
    /// (online mode).
    Bundle,
}

impl fmt::Display for PackageKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PackageKind::Built(flavor) => f.pad(flavor.dir_name()),
            PackageKind::Bundle => f.pad("bundle"),
        }
    }
}

/// A driver package directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageInfo {
    pub kind: PackageKind,
    pub dir: PathBuf,
    /// Required files that are missing (empty = complete).
    pub missing: Vec<&'static str>,
    /// Local time the .sys was written, e.g. `2026-11-10 22:51`.
    pub built_at: Option<String>,
    /// Age of the .sys, e.g. `3 h ago`.
    pub built_age: Option<String>,
    /// The INF's `DriverVer` value, e.g. `11/11/2026,8.50.53.934`.
    pub driver_ver: Option<String>,
}

impl PackageInfo {
    pub fn complete(&self) -> bool {
        self.missing.is_empty()
    }

    /// The version number in `DriverVer` (`8.50.53.934`).
    pub fn version(&self) -> Option<&str> {
        self.driver_ver.as_deref().and_then(version_number)
    }
}

/// `bundle 8.50.53.934` (the header's label in bundle mode), or just `bundle` when the INF has no version.
pub fn bundle_label(version: Option<&str>) -> String {
    match version {
        Some(v) => format!("bundle {v}"),
        None => "bundle".into(),
    }
}

/// The header's label in online mode: `online` while there is no package, then the release that was downloaded
/// and installed, `release 0.2.0` (or `release, driver 8.50.53.934` when its version is not known).
pub fn online_label(release: Option<&Version>, driver_version: Option<&str>) -> String {
    match (release, driver_version) {
        (Some(v), _) => format!("release {v}"),
        (None, Some(d)) => format!("release, driver {d}"),
        (None, None) => "online".into(),
    }
}

/// The A2DP devnode of one pair of buds.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NodeInfo {
    pub instance_id: String,
    /// Present = currently enumerated (buds paired; connected or not); phantom (a stale pairing) otherwise.
    pub present: bool,
    /// The node's own name. Once crossc_bth is bound this is the INF's device description, the same for all buds.
    pub name: Option<String>,
    /// Name of the Bluetooth device the node belongs to (what the buds were paired as), when it can be read.
    pub device_name: Option<String>,
    pub service: Option<String>,
    /// Device-manager problem code; `None` when it cannot be read (phantom node).
    pub problem: Option<u32>,
    pub inf: Option<String>,
    pub version: Option<String>,
    /// Driver stack top to bottom, e.g. `\Driver\ksthunk`, `\Driver\BthA2dp`, `\Driver\BthEnum`.
    pub stack: Vec<String>,
}

impl NodeInfo {
    pub fn runs(&self, service: &str) -> bool {
        self.service
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case(service))
    }

    /// Bound to `service` with no problem (the script's verification rule).
    pub fn healthy_on(&self, service: &str) -> bool {
        self.runs(service) && self.problem.unwrap_or(0) == 0
    }

    /// `ksthunk -> BthA2dp -> BthEnum`.
    pub fn stack_text(&self) -> String {
        self.stack
            .iter()
            .map(|s| s.trim_start_matches("\\Driver\\"))
            .collect::<Vec<_>>()
            .join(" -> ")
    }

    /// What to call the node: the buds' Bluetooth name, else the node's own name.
    pub fn label(&self) -> &str {
        self.device_name
            .as_deref()
            .or(self.name.as_deref())
            .unwrap_or("A2DP node")
    }
}

/// A node belongs to supported buds when one of its hardware IDs is, exactly (ignoring case), one of the IDs on
/// the INF's model lines. Apart from nodes that already run crossc_bth (`unlisted_on_driver`), nothing else is
/// ever verified or reported; in particular the generic A2DP compatible ID never matches, because the INF parser
/// returns only `_VID&..._PID&...` IDs.
pub fn hwid_matches(node_hardware_ids: &[String], inf_ids: &[String]) -> bool {
    node_hardware_ids
        .iter()
        .any(|h| inf_ids.iter().any(|i| i.eq_ignore_ascii_case(h)))
}

/// A node that runs crossc_bth although the package's INF does not list its model: it was bound by another
/// version of the package (or there is no INF to compare with, `inf_ids` empty). Such a node is not one Install
/// can put the driver on, but Install's package swap and Uninstall both take the driver away from it.
pub fn unlisted_on_driver(
    node_hardware_ids: &[String],
    service: Option<&str>,
    inf_ids: &[String],
) -> bool {
    service.is_some_and(|s| s.eq_ignore_ascii_case(DRIVER_SERVICE))
        && !hwid_matches(node_hardware_ids, inf_ids)
}

/// Present nodes first (they are the ones Install and Uninstall verify), stale pairings after; otherwise the
/// order the device list came in.
pub fn order_nodes(mut nodes: Vec<NodeInfo>) -> Vec<NodeInfo> {
    nodes.sort_by_key(|n| !n.present);
    nodes
}

/// The present nodes that are not running `service` cleanly.
pub fn not_healthy_on<'a>(nodes: &'a [NodeInfo], service: &str) -> Vec<&'a NodeInfo> {
    nodes
        .iter()
        .filter(|n| n.present && !n.healthy_on(service))
        .collect()
}

/// What the driver part of Install achieved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallVerdict<'a> {
    /// Every present supported node (this many) runs crossc_bth with no problem.
    Bound(usize),
    /// No supported buds are paired, but the package is in the driver store and its certificate is trusted: they
    /// pick it up when they are paired.
    Waiting,
    /// These present nodes do not run crossc_bth cleanly.
    NotBound(Vec<&'a NodeInfo>),
    /// No node to check, and the package is not staged or its certificate is not trusted.
    NotStaged,
}

/// `nodes`: the supported nodes as they are after pnputil ran. Stale pairings (not present) are not judged.
pub fn install_verdict(nodes: &[NodeInfo], staged: bool, cert_trusted: bool) -> InstallVerdict<'_> {
    let present = nodes.iter().filter(|n| n.present).count();
    let failed = not_healthy_on(nodes, DRIVER_SERVICE);
    if !failed.is_empty() {
        InstallVerdict::NotBound(failed)
    } else if present > 0 {
        InstallVerdict::Bound(present)
    } else if staged && cert_trusted {
        InstallVerdict::Waiting
    } else {
        InstallVerdict::NotStaged
    }
}

/// Whether the package's test certificate is trusted machine-wide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertInfo {
    pub thumbprint: String,
    pub in_root: bool,
    pub in_trusted_publisher: bool,
}

impl CertInfo {
    pub fn trusted(&self) -> bool {
        self.in_root && self.in_trusted_publisher
    }
}

/// State of the `crossc` Windows service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    NotInstalled,
    Stopped,
    StartPending,
    StopPending,
    Running,
    /// Paused or a pending pause/continue (the service does not use them), or a value Windows does not document.
    Other(u32),
}

/// Maps `SERVICE_STATUS.dwCurrentState` of an installed service.
pub fn service_state(current_state: u32) -> ServiceState {
    match current_state {
        1 => ServiceState::Stopped,
        2 => ServiceState::StartPending,
        3 => ServiceState::StopPending,
        4 => ServiceState::Running,
        n => ServiceState::Other(n),
    }
}

impl ServiceState {
    pub fn installed(self) -> bool {
        self != ServiceState::NotInstalled
    }
}

impl fmt::Display for ServiceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServiceState::NotInstalled => f.write_str("not installed"),
            ServiceState::Stopped => f.write_str("installed, stopped"),
            ServiceState::StartPending => f.write_str("installed, starting"),
            ServiceState::StopPending => f.write_str("installed, stopping"),
            ServiceState::Running => f.write_str("running"),
            ServiceState::Other(n) => write!(f, "installed, state {n}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceInfo {
    pub state: ServiceState,
    /// The executable the service is registered with (`None` when not installed or not readable).
    pub exe: Option<String>,
}

/// What Install wrote down in `HKLM\SOFTWARE\crossc` (`system::read_record`). Every value may be missing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InstallRecord {
    /// `CertThumbprint`: SHA-1 thumbprint of the certificate of the last install whose driver was verified.
    pub cert_thumbprint: Option<String>,
    /// `PendingCerts`: thumbprints (separated by spaces) of certificates an install trusted without getting as
    /// far as a verified driver, and of earlier certificates that could not be removed. The next install that
    /// verifies removes them (`cert_record_plan`).
    pub pending_certs: Option<String>,
    /// `Version`: version of the release that was installed (`0.2.0`); missing after an install from the repo.
    pub version: Option<String>,
    /// `DriverVer`: version number of the driver package that install staged (`8.50.53.934`).
    pub driver_ver: Option<String>,
}

/// What is known about the newest release on GitHub. Not part of what `system::query_status` reads: the check
/// goes over the network and only runs when it is asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Latest {
    NotChecked,
    Checking,
    /// The repository has no published release. `auth`: `anonymous` or where the token came from. `skipped`:
    /// tags of published releases that were passed over because they are not release tags this installer
    /// reads (`release::skipped_tags`), ready for display.
    NoReleases {
        repo: String,
        auth: String,
        skipped: Vec<String>,
    },
    Found {
        release: Box<LatestRelease>,
        repo: String,
        auth: String,
        skipped: Vec<String>,
    },
    /// The check failed; the text says why and, where there is something to do about it, what.
    Failed(String),
}

impl Latest {
    /// `mrquantumoff/crossc, anonymous`: which repository answered and how it was asked.
    pub fn source_text(&self) -> Option<String> {
        match self {
            Latest::NoReleases { repo, auth, .. } | Latest::Found { repo, auth, .. } => {
                Some(format!("{repo}, {auth}"))
            }
            _ => None,
        }
    }

    /// What to say about published releases the check passed over because of their tag; `None` when there
    /// were none. Without this a release tagged `v0.2.0-rc.01` would simply never show up.
    pub fn skipped_note(&self) -> Option<String> {
        let (Latest::NoReleases { skipped, .. } | Latest::Found { skipped, .. }) = self else {
            return None;
        };
        (!skipped.is_empty()).then(|| {
            format!(
                "ignored {}: not a release tag (vMAJOR.MINOR.PATCH or vMAJOR.MINOR.PATCH-suffix, numbers \
                 without leading zeros)",
                skipped.join(", ")
            )
        })
    }
}

/// A release that "Download and install" has downloaded, verified and unpacked, and that waits for the second
/// click before its certificate is trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedRelease {
    pub repo: String,
    pub tag: String,
    pub version: Version,
    /// Name of the zip asset.
    pub asset: String,
    /// SHA-256 of the zip as downloaded (it matched `SHA256SUMS.txt`), lower-case hex.
    pub sha256: String,
    /// Where it was unpacked: `%ProgramFiles%\crossc\releases\<tag>`.
    pub dir: PathBuf,
    /// Subject of `driver\WDRLocalTestCert.cer` in it.
    pub cert_subject: String,
    /// SHA-1 thumbprint of that certificate, upper-case hex.
    pub cert_thumbprint: String,
}

/// Everything the status checklist shows. `None` / `Err` mean "could not be determined" (with the reason).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub admin: bool,
    /// CODEINTEGRITY_OPTION_TESTSIGN from the running kernel.
    pub test_signing: Option<bool>,
    pub secure_boot: Option<bool>,
    /// CODEINTEGRITY_OPTION_HVCI_KMCI_ENABLED from the running kernel.
    pub hvci_running: Option<bool>,
    /// DeviceGuard\Scenarios\HypervisorEnforcedCodeIntegrity\Enabled (takes effect after a reboot).
    pub hvci_configured: Option<bool>,
    /// Never empty. Bundle: the one package. Repo: the selected flavour (index 0) and the other one.
    pub packages: Vec<PackageInfo>,
    /// Hardware IDs of every supported buds model, from the package INF (first package that has one).
    pub hardware_ids: Vec<String>,
    /// Every A2DP node that carries one of `hardware_ids`: none, one or several paired buds.
    pub nodes: Result<Vec<NodeInfo>, String>,
    /// A2DP nodes that run crossc_bth without being in `hardware_ids` (`unlisted_on_driver`); empty when they
    /// could not be enumerated.
    pub unlisted: Vec<NodeInfo>,
    pub cert: Result<CertInfo, String>,
    pub staged: Result<Vec<StagedPackage>, String>,
    pub service: Result<ServiceInfo, String>,
    /// The `crossc-service.exe` Install would copy (bundle: next to the installer, repo: `target\release`);
    /// `None` when it is not there or is not a complete Windows executable.
    pub service_exe: Option<PathBuf>,
    /// Online mode: the installer runs on its own (no `driver\` next to it, no repo). Its package is the release
    /// it installed earlier, if any; until then there is none and "Download and install" is the way.
    pub online: bool,
    pub record: InstallRecord,
    /// Version of the bundle Install would use (`release::bundle_version`); `None` in the repo and for a bundle
    /// that does not say.
    pub bundle_version: Option<Version>,
    pub latest: Latest,
}

impl Status {
    pub fn package(&self) -> &PackageInfo {
        &self.packages[0]
    }

    pub fn is_bundle(&self) -> bool {
        self.package().kind == PackageKind::Bundle
    }

    /// The selected build flavour; `None` in bundle mode.
    pub fn flavor(&self) -> Option<Flavor> {
        match self.package().kind {
            PackageKind::Built(f) => Some(f),
            PackageKind::Bundle => None,
        }
    }

    /// The supported nodes (empty when they could not be enumerated).
    pub fn nodes(&self) -> &[NodeInfo] {
        self.nodes.as_deref().unwrap_or(&[])
    }

    fn staged(&self) -> bool {
        self.staged.as_ref().is_ok_and(|p| !p.is_empty())
    }

    fn cert_trusted(&self) -> bool {
        self.cert.as_ref().is_ok_and(CertInfo::trusted)
    }

    /// The crossc version that is installed, as far as it is known: the version Install recorded, as long as the
    /// driver package it recorded with it is still the staged one. (The driver's own `DriverVer` is a build time
    /// stamp, not the release version, so it only serves to tell that the record still describes what is
    /// staged.) `None` when nothing is staged, after an install from the repo or by another tool, and when the
    /// staged packages cannot be listed.
    pub fn installed_version(&self) -> Option<Version> {
        let version = Version::parse(self.record.version.as_deref()?)?;
        let recorded = self.record.driver_ver.as_deref()?;
        self.staged
            .as_ref()
            .ok()?
            .iter()
            .any(|p| p.version.as_deref().and_then(version_number) == Some(recorded))
            .then_some(version)
    }

    /// Version of the complete bundle that Install would use (a bundle, or the release online mode kept).
    fn local_version(&self) -> Option<&Version> {
        (self.is_bundle() && self.package().complete())
            .then_some(self.bundle_version.as_ref())
            .flatten()
    }

    /// "the bundle", or what plays its part in online mode.
    fn bundle_noun(&self) -> &'static str {
        if self.online {
            "the downloaded release"
        } else {
            "the bundle"
        }
    }

    /// How to get a complete set of files again.
    fn refetch(&self) -> &'static str {
        if self.online {
            DOWNLOAD_AGAIN
        } else {
            UNPACK_AGAIN
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Good,
    Warn,
    Bad,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckItem {
    pub label: &'static str,
    pub level: Level,
    pub detail: String,
    /// A second, technical line under the detail (driver stack, INF, version of a node).
    pub sub: Option<String>,
}

fn item(label: &'static str, level: Level, detail: impl Into<String>) -> CheckItem {
    CheckItem {
        label,
        level,
        detail: detail.into(),
        sub: None,
    }
}

const UNPACK_AGAIN: &str = "unpack the whole release zip again";
const DOWNLOAD_AGAIN: &str = "use \"Download and install\" to fetch the release again";
/// Label of the button that asks GitHub for the newest release.
pub const CHECK_LABEL: &str = "Check for update";

/// The status checklist: one line per prerequisite, one per paired buds node.
pub fn checklist(s: &Status) -> Vec<CheckItem> {
    let mut v = Vec::new();
    let bundle = s.is_bundle();

    v.push(if s.admin {
        item(
            "Administrator",
            Level::Good,
            "elevated: install and uninstall are available",
        )
    } else if bundle {
        item(
            "Administrator",
            Level::Warn,
            "not elevated: Install/Uninstall need \"Restart as administrator\"",
        )
    } else {
        item(
            "Administrator",
            Level::Warn,
            "not elevated: Build works; Install/Uninstall need \"Restart as administrator\"",
        )
    });

    v.push(match s.test_signing {
        Some(true) => item(
            "Test signing",
            Level::Good,
            "on: test-signed drivers may load",
        ),
        Some(false) => item(
            "Test signing",
            Level::Bad,
            "off: bcdedit /set testsigning on (admin), then reboot",
        ),
        None => item(
            "Test signing",
            Level::Unknown,
            "unknown (code integrity query failed)",
        ),
    });

    v.push(match s.secure_boot {
        Some(false) => item("Secure Boot", Level::Good, "off"),
        Some(true) => item(
            "Secure Boot",
            Level::Bad,
            "on: test signing is ignored until it is off in firmware setup",
        ),
        None => item(
            "Secure Boot",
            Level::Unknown,
            "unknown (legacy BIOS or not readable)",
        ),
    });

    v.push(match (s.hvci_running, s.hvci_configured) {
        (Some(true), _) => item(
            "Memory integrity",
            Level::Warn,
            "running (HVCI): may refuse the driver (problem 39)",
        ),
        (Some(false), Some(true)) => item(
            "Memory integrity",
            Level::Warn,
            "off now, but enabled in settings: on after the next reboot",
        ),
        (Some(false), _) => item("Memory integrity", Level::Good, "off"),
        (None, _) => item("Memory integrity", Level::Unknown, "unknown"),
    });

    v.push(package_item(s));

    v.push(match &s.cert {
        Ok(c) if c.trusted() => item(
            "Certificate trusted",
            Level::Good,
            format!(
                "WDRLocalTestCert {} in Root + TrustedPublisher",
                short_tp(&c.thumbprint)
            ),
        ),
        Ok(c) => {
            let missing: Vec<&str> = [
                (!c.in_root).then_some("Root"),
                (!c.in_trusted_publisher).then_some("TrustedPublisher"),
            ]
            .into_iter()
            .flatten()
            .collect();
            item(
                "Certificate trusted",
                Level::Warn,
                format!(
                    "WDRLocalTestCert {} not in LocalMachine {} (Install adds it)",
                    short_tp(&c.thumbprint),
                    missing.join(" / ")
                ),
            )
        }
        Err(e) => item("Certificate trusted", Level::Unknown, e.clone()),
    });

    match &s.nodes {
        Err(e) => v.push(item("Buds", Level::Unknown, e.clone())),
        Ok(_) if s.hardware_ids.is_empty() => v.push(item(
            "Buds",
            Level::Unknown,
            if s.online {
                "no driver package yet, so the supported models are not known: they are listed once a release \
                 is installed"
            } else if bundle {
                "no INF to read the supported models from"
            } else {
                "no INF to read the supported models from: build first"
            },
        )),
        Ok(nodes) if nodes.is_empty() => v.push(item(
            "Buds",
            Level::Warn,
            format!(
                "none paired ({}): pair your Galaxy Buds with LE Audio off. Install works without them; they \
                 pick the driver up when paired",
                models(s.hardware_ids.len())
            ),
        )),
        Ok(nodes) => v.extend(nodes.iter().map(node_item)),
    }
    v.extend(s.unlisted.iter().map(|n| unlisted_item(n, s)));

    v.push(match &s.staged {
        Err(e) => item("Staged packages", Level::Unknown, e.clone()),
        Ok(pkgs) if pkgs.is_empty() => item(
            "Staged packages",
            Level::Good,
            "no crossc_bth package in the driver store",
        ),
        Ok(pkgs) => {
            let list: Vec<String> = pkgs
                .iter()
                .map(|p| match &p.version {
                    Some(ver) => format!("{} ({ver})", p.published),
                    None => p.published.clone(),
                })
                .collect();
            let level = if pkgs.len() > 1 {
                Level::Warn
            } else {
                Level::Good
            };
            let mut detail = list.join(", ");
            // Only said when it is certain: one staged package, both versions readable, and they differ.
            if let ([staged], Some(ours)) = (pkgs.as_slice(), s.package().version())
                && s.package().complete()
                && let Some(theirs) = staged.version.as_deref().and_then(version_number)
                && theirs != ours
            {
                detail.push_str(&format!(
                    "; the {} package is {ours}: Install replaces it",
                    s.package().kind
                ));
            }
            item("Staged packages", level, detail)
        }
    });

    v.push(service_item(s));
    v.push(latest_item(s));

    v
}

fn models(n: usize) -> String {
    match n {
        1 => "1 supported model".into(),
        n => format!("{n} supported models"),
    }
}

fn package_item(s: &Status) -> CheckItem {
    let p = s.package();
    let other = s.packages.get(1);
    if s.online && p.missing.len() == REQUIRED_FILES.len() {
        // Not a fault: a lone installer has no package until it has downloaded one.
        return item(
            "Driver package",
            Level::Warn,
            "none: this installer runs on its own. \"Download and install\" fetches the latest release from \
             GitHub",
        );
    }
    if s.is_bundle() {
        return if !p.complete() {
            item(
                "Driver package",
                Level::Bad,
                format!(
                    "{} incomplete, missing {} in {}: {}",
                    if s.online {
                        "downloaded release"
                    } else {
                        "bundle"
                    },
                    p.missing.join(", "),
                    p.dir.display(),
                    s.refetch()
                ),
            )
        } else if s.hardware_ids.is_empty() {
            item(
                "Driver package",
                Level::Bad,
                format!(
                    "the INF of {} lists no supported buds model",
                    s.bundle_noun()
                ),
            )
        } else {
            item(
                "Driver package",
                Level::Good,
                format!(
                    "{} complete, {}",
                    if s.online {
                        online_label(s.bundle_version.as_ref(), p.version())
                    } else {
                        bundle_label(p.version())
                    },
                    models(s.hardware_ids.len())
                ),
            )
        };
    }
    if p.complete() {
        let when = match (&p.built_at, &p.built_age) {
            (Some(at), Some(age)) => format!(", built {at} ({age})"),
            _ => String::new(),
        };
        let extra = match other {
            Some(o) if o.complete() => format!("; {} also built", o.kind),
            _ => String::new(),
        };
        if s.hardware_ids.is_empty() {
            return item(
                "Driver package",
                Level::Bad,
                format!("the {} package's INF lists no supported buds model", p.kind),
            );
        }
        item(
            "Driver package",
            Level::Good,
            format!(
                "{} package complete{when}, {}{extra}",
                p.kind,
                models(s.hardware_ids.len())
            ),
        )
    } else if p.missing.len() == REQUIRED_FILES.len() {
        let extra = match other {
            Some(o) if o.complete() => format!(" ({} is built)", o.kind),
            _ => String::new(),
        };
        item(
            "Driver package",
            Level::Bad,
            format!("{} package not built: click Build{extra}", p.kind),
        )
    } else {
        item(
            "Driver package",
            Level::Bad,
            format!(
                "{} package incomplete, missing {}",
                p.kind,
                p.missing.join(", ")
            ),
        )
    }
}

/// One paired pair of buds: name, service, problem code, instance id; stack / INF / version on the second line.
fn node_item(n: &NodeInfo) -> CheckItem {
    let service = n.service.as_deref().unwrap_or("no driver");
    let (level, state) = match n.problem {
        Some(code) if code != 0 => (Level::Bad, problem_text(code)),
        _ if !n.present => (
            Level::Warn,
            "not present (stale pairing, problem code not readable)".to_string(),
        ),
        Some(_) => (Level::Good, "problem 0".to_string()),
        None => (Level::Good, "problem code not readable".to_string()),
    };
    CheckItem {
        label: "Buds",
        level,
        detail: format!("{}: {service}, {state}, on {}", n.label(), n.instance_id),
        sub: Some(format!(
            "stack {}   inf {}   version {}",
            n.stack_text(),
            n.inf.as_deref().unwrap_or("-"),
            n.version.as_deref().unwrap_or("-")
        )),
    }
}

/// A node on crossc_bth that the package does not list: shown, because Install and Uninstall both change it.
fn unlisted_item(n: &NodeInfo, s: &Status) -> CheckItem {
    let mut row = node_item(n);
    if row.level == Level::Good {
        row.level = Level::Warn;
    }
    row.detail.push_str(if s.hardware_ids.is_empty() {
        "; Uninstall puts it back on the Windows driver"
    } else {
        "; its model is not in this package: Install replaces the package it runs and it goes back to the \
         Windows driver"
    });
    row
}

fn service_item(s: &Status) -> CheckItem {
    let info = match &s.service {
        Ok(info) => info,
        Err(e) => return item("Service", Level::Unknown, e.clone()),
    };
    let exe = info
        .exe
        .as_deref()
        .unwrap_or("executable path not readable");
    match info.state {
        ServiceState::Running => item("Service", Level::Good, format!("running: {exe}")),
        ServiceState::NotInstalled => match (&s.service_exe, s.is_bundle()) {
            (Some(_), _) => item(
                "Service",
                Level::Warn,
                "not installed (Install adds and starts it)",
            ),
            (None, true) if s.online && !s.package().complete() => item(
                "Service",
                Level::Warn,
                "not installed (\"Download and install\" adds and starts it)",
            ),
            (None, true) => item(
                "Service",
                Level::Bad,
                format!(
                    "not installed, and crossc-service.exe is missing from {} or damaged: {}",
                    s.bundle_noun(),
                    s.refetch()
                ),
            ),
            (None, false) => item(
                "Service",
                Level::Warn,
                "not installed; target\\release\\crossc-service.exe is not built, so Install skips it",
            ),
        },
        state => item("Service", Level::Warn, format!("{state}: {exe}")),
    }
}

fn short_tp(tp: &str) -> &str {
    &tp[..tp.len().min(8)]
}

/// The "Latest release" row: what the last check found, compared with what is installed.
pub fn latest_item(s: &Status) -> CheckItem {
    let mut row = latest_row_of(s);
    if let Some(note) = s.latest.skipped_note() {
        row.detail = format!("{}; {note}", row.detail);
    }
    row
}

fn latest_row_of(s: &Status) -> CheckItem {
    const LABEL: &str = "Latest release";
    let (release, repo) = match &s.latest {
        Latest::NotChecked => {
            return item(
                LABEL,
                Level::Unknown,
                format!("unknown: \"{CHECK_LABEL}\" asks GitHub for the newest release"),
            );
        }
        Latest::Checking => return item(LABEL, Level::Unknown, "checking..."),
        Latest::NoReleases { repo, .. } => {
            return item(
                LABEL,
                Level::Unknown,
                format!("{repo} has no published release yet"),
            );
        }
        Latest::Failed(why) => return item(LABEL, Level::Warn, format!("check failed: {why}")),
        Latest::Found { release, repo, .. } => (release, repo),
    };
    let tag = &release.tag;
    if let Err(why) = &release.assets {
        return item(
            LABEL,
            Level::Warn,
            format!(
                "{tag} is the latest release, but this installer cannot install it: {why}. See \
                 https://github.com/{repo}/releases"
            ),
        );
    }
    match (s.installed_version(), s.local_version()) {
        (Some(installed), _) if installed == release.version => item(
            LABEL,
            Level::Good,
            format!("up to date: {tag} is installed"),
        ),
        (Some(installed), _) if installed > release.version => item(
            LABEL,
            Level::Good,
            format!("{tag} is the latest release; the installed v{installed} is newer"),
        ),
        (Some(installed), _) => item(
            LABEL,
            Level::Warn,
            format!("update available: {tag} (installed v{installed})"),
        ),
        (None, Some(local)) if *local >= release.version => item(
            LABEL,
            Level::Good,
            format!(
                "{tag} is the latest release, and {} is v{local}: Install installs it",
                if s.online {
                    "the downloaded release"
                } else {
                    "this bundle"
                }
            ),
        ),
        (None, _) if s.staged() => item(
            LABEL,
            Level::Warn,
            format!("{tag} is available (which version is installed is not known)"),
        ),
        (None, _) => item(
            LABEL,
            Level::Warn,
            format!("{tag} is available (crossc is not installed)"),
        ),
    }
}

/// The release "Download and install" would fetch, when there is a point in it: the last check found a release
/// with its assets, and it is newer than both what is installed and the bundle Install would use (an unknown
/// version counts as older). In online mode it is also offered when the downloaded release is unusable, so
/// there is always a way to get the files again.
pub fn download_offer(s: &Status) -> Option<&LatestRelease> {
    let Latest::Found { release, .. } = &s.latest else {
        return None;
    };
    release.assets.as_ref().ok()?;
    let installed = s.installed_version();
    let have = installed
        .as_ref()
        .into_iter()
        .chain(s.local_version())
        .max();
    let unusable = s.online && (!s.package().complete() || s.service_exe.is_none());
    (unusable || have.is_none_or(|have| *have < release.version)).then_some(&**release)
}

/// Label of the button that downloads and installs `release`.
pub fn download_label(release: &LatestRelease) -> String {
    format!("Download and install {}", release.tag)
}

/// A SHA-1 thumbprint as the record holds it: 40 hex digits, returned in upper case. Anything else is `None`.
fn thumbprint_text(text: &str) -> Option<String> {
    let text = text.trim();
    (text.len() == 40 && text.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| text.to_ascii_uppercase())
}

/// The thumbprints in a `PendingCerts` value (separated by spaces, commas or semicolons), upper case, each
/// once, in order. Whatever is not a thumbprint is dropped: nothing but a thumbprint ever becomes a removal.
pub fn pending_certs(value: Option<&str>) -> Vec<String> {
    let mut list: Vec<String> = Vec::new();
    for tp in value
        .unwrap_or("")
        .split(|c: char| c.is_whitespace() || c == ',' || c == ';')
        .filter_map(thumbprint_text)
    {
        if !list.contains(&tp) {
            list.push(tp);
        }
    }
    list
}

/// The `PendingCerts` value for a list of thumbprints; `None` (delete the value) for an empty list.
pub fn pending_value(list: &[String]) -> Option<String> {
    (!list.is_empty()).then(|| list.join(" "))
}

/// Install is about to trust the certificate `about_to_trust`: the `PendingCerts` value to write **first**, or
/// `None` when the record covers that certificate already (it is the recorded one, or it is pending).
///
/// Written before the certificate goes into the stores, so that a certificate Install trusted is always named
/// by the record, whatever stops the install afterwards: either as `CertThumbprint` (a verified install) or
/// here. A later install then knows what to remove.
pub fn pending_before_trust(
    recorded: Option<&str>,
    pending: Option<&str>,
    about_to_trust: &str,
) -> Option<String> {
    let new = thumbprint_text(about_to_trust)?;
    let mut list = pending_certs(pending);
    if recorded.and_then(thumbprint_text).as_ref() == Some(&new) || list.contains(&new) {
        return None;
    }
    list.push(new);
    pending_value(&list)
}

/// What to do about the certificate record when the driver signed with the certificate `trusted_now` has been
/// verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertRecordPlan {
    /// Thumbprints (upper-case hex) of the certificates that are replaced now: the one an earlier install
    /// recorded, and the pending ones (trusted by installs that never got a verified driver). To be removed
    /// from Root and TrustedPublisher (`cert::Store::remove_exact`, which also insists on the subject).
    pub remove: Vec<String>,
    /// `CertThumbprint` has to be (re)written to name `trusted_now`.
    pub write: bool,
}

impl CertRecordPlan {
    /// The `PendingCerts` value once the removals were tried (`None`: delete it): the certificates that could
    /// not be removed stay on the list, so the next Install tries again instead of forgetting a trusted root.
    /// `failed`: the thumbprints of `remove` whose removal failed.
    pub fn pending_after(&self, failed: &[String]) -> Option<String> {
        let left: Vec<String> = self
            .remove
            .iter()
            .filter(|tp| failed.contains(tp))
            .cloned()
            .collect();
        pending_value(&left)
    }
}

/// `recorded`: the `CertThumbprint` value as read, `pending`: the `PendingCerts` value as read (anything that
/// is not 40 hex digits counts as not there). The same certificate again and nothing pending: nothing to do.
/// Otherwise every recorded or pending certificate other than `trusted_now` goes, and `trusted_now` is recorded.
///
/// This runs only after the new driver is verified. Until then the earlier certificate stays trusted and
/// recorded, so an update that fails leaves the earlier release installable.
pub fn cert_record_plan(
    recorded: Option<&str>,
    pending: Option<&str>,
    trusted_now: &str,
) -> CertRecordPlan {
    let now = trusted_now.trim().to_ascii_uppercase();
    let recorded = recorded.and_then(thumbprint_text);
    let mut remove: Vec<String> = Vec::new();
    for tp in recorded.iter().cloned().chain(pending_certs(pending)) {
        if tp != now && !remove.contains(&tp) {
            remove.push(tp);
        }
    }
    CertRecordPlan {
        remove,
        write: recorded.as_ref() != Some(&now),
    }
}

/// What staging the new package (`pnputil /add-driver`, without `/install`) came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Staged {
    /// The new package is in the driver store under this published name (`oemNN.inf`): every other crossc_bth
    /// package can go, this one stays.
    Keep(String),
    /// pnputil accepted the package, but which staged package it is cannot be told: Install falls back to
    /// removing all of them and adding the new one again.
    Unclear,
    /// The package is not in the driver store: pnputil refused it (a signature Windows does not accept, a
    /// broken INF). Nothing has been removed yet, and nothing should be.
    Refused,
}

/// Judges the staging by what the driver store holds, not by pnputil's words (they are localised).
///
/// `before` / `after`: the staged crossc_bth packages before and after the `/add-driver`; `driver_ver`: the
/// version number in the new package's INF; `accepted`: pnputil's exit code was not a failure.
///
/// * One package appeared: that is the new one, whatever the exit code said.
/// * None appeared, pnputil was content, and exactly one staged package has the INF's version: the same
///   package was staged already (a reinstall), and that is it.
/// * None appeared and pnputil failed: refused. A staged package with the same version does not count then:
///   it is an older one, and keeping it would pass a release off as installed whose driver Windows rejected.
/// * Anything else that pnputil was content with (several new ones, several or none with that version):
///   unclear.
pub fn staged_keeper(
    before: &[StagedPackage],
    after: &[StagedPackage],
    driver_ver: Option<&str>,
    accepted: bool,
) -> Staged {
    let new: Vec<&StagedPackage> = after
        .iter()
        .filter(|a| {
            !before
                .iter()
                .any(|b| b.published.eq_ignore_ascii_case(&a.published))
        })
        .collect();
    match new.as_slice() {
        [one] => Staged::Keep(one.published.clone()),
        [] if !accepted => Staged::Refused,
        [] => {
            let same: Vec<&StagedPackage> = after
                .iter()
                .filter(|p| {
                    driver_ver.is_some()
                        && p.version.as_deref().and_then(version_number) == driver_ver
                })
                .collect();
            match same.as_slice() {
                [one] => Staged::Keep(one.published.clone()),
                _ => Staged::Unclear,
            }
        }
        _ => Staged::Unclear,
    }
}

/// The staged packages Install removes once the new one is staged: all but `keep` (a published name, compared
/// ignoring case). `None` keeps nothing: Uninstall, and the fallback when the new package cannot be told apart.
pub fn packages_to_remove(staged: Vec<StagedPackage>, keep: Option<&str>) -> Vec<StagedPackage> {
    staged
        .into_iter()
        .filter(|p| !keep.is_some_and(|keep| p.published.eq_ignore_ascii_case(keep)))
        .collect()
}

/// What to say when the install of a downloaded release failed while an earlier release is still unpacked
/// (`earlier`: its folder, when the record names one that is still there and is not the one that failed).
pub fn earlier_release_note(
    failed_dir: &std::path::Path,
    earlier: Option<&std::path::Path>,
) -> Option<String> {
    let earlier = earlier.filter(|dir| *dir != failed_dir)?;
    Some(format!(
        "the earlier release is still unpacked in {} and its certificate is still trusted. To go back to it: \
         run crossc-installer.exe in that folder and click Install (with an installer that runs on its own: \
         Cancel, then Install)",
        earlier.display()
    ))
}

/// Heading of the confirmation shown after a release was downloaded, before its certificate is trusted.
pub fn confirm_title(f: &FetchedRelease) -> String {
    format!(
        "Confirm: trust this certificate and install crossc {}",
        f.tag
    )
}

/// The plain statement of what the second click does.
pub const CONFIRM_STATEMENT: &str = "This installs a test-signed kernel driver downloaded from GitHub. The \
    certificate below is added to this PC's trusted roots (LocalMachine Root and TrustedPublisher) so that \
    Windows accepts the driver; the certificate of an earlier crossc release is removed. Nothing has been \
    installed yet.";

/// The facts to check before confirming, as label and value.
pub fn confirm_lines(f: &FetchedRelease) -> Vec<(&'static str, String)> {
    vec![
        ("Release", format!("{} of github.com/{}", f.tag, f.repo)),
        ("Asset", f.asset.clone()),
        ("SHA-256", format!("{} (matches SHA256SUMS.txt)", f.sha256)),
        (
            "Certificate",
            format!("{} (self-signed test certificate)", f.cert_subject),
        ),
        ("Thumbprint", format!("{} (SHA-1)", f.cert_thumbprint)),
        ("Unpacked to", f.dir.display().to_string()),
    ]
}

/// A made-up downloaded release, to look at the confirmation step without downloading anything (the hidden
/// `--show-confirm` switch). Its folder does not exist, so confirming it stops at once and installs nothing.
pub fn example_fetched() -> FetchedRelease {
    FetchedRelease {
        repo: crate::release::DEFAULT_REPO.into(),
        tag: "v0.0.0-example".into(),
        version: Version::parse("0.0.0-example").expect("a version"),
        asset: "crossc-0.0.0-example-x64.zip".into(),
        sha256: "0123456789abcdef".repeat(4),
        dir: PathBuf::from(r"C:\example\crossc\releases\v0.0.0-example"),
        cert_subject: "CN=WDRLocalTestCert".into(),
        cert_thumbprint: "0123456789ABCDEF0123456789ABCDEF01234567".into(),
    }
}

/// Label of the button that confirms.
pub fn confirm_label(f: &FetchedRelease) -> String {
    format!("Trust certificate and install {}", f.tag)
}

/// The footer while a downloaded release waits for its confirmation.
pub fn confirm_step(f: &FetchedRelease) -> Guidance {
    guide(
        Level::Warn,
        format!(
            "{} is downloaded and verified. Check the certificate above, then click \"{}\" (or Cancel)",
            f.tag,
            confirm_label(f)
        ),
    )
}

/// What to say when the release that was just installed is newer than the installer that installed it; `None`
/// when it is not (or this installer's own version is not a version).
pub fn newer_installer_note(
    running: &str,
    release: &Version,
    exe: &std::path::Path,
) -> Option<String> {
    let running_version = Version::parse(running)?;
    (*release > running_version).then(|| {
        format!(
            "this installer is version {running}; release {} has a newer one. It does not replace itself: use \
             {} from now on",
            release.tag(),
            exe.display()
        )
    })
}

/// The footer: what the user should do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Guidance {
    pub level: Level,
    pub text: String,
}

fn guide(level: Level, text: impl Into<String>) -> Guidance {
    Guidance {
        level,
        text: text.into(),
    }
}

/// What the user picks in Windows once everything runs. The endpoint is named after the buds, so the exact text
/// is theirs to recognise.
const SELECT_OUTPUT: &str =
    "select \"Headphones (... Buds ...)\" as the output device in Windows sound settings";
const PAIR_LATER: &str = "pair your Galaxy Buds (LE Audio off) and they will pick the driver up";

/// "click Install", with the detour through elevation when it is needed.
fn click_install(s: &Status) -> &'static str {
    if s.admin {
        "click Install"
    } else {
        "click \"Restart as administrator\", then Install"
    }
}

/// Why Install would stop in its preflight, before it changes anything (`None`: it would go on). The same checks
/// in the same order as the top of `actions::install`, so the footer never sends anyone to a button that refuses.
pub fn install_blocker(s: &Status) -> Option<String> {
    if let Some(why) = fetch_blocker(s) {
        return Some(why);
    }
    let p = s.package();
    if !p.complete() {
        return Some(match p.kind {
            PackageKind::Built(flavor) => format!(
                "the {flavor} driver package is not built (Build it, or select the built one)"
            ),
            PackageKind::Bundle if s.online && p.missing.len() == REQUIRED_FILES.len() => {
                "there is no driver package next to this installer: use \"Download and install\""
                    .into()
            }
            PackageKind::Bundle => format!(
                "{} is incomplete (missing {} in {}): {}",
                s.bundle_noun(),
                p.missing.join(", "),
                p.dir.display(),
                s.refetch()
            ),
        });
    }
    if s.hardware_ids.is_empty() {
        return Some("the driver package's INF lists no supported buds model".into());
    }
    if s.is_bundle() && s.service_exe.is_none() {
        return Some(format!(
            "crossc-service.exe is missing from {} or damaged: {}",
            s.bundle_noun(),
            s.refetch()
        ));
    }
    None
}

/// Why "Download and install" would stop before it downloads anything (`None`: it would go on): the one Install
/// prerequisite that does not depend on the package. The same check as the top of `actions::fetch_release`.
pub fn fetch_blocker(s: &Status) -> Option<String> {
    match s.test_signing {
        Some(true) => None,
        Some(false) => {
            Some("test signing is off (bcdedit /set testsigning on as admin, then reboot)".into())
        }
        None => Some("the test signing state cannot be read".into()),
    }
}

/// Next step as a pure function of the status: a broken or finished install first, then the prerequisites in
/// setup order. When a newer release is on offer and nothing is in the way, the line ends by saying so.
pub fn next_step(s: &Status) -> Guidance {
    let mut g = next_step_without_offer(s);
    let lone = s.online && !s.package().complete();
    if !lone
        && g.level != Level::Bad
        && g.level != Level::Unknown
        && let Some(release) = download_offer(s)
    {
        g.text.push_str(&format!(
            ". A newer release is available: \"{}\"",
            download_label(release)
        ));
    }
    g
}

/// Online mode with no package: everything depends on the release check.
fn online_step(s: &Status) -> Guidance {
    const LONE: &str = "This installer runs on its own, without a driver package next to it";
    match &s.latest {
        Latest::NotChecked => guide(
            Level::Warn,
            format!("{LONE}: click \"{CHECK_LABEL}\" to look for the latest release on GitHub"),
        ),
        Latest::Checking => guide(
            Level::Unknown,
            "Looking for the latest release on GitHub...",
        ),
        Latest::NoReleases { repo, .. } => guide(
            Level::Bad,
            format!("{LONE}, and {repo} has no published release yet: there is nothing to install"),
        ),
        Latest::Failed(why) => guide(
            Level::Bad,
            format!("{LONE}, and the release check failed: {why}"),
        ),
        Latest::Found { release, repo, .. } => match &release.assets {
            Err(why) => guide(
                Level::Bad,
                format!(
                    "{} cannot be installed from here: {why}. Get the zip from \
                     https://github.com/{repo}/releases and unpack it",
                    release.tag
                ),
            ),
            // Installed by a bundle that is gone, or by this installer before its downloads were deleted:
            // nothing has to be done, and Uninstall needs no package.
            Ok(_)
                if s.installed_version()
                    .is_some_and(|installed| installed >= release.version) =>
            {
                guide(
                    Level::Good,
                    format!(
                        "crossc {} is installed and no newer release is out. This installer has no copy of it: \
                         Uninstall works as it is, and \"{}\" fetches the release again to reinstall it",
                        s.installed_version().map_or_else(String::new, |v| v.tag()),
                        download_label(release)
                    ),
                )
            }
            Ok(_) if s.admin => guide(
                Level::Good,
                format!("Ready: click \"{}\"", download_label(release)),
            ),
            Ok(_) => guide(
                Level::Warn,
                format!(
                    "Ready to install {}: click \"Restart as administrator\", then \"{}\"",
                    release.tag,
                    download_label(release)
                ),
            ),
        },
    }
}

fn next_step_without_offer(s: &Status) -> Guidance {
    let present: Vec<&NodeInfo> = s.nodes().iter().filter(|n| n.present).collect();
    if let Some((n, code)) = present.iter().find_map(|n| {
        let code = n.problem.unwrap_or(0);
        (n.runs(DRIVER_SERVICE) && code != 0).then_some((n, code))
    }) {
        return guide(
            Level::Bad,
            format!(
                "crossc_bth is bound to {} but not running: {}",
                n.label(),
                problem_text(code)
            ),
        );
    }
    let bound = present
        .iter()
        .filter(|n| n.healthy_on(DRIVER_SERVICE))
        .count();
    if bound > 0 && bound == present.len() {
        return installed_step(s, true);
    }
    if bound > 0 {
        let rest: Vec<&str> = present
            .iter()
            .filter(|n| !n.healthy_on(DRIVER_SERVICE))
            .map(|n| n.label())
            .collect();
        let state = format!(
            "{bound} of {} paired buds run crossc_bth; still on the Windows driver: {}",
            present.len(),
            rest.join(", ")
        );
        return match install_blocker(s) {
            None => guide(
                Level::Warn,
                format!("{state}. To switch them, {}", click_install(s)),
            ),
            Some(why) => guide(
                Level::Bad,
                format!("{state}. Install cannot switch them yet: {why}"),
            ),
        };
    }
    if s.test_signing == Some(false) {
        return guide(
            Level::Bad,
            "Test signing is off: run bcdedit /set testsigning on (admin) and reboot; Secure Boot must be off (docs/SETUP.md section 1)",
        );
    }
    if s.secure_boot == Some(true) {
        return guide(
            Level::Bad,
            "Secure Boot is on: turn it off in the firmware setup, or the test-signed driver will not load (problem 52)",
        );
    }
    let p = s.package();
    if !p.complete() {
        return match p.kind {
            PackageKind::Built(flavor) => guide(
                Level::Warn,
                format!("Build the {flavor} driver package (Build, or Build + Install)"),
            ),
            PackageKind::Bundle if s.online && p.missing.len() == REQUIRED_FILES.len() => {
                online_step(s)
            }
            PackageKind::Bundle => guide(
                Level::Bad,
                format!(
                    "{} is incomplete (missing {} in {}): {}",
                    if s.online {
                        "The downloaded release"
                    } else {
                        "The bundle"
                    },
                    p.missing.join(", "),
                    p.dir.display(),
                    s.refetch()
                ),
            ),
        };
    }
    if s.hardware_ids.is_empty() {
        return guide(
            Level::Bad,
            "The driver package's INF lists no supported buds model, so there is nothing to install on",
        );
    }
    if s.is_bundle() && s.service_exe.is_none() {
        return guide(
            Level::Bad,
            format!(
                "crossc-service.exe is missing from {} or damaged: {}",
                s.bundle_noun(),
                s.refetch()
            ),
        );
    }
    if let Err(e) = &s.nodes {
        return guide(Level::Unknown, format!("Could not enumerate devices: {e}"));
    }
    if present.is_empty() && s.staged() && s.cert_trusted() {
        return installed_step(s, false);
    }
    // Whatever else Install's preflight refuses (the test signing state being unreadable, so far): "Ready" is
    // only said when Install would really go on.
    if let Some(why) = install_blocker(s) {
        return guide(Level::Bad, format!("Install cannot run yet: {why}"));
    }
    let pairing = if present.is_empty() {
        format!(". No supported buds are paired yet; that is fine: {PAIR_LATER}")
    } else {
        String::new()
    };
    if !s.admin {
        return guide(
            Level::Warn,
            format!("Ready to install: click \"Restart as administrator\", then Install{pairing}"),
        );
    }
    let mut text = format!("Ready: click Install{pairing}");
    if s.hvci_running == Some(true) {
        text.push_str(
            " (Memory integrity is on: if the node shows problem 39, turn it off and reboot)",
        );
    }
    guide(Level::Good, text)
}

/// The driver part is done (`bound`: buds are paired and run it; otherwise it is staged and waits for buds). What
/// is left depends on the service. Install is what adds or starts it (it starts a registered service even when it
/// has no `crossc-service.exe` to update it with), so Install is only suggested when its preflight would pass.
fn installed_step(s: &Status, bound: bool) -> Guidance {
    let driver = if bound {
        "The driver is installed"
    } else {
        "The driver is staged"
    };
    let info = match &s.service {
        Ok(info) => info,
        Err(e) => {
            return guide(
                Level::Unknown,
                format!("{driver}, but the crossc service state could not be read: {e}"),
            );
        }
    };
    match info.state {
        ServiceState::Running if bound => guide(
            Level::Good,
            format!("Installed and running: {SELECT_OUTPUT}"),
        ),
        ServiceState::Running => guide(
            Level::Good,
            format!("Installed, waiting for buds: {PAIR_LATER}; then {SELECT_OUTPUT}"),
        ),
        ServiceState::NotInstalled if s.is_bundle() && s.service_exe.is_none() => guide(
            Level::Bad,
            format!(
                "{driver}, but the crossc service is not, and crossc-service.exe is missing from {} or \
                 damaged: {}",
                s.bundle_noun(),
                s.refetch()
            ),
        ),
        ServiceState::NotInstalled if s.service_exe.is_some() => match install_blocker(s) {
            None => guide(
                Level::Warn,
                format!(
                    "{driver}, but the crossc service is not: {} to add and start it",
                    click_install(s)
                ),
            ),
            Some(why) => guide(
                Level::Bad,
                format!(
                    "{driver}, but the crossc service is not, and Install cannot add it yet: {why}"
                ),
            ),
        },
        ServiceState::NotInstalled => guide(
            Level::Warn,
            format!(
                "{driver}, but crossc-service.exe is not built, so nothing streams to the buds yet: Build, then \
                 Install. Diagnostics: crossc-cli probe as admin"
            ),
        ),
        state => match install_blocker(s) {
            None => guide(
                Level::Warn,
                format!(
                    "{driver}, but the crossc service is not running ({state}): {} to start it",
                    click_install(s)
                ),
            ),
            Some(why) => guide(
                Level::Bad,
                format!(
                    "{driver}, but the crossc service is not running ({state}), and Install cannot start it \
                     yet: {why}"
                ),
            ),
        },
    }
}

/// Heading of the warning shown in bundle mode before the first Install.
pub const INSTALL_WARNING_TITLE: &str =
    "Before you install: this is an experimental, test-signed kernel driver";

/// The warning's paragraphs: what it is, what can go wrong, how to get rid of it.
pub const INSTALL_WARNING: [&str; 3] = [
    "crossc_bth is a kernel driver signed with a test certificate, not by Microsoft. It only loads with Windows \
     test signing on (bcdedit /set testsigning on) and Secure Boot off, and Install adds its certificate to this \
     PC's trusted roots (LocalMachine Root and TrustedPublisher).",
    "It is experimental. A bug in a kernel driver can crash Windows (blue screen). It replaces Windows' own \
     Bluetooth audio driver for the supported Galaxy Buds only; no other Bluetooth device is touched. Install also \
     adds the crossc service (runs as LocalSystem, starts with Windows) in %ProgramFiles%\\crossc.",
    "Uninstall undoes it: it stops and removes the crossc service, deletes %ProgramFiles%\\crossc, removes the \
     driver package, and the buds go back to Windows' built-in driver (BthA2dp). \"Remove certificate\" then \
     untrusts the test certificate.",
];

/// The warning is for people who got a release bundle and have not installed it yet: bundle mode, no crossc_bth
/// package in the driver store and no node running crossc_bth. Once it is installed it has served its purpose.
pub fn show_install_warning(s: &Status) -> bool {
    let on_driver = |n: &NodeInfo| n.runs(DRIVER_SERVICE);
    s.is_bundle() && !s.staged() && !s.nodes().iter().chain(&s.unlisted).any(on_driver)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    const A013: &str = r"BTHENUM\{0000110b-0000-1000-8000-00805f9b34fb}_VID&00010075_PID&a013";
    const A00F: &str = r"BTHENUM\{0000110b-0000-1000-8000-00805f9b34fb}_VID&00010075_PID&a00f";

    fn package(kind: PackageKind, complete: bool) -> PackageInfo {
        PackageInfo {
            kind,
            dir: PathBuf::from(format!(r"C:\x\target\{kind}\crossc_bth_package")),
            missing: if complete {
                vec![]
            } else {
                REQUIRED_FILES.to_vec()
            },
            built_at: complete.then(|| "2026-11-10 22:51".into()),
            built_age: complete.then(|| "3 h ago".into()),
            driver_ver: complete.then(|| "11/11/2026,8.50.53.934".into()),
        }
    }

    fn built(flavor: Flavor, complete: bool) -> PackageInfo {
        package(PackageKind::Built(flavor), complete)
    }

    fn node(service: &str, problem: u32) -> NodeInfo {
        NodeInfo {
            instance_id: r"BTHENUM\{0000110B-0000-1000-8000-00805F9B34FB}_VID&00010075_PID&A013\B&1&2_C00000000".into(),
            present: true,
            name: Some("Buds A2DP".into()),
            device_name: Some("Buds4 Pro".into()),
            service: Some(service.into()),
            problem: Some(problem),
            inf: Some("oem1.inf".into()),
            version: Some("1.0".into()),
            stack: vec![r"\Driver\ksthunk".into(), format!(r"\Driver\{service}"), r"\Driver\BthEnum".into()],
        }
    }

    /// A second pair of buds (another model).
    fn node2(service: &str, problem: u32) -> NodeInfo {
        NodeInfo {
            instance_id: r"BTHENUM\{0000110B-0000-1000-8000-00805F9B34FB}_VID&00010075_PID&A00F\B&1&3_C00000000".into(),
            device_name: Some("Buds3 Pro".into()),
            ..node(service, problem)
        }
    }

    fn phantom(service: &str) -> NodeInfo {
        NodeInfo {
            instance_id: r"BTHENUM\{0000110B-0000-1000-8000-00805F9B34FB}_VID&00010075_PID&A013\B&1&9_C00000000".into(),
            present: false,
            problem: None,
            device_name: None,
            ..node(service, 0)
        }
    }

    fn service(state: ServiceState) -> Result<ServiceInfo, String> {
        Ok(ServiceInfo {
            state,
            exe: state
                .installed()
                .then(|| r"C:\Program Files\crossc\crossc-service.exe".into()),
        })
    }

    /// A machine that is ready to install from the repo: admin, test signing on, Secure Boot off, package built,
    /// one pair of buds on BthA2dp, service built but not installed.
    pub fn ready() -> Status {
        Status {
            admin: true,
            test_signing: Some(true),
            secure_boot: Some(false),
            hvci_running: Some(false),
            hvci_configured: Some(false),
            packages: vec![built(Flavor::Debug, true), built(Flavor::Release, false)],
            hardware_ids: vec![A013.into(), A00F.into()],
            nodes: Ok(vec![node(INBOX_SERVICE, 0)]),
            unlisted: vec![],
            cert: Ok(CertInfo {
                thumbprint: "0123456789ABCDEF".into(),
                in_root: false,
                in_trusted_publisher: false,
            }),
            staged: Ok(vec![]),
            service: service(ServiceState::NotInstalled),
            service_exe: Some(PathBuf::from(r"C:\x\target\release\crossc-service.exe")),
            online: false,
            record: InstallRecord::default(),
            bundle_version: None,
            latest: Latest::NotChecked,
        }
    }

    /// The same machine with a release bundle unpacked instead of a checkout.
    pub fn ready_bundle() -> Status {
        Status {
            packages: vec![PackageInfo {
                dir: PathBuf::from(r"C:\crossc-0.1\driver"),
                ..package(PackageKind::Bundle, true)
            }],
            service_exe: Some(PathBuf::from(r"C:\crossc-0.1\crossc-service.exe")),
            ..ready()
        }
    }

    /// Driver staged, certificate trusted, every paired node on crossc_bth, service running.
    fn installed(mut s: Status) -> Status {
        s.nodes = Ok(vec![node(DRIVER_SERVICE, 0)]);
        s.cert = Ok(CertInfo {
            thumbprint: "0123456789ABCDEF".into(),
            in_root: true,
            in_trusted_publisher: true,
        });
        s.staged = Ok(vec![StagedPackage {
            published: "oem7.inf".into(),
            version: Some("11/11/2026 8.50.53.934".into()),
        }]);
        s.service = service(ServiceState::Running);
        s
    }

    fn row<'a>(c: &'a [CheckItem], label: &str) -> &'a CheckItem {
        c.iter().find(|i| i.label == label).unwrap()
    }

    // ---- node matching -------------------------------------------------------------------------------------------

    #[test]
    fn node_matches_only_on_an_exact_inf_id() {
        let inf: Vec<String> = vec![A013.into(), A00F.into()];
        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // What Windows reports for the buds' A2DP node: the specific ID, then generic ones.
        assert!(hwid_matches(
            &ids(&[
                r"BTHENUM\{0000110B-0000-1000-8000-00805F9B34FB}_VID&00010075_PID&A013",
                r"BTHENUM\{0000110B-0000-1000-8000-00805F9B34FB}_LOCALMFG&0002",
            ]),
            &inf
        ));
        assert!(hwid_matches(
            &ids(&[r"bthenum\{0000110b-0000-1000-8000-00805f9b34fb}_vid&00010075_pid&A00F"]),
            &inf
        ));
        // Another headset, the generic compatible ID, a prefix of a supported ID, another profile of the buds.
        for other in [
            r"BTHENUM\{0000110b-0000-1000-8000-00805f9b34fb}_VID&0001000f_PID&1200",
            r"BTHENUM\{0000110b-0000-1000-8000-00805f9b34fb}",
            r"BTHENUM\{0000110b-0000-1000-8000-00805f9b34fb}_VID&00010075_PID&a01",
            r"BTHENUM\{0000110b-0000-1000-8000-00805f9b34fb}_VID&00010075_PID&a0133",
            r"BTHENUM\{0000111e-0000-1000-8000-00805f9b34fb}_VID&00010075_PID&a013",
        ] {
            assert!(!hwid_matches(&ids(&[other]), &inf), "{other}");
        }
        assert!(!hwid_matches(&[], &inf));
        // No INF IDs (nothing built): nothing matches, so nothing is ever looked at.
        assert!(!hwid_matches(&ids(&[A013]), &[]));
    }

    #[test]
    fn unlisted_nodes_are_those_on_the_driver_without_an_inf_id() {
        let inf: Vec<String> = vec![A00F.into()];
        let a013: Vec<String> = vec![A013.to_uppercase()];
        // Bound by an older package that still listed the model.
        assert!(unlisted_on_driver(&a013, Some("crossc_bth"), &inf));
        assert!(unlisted_on_driver(&a013, Some("CROSSC_BTH"), &inf));
        // No INF at all (a stray installer exe): every node on the driver.
        assert!(unlisted_on_driver(&a013, Some("crossc_bth"), &[]));
        // Listed models are the ordinary buds nodes, not this.
        assert!(!unlisted_on_driver(
            &a013,
            Some("crossc_bth"),
            &[A013.into()]
        ));
        // Any other headset, and buds that are on the Windows driver or have none.
        assert!(!unlisted_on_driver(&a013, Some("BthA2dp"), &inf));
        assert!(!unlisted_on_driver(&a013, None, &inf));
        assert!(!unlisted_on_driver(&[], Some("BthA2dp"), &[]));
    }

    #[test]
    fn present_nodes_come_first() {
        let nodes = order_nodes(vec![
            phantom(INBOX_SERVICE),
            node(INBOX_SERVICE, 0),
            node2(DRIVER_SERVICE, 0),
        ]);
        let ids: Vec<&str> = nodes.iter().map(|n| n.label()).collect();
        assert_eq!(ids, ["Buds4 Pro", "Buds3 Pro", "Buds A2DP"]);
        assert!(order_nodes(vec![]).is_empty());
    }

    #[test]
    fn node_label_prefers_the_bluetooth_name() {
        assert_eq!(node(INBOX_SERVICE, 0).label(), "Buds4 Pro");
        assert_eq!(phantom(INBOX_SERVICE).label(), "Buds A2DP");
        assert_eq!(NodeInfo::default().label(), "A2DP node");
    }

    // ---- verdicts ------------------------------------------------------------------------------------------------

    #[test]
    fn install_verdict_with_no_buds_needs_the_package_staged_and_trusted() {
        assert_eq!(install_verdict(&[], true, true), InstallVerdict::Waiting);
        assert_eq!(install_verdict(&[], false, true), InstallVerdict::NotStaged);
        assert_eq!(install_verdict(&[], true, false), InstallVerdict::NotStaged);
        // Only a stale pairing: nothing present to bind, same as no buds.
        let stale = [phantom(INBOX_SERVICE)];
        assert_eq!(install_verdict(&stale, true, true), InstallVerdict::Waiting);
    }

    #[test]
    fn install_verdict_covers_every_present_node() {
        let all = [node(DRIVER_SERVICE, 0), node2(DRIVER_SERVICE, 0)];
        assert_eq!(install_verdict(&all, true, true), InstallVerdict::Bound(2));
        // A stale pairing still on BthA2dp does not fail it.
        let with_stale = [node(DRIVER_SERVICE, 0), phantom(INBOX_SERVICE)];
        assert_eq!(
            install_verdict(&with_stale, true, true),
            InstallVerdict::Bound(1)
        );

        // One of two did not switch, or switched with a problem: it is named.
        let one_left = [node(DRIVER_SERVICE, 0), node2(INBOX_SERVICE, 0)];
        assert_eq!(
            install_verdict(&one_left, true, true),
            InstallVerdict::NotBound(vec![&one_left[1]])
        );
        let broken = [node(DRIVER_SERVICE, 52), node2(DRIVER_SERVICE, 0)];
        assert_eq!(
            install_verdict(&broken, true, true),
            InstallVerdict::NotBound(vec![&broken[0]])
        );
    }

    #[test]
    fn uninstall_checks_every_present_node_is_back_on_the_inbox_driver() {
        let back = [
            node(INBOX_SERVICE, 0),
            node2(INBOX_SERVICE, 0),
            phantom(DRIVER_SERVICE),
        ];
        assert!(not_healthy_on(&back, INBOX_SERVICE).is_empty());
        let one_left = [node(INBOX_SERVICE, 0), node2(DRIVER_SERVICE, 0)];
        assert_eq!(not_healthy_on(&one_left, INBOX_SERVICE), vec![&one_left[1]]);
        let no_driver = [NodeInfo {
            service: None,
            problem: Some(28),
            ..node(INBOX_SERVICE, 0)
        }];
        assert_eq!(not_healthy_on(&no_driver, INBOX_SERVICE).len(), 1);
        assert!(not_healthy_on(&[], INBOX_SERVICE).is_empty());
    }

    // ---- service state -------------------------------------------------------------------------------------------

    #[test]
    fn service_states() {
        assert_eq!(service_state(1), ServiceState::Stopped);
        assert_eq!(service_state(2), ServiceState::StartPending);
        assert_eq!(service_state(3), ServiceState::StopPending);
        assert_eq!(service_state(4), ServiceState::Running);
        assert_eq!(service_state(7), ServiceState::Other(7));
        assert_eq!(service_state(0), ServiceState::Other(0));
        assert!(!ServiceState::NotInstalled.installed());
        assert!(ServiceState::Stopped.installed());
        assert!(ServiceState::Other(7).installed());
        assert_eq!(ServiceState::NotInstalled.to_string(), "not installed");
        assert_eq!(ServiceState::Stopped.to_string(), "installed, stopped");
        assert_eq!(ServiceState::Running.to_string(), "running");
        assert_eq!(ServiceState::Other(7).to_string(), "installed, state 7");
    }

    // ---- next step -----------------------------------------------------------------------------------------------

    #[test]
    fn guidance_ready() {
        let g = next_step(&ready());
        assert_eq!(g.level, Level::Good);
        assert_eq!(g.text, "Ready: click Install");
        assert_eq!(next_step(&ready_bundle()).text, "Ready: click Install");
    }

    #[test]
    fn guidance_installed_and_running_points_at_the_sound_settings() {
        let mut s = installed(ready_bundle());
        // Installed wins over everything else, even a stale test-signing reading.
        s.test_signing = None;
        s.admin = false;
        let g = next_step(&s);
        assert_eq!(g.level, Level::Good);
        assert_eq!(
            g.text,
            "Installed and running: select \"Headphones (... Buds ...)\" as the output device in Windows sound settings"
        );
        // Two pairs of buds, both bound: the same.
        s.nodes = Ok(vec![node(DRIVER_SERVICE, 0), node2(DRIVER_SERVICE, 0)]);
        assert!(next_step(&s).text.starts_with("Installed and running"));
    }

    #[test]
    fn guidance_installed_but_service_not_running() {
        let mut s = installed(ready_bundle());
        s.service = service(ServiceState::Stopped);
        let g = next_step(&s);
        assert_eq!(g.level, Level::Warn);
        assert_eq!(
            g.text,
            "The driver is installed, but the crossc service is not running (installed, stopped): click Install to start it"
        );
        s.admin = false;
        assert!(
            next_step(&s)
                .text
                .ends_with("click \"Restart as administrator\", then Install to start it")
        );

        s.service = service(ServiceState::NotInstalled);
        let g = next_step(&s);
        assert_eq!(g.level, Level::Warn);
        assert!(
            g.text
                .starts_with("The driver is installed, but the crossc service is not: "),
            "{}",
            g.text
        );
        assert!(g.text.ends_with("to add and start it"), "{}", g.text);

        s.service = Err("access denied".into());
        let g = next_step(&s);
        assert_eq!(g.level, Level::Unknown);
        assert!(g.text.contains("access denied"), "{}", g.text);
    }

    #[test]
    fn guidance_never_points_at_an_install_that_would_refuse() {
        // Bundle, driver bound, service installed but stopped, crossc-service.exe gone from the bundle: Install
        // stops in its preflight, so "click Install" would loop.
        let mut s = installed(ready_bundle());
        s.service = service(ServiceState::Stopped);
        s.service_exe = None;
        let g = next_step(&s);
        assert_eq!(g.level, Level::Bad);
        assert_eq!(
            g.text,
            "The driver is installed, but the crossc service is not running (installed, stopped), and Install \
             cannot start it yet: crossc-service.exe is missing from the bundle or damaged: unpack the whole \
             release zip again"
        );
        assert!(!g.text.contains("click"), "{}", g.text);

        // The same with the driver package incomplete.
        let mut s = installed(ready_bundle());
        s.service = service(ServiceState::Stopped);
        s.packages[0].missing = vec!["crossc_bth.sys"];
        let g = next_step(&s);
        assert_eq!(g.level, Level::Bad);
        assert_eq!(
            g.text,
            r"The driver is installed, but the crossc service is not running (installed, stopped), and Install cannot start it yet: the bundle is incomplete (missing crossc_bth.sys in C:\crossc-0.1\driver): unpack the whole release zip again"
        );
        // Service not registered at all, exe there, package incomplete: Install cannot add it either.
        s.service = service(ServiceState::NotInstalled);
        let g = next_step(&s);
        assert_eq!(g.level, Level::Bad);
        assert!(
            g.text.starts_with(
                "The driver is installed, but the crossc service is not, and Install cannot add it yet: the \
                 bundle is incomplete"
            ),
            "{}",
            g.text
        );

        // Repo: the release driver is installed, the debug flavour is selected and not built.
        let mut s = installed(ready());
        s.service = service(ServiceState::StopPending);
        s.packages = vec![built(Flavor::Debug, false), built(Flavor::Release, true)];
        let g = next_step(&s);
        assert_eq!(g.level, Level::Bad);
        assert_eq!(
            g.text,
            "The driver is installed, but the crossc service is not running (installed, stopping), and Install \
             cannot start it yet: the debug driver package is not built (Build it, or select the built one)"
        );

        // Staged and waiting for buds, service stopped, test signing unreadable: Install refuses there too.
        let mut s = installed(ready_bundle());
        s.nodes = Ok(vec![]);
        s.service = service(ServiceState::Stopped);
        s.test_signing = None;
        let g = next_step(&s);
        assert_eq!(g.level, Level::Bad);
        assert!(
            g.text
                .starts_with("The driver is staged, but the crossc service is not running")
                && g.text.ends_with("the test signing state cannot be read"),
            "{}",
            g.text
        );
    }

    #[test]
    fn guidance_repo_starts_a_stopped_service_without_a_built_exe() {
        // Repo, driver bound, service registered but stopped, target\release\crossc-service.exe not built:
        // Install goes on without the exe and starts the registered service, so the advice holds.
        let mut s = installed(ready());
        s.service = service(ServiceState::Stopped);
        s.service_exe = None;
        assert_eq!(install_blocker(&s), None);
        let g = next_step(&s);
        assert_eq!(g.level, Level::Warn);
        assert_eq!(
            g.text,
            "The driver is installed, but the crossc service is not running (installed, stopped): click Install \
             to start it"
        );
    }

    #[test]
    fn install_blockers_follow_the_preflight_order() {
        assert_eq!(install_blocker(&ready()), None);
        assert_eq!(install_blocker(&ready_bundle()), None);
        // Not blockers: no admin yet (the button says so), Secure Boot (a warning in Install), no buds paired.
        let mut s = ready_bundle();
        s.admin = false;
        s.secure_boot = Some(true);
        s.nodes = Ok(vec![]);
        assert_eq!(install_blocker(&s), None);

        // Everything wrong at once: the first check wins, then the next one.
        let mut s = ready_bundle();
        s.test_signing = Some(false);
        s.packages[0].missing = vec!["crossc_bth.cat"];
        s.hardware_ids.clear();
        s.service_exe = None;
        let why = |s: &Status| install_blocker(s).unwrap();
        assert!(why(&s).starts_with("test signing is off"));
        s.test_signing = None;
        assert_eq!(why(&s), "the test signing state cannot be read");
        s.test_signing = Some(true);
        assert!(why(&s).starts_with("the bundle is incomplete (missing crossc_bth.cat"));
        s.packages[0].missing.clear();
        assert!(why(&s).contains("lists no supported buds model"));
        s.hardware_ids = vec![A013.into()];
        assert!(why(&s).starts_with("crossc-service.exe is missing from the bundle"));
        s.service_exe = ready_bundle().service_exe;
        assert_eq!(install_blocker(&s), None);

        // In the repo the service exe is optional.
        let mut s = ready();
        s.service_exe = None;
        assert_eq!(install_blocker(&s), None);
        s.packages = vec![built(Flavor::Release, false), built(Flavor::Debug, true)];
        assert!(why(&s).starts_with("the release driver package is not built"));
    }

    #[test]
    fn guidance_repo_without_a_built_service() {
        let mut s = installed(ready());
        s.service = service(ServiceState::NotInstalled);
        s.service_exe = None;
        let g = next_step(&s);
        assert_eq!(g.level, Level::Warn);
        assert!(
            g.text.contains("crossc-service.exe is not built"),
            "{}",
            g.text
        );
        assert!(g.text.contains("crossc-cli probe"), "{}", g.text);

        // A bundle has nothing to build: the zip was not unpacked completely.
        let mut s = installed(ready_bundle());
        s.service = service(ServiceState::NotInstalled);
        s.service_exe = None;
        let g = next_step(&s);
        assert_eq!(g.level, Level::Bad);
        assert_eq!(
            g.text,
            "The driver is installed, but the crossc service is not, and crossc-service.exe is missing from the \
             bundle or damaged: unpack the whole release zip again"
        );
    }

    #[test]
    fn guidance_bound_with_problem() {
        let mut s = installed(ready());
        s.nodes = Ok(vec![node(DRIVER_SERVICE, 0), node2(DRIVER_SERVICE, 52)]);
        let g = next_step(&s);
        assert_eq!(g.level, Level::Bad);
        assert!(g.text.contains("problem 52"), "{}", g.text);
        assert!(g.text.contains("Buds3 Pro"), "{}", g.text);
    }

    #[test]
    fn guidance_some_buds_not_switched_yet() {
        let mut s = installed(ready_bundle());
        s.nodes = Ok(vec![
            node(DRIVER_SERVICE, 0),
            node2(INBOX_SERVICE, 0),
            phantom(INBOX_SERVICE),
        ]);
        let g = next_step(&s);
        assert_eq!(g.level, Level::Warn);
        assert_eq!(
            g.text,
            "1 of 2 paired buds run crossc_bth; still on the Windows driver: Buds3 Pro. To switch them, click Install"
        );

        // Install would refuse in its preflight (the bundle lost its service exe): no "click Install" then.
        s.service_exe = None;
        let g = next_step(&s);
        assert_eq!(g.level, Level::Bad);
        assert_eq!(
            g.text,
            "1 of 2 paired buds run crossc_bth; still on the Windows driver: Buds3 Pro. Install cannot switch them \
             yet: crossc-service.exe is missing from the bundle or damaged: unpack the whole release zip again"
        );
        assert!(!g.text.contains("click"), "{}", g.text);
    }

    #[test]
    fn guidance_is_not_ready_while_the_test_signing_state_is_unknown() {
        // Install stops when it cannot read the test signing state, so neither layout may say "Ready".
        for mut s in [ready(), ready_bundle()] {
            s.test_signing = None;
            for admin in [true, false] {
                s.admin = admin;
                let g = next_step(&s);
                assert_eq!(g.level, Level::Bad);
                assert_eq!(
                    g.text,
                    "Install cannot run yet: the test signing state cannot be read"
                );
                assert!(!g.text.contains("click"), "{}", g.text);
            }
            // No buds paired does not change that.
            s.nodes = Ok(vec![]);
            assert!(next_step(&s).text.starts_with("Install cannot run yet"));
        }
    }

    #[test]
    fn guidance_test_signing_off_first() {
        let mut s = ready();
        s.test_signing = Some(false);
        s.secure_boot = Some(true);
        s.admin = false;
        s.packages = vec![built(Flavor::Debug, false), built(Flavor::Release, false)];
        let g = next_step(&s);
        assert_eq!(g.level, Level::Bad);
        assert!(g.text.contains("bcdedit /set testsigning on"), "{}", g.text);
        assert!(g.text.contains("Secure Boot must be off"), "{}", g.text);
        // The same explanation in bundle mode, before anything about the bundle.
        let mut b = ready_bundle();
        b.test_signing = Some(false);
        assert_eq!(next_step(&b).text, g.text);
    }

    #[test]
    fn guidance_secure_boot_on() {
        let mut s = ready();
        s.secure_boot = Some(true);
        assert!(next_step(&s).text.starts_with("Secure Boot is on"));
        let mut b = ready_bundle();
        b.secure_boot = Some(true);
        assert!(next_step(&b).text.contains("problem 52"));
    }

    #[test]
    fn guidance_build_then_elevate() {
        let mut s = ready();
        s.packages = vec![built(Flavor::Release, false), built(Flavor::Debug, true)];
        assert_eq!(
            next_step(&s).text,
            "Build the release driver package (Build, or Build + Install)"
        );

        let mut s = ready();
        s.admin = false;
        let g = next_step(&s);
        assert_eq!(g.level, Level::Warn);
        assert_eq!(
            g.text,
            "Ready to install: click \"Restart as administrator\", then Install"
        );

        let mut s = ready();
        s.nodes = Err("CM_Get_Device_ID_List failed".into());
        let g = next_step(&s);
        assert_eq!(g.level, Level::Unknown);
        assert!(g.text.starts_with("Could not enumerate devices"));
    }

    #[test]
    fn guidance_no_buds_paired_does_not_block_the_install() {
        for mut s in [ready(), ready_bundle()] {
            s.nodes = Ok(vec![]);
            let g = next_step(&s);
            assert_eq!(g.level, Level::Good);
            assert_eq!(
                g.text,
                "Ready: click Install. No supported buds are paired yet; that is fine: pair your Galaxy Buds \
                 (LE Audio off) and they will pick the driver up"
            );
            // A stale pairing is not a pair of buds to install on either.
            s.nodes = Ok(vec![phantom(INBOX_SERVICE)]);
            assert_eq!(next_step(&s).text, g.text);
            s.admin = false;
            let g = next_step(&s);
            assert_eq!(g.level, Level::Warn);
            assert!(g.text.starts_with("Ready to install: click \"Restart as administrator\", then Install. No supported buds"));
        }
    }

    #[test]
    fn guidance_staged_and_waiting_for_buds() {
        let mut s = installed(ready_bundle());
        s.nodes = Ok(vec![]);
        let g = next_step(&s);
        assert_eq!(g.level, Level::Good);
        assert_eq!(
            g.text,
            "Installed, waiting for buds: pair your Galaxy Buds (LE Audio off) and they will pick the driver up; \
             then select \"Headphones (... Buds ...)\" as the output device in Windows sound settings"
        );
        s.service = service(ServiceState::Stopped);
        let g = next_step(&s);
        assert_eq!(g.level, Level::Warn);
        assert!(
            g.text
                .starts_with("The driver is staged, but the crossc service is not running")
        );

        // Staged but the certificate is not trusted (or the other way round): not installed, Install fixes it.
        let mut s = installed(ready_bundle());
        s.nodes = Ok(vec![]);
        s.cert = ready().cert;
        assert!(next_step(&s).text.starts_with("Ready: click Install"));
        let mut s = installed(ready_bundle());
        s.nodes = Ok(vec![]);
        s.staged = Ok(vec![]);
        assert!(next_step(&s).text.starts_with("Ready: click Install"));
    }

    #[test]
    fn guidance_incomplete_bundle() {
        let mut s = ready_bundle();
        s.packages[0].missing = vec!["crossc_bth.sys", "crossc_bth.cat"];
        let g = next_step(&s);
        assert_eq!(g.level, Level::Bad);
        assert_eq!(
            g.text,
            r"The bundle is incomplete (missing crossc_bth.sys, crossc_bth.cat in C:\crossc-0.1\driver): unpack the whole release zip again"
        );
        assert!(!g.text.contains("Build"));

        let mut s = ready_bundle();
        s.service_exe = None;
        let g = next_step(&s);
        assert_eq!(g.level, Level::Bad);
        assert!(
            g.text
                .starts_with("crossc-service.exe is missing from the bundle or damaged")
        );
        // In the repo a missing service exe does not block: Install skips the service.
        let mut s = ready();
        s.service_exe = None;
        assert_eq!(next_step(&s).text, "Ready: click Install");

        let mut s = ready_bundle();
        s.hardware_ids.clear();
        s.nodes = Ok(vec![]);
        assert_eq!(next_step(&s).level, Level::Bad);
    }

    #[test]
    fn guidance_hvci_note() {
        let mut s = ready();
        s.hvci_running = Some(true);
        assert!(next_step(&s).text.contains("problem 39"));
    }

    // ---- warning -------------------------------------------------------------------------------------------------

    #[test]
    fn install_warning_only_in_a_bundle_that_is_not_installed_yet() {
        assert!(show_install_warning(&ready_bundle()));
        // Never in the repo: whoever builds the driver knows what it is.
        assert!(!show_install_warning(&ready()));
        assert!(!show_install_warning(&installed(ready_bundle())));
        // Staged and waiting for buds counts as installed; so does a bound node whose package cannot be listed.
        let mut s = installed(ready_bundle());
        s.nodes = Ok(vec![]);
        assert!(!show_install_warning(&s));
        let mut s = installed(ready_bundle());
        s.staged = Err("pnputil failed to start".into());
        assert!(!show_install_warning(&s));
        // Unknown staging with nothing bound: better shown once too often.
        let mut s = ready_bundle();
        s.staged = Err("pnputil failed to start".into());
        assert!(show_install_warning(&s));
    }

    #[test]
    fn install_warning_says_what_it_is_and_how_to_undo_it() {
        assert!(INSTALL_WARNING_TITLE.contains("experimental"));
        assert!(INSTALL_WARNING_TITLE.contains("test-signed kernel driver"));
        let text = INSTALL_WARNING.join("\n");
        for needle in [
            "kernel driver",
            "test signing on",
            "Secure Boot off",
            "trusted roots",
            "experimental",
            "crash Windows",
            "no other Bluetooth device",
            "Uninstall undoes it",
            "removes the crossc service",
            r"%ProgramFiles%\crossc",
            "BthA2dp",
            "Remove certificate",
        ] {
            assert!(text.contains(needle), "{needle} missing");
        }
    }

    // ---- checklist -----------------------------------------------------------------------------------------------

    #[test]
    fn checklist_levels() {
        let s = ready();
        let c = checklist(&s);
        let labels: Vec<&str> = c.iter().map(|i| i.label).collect();
        assert_eq!(
            labels,
            [
                "Administrator",
                "Test signing",
                "Secure Boot",
                "Memory integrity",
                "Driver package",
                "Certificate trusted",
                "Buds",
                "Staged packages",
                "Service",
                "Latest release"
            ]
        );
        assert_eq!(row(&c, "Administrator").level, Level::Good);
        assert_eq!(row(&c, "Certificate trusted").level, Level::Warn);
        assert_eq!(
            row(&c, "Driver package").detail,
            "debug package complete, built 2026-11-10 22:51 (3 h ago), 2 supported models"
        );
        let buds = row(&c, "Buds");
        assert_eq!(buds.level, Level::Good);
        assert_eq!(
            buds.detail,
            r"Buds4 Pro: BthA2dp, problem 0, on BTHENUM\{0000110B-0000-1000-8000-00805F9B34FB}_VID&00010075_PID&A013\B&1&2_C00000000"
        );
        assert_eq!(
            buds.sub.as_deref(),
            Some("stack ksthunk -> BthA2dp -> BthEnum   inf oem1.inf   version 1.0")
        );
        assert_eq!(row(&c, "Service").level, Level::Warn);
        assert_eq!(
            row(&c, "Service").detail,
            "not installed (Install adds and starts it)"
        );
    }

    #[test]
    fn checklist_bundle_rows() {
        let c = checklist(&ready_bundle());
        assert_eq!(
            row(&c, "Driver package").detail,
            "bundle 8.50.53.934 complete, 2 supported models"
        );
        let mut s = ready_bundle();
        s.admin = false;
        let c = checklist(&s);
        assert!(!row(&c, "Administrator").detail.contains("Build"));

        s.packages[0].missing = vec!["crossc_bth.cat"];
        s.service_exe = None;
        let c = checklist(&s);
        assert_eq!(row(&c, "Driver package").level, Level::Bad);
        assert_eq!(
            row(&c, "Driver package").detail,
            r"bundle incomplete, missing crossc_bth.cat in C:\crossc-0.1\driver: unpack the whole release zip again"
        );
        assert_eq!(row(&c, "Service").level, Level::Bad);
        assert!(
            row(&c, "Service")
                .detail
                .contains("missing from the bundle or damaged")
        );
    }

    #[test]
    fn checklist_lists_every_paired_node() {
        let mut s = installed(ready_bundle());
        s.nodes = Ok(vec![
            node(DRIVER_SERVICE, 0),
            node2(DRIVER_SERVICE, 39),
            phantom(INBOX_SERVICE),
        ]);
        let c = checklist(&s);
        let buds: Vec<&CheckItem> = c.iter().filter(|i| i.label == "Buds").collect();
        assert_eq!(buds.len(), 3);
        assert_eq!(buds[0].level, Level::Good);
        assert!(
            buds[0]
                .detail
                .starts_with("Buds4 Pro: crossc_bth, problem 0, on BTHENUM")
        );
        assert_eq!(buds[1].level, Level::Bad);
        assert!(
            buds[1]
                .detail
                .starts_with("Buds3 Pro: crossc_bth, problem 39")
        );
        assert!(buds[1].detail.contains(r"PID&A00F\B&1&3_C00000000"));
        assert_eq!(buds[2].level, Level::Warn);
        assert!(
            buds[2]
                .detail
                .starts_with("Buds A2DP: BthA2dp, not present")
        );
        assert!(buds.iter().all(|b| b.sub.is_some()));
        // The rows after the nodes are still there.
        assert_eq!(c[c.len() - 2].label, "Service");
        assert_eq!(c.last().unwrap().label, "Latest release");
        assert_eq!(
            row(&c, "Service").detail,
            r"running: C:\Program Files\crossc\crossc-service.exe"
        );
        assert_eq!(row(&c, "Service").level, Level::Good);
    }

    #[test]
    fn checklist_shows_nodes_on_the_driver_that_the_package_does_not_list() {
        let buds = |s: &Status| -> Vec<CheckItem> {
            checklist(s)
                .into_iter()
                .filter(|i| i.label == "Buds")
                .collect()
        };
        // The package lists other models only; the paired buds run crossc_bth from an older package.
        let mut s = installed(ready_bundle());
        s.nodes = Ok(vec![]);
        s.unlisted = vec![node(DRIVER_SERVICE, 0)];
        let rows = buds(&s);
        assert_eq!(rows.len(), 2);
        assert!(
            rows[0].detail.starts_with("none paired"),
            "{}",
            rows[0].detail
        );
        assert_eq!(rows[1].level, Level::Warn);
        assert!(
            rows[1]
                .detail
                .starts_with("Buds4 Pro: crossc_bth, problem 0, on BTHENUM"),
            "{}",
            rows[1].detail
        );
        assert!(
            rows[1].detail.ends_with(
                "; its model is not in this package: Install replaces the package it runs and it goes back to \
                 the Windows driver"
            ),
            "{}",
            rows[1].detail
        );
        assert!(rows[1].sub.is_some());
        // A problem code stays red.
        s.unlisted = vec![node(DRIVER_SERVICE, 39)];
        assert_eq!(buds(&s)[1].level, Level::Bad);
        // No INF at all: the node is still shown, with what Uninstall does to it.
        s.hardware_ids.clear();
        s.unlisted = vec![node(DRIVER_SERVICE, 0)];
        let rows = buds(&s);
        assert_eq!(rows.len(), 2);
        assert!(
            rows[1]
                .detail
                .ends_with("; Uninstall puts it back on the Windows driver")
        );
        // It counts as "installed" for the first-install warning even when the driver store cannot be listed.
        let mut s = ready_bundle();
        s.staged = Err("pnputil failed to start".into());
        s.nodes = Ok(vec![]);
        assert!(show_install_warning(&s));
        s.unlisted = vec![node(DRIVER_SERVICE, 0)];
        assert!(!show_install_warning(&s));
    }

    #[test]
    fn checklist_no_buds_is_a_warning_not_a_blocker() {
        let mut s = ready_bundle();
        s.nodes = Ok(vec![]);
        let c = checklist(&s);
        let buds = row(&c, "Buds");
        assert_eq!(buds.level, Level::Warn);
        assert!(
            buds.detail.starts_with("none paired (2 supported models)"),
            "{}",
            buds.detail
        );
        assert!(buds.detail.contains("Install works without them"));
        assert!(buds.sub.is_none());

        s.hardware_ids.clear();
        assert_eq!(row(&checklist(&s), "Buds").level, Level::Unknown);
        s.nodes = Err("boom".into());
        assert_eq!(row(&checklist(&s), "Buds").detail, "boom");
    }

    #[test]
    fn checklist_unknowns_and_problems() {
        let mut s = ready();
        s.admin = false;
        s.test_signing = None;
        s.secure_boot = Some(true);
        s.hvci_running = Some(false);
        s.hvci_configured = Some(true);
        s.nodes = Ok(vec![node(DRIVER_SERVICE, 39)]);
        s.staged = Ok(vec![
            StagedPackage {
                published: "oem1.inf".into(),
                version: None,
            },
            StagedPackage {
                published: "oem2.inf".into(),
                version: Some("v".into()),
            },
        ]);
        s.service = service(ServiceState::Stopped);
        let c = checklist(&s);
        assert_eq!(c[0].level, Level::Warn);
        assert!(c[0].detail.contains("Build works"));
        assert_eq!(c[1].level, Level::Unknown);
        assert_eq!(c[2].level, Level::Bad);
        assert_eq!(c[3].level, Level::Warn);
        assert_eq!(c[6].level, Level::Bad);
        assert!(c[6].detail.contains("problem 39"));
        assert_eq!(c[7].level, Level::Warn);
        assert_eq!(c[7].detail, "oem1.inf, oem2.inf (v)");
        assert_eq!(c[8].level, Level::Warn);
        assert_eq!(
            c[8].detail,
            r"installed, stopped: C:\Program Files\crossc\crossc-service.exe"
        );

        s.service = Err("OpenSCManager failed".into());
        s.service_exe = None;
        assert_eq!(row(&checklist(&s), "Service").level, Level::Unknown);
        s.service = service(ServiceState::NotInstalled);
        let c = checklist(&s);
        assert_eq!(row(&c, "Service").level, Level::Warn);
        assert!(row(&c, "Service").detail.contains("Install skips it"));
    }

    #[test]
    fn checklist_says_when_the_staged_package_is_another_version() {
        let mut s = installed(ready_bundle());
        assert_eq!(
            row(&checklist(&s), "Staged packages").detail,
            "oem7.inf (11/11/2026 8.50.53.934)"
        );
        s.staged = Ok(vec![StagedPackage {
            published: "oem7.inf".into(),
            version: Some("11/10/2026 22.51.13.530".into()),
        }]);
        assert_eq!(
            row(&checklist(&s), "Staged packages").detail,
            "oem7.inf (11/10/2026 22.51.13.530); the bundle package is 8.50.53.934: Install replaces it"
        );
        // Not claimed when either version is unknown.
        s.packages[0].driver_ver = None;
        assert_eq!(
            row(&checklist(&s), "Staged packages").detail,
            "oem7.inf (11/10/2026 22.51.13.530)"
        );
    }

    #[test]
    fn bundle_labels() {
        assert_eq!(bundle_label(Some("8.50.53.934")), "bundle 8.50.53.934");
        assert_eq!(bundle_label(None), "bundle");
        assert_eq!(
            bundle_label(package(PackageKind::Bundle, true).version()),
            "bundle 8.50.53.934"
        );
        assert_eq!(PackageKind::Bundle.to_string(), "bundle");
        assert_eq!(PackageKind::Built(Flavor::Release).to_string(), "release");
        assert_eq!(ready().flavor(), Some(Flavor::Debug));
        assert_eq!(ready_bundle().flavor(), None);
        assert!(ready_bundle().is_bundle());
    }

    #[test]
    fn stack_text_strips_driver_prefix() {
        assert_eq!(
            node(INBOX_SERVICE, 0).stack_text(),
            "ksthunk -> BthA2dp -> BthEnum"
        );
    }

    // ---- latest release, the offer, online mode -------------------------------------------------------------------

    use crate::release::{Asset, ReleaseAssets};

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    fn release(version: &str) -> LatestRelease {
        let asset = |name: String| Asset {
            name,
            api_url: "https://api.github.com/repos/mrquantumoff/crossc/releases/assets/1".into(),
            size: 1000,
            sha256: None,
        };
        LatestRelease {
            tag: format!("v{version}"),
            version: v(version),
            assets: Ok(ReleaseAssets {
                zip: asset(format!("crossc-{version}-x64.zip")),
                sums: asset("SHA256SUMS.txt".into()),
            }),
        }
    }

    fn found(version: &str) -> Latest {
        Latest::Found {
            release: Box::new(release(version)),
            repo: "mrquantumoff/crossc".into(),
            auth: "anonymous".into(),
            skipped: Vec::new(),
        }
    }

    /// `s` with `version` installed: the record names it and the staged package it was installed with.
    fn with_version(mut s: Status, version: &str) -> Status {
        s = installed(s);
        s.record = InstallRecord {
            cert_thumbprint: Some("0123456789ABCDEF0123456789ABCDEF01234567".into()),
            pending_certs: None,
            version: Some(version.into()),
            driver_ver: Some("8.50.53.934".into()),
        };
        s
    }

    /// The installer on its own on a machine that is otherwise ready: no package, nothing installed.
    fn lone() -> Status {
        let mut s = ready_bundle();
        s.online = true;
        s.packages = vec![PackageInfo {
            dir: PathBuf::from(r"C:\Users\x\Downloads\driver"),
            ..package(PackageKind::Bundle, false)
        }];
        s.hardware_ids = vec![];
        s.nodes = Ok(vec![]);
        s.cert = Err("no WDRLocalTestCert.cer in the driver package".into());
        s.service_exe = None;
        s
    }

    /// Online mode after "Download and install": the installed release is the bundle.
    fn online_installed(version: &str) -> Status {
        let mut s = with_version(ready_bundle(), version);
        s.online = true;
        s.bundle_version = Some(v(version));
        s.packages[0].dir = PathBuf::from(format!(
            r"C:\Program Files\crossc\releases\v{version}\driver"
        ));
        s
    }

    fn latest_row(s: &Status) -> CheckItem {
        row(&checklist(s), "Latest release").clone()
    }

    #[test]
    fn installed_version_needs_the_record_to_match_what_is_staged() {
        let s = with_version(ready_bundle(), "0.1.0");
        assert_eq!(s.installed_version(), Some(v("0.1.0")));
        // Nothing recorded (an install from the repo, or by the script).
        let mut none = installed(ready());
        assert_eq!(none.installed_version(), None);
        // A record that an install from the repo left without a version, or with something that is not one.
        none.record.driver_ver = Some("8.50.53.934".into());
        assert_eq!(none.installed_version(), None);
        none.record.version = Some("dev".into());
        assert_eq!(none.installed_version(), None);
        // The record is about another driver package than the one that is staged now.
        let mut stale = s.clone();
        stale.record.driver_ver = Some("1.2.3.4".into());
        assert_eq!(stale.installed_version(), None);
        stale.record.driver_ver = None;
        assert_eq!(stale.installed_version(), None);
        // Nothing staged any more (uninstalled by hand), or the driver store cannot be listed.
        let mut gone = s.clone();
        gone.staged = Ok(vec![]);
        assert_eq!(gone.installed_version(), None);
        gone.staged = Err("pnputil failed".into());
        assert_eq!(gone.installed_version(), None);
        // Two staged packages: the recorded one is among them.
        let mut two = s.clone();
        two.staged.as_mut().unwrap().insert(
            0,
            StagedPackage {
                published: "oem3.inf".into(),
                version: Some("11/10/2026 1.0.0.0".into()),
            },
        );
        assert_eq!(two.installed_version(), Some(v("0.1.0")));
    }

    #[test]
    fn latest_release_row() {
        // Before any check, and while one runs.
        let s = ready_bundle();
        let r = latest_row(&s);
        assert_eq!(r.level, Level::Unknown);
        assert_eq!(
            r.detail,
            "unknown: \"Check for update\" asks GitHub for the newest release"
        );
        let mut s = ready_bundle();
        s.latest = Latest::Checking;
        assert_eq!(
            (latest_row(&s).level, latest_row(&s).detail.as_str()),
            (Level::Unknown, "checking...")
        );

        // Up to date, update available, installed newer than the latest release.
        let mut s = with_version(ready_bundle(), "0.2.0");
        s.latest = found("0.2.0");
        assert_eq!(
            (latest_row(&s).level, latest_row(&s).detail.as_str()),
            (Level::Good, "up to date: v0.2.0 is installed")
        );
        s.latest = found("0.3.0");
        assert_eq!(
            (latest_row(&s).level, latest_row(&s).detail.as_str()),
            (Level::Warn, "update available: v0.3.0 (installed v0.2.0)")
        );
        s.latest = found("0.1.0");
        assert_eq!(
            (latest_row(&s).level, latest_row(&s).detail.as_str()),
            (
                Level::Good,
                "v0.1.0 is the latest release; the installed v0.2.0 is newer"
            )
        );

        // Nothing installed: a bundle that is the latest release says so; an older one, or the repo, is told
        // there is a release.
        let mut s = ready_bundle();
        s.bundle_version = Some(v("0.2.0"));
        s.latest = found("0.2.0");
        assert_eq!(
            (latest_row(&s).level, latest_row(&s).detail.as_str()),
            (
                Level::Good,
                "v0.2.0 is the latest release, and this bundle is v0.2.0: Install installs it"
            )
        );
        s.latest = found("0.3.0");
        assert_eq!(
            (latest_row(&s).level, latest_row(&s).detail.as_str()),
            (Level::Warn, "v0.3.0 is available (crossc is not installed)")
        );
        let mut s = ready();
        s.latest = found("0.3.0");
        assert_eq!(
            latest_row(&s).detail,
            "v0.3.0 is available (crossc is not installed)"
        );
        // Installed, but by a build from the repo: the version is not known.
        let mut s = installed(ready());
        s.latest = found("0.3.0");
        assert_eq!(
            latest_row(&s).detail,
            "v0.3.0 is available (which version is installed is not known)"
        );

        // No release, a failed check, a release without its assets.
        let mut s = ready_bundle();
        s.latest = Latest::NoReleases {
            repo: "mrquantumoff/crossc".into(),
            auth: "token from gh auth token".into(),
            skipped: Vec::new(),
        };
        assert_eq!(
            (latest_row(&s).level, latest_row(&s).detail.as_str()),
            (
                Level::Unknown,
                "mrquantumoff/crossc has no published release yet"
            )
        );
        assert_eq!(
            s.latest.source_text().as_deref(),
            Some("mrquantumoff/crossc, token from gh auth token")
        );
        assert_eq!(s.latest.skipped_note(), None);
        // A published release whose tag is not a release tag is named in the row, not passed over in silence:
        // with no other release, and next to the one that is offered.
        s.latest = Latest::NoReleases {
            repo: "mrquantumoff/crossc".into(),
            auth: "anonymous".into(),
            skipped: vec!["v0.2.0-rc.01".into(), "v01.2.3".into()],
        };
        assert_eq!(
            latest_row(&s).detail,
            "mrquantumoff/crossc has no published release yet; ignored v0.2.0-rc.01, v01.2.3: not a release \
             tag (vMAJOR.MINOR.PATCH or vMAJOR.MINOR.PATCH-suffix, numbers without leading zeros)"
        );
        s.latest = Latest::Found {
            release: Box::new(release("0.1.0")),
            repo: "mrquantumoff/crossc".into(),
            auth: "anonymous".into(),
            skipped: vec!["v0.2.0-rc.01".into()],
        };
        let detail = latest_row(&s).detail;
        assert!(detail.contains("v0.1.0"), "{detail}");
        assert!(
            detail.ends_with(
                "; ignored v0.2.0-rc.01: not a release tag (vMAJOR.MINOR.PATCH or \
                 vMAJOR.MINOR.PATCH-suffix, numbers without leading zeros)"
            ),
            "{detail}"
        );
        for quiet in [
            Latest::NotChecked,
            Latest::Checking,
            Latest::Failed("no network".into()),
        ] {
            assert_eq!(quiet.skipped_note(), None);
        }
        s.latest = Latest::NoReleases {
            repo: "mrquantumoff/crossc".into(),
            auth: "token from gh auth token".into(),
            skipped: Vec::new(),
        };
        s.latest = Latest::Failed(
            "mrquantumoff/crossc was not found (HTTP 404). If the repository is private, set GITHUB_TOKEN \
             (anonymous)"
                .into(),
        );
        let r = latest_row(&s);
        assert_eq!(r.level, Level::Warn);
        assert!(
            r.detail
                .starts_with("check failed: mrquantumoff/crossc was not found"),
            "{}",
            r.detail
        );
        assert!(
            r.detail.contains("If the repository is private"),
            "{}",
            r.detail
        );
        assert_eq!(s.latest.source_text(), None);
        let mut broken = release("0.3.0");
        broken.assets = Err("release v0.3.0 has no crossc-0.3.0-x64.zip asset".into());
        s.latest = Latest::Found {
            release: Box::new(broken),
            repo: "mrquantumoff/crossc".into(),
            auth: "anonymous".into(),
            skipped: Vec::new(),
        };
        let r = latest_row(&s);
        assert_eq!(r.level, Level::Warn);
        assert_eq!(
            r.detail,
            "v0.3.0 is the latest release, but this installer cannot install it: release v0.3.0 has no \
             crossc-0.3.0-x64.zip asset. See https://github.com/mrquantumoff/crossc/releases"
        );
        assert_eq!(download_offer(&s), None);
    }

    #[test]
    fn download_is_offered_when_the_release_is_newer_than_what_is_here() {
        let offer = |s: &Status| download_offer(s).map(|r| r.tag.clone());
        // Never without a successful check.
        for latest in [
            Latest::NotChecked,
            Latest::Checking,
            Latest::Failed("no network".into()),
            Latest::NoReleases {
                repo: "a/b".into(),
                auth: "anonymous".into(),
                skipped: Vec::new(),
            },
        ] {
            let mut s = lone();
            s.latest = latest;
            assert_eq!(offer(&s), None);
        }

        // Every mode, nothing installed: offered. A bundle that already is that release (or newer): not, its
        // own Install does it.
        for mut s in [ready(), ready_bundle(), lone()] {
            s.latest = found("0.2.0");
            assert_eq!(offer(&s).as_deref(), Some("v0.2.0"));
        }
        let mut s = ready_bundle();
        s.latest = found("0.2.0");
        s.bundle_version = Some(v("0.2.0"));
        assert_eq!(offer(&s), None);
        s.bundle_version = Some(v("0.3.0-rc.1"));
        assert_eq!(offer(&s), None);
        s.bundle_version = Some(v("0.1.0"));
        assert_eq!(offer(&s).as_deref(), Some("v0.2.0"));
        // An incomplete bundle does not count, whatever version it says it is.
        s.bundle_version = Some(v("0.2.0"));
        s.packages[0].missing = vec!["crossc_bth.sys"];
        assert_eq!(offer(&s).as_deref(), Some("v0.2.0"));

        // Installed: offered only when the release is newer.
        let mut s = with_version(ready_bundle(), "0.2.0");
        s.latest = found("0.2.0");
        assert_eq!(offer(&s), None);
        s.latest = found("0.1.0");
        assert_eq!(offer(&s), None);
        s.latest = found("0.2.1");
        assert_eq!(offer(&s).as_deref(), Some("v0.2.1"));
        // 0.2.0-rc.1 installed, 0.2.0 released.
        let mut s = with_version(ready_bundle(), "0.2.0-rc.1");
        s.latest = found("0.2.0");
        assert_eq!(offer(&s).as_deref(), Some("v0.2.0"));
        // Installed from the repo (version unknown): offered, in the repo too.
        let mut s = installed(ready());
        s.latest = found("0.2.0");
        assert_eq!(offer(&s).as_deref(), Some("v0.2.0"));

        // Online with that release installed: nothing to offer, until its files are damaged.
        let mut s = online_installed("0.2.0");
        s.latest = found("0.2.0");
        assert_eq!(offer(&s), None);
        s.service_exe = None;
        assert_eq!(offer(&s).as_deref(), Some("v0.2.0"));
        let mut s = online_installed("0.2.0");
        s.latest = found("0.2.0");
        s.packages[0].missing = vec!["crossc_bth.cat"];
        assert_eq!(offer(&s).as_deref(), Some("v0.2.0"));
        assert_eq!(
            download_label(&release("0.2.0")),
            "Download and install v0.2.0"
        );
    }

    #[test]
    fn the_lone_installer_is_not_an_incomplete_bundle() {
        let s = lone();
        let c = checklist(&s);
        let package = row(&c, "Driver package");
        assert_eq!(package.level, Level::Warn);
        assert_eq!(
            package.detail,
            "none: this installer runs on its own. \"Download and install\" fetches the latest release from GitHub"
        );
        let buds = row(&c, "Buds");
        assert_eq!(buds.level, Level::Unknown);
        assert!(
            buds.detail.starts_with("no driver package yet"),
            "{}",
            buds.detail
        );
        let service = row(&c, "Service");
        assert_eq!(
            (service.level, service.detail.as_str()),
            (
                Level::Warn,
                "not installed (\"Download and install\" adds and starts it)"
            )
        );
        // Nothing in the window speaks of a bundle to unpack.
        for item in &c {
            assert!(!item.detail.contains("unpack"), "{item:?}");
            assert!(!item.detail.contains("incomplete"), "{item:?}");
        }
        assert_eq!(
            install_blocker(&s).as_deref(),
            Some("there is no driver package next to this installer: use \"Download and install\"")
        );
        // The warning about what is about to be installed is shown here as it is for a bundle.
        assert!(show_install_warning(&s));
        assert_eq!(online_label(None, None), "online");
        assert_eq!(
            online_label(Some(&v("0.2.0")), Some("8.50.53.934")),
            "release 0.2.0"
        );
        assert_eq!(
            online_label(None, Some("8.50.53.934")),
            "release, driver 8.50.53.934"
        );
    }

    #[test]
    fn guidance_for_the_lone_installer_follows_the_release_check() {
        let step = |s: &Status| {
            let g = next_step(s);
            (g.level, g.text)
        };
        let mut s = lone();
        assert_eq!(
            step(&s),
            (
                Level::Warn,
                "This installer runs on its own, without a driver package next to it: click \"Check for \
                 update\" to look for the latest release on GitHub"
                    .to_string()
            )
        );
        s.latest = Latest::Checking;
        assert_eq!(
            step(&s),
            (
                Level::Unknown,
                "Looking for the latest release on GitHub...".to_string()
            )
        );

        s.latest = found("0.2.0");
        assert_eq!(
            step(&s),
            (
                Level::Good,
                "Ready: click \"Download and install v0.2.0\"".to_string()
            )
        );
        s.admin = false;
        assert_eq!(
            step(&s),
            (
                Level::Warn,
                "Ready to install v0.2.0: click \"Restart as administrator\", then \"Download and install \
                 v0.2.0\""
                    .to_string()
            )
        );

        // What stands in the way of any install comes first, as in the other modes; the download would stop
        // at the same thing before it fetched a byte.
        s.test_signing = Some(false);
        let (level, text) = step(&s);
        assert_eq!(level, Level::Bad);
        assert!(text.starts_with("Test signing is off"), "{text}");
        assert_eq!(
            fetch_blocker(&s).as_deref(),
            Some("test signing is off (bcdedit /set testsigning on as admin, then reboot)")
        );
        s.test_signing = None;
        assert_eq!(
            fetch_blocker(&s).as_deref(),
            Some("the test signing state cannot be read")
        );
        s.test_signing = Some(true);
        assert_eq!(fetch_blocker(&s), None);

        // Nothing to download, and a check that failed: said as such, with the reason.
        s.latest = Latest::NoReleases {
            repo: "mrquantumoff/crossc".into(),
            auth: "anonymous".into(),
            skipped: Vec::new(),
        };
        let (level, text) = step(&s);
        assert_eq!(level, Level::Bad);
        assert!(
            text.ends_with(
                "and mrquantumoff/crossc has no published release yet: there is nothing to install"
            ),
            "{text}"
        );
        s.latest = Latest::Failed("could not connect (anonymous)".into());
        let (level, text) = step(&s);
        assert_eq!(level, Level::Bad);
        assert!(
            text.ends_with("and the release check failed: could not connect (anonymous)"),
            "{text}"
        );
        let mut broken = release("0.2.0");
        broken.assets = Err("release v0.2.0 has no SHA256SUMS.txt asset".into());
        s.latest = Latest::Found {
            release: Box::new(broken),
            repo: "mrquantumoff/crossc".into(),
            auth: "anonymous".into(),
            skipped: Vec::new(),
        };
        let (level, text) = step(&s);
        assert_eq!(level, Level::Bad);
        assert_eq!(
            text,
            "v0.2.0 cannot be installed from here: release v0.2.0 has no SHA256SUMS.txt asset. Get the zip from \
             https://github.com/mrquantumoff/crossc/releases and unpack it"
        );
    }

    #[test]
    fn online_mode_with_an_installed_release_reads_like_a_bundle() {
        let mut s = online_installed("0.2.0");
        let c = checklist(&s);
        assert_eq!(
            (
                row(&c, "Driver package").level,
                row(&c, "Driver package").detail.as_str()
            ),
            (Level::Good, "release 0.2.0 complete, 2 supported models")
        );
        assert_eq!(
            next_step(&s).text,
            "Installed and running: select \"Headphones (... Buds ...)\" as the output device in Windows sound \
             settings"
        );
        assert!(!show_install_warning(&s));

        // Its files damaged: the way to get them again is the download, not unpacking a zip.
        s.service_exe = None;
        s.service = service(ServiceState::NotInstalled);
        let text = next_step(&s).text;
        assert!(
            text.contains("crossc-service.exe is missing from the downloaded release or damaged"),
            "{text}"
        );
        assert!(
            text.contains("use \"Download and install\" to fetch the release again"),
            "{text}"
        );
        assert!(!text.contains("unpack"), "{text}");
        let service_row = row(&checklist(&s), "Service").clone();
        assert_eq!(service_row.level, Level::Bad);
        assert!(
            service_row
                .detail
                .contains("missing from the downloaded release or damaged"),
            "{service_row:?}"
        );
        let mut s = online_installed("0.2.0");
        s.packages[0].missing = vec!["crossc_bth.cat"];
        let package = row(&checklist(&s), "Driver package").clone();
        assert_eq!(package.level, Level::Bad);
        assert_eq!(
            package.detail,
            r#"downloaded release incomplete, missing crossc_bth.cat in C:\Program Files\crossc\releases\v0.2.0\driver: use "Download and install" to fetch the release again"#
        );
        assert!(
            install_blocker(&s)
                .unwrap()
                .starts_with("the downloaded release is incomplete (missing crossc_bth.cat")
        );
    }

    #[test]
    fn the_next_step_mentions_a_newer_release_only_when_nothing_is_in_the_way() {
        // Installed and running, a newer release out: said after the usual line.
        let mut s = with_version(ready_bundle(), "0.1.0");
        s.latest = found("0.2.0");
        let g = next_step(&s);
        assert_eq!(g.level, Level::Good);
        assert!(
            g.text.starts_with("Installed and running: select"),
            "{}",
            g.text
        );
        assert!(
            g.text
                .ends_with(". A newer release is available: \"Download and install v0.2.0\""),
            "{}",
            g.text
        );
        // Up to date: the usual line alone.
        s.latest = found("0.1.0");
        assert!(!next_step(&s).text.contains("newer release"));
        // A bundle that is older than the release, ready to install: both ways are named.
        let mut s = ready_bundle();
        s.bundle_version = Some(v("0.1.0"));
        s.latest = found("0.2.0");
        let text = next_step(&s).text;
        assert!(text.starts_with("Ready: click Install"), "{text}");
        assert!(
            text.ends_with("A newer release is available: \"Download and install v0.2.0\""),
            "{text}"
        );
        // Something is wrong: that is the message, without an advertisement behind it.
        s.test_signing = Some(false);
        let g = next_step(&s);
        assert_eq!(g.level, Level::Bad);
        assert!(!g.text.contains("newer release"), "{}", g.text);
        // The lone installer's line names the download itself; nothing is appended.
        let mut s = lone();
        s.latest = found("0.2.0");
        assert_eq!(
            next_step(&s).text,
            "Ready: click \"Download and install v0.2.0\""
        );
        // The lone installer on a machine where that release is installed already (from a bundle that is gone):
        // nothing to do, but the release can be fetched again.
        let mut s = lone();
        s.staged = with_version(ready_bundle(), "0.2.0").staged;
        s.record = with_version(ready_bundle(), "0.2.0").record;
        s.latest = found("0.2.0");
        assert_eq!(latest_row(&s).detail, "up to date: v0.2.0 is installed");
        let g = next_step(&s);
        assert_eq!(g.level, Level::Good);
        assert_eq!(
            g.text,
            "crossc v0.2.0 is installed and no newer release is out. This installer has no copy of it: Uninstall \
             works as it is, and \"Download and install v0.2.0\" fetches the release again to reinstall it"
        );
        assert_eq!(download_offer(&s).map(|r| r.tag.as_str()), Some("v0.2.0"));
        // An older release installed: the update is the next step.
        s.latest = found("0.3.0");
        assert_eq!(
            next_step(&s).text,
            "Ready: click \"Download and install v0.3.0\""
        );
    }

    // ---- the certificate record ---------------------------------------------------------------------------------

    const TP_A: &str = "00E56600170648F8D7B71EACA4392DBFCFC615FD";
    const TP_B: &str = "BF82EE9838ED6EE917234D4EF9319BC4EF8C39FF";

    const TP_C: &str = "0123456789ABCDEF0123456789ABCDEF01234567";

    #[test]
    fn the_recorded_certificate_is_replaced_only_by_a_different_one() {
        // First install on this machine: nothing to remove, the certificate is recorded.
        assert_eq!(
            cert_record_plan(None, None, TP_A),
            CertRecordPlan {
                remove: vec![],
                write: true
            }
        );
        // The same certificate again (a reinstall, or a repo build that reuses its certificate): nothing.
        for same in [
            TP_A.to_string(),
            TP_A.to_lowercase(),
            format!("  {TP_A}\r\n"),
        ] {
            assert_eq!(
                cert_record_plan(Some(&same), None, TP_A),
                CertRecordPlan {
                    remove: vec![],
                    write: false
                }
            );
            // ... also when it is on the pending list itself (it was put there before it was trusted).
            assert_eq!(
                cert_record_plan(Some(&same), Some(&same), &TP_A.to_lowercase()),
                CertRecordPlan {
                    remove: vec![],
                    write: false
                }
            );
        }
        // An update: every release has its own certificate. The recorded one goes, the new one is recorded.
        assert_eq!(
            cert_record_plan(Some(TP_A), Some(TP_B), TP_B),
            CertRecordPlan {
                remove: vec![TP_A.into()],
                write: true
            }
        );
        assert_eq!(
            cert_record_plan(Some(&TP_A.to_lowercase()), None, TP_B).remove,
            vec![TP_A.to_string()]
        );
        // A record that is not a thumbprint is never turned into a removal.
        for junk in [
            "",
            "CN=WDRLocalTestCert",
            "*",
            &TP_A[..39],
            &format!("{TP_A}0"),
            &"Z".repeat(40),
        ] {
            assert_eq!(
                cert_record_plan(Some(junk), Some(junk), TP_B),
                CertRecordPlan {
                    remove: vec![],
                    write: true
                },
                "{junk:?}"
            );
        }
    }

    #[test]
    fn a_certificate_is_on_record_before_it_is_trusted() {
        // Nothing recorded: the certificate goes on the pending list first.
        assert_eq!(
            pending_before_trust(None, None, TP_A).as_deref(),
            Some(TP_A)
        );
        assert_eq!(
            pending_before_trust(None, None, &TP_A.to_lowercase()).as_deref(),
            Some(TP_A)
        );
        // An update: the recorded certificate stays the recorded one, the new one is pending next to whatever
        // an earlier failed install left there.
        assert_eq!(
            pending_before_trust(Some(TP_A), None, TP_B).as_deref(),
            Some(TP_B)
        );
        assert_eq!(
            pending_before_trust(Some(TP_A), Some(TP_C), TP_B),
            Some(format!("{TP_C} {TP_B}"))
        );
        // Already covered: it is the recorded certificate (a reinstall), or it is pending (a second try).
        assert_eq!(pending_before_trust(Some(TP_A), None, TP_A), None);
        assert_eq!(
            pending_before_trust(Some(&TP_A.to_lowercase()), Some(TP_C), TP_A),
            None
        );
        assert_eq!(
            pending_before_trust(Some(TP_A), Some(&format!("{TP_C} {TP_B}")), TP_B),
            None
        );
        // Junk in the record does not count as covering it, and is not carried along.
        assert_eq!(
            pending_before_trust(Some("junk"), Some("junk, CN=WDRLocalTestCert"), TP_B).as_deref(),
            Some(TP_B)
        );
        // Something that is not a thumbprint is never written.
        assert_eq!(
            pending_before_trust(None, None, "CN=WDRLocalTestCert"),
            None
        );
    }

    #[test]
    fn pending_certificates_are_read_strictly() {
        assert_eq!(pending_certs(None), Vec::<String>::new());
        assert_eq!(pending_certs(Some("")), Vec::<String>::new());
        assert_eq!(
            pending_certs(Some(&format!(
                " {TP_A},{}\r\n{TP_B};{TP_A} * CN=WDRLocalTestCert {}",
                TP_B.to_lowercase(),
                &TP_C[..39]
            ))),
            vec![TP_A.to_string(), TP_B.to_string()]
        );
        assert_eq!(pending_value(&[]), None);
        assert_eq!(
            pending_value(&[TP_A.to_string(), TP_B.to_string()]),
            Some(format!("{TP_A} {TP_B}"))
        );
        // Round trip.
        let value = pending_value(&[TP_A.to_string(), TP_B.to_string()]);
        assert_eq!(
            pending_certs(value.as_deref()),
            vec![TP_A.to_string(), TP_B.to_string()]
        );
    }

    #[test]
    fn an_install_that_verified_cleans_up_what_earlier_ones_trusted() {
        // The update went through: recorded A, B pending since it was trusted. A goes, B is recorded.
        let update = cert_record_plan(Some(TP_A), Some(TP_B), TP_B);
        assert_eq!(update.remove, vec![TP_A.to_string()]);
        assert!(update.write);
        assert_eq!(update.pending_after(&[]), None);
        // A could not be removed: it stays on the list, so the next Install tries again.
        assert_eq!(
            update.pending_after(&[TP_A.to_string()]).as_deref(),
            Some(TP_A)
        );

        // The regression this guards: an install of B trusted its certificate and then failed (the package did
        // not bind, or listing the staged packages failed right after the certificate was trusted). B is
        // pending, A still recorded. Whatever is installed next knows about B:
        // ... the earlier release again (A): B goes, the record already names A.
        let back = cert_record_plan(Some(TP_A), Some(TP_B), TP_A);
        assert_eq!(
            back,
            CertRecordPlan {
                remove: vec![TP_B.into()],
                write: false
            }
        );
        assert_eq!(back.pending_after(&[]), None);
        // ... a third release (C, itself pending by now): A and B both go.
        let third = cert_record_plan(Some(TP_A), Some(&format!("{TP_B} {TP_C}")), TP_C);
        assert_eq!(third.remove, vec![TP_A.to_string(), TP_B.to_string()]);
        assert!(third.write);
        assert_eq!(
            third.pending_after(&[TP_B.to_string()]).as_deref(),
            Some(TP_B)
        );
        // ... and with no earlier record at all (the first install ever failed, the second is another build).
        let first = cert_record_plan(None, Some(TP_B), TP_C);
        assert_eq!(first.remove, vec![TP_B.to_string()]);
        // The certificate that is trusted now is never on the removal list, however it is recorded.
        for (recorded, pending) in [
            (Some(TP_B), Some(TP_B)),
            (None, Some(TP_B)),
            (Some(TP_A), Some(TP_B)),
            (Some(TP_B), None),
        ] {
            assert!(
                !cert_record_plan(recorded, pending, &TP_B.to_lowercase())
                    .remove
                    .contains(&TP_B.to_string())
            );
        }
        // A failure reported for something that was never to be removed does not get onto the list.
        assert_eq!(update.pending_after(&[TP_C.to_string()]), None);
    }

    // ---- staging the new package before the old ones go --------------------------------------------------------

    fn staged_pkg(published: &str, version: &str) -> StagedPackage {
        StagedPackage {
            published: published.into(),
            version: Some(format!("11/10/2026 {version}")),
        }
    }

    #[test]
    fn the_new_package_is_told_from_the_older_ones_by_the_driver_store() {
        let old = staged_pkg("oem5.inf", "1.0.0.1");
        let new = staged_pkg("oem9.inf", "2.0.0.2");
        // An update: one package appeared. It is kept whatever pnputil's exit code was.
        for accepted in [true, false] {
            assert_eq!(
                staged_keeper(
                    std::slice::from_ref(&old),
                    &[old.clone(), new.clone()],
                    Some("2.0.0.2"),
                    accepted
                ),
                Staged::Keep("oem9.inf".into())
            );
        }
        // The first install: nothing before.
        assert_eq!(
            staged_keeper(&[], std::slice::from_ref(&new), Some("2.0.0.2"), true),
            Staged::Keep("oem9.inf".into())
        );
        // A reinstall of the very same package: nothing appears, pnputil is content, and the staged package
        // with the INF's version is the one.
        assert_eq!(
            staged_keeper(
                std::slice::from_ref(&new),
                std::slice::from_ref(&new),
                Some("2.0.0.2"),
                true
            ),
            Staged::Keep("oem9.inf".into())
        );
        assert_eq!(
            staged_keeper(
                &[old.clone(), new.clone()],
                &[old.clone(), new.clone()],
                Some("2.0.0.2"),
                true
            ),
            Staged::Keep("oem9.inf".into())
        );
    }

    #[test]
    fn a_package_windows_refuses_is_not_mistaken_for_a_staged_one() {
        let old = staged_pkg("oem5.inf", "1.0.0.1");
        // The regression this guards: the new release's package is refused (its catalog is not signed by the
        // certificate in the bundle, say). Nothing appeared and pnputil failed: refused, so Install stops
        // before it has removed the old package or the old certificate.
        assert_eq!(
            staged_keeper(
                std::slice::from_ref(&old),
                std::slice::from_ref(&old),
                Some("2.0.0.2"),
                false
            ),
            Staged::Refused
        );
        assert_eq!(
            staged_keeper(&[], &[], Some("2.0.0.2"), false),
            Staged::Refused
        );
        // Even when an older staged package carries the same version number: pnputil failed, so that package is
        // not the new one.
        assert_eq!(
            staged_keeper(
                std::slice::from_ref(&old),
                std::slice::from_ref(&old),
                Some("1.0.0.1"),
                false
            ),
            Staged::Refused
        );
        // pnputil was content but the store does not say which package it is: unclear (Install then removes
        // all and adds again, as it always did), never "keep some package".
        let twin = staged_pkg("oem6.inf", "1.0.0.1");
        for (before, after, ver) in [
            // nothing with that version
            (vec![old.clone()], vec![old.clone()], Some("2.0.0.2")),
            // two with that version
            (
                vec![old.clone(), twin.clone()],
                vec![old.clone(), twin.clone()],
                Some("1.0.0.1"),
            ),
            // an INF without a version
            (vec![old.clone()], vec![old.clone()], None),
            // two appeared at once
            (vec![], vec![old.clone(), twin.clone()], Some("1.0.0.1")),
            // nothing staged at all although pnputil said yes
            (vec![], vec![], Some("1.0.0.1")),
        ] {
            assert_eq!(
                staged_keeper(&before, &after, ver, true),
                Staged::Unclear,
                "{before:?} -> {after:?}"
            );
        }
        // A package without a version line never matches an INF without a version.
        let bare = StagedPackage {
            published: "oem7.inf".into(),
            version: None,
        };
        assert_eq!(
            staged_keeper(
                std::slice::from_ref(&bare),
                std::slice::from_ref(&bare),
                None,
                true
            ),
            Staged::Unclear
        );
    }

    #[test]
    fn only_the_older_packages_are_removed() {
        let old = staged_pkg("oem5.inf", "1.0.0.1");
        let new = staged_pkg("oem9.inf", "2.0.0.2");
        let both = || vec![old.clone(), new.clone()];
        assert_eq!(
            packages_to_remove(both(), Some("oem9.inf")),
            vec![old.clone()]
        );
        assert_eq!(
            packages_to_remove(both(), Some("OEM9.INF")),
            vec![old.clone()]
        );
        // Uninstall, and the fallback: everything.
        assert_eq!(packages_to_remove(both(), None), both());
        // A name that is not staged keeps nothing back.
        assert_eq!(packages_to_remove(both(), Some("oem1.inf")), both());
        assert_eq!(packages_to_remove(vec![], Some("oem9.inf")), vec![]);
    }

    #[test]
    fn a_failed_update_points_back_to_the_earlier_release() {
        let failed = std::path::Path::new(r"C:\Program Files\crossc\releases\v0.3.0");
        let earlier = std::path::Path::new(r"C:\Program Files\crossc\releases\v0.2.0");
        let note = earlier_release_note(failed, Some(earlier)).unwrap();
        assert!(note.contains(r"releases\v0.2.0"), "{note}");
        assert!(note.contains("still trusted"), "{note}");
        // No earlier release, or the one that failed is the recorded one (a reinstall): nothing to point at.
        assert_eq!(earlier_release_note(failed, None), None);
        assert_eq!(earlier_release_note(failed, Some(failed)), None);
    }

    // ---- the confirmation ---------------------------------------------------------------------------------------

    fn fetched() -> FetchedRelease {
        FetchedRelease {
            repo: "mrquantumoff/crossc".into(),
            tag: "v0.2.0".into(),
            version: v("0.2.0"),
            asset: "crossc-0.2.0-x64.zip".into(),
            sha256: "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08".into(),
            dir: PathBuf::from(r"C:\Program Files\crossc\releases\v0.2.0"),
            cert_subject: "CN=WDRLocalTestCert".into(),
            cert_thumbprint: TP_A.into(),
        }
    }

    #[test]
    fn the_confirmation_names_everything_that_is_about_to_be_trusted() {
        let f = fetched();
        assert_eq!(
            confirm_title(&f),
            "Confirm: trust this certificate and install crossc v0.2.0"
        );
        assert_eq!(confirm_label(&f), "Trust certificate and install v0.2.0");
        assert!(
            CONFIRM_STATEMENT
                .starts_with("This installs a test-signed kernel driver downloaded from GitHub.")
        );
        assert!(CONFIRM_STATEMENT.contains("LocalMachine Root and TrustedPublisher"));
        assert!(CONFIRM_STATEMENT.contains("Nothing has been installed yet"));
        assert_eq!(
            confirm_lines(&f),
            vec![
                ("Release", "v0.2.0 of github.com/mrquantumoff/crossc".to_string()),
                ("Asset", "crossc-0.2.0-x64.zip".to_string()),
                (
                    "SHA-256",
                    "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08 (matches SHA256SUMS.txt)"
                        .to_string()
                ),
                ("Certificate", "CN=WDRLocalTestCert (self-signed test certificate)".to_string()),
                ("Thumbprint", format!("{TP_A} (SHA-1)")),
                ("Unpacked to", r"C:\Program Files\crossc\releases\v0.2.0".to_string()),
            ]
        );
        let g = confirm_step(&f);
        assert_eq!(g.level, Level::Warn);
        assert_eq!(
            g.text,
            "v0.2.0 is downloaded and verified. Check the certificate above, then click \"Trust certificate and \
             install v0.2.0\" (or Cancel)"
        );
    }

    #[test]
    fn the_example_release_cannot_be_installed_or_deleted() {
        let f = example_fetched();
        // Shaped like a real one, so the panel looks as it will.
        assert_eq!(Version::from_tag(&f.tag), Some(f.version.clone()));
        assert_eq!(f.sha256.len(), 64);
        assert_eq!(f.cert_thumbprint.len(), 40);
        assert_eq!(confirm_lines(&f).len(), 6);
        // Its folder is not below any Program Files folder: there is nothing to trust, install or discard.
        assert!(!f.dir.exists());
        assert!(
            !f.dir
                .to_string_lossy()
                .to_lowercase()
                .contains("program files")
        );
    }

    #[test]
    fn a_newer_installer_in_the_release_is_pointed_out() {
        let exe =
            std::path::Path::new(r"C:\Program Files\crossc\releases\v0.2.0\crossc-installer.exe");
        let note = newer_installer_note("0.1.0", &v("0.2.0"), exe).unwrap();
        assert_eq!(
            note,
            r"this installer is version 0.1.0; release v0.2.0 has a newer one. It does not replace itself: use C:\Program Files\crossc\releases\v0.2.0\crossc-installer.exe from now on"
        );
        // The same version, an older release, a release candidate of the running version.
        assert_eq!(newer_installer_note("0.2.0", &v("0.2.0"), exe), None);
        assert_eq!(newer_installer_note("0.3.0", &v("0.2.0"), exe), None);
        assert_eq!(newer_installer_note("0.2.0", &v("0.2.0-rc.1"), exe), None);
        assert!(newer_installer_note("0.2.0-rc.1", &v("0.2.0"), exe).is_some());
        // An installer whose own version is not a version says nothing rather than something wrong.
        assert_eq!(newer_installer_note("dev", &v("0.2.0"), exe), None);
    }
}
