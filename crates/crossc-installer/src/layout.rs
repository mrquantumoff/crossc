//! Where the driver package and the companion executables are. Three layouts:
//!
//! * **bundle**: an unpacked release zip, `crossc-installer.exe`, `crossc-service.exe`, `crossc-cli.exe` and
//!   `driver\{crossc_bth.sys,.inf,.cat,WDRLocalTestCert.cer}` side by side. No cargo and no repo.
//! * **repo**: a checkout, with the package under `drivers\crossc-bth\target\<flavor>\crossc_bth_package` and the
//!   executables under `target\release`.
//! * **online**: the installer on its own, with neither next to it. Its bundle is the release it downloaded and
//!   installed earlier (`%ProgramFiles%\crossc\releases\<tag>`), once there is one.
//!
//! Never derived from the working directory: a "runas" relaunch starts elsewhere (often System32), so everything
//! comes from the exe's location or, failing that, the compile-time path.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use crate::model::{Flavor, PackageKind};
use crate::parse::INF_NAME;
use crate::release::{Version, bundle_version};

/// Folder next to the exe that holds the driver package in a release bundle.
pub const BUNDLE_DRIVER_DIR: &str = "driver";
pub const SERVICE_EXE: &str = "crossc-service.exe";
pub const CLI_EXE: &str = "crossc-cli.exe";
pub const INSTALLER_EXE: &str = "crossc-installer.exe";
/// Folder below the install folder that downloaded releases are unpacked to, one folder per tag.
pub const RELEASES_DIR: &str = "releases";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Repo,
    Bundle,
    /// No bundle next to the exe and no repo: releases are downloaded.
    Online,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub mode: Mode,
    /// Repo root, or the folder the bundle was unpacked to. Online: the folder the installer is in, until
    /// `with_release` puts the folder of the installed release in its place.
    pub root: PathBuf,
}

impl Layout {
    pub fn locate() -> Layout {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf));
        let compiled = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
        let compiled = std::fs::canonicalize(&compiled)
            .map(strip_verbatim)
            .unwrap_or(compiled);
        detect(exe_dir.as_deref(), &compiled)
    }

    /// Online mode: this layout with the folder of the release that is installed (`kept_release_dir`) as its
    /// bundle. Any other mode, or no such release: unchanged. The status and the actions call this each time,
    /// because a download changes the answer while the window is open.
    pub fn with_release(&self, release_dir: Option<PathBuf>) -> Layout {
        match (self.mode, release_dir) {
            (Mode::Online, Some(root)) => Layout {
                mode: Mode::Online,
                root,
            },
            _ => self.clone(),
        }
    }

    /// The packages this layout can offer, the one Install uses first: the bundle's single package, or the selected
    /// flavour followed by the other one.
    pub fn packages(&self, flavor: Flavor) -> Vec<(PackageKind, PathBuf)> {
        match self.mode {
            Mode::Bundle | Mode::Online => {
                vec![(PackageKind::Bundle, self.root.join(BUNDLE_DRIVER_DIR))]
            }
            Mode::Repo => [flavor, flavor.other()]
                .into_iter()
                .map(|f| (PackageKind::Built(f), self.built_package_dir(f)))
                .collect(),
        }
    }

    /// The package directory Install uses.
    pub fn package_dir(&self, flavor: Flavor) -> PathBuf {
        match self.mode {
            Mode::Bundle | Mode::Online => self.root.join(BUNDLE_DRIVER_DIR),
            Mode::Repo => self.built_package_dir(flavor),
        }
    }

    /// Repo only: the driver crate (`cargo make` runs here).
    pub fn driver_dir(&self) -> PathBuf {
        self.root.join("drivers").join("crossc-bth")
    }

    fn built_package_dir(&self, flavor: Flavor) -> PathBuf {
        self.driver_dir()
            .join("target")
            .join(flavor.dir_name())
            .join("crossc_bth_package")
    }

    /// Where the companion executables are: next to the installer in a bundle, `target\release` in the repo.
    fn exe_dir(&self) -> PathBuf {
        match self.mode {
            Mode::Bundle | Mode::Online => self.root.clone(),
            Mode::Repo => self.root.join("target").join("release"),
        }
    }

    pub fn service_exe(&self) -> PathBuf {
        self.exe_dir().join(SERVICE_EXE)
    }

    pub fn cli_exe(&self) -> PathBuf {
        self.exe_dir().join(CLI_EXE)
    }

    /// Repo only: the workspace has the service crate, so Build can build it.
    pub fn has_service_crate(&self) -> bool {
        self.mode == Mode::Repo
            && self
                .root
                .join("crates")
                .join("crossc-service")
                .join("Cargo.toml")
                .is_file()
    }
}

