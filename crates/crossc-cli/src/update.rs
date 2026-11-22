//! `crossc-cli update` (Linux only): updates the PipeWire SSC plugin and this
//! program from the latest GitHub release.
//!
//! The release's `crossc-linux-<version>-<arch>.tar.gz` is checked against its
//! `.sha256` asset (and GitHub's own digest when the API gives one), unpacked,
//! and installed by the `install.sh` inside it, the same script a first install
//! runs. HTTP and unpacking go through the system's `curl` and `tar`, so the
//! system's certificates and proxy settings apply.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sha2::{Digest, Sha256};

pub const UPDATE_USAGE: &str = "update [--check] [--force] [--version vX.Y.Z]   update the PipeWire SSC plugin and crossc-cli from GitHub releases";

const DEFAULT_REPO: &str = "mrquantumoff/crossc";
const REPO_ENV: &str = "CROSSC_REPO";
const MARKER: &[u8] = b"crossc-linux-plugin-version=";
const SPA_DIRS: [&str; 4] = [
    "/usr/lib/spa-0.2",
    "/usr/lib64/spa-0.2",
    "/usr/lib/x86_64-linux-gnu/spa-0.2",
    "/usr/lib/aarch64-linux-gnu/spa-0.2",
];
const SLOTS: [&str; 2] = ["ssc", "lc3plus"];

#[derive(Debug, Default, PartialEq, Eq)]
pub struct UpdateArgs {
    /// Only report whether an update exists.
    pub check: bool,
    /// Reinstall even when the installed version is current.
    pub force: bool,
    /// A specific release tag instead of the latest.
    pub tag: Option<String>,
}

pub fn parse_update_args(args: &[String]) -> Result<UpdateArgs, String> {
    let mut a = UpdateArgs::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--check" => a.check = true,
            "--force" => a.force = true,
            "--version" => {
                let tag = it.next().ok_or("--version needs a tag, e.g. v0.4.0")?;
                Version::parse_tag(tag).ok_or(format!("{tag:?} is not a release tag (vX.Y.Z)"))?;
                a.tag = Some(tag.clone());
            }
            o => return Err(format!("update: unknown option {o:?}")),
        }
    }
    Ok(a)
}

/// `MAJOR.MINOR.PATCH[-pre]`; a pre-release sorts before its release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    core: (u64, u64, u64),
    pre: Option<String>,
}

impl Version {
    pub fn parse(s: &str) -> Option<Version> {
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) if !p.is_empty() => (c, Some(p.to_string())),
            Some(_) => return None,
            None => (s, None),
        };
        let mut n = core.split('.').map(|p| p.parse::<u64>().ok());
        let v = (n.next()??, n.next()??, n.next()??);
        n.next().is_none().then_some(Version { core: v, pre })
    }

    pub fn parse_tag(tag: &str) -> Option<Version> {
        Version::parse(tag.strip_prefix('v')?)
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.core
            .cmp(&other.core)
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => std::cmp::Ordering::Equal,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (Some(_), None) => std::cmp::Ordering::Less,
                (Some(a), Some(b)) => a.cmp(b),
            })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (a, b, c) = self.core;
        write!(f, "{a}.{b}.{c}")?;
        if let Some(p) = &self.pre {
            write!(f, "-{p}")?;
        }
        Ok(())
    }
}

/// The version stamped into a plugin binary, if it is crossc's.
pub fn plugin_version(binary: &[u8]) -> Option<Version> {
    let at = binary.windows(MARKER.len()).position(|w| w == MARKER)? + MARKER.len();
    let rest = &binary[at..];
    let end = rest.iter().position(|&b| b == 0)?;
    Version::parse(std::str::from_utf8(&rest[..end]).ok()?)
}

#[derive(Debug, PartialEq, Eq)]
pub struct Installed {
    pub path: PathBuf,
    pub version: Version,
}

fn spa_dirs() -> Vec<PathBuf> {
    // Same override as install.sh.
    if let Some(dir) = std::env::var_os("SPA_DIR") {
        return vec![PathBuf::from(dir)];
    }
    let from_pkg_config = Command::new("pkg-config")
        .args(["--variable=plugindir", "libspa-0.2"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()));
    from_pkg_config
        .into_iter()
        .chain(SPA_DIRS.iter().map(PathBuf::from))
        .collect()
}

fn installed_plugin() -> Option<Installed> {
    spa_dirs().iter().find_map(|dir| {
        SLOTS.iter().find_map(|slot| {
            let path = dir.join(format!("bluez5/libspa-codec-bluez5-{slot}.so"));
            let version = plugin_version(&fs::read(&path).ok()?)?;
            Some(Installed { path, version })
        })
    })
}