/// Decides the layout from the folder the exe is in (reads the file system, nothing else):
///
/// 1. `<exe dir>\driver\crossc_bth.inf` exists: bundle. Checked first, so a bundle unpacked inside a checkout is
///    still a bundle.
/// 2. A folder above the exe is a repo root: that repo.
/// 3. The repo this binary was compiled in still exists: that repo.
/// 4. Neither: online. The installer was copied or downloaded on its own; what it installs it downloads.
pub fn detect(exe_dir: Option<&Path>, compiled_repo: &Path) -> Layout {
    let layout = |mode, root: &Path| Layout {
        mode,
        root: root.to_path_buf(),
    };
    if let Some(dir) = exe_dir {
        if is_bundle_dir(dir) {
            return layout(Mode::Bundle, dir);
        }
        if let Some(root) = dir.ancestors().find(|d| is_repo_root(d)) {
            return layout(Mode::Repo, root);
        }
    }
    match exe_dir {
        Some(dir) if !is_repo_root(compiled_repo) => layout(Mode::Online, dir),
        _ => layout(Mode::Repo, compiled_repo),
    }
}

fn is_bundle_dir(d: &Path) -> bool {
    d.join(BUNDLE_DRIVER_DIR).join(INF_NAME).is_file()
}

/// Where the service and the CLI are installed: `<Program Files>\crossc`.
///
/// `program_files` is the Program Files folder **as the system reports it** (`system::program_files`, the known
/// folder), never the `ProgramFiles` environment variable: a release is downloaded, verified, unpacked and
/// installed from below this folder, and that is only safe where nothing without administrator rights can
/// write. An environment variable is whatever the process that started the installer put there.
///
/// Uninstall deletes this folder, so even that value is only used when it is an absolute path with a drive and
/// without `.` or `..` (`C:\Program Files`); anything else (not reported, empty, relative, a bare root) falls
/// back to `C:\Program Files`, so the result always names a `crossc` folder two levels down from a drive root at
/// least.
pub fn install_dir(program_files: Option<&OsStr>) -> PathBuf {
    const FALLBACK: &str = r"C:\Program Files";
    let base = program_files
        .map(Path::new)
        .filter(|p| {
            let mut parts = p.components();
            matches!(parts.next(), Some(Component::Prefix(_)))
                && matches!(parts.next(), Some(Component::RootDir))
                && matches!(parts.next(), Some(Component::Normal(_)))
                && parts.all(|part| matches!(part, Component::Normal(_)))
                // `components()` drops a `.` in the middle of a path without a word.
                && !p.to_string_lossy().split(['\\', '/']).any(|part| part == ".")
        })
        .unwrap_or(Path::new(FALLBACK));
    base.join("crossc")
}

/// Where downloaded releases are unpacked: `<Program Files>\crossc\releases` (`program_files` as for
/// [`install_dir`]). Like everything below Program Files it can only be written with administrator rights, so
/// what is unpacked there cannot be swapped by a program without them between the checksum check and the
/// install.
pub fn releases_dir(program_files: Option<&OsStr>) -> PathBuf {
    install_dir(program_files).join(RELEASES_DIR)
}

/// The folder of the release that Install recorded as installed (`recorded_version`: the `Version` value of the
/// install record), if it is still there: `<releases>\v<version>` with a driver INF in it. A release that was
/// downloaded but never confirmed is not recorded, so it is never returned here. (`system::resolve_layout` also
/// wants its certificate to be trusted still.)
pub fn kept_release_dir(releases: &Path, recorded_version: Option<&str>) -> Option<PathBuf> {
    let dir = releases.join(Version::parse(recorded_version?)?.tag());
    is_bundle_dir(&dir).then_some(dir)
}

/// The version of the bundle in `root` (`release::bundle_version`): `version.txt`, else the first line of
/// `BUILD-INFO.txt`, else the folder's name.
pub fn bundle_version_of(root: &Path) -> Option<Version> {
    let read = |name: &str| {
        let bytes = std::fs::read(root.join(name)).ok()?;
        Some(String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]).into_owned())
    };
    bundle_version(
        read("version.txt").as_deref(),
        read("BUILD-INFO.txt").as_deref(),
        root.file_name().and_then(|name| name.to_str()),
    )
}

fn is_repo_root(d: &Path) -> bool {
    d.join("drivers")
        .join("crossc-bth")
        .join("Makefile.toml")
        .is_file()
        && d.join("Cargo.toml").is_file()
}