#[derive(Debug, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    /// GitHub's `sha256:<hex>` digest, when the API reports one.
    pub sha256: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    pub version: Version,
    pub tarball: Asset,
    pub checksum: Asset,
}

pub fn tarball_name(version: &Version, arch: &str) -> String {
    format!("crossc-linux-{version}-{arch}.tar.gz")
}

/// Picks the Linux tarball and its checksum out of a GitHub release object.
pub fn parse_release(json: &str, arch: &str) -> Result<Release, String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("release JSON: {e}"))?;
    let tag = v["tag_name"]
        .as_str()
        .ok_or("release JSON has no tag_name")?
        .to_string();
    let version = Version::parse_tag(&tag).ok_or(format!("release tag {tag:?} is not vX.Y.Z"))?;
    let want = tarball_name(&version, arch);
    let assets = v["assets"].as_array().ok_or("release JSON has no assets")?;
    let find = |name: &str| -> Option<Asset> {
        assets.iter().find(|a| a["name"] == name).and_then(|a| {
            Some(Asset {
                name: name.to_string(),
                url: a["browser_download_url"].as_str()?.to_string(),
                sha256: a["digest"]
                    .as_str()
                    .and_then(|d| d.strip_prefix("sha256:"))
                    .map(str::to_lowercase),
            })
        })
    };
    let tarball = find(&want).ok_or(format!(
        "{tag} has no {want} (no Linux build in that release)"
    ))?;
    let checksum = find(&format!("{want}.sha256")).ok_or(format!("{tag} has no {want}.sha256"))?;
    Ok(Release {
        tag,
        version,
        tarball,
        checksum,
    })
}

/// The hash for `name` in a `sha256sum` file.
pub fn expected_sha256(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let (hash, file) = l.split_once(char::is_whitespace)?;
        let file = file.trim_start().trim_start_matches('*');
        (file == name && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| hash.to_lowercase())
    })
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn repo() -> Result<String, String> {
    let Ok(r) = std::env::var(REPO_ENV) else {
        return Ok(DEFAULT_REPO.into());
    };
    let ok_part = |p: &str| {
        !p.is_empty()
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    };
    match r.split_once('/') {
        Some((o, n)) if ok_part(o) && ok_part(n) => Ok(r),
        _ => Err(format!(
            "{REPO_ENV}={r:?} is not a GitHub repository in the form owner/name"
        )),
    }
}

fn arch() -> Result<&'static str, String> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("x86_64"),
        a => Err(format!(
            "no prebuilt Linux release for {a}; build from source: linux/install.sh --from-source"
        )),
    }
}