/// `\\?\C:\x` -> `C:\x` (canonicalize adds the verbatim prefix, which cargo-make and pnputil dislike).
fn strip_verbatim(p: PathBuf) -> PathBuf {
    match p.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => p,
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A fresh empty folder under the system temp directory, removed on drop.
    pub struct TempDir(pub PathBuf);

    impl TempDir {
        pub fn new(tag: &str) -> TempDir {
            let dir = std::env::temp_dir().join(format!(
                "crossc-installer-test-{tag}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        pub fn file(&self, rel: &str, content: &str) -> PathBuf {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fake_repo(t: &TempDir) {
        t.file("Cargo.toml", "[workspace]");
        t.file(r"drivers\crossc-bth\Makefile.toml", "");
    }

    #[test]
    fn locate_finds_this_repo() {
        let layout = Layout::locate();
        assert_eq!(layout.mode, Mode::Repo);
        assert!(is_repo_root(&layout.root), "{}", layout.root.display());
        assert!(!layout.root.to_string_lossy().starts_with(r"\\?\"));
        assert!(
            layout
                .package_dir(Flavor::Release)
                .ends_with(r"drivers\crossc-bth\target\release\crossc_bth_package")
        );
        assert!(
            layout
                .service_exe()
                .ends_with(r"target\release\crossc-service.exe")
        );
        assert!(layout.cli_exe().ends_with(r"target\release\crossc-cli.exe"));
    }

    #[test]
    fn bundle_is_detected_by_the_inf_next_to_the_exe() {
        let t = TempDir::new("bundle");
        t.file(r"driver\crossc_bth.inf", "");
        let nowhere = t.0.join("no-such-repo");
        let layout = detect(Some(&t.0), &nowhere);
        assert_eq!(layout.mode, Mode::Bundle);
        assert_eq!(layout.root, t.0);
        // One package, whatever flavour is asked for; executables next to the installer.
        assert_eq!(
            layout.packages(Flavor::Release),
            vec![(PackageKind::Bundle, t.0.join("driver"))]
        );
        assert_eq!(layout.package_dir(Flavor::Debug), t.0.join("driver"));
        assert_eq!(layout.service_exe(), t.0.join("crossc-service.exe"));
        assert_eq!(layout.cli_exe(), t.0.join("crossc-cli.exe"));
        assert!(!layout.has_service_crate());
    }

    #[test]
    fn bundle_wins_inside_a_checkout_and_over_the_compiled_repo() {
        let t = TempDir::new("bundle-in-repo");
        fake_repo(&t);
        t.file(r"unzipped\driver\crossc_bth.inf", "");
        let exe_dir = t.0.join("unzipped");
        let layout = detect(Some(&exe_dir), &t.0);
        assert_eq!((layout.mode, &layout.root), (Mode::Bundle, &exe_dir));
    }

    #[test]
    fn repo_is_found_above_the_exe_or_at_the_compiled_path() {
        let t = TempDir::new("repo");
        fake_repo(&t);
        let exe = t.file(r"target\release\x.exe", "");
        let exe_dir = exe.parent().unwrap();
        let nowhere = t.0.join("no-such-repo");
        let layout = detect(Some(exe_dir), &nowhere);
        assert_eq!((layout.mode, &layout.root), (Mode::Repo, &t.0));
        assert_eq!(
            layout.packages(Flavor::Debug),
            vec![
                (
                    PackageKind::Built(Flavor::Debug),
                    t.0.join(r"drivers\crossc-bth\target\debug\crossc_bth_package")
                ),
                (
                    PackageKind::Built(Flavor::Release),
                    t.0.join(r"drivers\crossc-bth\target\release\crossc_bth_package")
                ),
            ]
        );
        assert!(!layout.has_service_crate());
        t.file(r"crates\crossc-service\Cargo.toml", "");
        assert!(layout.has_service_crate());

        // The exe sits outside any repo (or its path is unknown): the repo it was compiled in.
        let elsewhere = TempDir::new("elsewhere");
        for exe_dir in [Some(elsewhere.0.as_path()), None] {
            let layout = detect(exe_dir, &t.0);
            assert_eq!((layout.mode, &layout.root), (Mode::Repo, &t.0));
        }
    }

    #[test]
    fn no_bundle_and_no_repo_is_online_mode() {
        let t = TempDir::new("stray");
        let nowhere = t.0.join("no-such-repo");
        let layout = detect(Some(&t.0), &nowhere);
        assert_eq!((layout.mode, &layout.root), (Mode::Online, &t.0));
        // Until a release is installed its package is the (missing) driver folder next to the exe.
        assert_eq!(
            layout.packages(Flavor::Debug),
            vec![(PackageKind::Bundle, t.0.join("driver"))]
        );
        assert!(!layout.has_service_crate());
        // A driver folder without the INF is not detected as a bundle, and it is not a repo either.
        t.file(r"driver\crossc_bth.sys", "");
        assert_eq!(detect(Some(&t.0), &nowhere).mode, Mode::Online);
    }

    #[test]
    fn online_mode_uses_the_installed_release_as_its_bundle() {
        let t = TempDir::new("online");
        let online = Layout {
            mode: Mode::Online,
            root: t.0.join("downloads"),
        };
        let releases = t.0.join("releases");
        // Nothing recorded, a record without that folder, a folder without a driver INF: no release.
        assert_eq!(kept_release_dir(&releases, None), None);
        assert_eq!(kept_release_dir(&releases, Some("0.2.0")), None);
        t.file(r"releases\v0.2.0\crossc-service.exe", "");
        assert_eq!(kept_release_dir(&releases, Some("0.2.0")), None);
        assert_eq!(online.with_release(None), online);

        t.file(r"releases\v0.2.0\driver\crossc_bth.inf", "");
        let kept = kept_release_dir(&releases, Some("0.2.0")).unwrap();
        assert_eq!(kept, releases.join("v0.2.0"));
        assert_eq!(
            kept_release_dir(&releases, Some("v0.2.0")),
            Some(kept.clone())
        );
        let with = online.with_release(Some(kept.clone()));
        assert_eq!((with.mode, &with.root), (Mode::Online, &kept));
        assert_eq!(with.package_dir(Flavor::Release), kept.join("driver"));
        assert_eq!(with.service_exe(), kept.join("crossc-service.exe"));
        assert_eq!(with.cli_exe(), kept.join("crossc-cli.exe"));

        // A release that was downloaded but is not the recorded one (never confirmed, or older) is not used,
        // and neither is anything a record that is not a version might point at.
        t.file(r"releases\v0.3.0\driver\crossc_bth.inf", "");
        assert_eq!(
            kept_release_dir(&releases, Some("0.2.0")),
            Some(kept.clone())
        );
        for odd in ["", "latest", r"..\..\Windows", "0.3", "0.3.0/../v0.2.0"] {
            assert_eq!(kept_release_dir(&releases, Some(odd)), None, "{odd:?}");
        }
        // Only online mode takes its bundle from there.
        for mode in [Mode::Bundle, Mode::Repo] {
            let layout = Layout {
                mode,
                root: t.0.clone(),
            };
            assert_eq!(layout.with_release(Some(kept.clone())), layout);
        }
    }

    #[test]
    fn releases_live_below_the_install_folder() {
        assert_eq!(
            releases_dir(Some(OsStr::new(r"C:\Program Files"))),
            PathBuf::from(r"C:\Program Files\crossc\releases")
        );
        // An unusable %ProgramFiles% never moves it somewhere odd.
        for bad in [
            None,
            Some(OsStr::new("")),
            Some(OsStr::new(r"\Temp")),
            Some(OsStr::new("x")),
            Some(OsStr::new(r"C:\Program Files\..\Users\Public")),
        ] {
            assert_eq!(
                releases_dir(bad),
                PathBuf::from(r"C:\Program Files\crossc\releases")
            );
        }
    }

    #[test]
    fn bundle_version_is_read_from_the_bundle_folder() {
        let t = TempDir::new("bundle-version");
        // The folder name, as the zip unpacks.
        let named = t.0.join("crossc-0.1.0-x64");
        std::fs::create_dir_all(&named).unwrap();
        assert_eq!(bundle_version_of(&named), Version::parse("0.1.0"));
        // BUILD-INFO.txt (what tools/package.ps1 writes today) beats the folder name, version.txt beats both.
        t.file(
            r"crossc-0.1.0-x64\BUILD-INFO.txt",
            "crossc 0.2.0 (x64)\n\ncommit    : abc\n",
        );
        assert_eq!(bundle_version_of(&named), Version::parse("0.2.0"));
        t.file(r"crossc-0.1.0-x64\version.txt", "0.3.0\r\n");
        assert_eq!(bundle_version_of(&named), Version::parse("0.3.0"));
        // A renamed folder with no file, and a folder that does not exist.
        let plain = t.0.join("stuff");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(bundle_version_of(&plain), None);
        assert_eq!(bundle_version_of(&t.0.join("missing")), None);
    }

    #[test]
    fn verbatim_prefix() {
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\C:\a\b")),
            PathBuf::from(r"C:\a\b")
        );
        assert_eq!(
            strip_verbatim(PathBuf::from(r"C:\a")),
            PathBuf::from(r"C:\a")
        );
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\UNC\s\x")),
            PathBuf::from(r"\\?\UNC\s\x")
        );
    }
}