fn curl(url: &str, out: Option<&Path>) -> Result<Vec<u8>, String> {
    let mut cmd = Command::new("curl");
    cmd.args([
        "-fsSL",
        "--proto",
        "=https",
        "-H",
        "Accept: application/vnd.github+json",
        url,
    ]);
    if let Some(path) = out {
        cmd.arg("-o").arg(path);
    }
    let o = cmd
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("cannot run curl ({e}); install curl"))?;
    if !o.status.success() {
        return Err(format!("download failed: {url}"));
    }
    Ok(o.stdout)
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Result<TempDir, String> {
        let p = std::env::temp_dir().join(format!("crossc-update-{}", std::process::id()));
        fs::create_dir_all(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        Ok(TempDir(p))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn run(a: &UpdateArgs) -> Result<(), String> {
    let repo = repo()?;
    let arch = arch()?;
    let installed = installed_plugin();
    match &installed {
        Some(i) => println!("installed: crossc {} ({})", i.version, i.path.display()),
        None => println!("installed: no crossc plugin found"),
    }

    let api = match &a.tag {
        Some(tag) => format!("https://api.github.com/repos/{repo}/releases/tags/{tag}"),
        None => format!("https://api.github.com/repos/{repo}/releases/latest"),
    };
    let json = curl(&api, None)?;
    let release = parse_release(&String::from_utf8_lossy(&json), arch)?;
    println!(
        "release:   crossc {} ({repo} {})",
        release.version, release.tag
    );

    let current = installed
        .as_ref()
        .is_some_and(|i| i.version >= release.version);
    if a.check {
        println!(
            "{}",
            if current {
                "up to date"
            } else {
                "update available: crossc-cli update"
            }
        );
        return Ok(());
    }
    if current && !a.force && a.tag.is_none() {
        println!("up to date");
        return Ok(());
    }

    let tmp = TempDir::new()?;
    let tarball = tmp.0.join(&release.tarball.name);
    println!("downloading {}", release.tarball.name);
    curl(&release.tarball.url, Some(&tarball))?;
    let sums = curl(&release.checksum.url, None)?;
    let expected =
        expected_sha256(&String::from_utf8_lossy(&sums), &release.tarball.name).ok_or(format!(
            "{} does not list {}",
            release.checksum.name, release.tarball.name
        ))?;
    let actual =
        sha256_hex(&fs::read(&tarball).map_err(|e| format!("{}: {e}", tarball.display()))?);
    if actual != expected
        || release
            .tarball
            .sha256
            .as_ref()
            .is_some_and(|d| *d != actual)
    {
        return Err(format!(
            "checksum mismatch for {}: got {actual}, expected {expected}",
            release.tarball.name
        ));
    }

    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(&tmp.0)
        .status()
        .map_err(|e| format!("cannot run tar: {e}"))?;
    if !status.success() {
        return Err(format!("cannot unpack {}", release.tarball.name));
    }
    let dir = tmp.0.join(release.tarball.name.trim_end_matches(".tar.gz"));
    let status = Command::new("bash")
        .arg(dir.join("install.sh"))
        .arg("--local")
        .arg(&dir)
        .env(REPO_ENV, &repo)
        .status()
        .map_err(|e| format!("cannot run install.sh: {e}"))?;
    if !status.success() {
        return Err("install.sh failed".into());
    }
    println!("updated to crossc {}", release.version);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn versions_order_like_semver() {
        let v = |s| Version::parse(s).unwrap();
        assert!(v("0.4.0") > v("0.3.9"));
        assert!(v("0.4.0") > v("0.4.0-rc.1"));
        assert!(v("0.10.0") > v("0.9.0"));
        assert_eq!(Version::parse_tag("v1.2.3"), Some(v("1.2.3")));
        assert_eq!(Version::parse_tag("1.2.3"), None);
        assert_eq!(Version::parse("1.2"), None);
        assert_eq!(Version::parse("1.2.3-"), None);
        assert_eq!(v("1.2.3-rc.1").to_string(), "1.2.3-rc.1");
    }

    #[test]
    fn plugin_marker() {
        let mut bin = b"\x7fELF....crossc-linux-plugin-version=0.3.0\0more".to_vec();
        assert_eq!(plugin_version(&bin), Version::parse("0.3.0"));
        bin.truncate(20);
        assert_eq!(plugin_version(&bin), None);
        assert_eq!(plugin_version(b"PipeWire's own LC3plus codec"), None);
    }

    #[test]
    fn release_assets() {
        let json = r#"{"tag_name":"v0.4.0","assets":[
            {"name":"crossc-0.4.0-x64.zip","browser_download_url":"https://github.com/o/r/releases/download/v0.4.0/crossc-0.4.0-x64.zip"},
            {"name":"crossc-linux-0.4.0-x86_64.tar.gz","browser_download_url":"https://github.com/o/r/releases/download/v0.4.0/crossc-linux-0.4.0-x86_64.tar.gz","digest":"sha256:ABCD"},
            {"name":"crossc-linux-0.4.0-x86_64.tar.gz.sha256","browser_download_url":"https://github.com/o/r/releases/download/v0.4.0/crossc-linux-0.4.0-x86_64.tar.gz.sha256"}]}"#;
        let r = parse_release(json, "x86_64").unwrap();
        assert_eq!(r.version, Version::parse("0.4.0").unwrap());
        assert_eq!(r.tarball.name, "crossc-linux-0.4.0-x86_64.tar.gz");
        assert_eq!(r.tarball.sha256.as_deref(), Some("abcd"));
        assert!(r.checksum.url.ends_with(".tar.gz.sha256"));
        let windows_only = r#"{"tag_name":"v0.3.0","assets":[{"name":"crossc-0.3.0-x64.zip","browser_download_url":"x"}]}"#;
        assert!(
            parse_release(windows_only, "x86_64")
                .unwrap_err()
                .contains("no Linux build")
        );
    }

    #[test]
    fn checksum_file() {
        let h = "a".repeat(64);
        let sums = format!(
            "{h}  crossc-linux-0.4.0-x86_64.tar.gz\n{}  other\n",
            "b".repeat(64)
        );
        assert_eq!(
            expected_sha256(&sums, "crossc-linux-0.4.0-x86_64.tar.gz"),
            Some(h.clone())
        );
        assert_eq!(
            expected_sha256(&format!("{h} *x.tar.gz"), "x.tar.gz"),
            Some(h)
        );
        assert_eq!(expected_sha256("zz  x.tar.gz", "x.tar.gz"), None);
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn args() {
        assert_eq!(parse_update_args(&s(&[])).unwrap(), UpdateArgs::default());
        let a = parse_update_args(&s(&["--check", "--version", "v0.4.0"])).unwrap();
        assert!(a.check && a.tag.as_deref() == Some("v0.4.0"));
        assert!(parse_update_args(&s(&["--version", "0.4"])).is_err());
        assert!(parse_update_args(&s(&["--nope"])).is_err());
    }
}
