//! "Install the latest release from GitHub", the part that does I/O: finding a token, asking for the release
//! list, downloading `SHA256SUMS.txt` and the zip, checking the zip's SHA-256 and unpacking it. Every decision
//! (which release, which hosts, which headers, which paths) is made by the pure functions in `release`.
//!
//! The release check works unelevated and writes nothing. The download writes only below the folder it is given
//! (`%ProgramFiles%\crossc\releases`, which only administrators can write to), so `actions` calls it elevated.

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use windows::Win32::Security::Cryptography::{
    BCRYPT_HASH_HANDLE, BCRYPT_SHA256_ALG_HANDLE, BCryptCreateHash, BCryptDestroyHash,
    BCryptFinishHash, BCryptHashData,
};

use crate::actions::is_inside;
use crate::http::{Response, Transport, WinHttp};
use crate::model::Latest;
use crate::parse::LineKind;
use crate::release::{
    ACCEPT_BINARY, ACCEPT_JSON, LatestRelease, MAX_ENTRIES, MAX_JSON_BYTES, MAX_SUMS_BYTES,
    MAX_UNPACKED_BYTES, MAX_ZIP_BYTES, Next, Policy, REPO_ENV, SUMS_NAME, TOKEN_ENVS, Token,
    TokenSource, Url, Version, api_message, auth_text, check_sha256, discover_token, entry_path,
    expected_sha256, http_failure, latest_release, next_hop, progress_step, progress_text,
    releases_url, repo_slug, request_headers, size_text, skipped_tags,
};
use crate::system::CREATE_NO_WINDOW;

/// The release check: 10 s for each step of the request (name lookup, connect, send, each wait for data).
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);
/// A download: the same steps with more patience. It bounds a stall, not the download as a whole.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30);
/// How long `gh auth token` may take.
const GH_TIMEOUT: Duration = Duration::from_secs(5);
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
/// Others may read the file while it is open, nobody may write, rename or delete it.
const FILE_SHARE_READ: u32 = 1;

/// Where the releases are asked for.
pub struct Source {
    /// `https://api.github.com`.
    pub api_base: String,
    /// `owner/name`.
    pub repo: String,
    pub policy: Policy,
    /// Tests: talk to the mock server directly, without looking for a proxy.
    direct: bool,
}

impl Source {
    /// GitHub, for the repository `CROSSC_REPO` names or the default one.
    pub fn github(repo_env: Option<&str>) -> Result<Source, String> {
        Ok(Source {
            api_base: "https://api.github.com".into(),
            repo: repo_slug(repo_env)?,
            policy: Policy::github(),
            direct: false,
        })
    }

    fn transport(&self, timeout: Duration) -> Result<WinHttp, String> {
        WinHttp::new(timeout, self.direct)
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The token
// ---------------------------------------------------------------------------------------------------------------

/// How requests are authenticated.
#[derive(Debug)]
pub struct Auth {
    pub token: Option<Token>,
    pub source: Option<TokenSource>,
    /// Why `gh` was not asked although it is there (to be shown when the check then fails).
    pub note: Option<String>,
}

impl Auth {
    #[cfg(test)]
    pub fn anonymous() -> Auth {
        Auth {
            token: None,
            source: None,
            note: None,
        }
    }
}

/// Finds a token: `GITHUB_TOKEN`, `GH_TOKEN`, then `gh auth token` if `gh.exe` is on PATH. The token itself is
/// never logged; only where it came from is.
pub fn find_token(elevated: bool) -> Auth {
    let note = std::cell::RefCell::new(None);
    let env = |name: &str| std::env::var(name).ok();
    let gh = || {
        let found = gh_on_path(std::env::var_os("PATH").as_deref())?;
        let Some(gh) = gh_to_run(&found, elevated, &crate::system::protected_dirs()) else {
            *note.borrow_mut() = Some(format!(
                "{} was not asked for a token: this window is elevated and that file is not in Program Files \
                 (it could have been put there without administrator rights). Set {} if a token is needed",
                found.display(),
                TOKEN_ENVS[0]
            ));
            return None;
        };
        run_for_output(&gh, &["auth", "token"], GH_TIMEOUT)
    };
    let found = discover_token(&env, &gh);
    let (token, source) = found.map_or((None, None), |(t, s)| (Some(t), Some(s)));
    Auth {
        token,
        source,
        note: note.into_inner(),
    }
}

/// The first `gh.exe` in the folders of `PATH` (absolute folders only: never the current directory).
pub fn gh_on_path(path_var: Option<&OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path_var?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("gh.exe"))
        .find(|exe| exe.is_file())
}

/// The `gh.exe` to run for the one found on PATH, or `None` when it may not be run.
///
/// Unelevated: the file as found (it runs with the user's own rights anyway). Elevated: only a file that really
/// is below one of the `protected` folders (Program Files, which takes administrator rights to change), so that
/// a `gh.exe` planted in a user-writable folder on PATH is never run as administrator. "Really": the path is
/// resolved by the file system first (`..`, junctions, symbolic links), and so are the folders, so neither
/// `C:\Program Files\..\Users\x\gh.exe` nor a link below Program Files that leads elsewhere passes. The resolved
/// path is what is returned and run. A path that cannot be resolved is refused.
pub fn gh_to_run(gh: &Path, elevated: bool, protected: &[PathBuf]) -> Option<PathBuf> {
    if !elevated {
        return Some(gh.to_path_buf());
    }
    let real = std::fs::canonicalize(gh).ok()?;
    protected
        .iter()
        .filter_map(|dir| std::fs::canonicalize(dir).ok())
        .any(|dir| is_inside(&real, &dir))
        .then_some(real)
}

/// Runs a program without a window and returns its standard output; `None` when it cannot be started, does not
/// exit with 0, or takes longer than `timeout` (it is killed then).
fn run_for_output(program: &Path, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    // A token is a few dozen bytes: it fits in the pipe, so reading after the exit cannot block.
    let mut out = String::new();
    child
        .stdout
        .take()?
        .take(64 * 1024)
        .read_to_string(&mut out)
        .ok()?;
    Some(out)
}

// ---------------------------------------------------------------------------------------------------------------
// One download
// ---------------------------------------------------------------------------------------------------------------

/// Why a download did not deliver.
#[derive(Debug, PartialEq, Eq)]
pub enum FetchError {
    /// The server answered, but not with 200 or an acceptable redirect.
    Http {
        status: u32,
        remaining: Option<String>,
        message: Option<String>,
    },
    Other(String),
}

/// What to download.
pub struct Get<'a> {
    pub url: &'a str,
    /// The `Accept` header.
    pub accept: &'a str,
    pub token: Option<&'a Token>,
    /// The most bytes to take.
    pub cap: u64,
}

/// GETs `get.url` into `sink`, following redirects by the rules of `release::next_hop` (allow-list and HTTPS on
/// every hop, a limited number of hops) with the headers of `release::request_headers` (the token only to the
/// API). Stops with an error as soon as more than `get.cap` bytes are announced or have arrived.
/// `progress(bytes so far, total if announced)` is called now and then. Returns the number of bytes delivered.
pub fn fetch(
    transport: &dyn Transport,
    policy: &Policy,
    get: &Get,
    sink: &mut dyn Write,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<u64, FetchError> {
    let cap = get.cap;
    let mut url =
        Url::parse(get.url).map_err(|e| FetchError::Other(format!("the address is {e}")))?;
    policy.check(&url).map_err(FetchError::Other)?;
    let mut followed = 0;
    loop {
        let headers = request_headers(policy, &url, get.accept, get.token);
        let response = transport.get(&url, &headers).map_err(FetchError::Other)?;
        match next_hop(
            policy,
            &url,
            response.status,
            response.location.as_deref(),
            followed,
        ) {
            Ok(Next::Body) => {
                return read_body(response, &url, cap, sink, progress).map_err(FetchError::Other);
            }
            Ok(Next::Redirect(next)) => {
                followed += 1;
                url = next;
            }
            Err(Some(why)) => return Err(FetchError::Other(why)),
            Err(None) => {
                let mut body = Vec::new();
                let _ = response.body.take(64 * 1024).read_to_end(&mut body);
                return Err(FetchError::Http {
                    status: response.status,
                    remaining: response.rate_remaining,
                    message: api_message(&String::from_utf8_lossy(&body)),
                });
            }
        }
    }
}

fn read_body(
    mut response: Response,
    url: &Url,
    cap: u64,
    sink: &mut dyn Write,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<u64, String> {
    let too_large = |size: u64| {
        format!(
            "{} sends more than the {} allowed here ({}): refused",
            url.host,
            size_text(cap),
            size_text(size)
        )
    };
    let announced = response.content_length;
    if let Some(size) = announced.filter(|size| *size > cap) {
        return Err(too_large(size));
    }
    let step = progress_step(announced);
    let mut next_mark = step;
    let mut last_mark = 0u64;
    let mut total = 0u64;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = response.body.read(&mut buf).map_err(|e| {
            format!(
                "the download from {} broke off after {}: {e}",
                url.host,
                size_text(total)
            )
        })?;
        if n == 0 {
            break;
        }
        total += n as u64;
        // Counted as it arrives: a server that announces no size, or a wrong one, is stopped here.
        if total > cap {
            return Err(too_large(total));
        }
        sink.write_all(&buf[..n])
            .map_err(|e| format!("cannot write the download: {e}"))?;
        if total >= next_mark {
            progress(total, announced);
            last_mark = total;
            next_mark = total + step;
        }
    }
    // The end is always reported, so a download smaller than one step still shows a line.
    if total != last_mark {
        progress(total, announced);
    }
    if let Some(size) = announced.filter(|size| *size != total) {
        return Err(format!(
            "the download from {} is incomplete: {} of {}",
            url.host,
            size_text(total),
            size_text(size)
        ));
    }
    Ok(total)
}

fn failure_text(e: FetchError, repo: &str, authenticated: bool) -> String {
    match e {
        FetchError::Http {
            status,
            remaining,
            message,
        } => http_failure(
            status,
            repo,
            authenticated,
            remaining.as_deref(),
            message.as_deref(),
        ),
        FetchError::Other(why) => why,
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The release check
// ---------------------------------------------------------------------------------------------------------------

/// Asks GitHub for the newest release. Never panics and never blocks for long: every failure, with its reason,
/// is a `Latest::Failed`. Works unelevated; writes nothing.
pub fn check(installed: Option<&Version>) -> Latest {
    let source = match Source::github(std::env::var(REPO_ENV).ok().as_deref()) {
        Ok(source) => source,
        Err(e) => return Latest::Failed(e),
    };
    let auth = find_token(crate::system::is_admin());
    check_latest(&source, &auth, installed)
}

/// The check against a given source.
pub fn check_latest(source: &Source, auth: &Auth, installed: Option<&Version>) -> Latest {
    let ask = || -> Result<(Option<LatestRelease>, Vec<String>), String> {
        let http = source.transport(CHECK_TIMEOUT)?;
        let mut body = Vec::new();
        fetch(
            &http,
            &source.policy,
            &Get {
                url: &releases_url(&source.api_base, &source.repo),
                accept: ACCEPT_JSON,
                token: auth.token.as_ref(),
                cap: MAX_JSON_BYTES,
            },
            &mut body,
            &mut |_, _| {},
        )
        .map_err(|e| failure_text(e, &source.repo, auth.token.is_some()))?;
        let json = String::from_utf8_lossy(&body);
        Ok((latest_release(&json, installed)?, skipped_tags(&json)))
    };
    let repo = source.repo.clone();
    let auth_text = auth_text(auth.source);
    match ask() {
        Ok((Some(release), skipped)) => Latest::Found {
            release: Box::new(release),
            repo,
            auth: auth_text,
            skipped,
        },
        Ok((None, skipped)) => Latest::NoReleases {
            repo,
            auth: auth_text,
            skipped,
        },
        Err(why) => Latest::Failed(match &auth.note {
            Some(note) => format!("{why} ({auth_text}; {note})"),
            None => format!("{why} ({auth_text})"),
        }),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// SHA-256
// ---------------------------------------------------------------------------------------------------------------

/// SHA-256 from the Windows crypto library (BCrypt).
pub struct Sha256(BCRYPT_HASH_HANDLE);

impl Sha256 {
    pub fn new() -> Result<Sha256, String> {
        let mut handle = BCRYPT_HASH_HANDLE::default();
        unsafe { BCryptCreateHash(BCRYPT_SHA256_ALG_HANDLE, &mut handle, None, None, 0) }
            .ok()
            .map_err(|e| format!("cannot start SHA-256: {e}"))?;
        Ok(Sha256(handle))
    }

    pub fn update(&mut self, data: &[u8]) -> Result<(), String> {
        // BCrypt takes at most 4 GB at a time; callers hash 64 KB buffers.
        for chunk in data.chunks(1 << 30) {
            unsafe { BCryptHashData(self.0, chunk, 0) }
                .ok()
                .map_err(|e| format!("SHA-256 failed: {e}"))?;
        }
        Ok(())
    }

    /// The digest, lower-case hex.
    pub fn finish(self) -> Result<String, String> {
        let mut digest = [0u8; 32];
        unsafe { BCryptFinishHash(self.0, &mut digest, 0) }
            .ok()
            .map_err(|e| format!("SHA-256 failed: {e}"))?;
        Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
    }
}

impl Drop for Sha256 {
    fn drop(&mut self) {
        unsafe {
            let _ = BCryptDestroyHash(self.0);
        }
    }
}

/// Writes to the file and feeds the same bytes to the hash.
struct HashingFile<'a> {
    file: &'a mut File,
    hash: &'a mut Sha256,
}

impl Write for HashingFile<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write_all(buf)?;
        self.hash.update(buf).map_err(io::Error::other)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Download, verify, unpack
// ---------------------------------------------------------------------------------------------------------------

/// A release that was downloaded, verified and unpacked.
#[derive(Debug, PartialEq, Eq)]
pub struct Unpacked {
    /// `<releases>\<tag>`.
    pub dir: PathBuf,
    /// Name of the zip asset.
    pub asset: String,
    /// SHA-256 of the zip, lower-case hex.
    pub sha256: String,
    pub files: usize,
}

/// Deletes what a failed download leaves behind, unless it is told that all went well.
struct Cleanup {
    paths: Vec<PathBuf>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for path in &self.paths {
            let _ = remove_path(path);
        }
    }
}

pub fn remove_path(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(meta) => {
            let attributes = meta.file_attributes();
            let directory = attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
            let link = attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
            match (directory, link) {
                // A junction or a directory link: the link goes, what it points to stays.
                (true, true) => std::fs::remove_dir(path),
                (true, false) => std::fs::remove_dir_all(path),
                (false, _) => std::fs::remove_file(path),
            }
        }
    }
}

/// Creates `dir` if it is missing and checks that it is a real folder: a junction or symbolic link in its place
/// would send everything written "below" it somewhere else.
fn real_dir(dir: &Path) -> Result<(), String> {
    match std::fs::create_dir(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("cannot create {}: {e}", dir.display())),
    }
    let meta = std::fs::symlink_metadata(dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    if !meta.is_dir() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(format!(
            "{} is not a plain folder (a link or a file is in its place): refusing to write there",
            dir.display()
        ));
    }
    Ok(())
}

/// Downloads `release` into `releases_dir\<tag>`:
///
/// 1. `SHA256SUMS.txt` (at most 64 KB) gives the checksum the zip must have;
/// 2. the zip (at most 200 MB) is written to `<tag>.download`, a file opened so that nobody else can write to
///    it, and hashed while it arrives;
/// 3. the SHA-256 must equal the one in `SHA256SUMS.txt` (and the one GitHub reports for the asset, when it
///    does): on any mismatch the file is deleted and nothing is unpacked;
/// 4. the zip is unpacked from that same open file into `<tag>.partial` (`extract_bundle`), which then becomes
///    `<tag>`.
///
/// `releases_dir` and its parent are created if missing; both must be real folders. Whatever fails, nothing of
/// this download is left behind. `log` gets progress lines.
pub fn download_release(
    source: &Source,
    auth: &Auth,
    release: &LatestRelease,
    releases_dir: &Path,
    log: &dyn Fn(LineKind, String),
) -> Result<Unpacked, String> {
    let assets = release.assets.as_ref().map_err(Clone::clone)?;
    // The tag becomes a folder name: it must be exactly the canonical tag of the version (`release::from_tag`).
    if Version::from_tag(&release.tag).as_ref() != Some(&release.version) {
        return Err(format!(
            "\"{}\" is not a release tag",
            release.tag.escape_default()
        ));
    }
    let tag = release.tag.as_str();
    let top = assets
        .zip
        .name
        .strip_suffix(".zip")
        .ok_or_else(|| format!("{} is not a zip", assets.zip.name))?;
    let target = releases_dir.join(tag);
    if target.exists() && std::env::current_exe().is_ok_and(|me| is_inside(&me, &target)) {
        return Err(format!(
            "this installer runs from {}, which the download would replace: start it from somewhere else",
            target.display()
        ));
    }
    if let Some(parent) = releases_dir.parent() {
        real_dir(parent)?;
    }
    real_dir(releases_dir)?;

    let http = source.transport(DOWNLOAD_TIMEOUT)?;
    let authenticated = auth.token.is_some();
    let describe = |e: FetchError| failure_text(e, &source.repo, authenticated);

    log(LineKind::Info, format!("downloading {SUMS_NAME} of {tag}"));
    let mut sums = Vec::new();
    fetch(
        &http,
        &source.policy,
        &Get {
            url: &assets.sums.api_url,
            accept: ACCEPT_BINARY,
            token: auth.token.as_ref(),
            cap: MAX_SUMS_BYTES,
        },
        &mut sums,
        &mut |_, _| {},
    )
    .map_err(describe)?;
    let sums = String::from_utf8(sums).map_err(|_| format!("{SUMS_NAME} of {tag} is not text"))?;
    let expected = expected_sha256(&sums, &assets.zip.name)?;
    if let Some(reported) = assets
        .zip
        .sha256
        .as_deref()
        .filter(|r| !r.eq_ignore_ascii_case(&expected))
    {
        return Err(format!(
            "{SUMS_NAME} lists {expected} for {}, but GitHub reports {reported} for that asset: refusing to \
             download it",
            assets.zip.name
        ));
    }
    log(
        LineKind::Info,
        format!("expected SHA-256 of {}: {expected}", assets.zip.name),
    );

    let download = releases_dir.join(format!("{tag}.download"));
    let partial = releases_dir.join(format!("{tag}.partial"));
    for stale in [&download, &partial] {
        remove_path(stale)
            .map_err(|e| format!("cannot remove the leftover {}: {e}", stale.display()))?;
    }
    let mut cleanup = Cleanup {
        paths: vec![download.clone(), partial.clone()],
    };
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .share_mode(FILE_SHARE_READ)
        .open(&download)
        .map_err(|e| format!("cannot create {}: {e}", download.display()))?;

    log(
        LineKind::Info,
        format!(
            "downloading {} ({})",
            assets.zip.name,
            size_text(assets.zip.size)
        ),
    );
    let mut hash = Sha256::new()?;
    let size = fetch(
        &http,
        &source.policy,
        &Get {
            url: &assets.zip.api_url,
            accept: ACCEPT_BINARY,
            token: auth.token.as_ref(),
            cap: MAX_ZIP_BYTES,
        },
        &mut HashingFile {
            file: &mut file,
            hash: &mut hash,
        },
        &mut |done, total| log(LineKind::Info, format!("  {}", progress_text(done, total))),
    )
    .map_err(describe)?;
    let sha256 = hash.finish()?;
    log(
        LineKind::Info,
        format!("downloaded {}: SHA-256 {sha256}", size_text(size)),
    );
    check_sha256(&assets.zip.name, &expected, &sha256)?;
    log(
        LineKind::Success,
        format!("the SHA-256 matches {SUMS_NAME}"),
    );

    // The same handle that was hashed: what is unpacked is what was verified.
    file.flush()
        .map_err(|e| format!("cannot write {}: {e}", download.display()))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|e| format!("cannot reread {}: {e}", download.display()))?;
    std::fs::create_dir(&partial)
        .map_err(|e| format!("cannot create {}: {e}", partial.display()))?;
    real_dir(&partial)?;
    let (files, bytes) = extract_bundle(&mut file, &partial, top, MAX_UNPACKED_BYTES, MAX_ENTRIES)?;
    drop(file);

    if target.exists() {
        remove_path(&target).map_err(|e| format!("cannot replace {}: {e}", target.display()))?;
    }
    std::fs::rename(&partial, &target).map_err(|e| {
        format!(
            "cannot move the unpacked release to {}: {e}",
            target.display()
        )
    })?;
    cleanup.paths.retain(|p| *p != partial);
    drop(cleanup);
    log(
        LineKind::Success,
        format!(
            "unpacked {files} files ({}) to {}",
            size_text(bytes),
            target.display()
        ),
    );
    Ok(Unpacked {
        dir: target,
        asset: assets.zip.name.clone(),
        sha256,
        files,
    })
}

/// Unpacks a release zip into `dest` (an existing, empty folder), dropping the zip's one top folder `top`.
///
/// Every entry must lie inside `top/` and pass `release::entry_path` (no absolute path, no `..`, no backslash
/// or colon, no device name, ...); symbolic links are refused; files are only ever created, never overwritten
/// (so two entries cannot name the same file); at most `max_entries` entries; at most `max_bytes` in all,
/// counted as the data is unpacked rather than taken from the sizes the zip claims. Only stored and deflated
/// entries are accepted, and no encrypted ones (the `zip` crate is built without anything else, and without
/// decryption).
/// Returns how many files and bytes were written.
pub fn extract_bundle<R: Read + Seek>(
    zip: R,
    dest: &Path,
    top: &str,
    max_bytes: u64,
    max_entries: usize,
) -> Result<(usize, u64), String> {
    let mut archive = zip::ZipArchive::new(zip)
        .map_err(|e| format!("the download is not a usable zip file: {e}"))?;
    if archive.len() > max_entries {
        return Err(format!(
            "the zip has {} entries, more than the {max_entries} allowed",
            archive.len()
        ));
    }
    let mut left = max_bytes;
    let mut files = 0usize;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|e| format!("zip entry {index} cannot be read: {e}"))?;
        let name = entry
            .name()
            .map_err(|e| format!("zip entry {index} has a name that cannot be read: {e}"))?
            .into_owned();
        if entry.is_symlink() {
            return Err(format!(
                "zip entry \"{}\" is a symbolic link",
                name.escape_default()
            ));
        }
        // Checked here, before anything is created: the `zip` crate only notices an unsupported method once the
        // data is read.
        if entry.encrypted() {
            return Err(format!(
                "zip entry \"{}\" is encrypted",
                name.escape_default()
            ));
        }
        let method = entry.compression();
        if !matches!(
            method,
            zip::CompressionMethod::Stored | zip::CompressionMethod::Deflated
        ) {
            return Err(format!(
                "zip entry \"{}\" is compressed with {method:?}, not stored or deflated",
                name.escape_default()
            ));
        }
        let Some(parts) = entry_path(&name, top)? else {
            continue;
        };
        let target = parts
            .iter()
            .fold(dest.to_path_buf(), |path, part| path.join(part));
        let failed = |what: &str, e: io::Error| format!("cannot {what} {}: {e}", target.display());
        if entry.is_dir() {
            std::fs::create_dir_all(&target).map_err(|e| failed("create", e))?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| failed("create the folder of", e))?;
        }
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|e| failed("create", e))?;
        // One byte more than is left: if it arrives, the cap is exceeded.
        let written = io::copy(&mut (&mut entry).take(left + 1), &mut out)
            .map_err(|e| failed("unpack", e))?;
        if written > left {
            return Err(format!(
                "the zip unpacks to more than the {} allowed",
                size_text(max_bytes)
            ));
        }
        left -= written;
        files += 1;
    }
    if files == 0 {
        return Err(format!("the zip has no files in {top}/"));
    }
    Ok((files, max_bytes - left))
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::layout::tests::TempDir;
    use crate::release::tests::{asset_json, release_json};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::io::Cursor;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use zip::write::SimpleFileOptions;

    // ---- SHA-256 ---------------------------------------------------------------------------------------------

    fn sha256_of(data: &[u8]) -> String {
        let mut hash = Sha256::new().unwrap();
        // In pieces, as a download arrives.
        for chunk in data.chunks(7) {
            hash.update(chunk).unwrap();
        }
        hash.finish().unwrap()
    }

    #[test]
    fn sha256_known_answers() {
        assert_eq!(
            sha256_of(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_of(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_of(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(
            sha256_of(&[b'a'; 1_000_000]),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    // ---- gh ---------------------------------------------------------------------------------------------------

    #[test]
    fn gh_is_only_run_elevated_from_a_protected_folder() {
        // Real folders: the decision is made on what the file system says a path is.
        let t = TempDir::new("gh-allowed");
        let installed = t.file(r"Program Files\GitHub CLI\gh.exe", "");
        let user = t.file(r"Users\someone\bin\gh.exe", "");
        let lookalike = t.file(r"Program Files Evil\gh.exe", "");
        let protected = [t.0.join("Program Files")];
        // Unelevated it runs with the user's own rights: any gh on PATH is as good as another, as it was found.
        for gh in [&installed, &user, &lookalike] {
            assert_eq!(gh_to_run(gh, false, &protected), Some(gh.clone()));
        }
        // Elevated only the one that takes an administrator to replace, whatever the spelling.
        let real = std::fs::canonicalize(&installed).unwrap();
        assert_eq!(
            gh_to_run(&installed, true, &protected).as_ref(),
            Some(&real)
        );
        let shouted = PathBuf::from(installed.to_string_lossy().to_uppercase());
        assert!(gh_to_run(&shouted, true, &protected).is_some());
        assert_eq!(gh_to_run(&user, true, &protected), None);
        assert_eq!(gh_to_run(&lookalike, true, &protected), None);
        assert_eq!(gh_to_run(&installed, true, &[]), None);
        // A file that is not there, and a protected folder that is not there, allow nothing.
        assert_eq!(
            gh_to_run(&t.0.join(r"Program Files\gone\gh.exe"), true, &protected),
            None
        );
        assert_eq!(
            gh_to_run(&installed, true, &[t.0.join("no such folder")]),
            None
        );
    }

    #[test]
    fn a_path_that_only_spells_a_protected_folder_does_not_pass() {
        let t = TempDir::new("gh-dotdot");
        t.file(r"Program Files\GitHub CLI\gh.exe", "");
        let planted = t.file(r"Users\someone\bin\gh.exe", "");
        let protected = [t.0.join("Program Files")];
        // `<Program Files>\..\Users\someone\bin\gh.exe` starts with the protected folder as text, and is the
        // planted file. A PATH entry spelled like that is found as such by `gh_on_path`.
        let dotted_dir = protected[0]
            .join("..")
            .join("Users")
            .join("someone")
            .join("bin");
        let found = gh_on_path(Some(dotted_dir.as_os_str())).unwrap();
        assert_eq!(found, dotted_dir.join("gh.exe"));
        assert!(found.starts_with(&protected[0]));
        assert_eq!(
            std::fs::canonicalize(&found).unwrap(),
            std::fs::canonicalize(&planted).unwrap()
        );
        assert_eq!(gh_to_run(&found, true, &protected), None);
        // The plain text comparison does not fall for it either.
        assert!(!is_inside(&found, &protected[0]));
        // The other way round: an odd spelling of a file that really is inside is allowed, and what is run is
        // its resolved path.
        let odd = protected[0]
            .join("GitHub CLI")
            .join("..")
            .join("GitHub CLI")
            .join("gh.exe");
        let run = gh_to_run(&odd, true, &protected).unwrap();
        assert!(
            !run.components()
                .any(|c| matches!(c, std::path::Component::ParentDir)),
            "{}",
            run.display()
        );
        // A link below the protected folder that leads out of it (a junction needs no special rights).
        let junction = protected[0].join("link");
        let made = Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&junction)
            .arg(t.0.join(r"Users\someone\bin"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        assert!(made, "mklink /J failed");
        let through = junction.join("gh.exe");
        assert!(through.is_file());
        assert_eq!(gh_to_run(&through, true, &protected), None);
        let _ = std::fs::remove_dir(&junction);
    }

    #[test]
    fn the_resolved_path_of_a_program_can_be_run() {
        // What `gh_to_run` hands back elevated is a resolved path (it starts with the long-path prefix): a
        // program is started from one just the same.
        let cmd = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32")
            .join("cmd.exe");
        let windows = [PathBuf::from(std::env::var_os("SystemRoot").unwrap())];
        let resolved = gh_to_run(&cmd, true, &windows).unwrap();
        assert!(
            resolved.to_string_lossy().starts_with(r"\\?\"),
            "{}",
            resolved.display()
        );
        let out = run_for_output(&resolved, &["/c", "echo", "token"], GH_TIMEOUT);
        assert_eq!(out.as_deref().map(str::trim), Some("token"));
    }

    #[test]
    fn the_folders_gh_may_run_from_elevated_exclude_the_windows_folder() {
        let protected = crate::system::protected_dirs();
        assert!(!protected.is_empty());
        let windows = PathBuf::from(std::env::var_os("SystemRoot").unwrap());
        // Parts of the Windows folder take new files from ordinary users (Tasks, tracing, spool\drivers\color):
        // a gh.exe there is not run elevated. cmd.exe stands in for "a real file below the Windows folder".
        let in_windows = windows.join("System32").join("cmd.exe");
        assert!(in_windows.is_file());
        assert_eq!(gh_to_run(&in_windows, true, &protected), None);
        for writable in ["Tasks", "tracing", r"System32\spool\drivers\color"] {
            assert!(
                !protected
                    .iter()
                    .any(|dir| is_inside(&windows.join(writable).join("gh.exe"), dir)),
                "{writable}"
            );
        }
    }

    #[test]
    fn gh_is_found_on_path_in_absolute_folders_only() {
        let t = TempDir::new("gh-path");
        let with_gh = t.file(r"bin\gh.exe", "").parent().unwrap().to_path_buf();
        let without = t.0.join("empty");
        std::fs::create_dir_all(&without).unwrap();
        let path = |dirs: &[&Path]| std::env::join_paths(dirs).unwrap();
        assert_eq!(
            gh_on_path(Some(&path(&[&without, &with_gh]))),
            Some(with_gh.join("gh.exe"))
        );
        assert_eq!(gh_on_path(Some(&path(&[&without]))), None);
        assert_eq!(gh_on_path(Some(OsStr::new(""))), None);
        assert_eq!(gh_on_path(None), None);
        // A relative entry ("." or "bin") would mean the current directory: never looked at.
        let relative = path(&[Path::new("."), Path::new("bin")]);
        assert_eq!(gh_on_path(Some(&relative)), None);
    }

    #[test]
    fn a_program_that_hangs_or_fails_yields_no_output() {
        let cmd = Path::new(r"C:\Windows\System32\cmd.exe");
        assert_eq!(
            run_for_output(
                cmd,
                &["/d", "/c", "echo token-text"],
                Duration::from_secs(5)
            )
            .as_deref()
            .map(str::trim),
            Some("token-text")
        );
        assert_eq!(
            run_for_output(cmd, &["/d", "/c", "exit /b 1"], Duration::from_secs(5)),
            None
        );
        let started = Instant::now();
        assert_eq!(
            run_for_output(
                cmd,
                &["/d", "/c", "ping -n 30 127.0.0.1 >nul"],
                Duration::from_millis(300)
            ),
            None
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(
            run_for_output(Path::new(r"C:\no\such\gh.exe"), &[], Duration::from_secs(1)),
            None
        );
    }

    // ---- fetch, against a scripted transport -----------------------------------------------------------------

    /// One scripted answer.
    #[derive(Clone)]
    struct Answer {
        status: u32,
        location: Option<&'static str>,
        content_length: Option<u64>,
        remaining: Option<&'static str>,
        body: Vec<u8>,
    }

    fn ok(body: &[u8]) -> Answer {
        Answer {
            status: 200,
            location: None,
            content_length: Some(body.len() as u64),
            remaining: None,
            body: body.to_vec(),
        }
    }

    fn redirect(status: u32, to: &'static str) -> Answer {
        Answer {
            status,
            location: Some(to),
            content_length: Some(0),
            remaining: None,
            body: vec![],
        }
    }

    /// A request the scripted transport got: the URL without its query, and the headers.
    type SeenRequest = (String, Vec<(&'static str, String)>);

    /// Answers by URL (without the query) and records every request with its headers.
    struct Script {
        answers: HashMap<&'static str, Answer>,
        seen: RefCell<Vec<SeenRequest>>,
    }

    impl Script {
        fn new(answers: &[(&'static str, Answer)]) -> Script {
            Script {
                answers: answers.iter().cloned().collect(),
                seen: RefCell::new(Vec::new()),
            }
        }

        fn urls(&self) -> Vec<String> {
            self.seen
                .borrow()
                .iter()
                .map(|(url, _)| url.clone())
                .collect()
        }
    }

    impl Transport for Script {
        fn get(&self, url: &Url, headers: &[(&'static str, String)]) -> Result<Response, String> {
            let key = url.redacted();
            self.seen.borrow_mut().push((key.clone(), headers.to_vec()));
            let answer = self
                .answers
                .get(key.as_str())
                .ok_or(format!("no route for {key}"))?;
            Ok(Response {
                status: answer.status,
                location: answer.location.map(str::to_string),
                content_length: answer.content_length,
                rate_remaining: answer.remaining.map(str::to_string),
                body: Box::new(Cursor::new(answer.body.clone())),
            })
        }
    }

    const ASSET: &str = "https://api.github.com/repos/o/r/releases/assets/1";
    const SIGNED: &str =
        "https://release-assets.githubusercontent.com/github-production-release-asset/1/2?sig=S";
    const SIGNED_KEY: &str =
        "https://release-assets.githubusercontent.com/github-production-release-asset/1/2";

    fn binary<'a>(url: &'a str, token: Option<&'a Token>, cap: u64) -> Get<'a> {
        Get {
            url,
            accept: ACCEPT_BINARY,
            token,
            cap,
        }
    }

    fn get(
        script: &Script,
        url: &str,
        token: Option<&Token>,
        cap: u64,
    ) -> Result<Vec<u8>, FetchError> {
        let mut out = Vec::new();
        let n = fetch(
            script,
            &Policy::github(),
            &binary(url, token, cap),
            &mut out,
            &mut |_, _| {},
        )?;
        assert_eq!(n, out.len() as u64);
        Ok(out)
    }

    fn other(result: Result<Vec<u8>, FetchError>) -> String {
        match result {
            Err(FetchError::Other(why)) => why,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn an_asset_is_fetched_through_its_redirect_and_the_token_stays_with_the_api() {
        let script = Script::new(&[
            (ASSET, redirect(302, SIGNED)),
            (SIGNED_KEY, ok(b"zip bytes")),
        ]);
        let token = Token::new("ghp_SECRET").unwrap();
        assert_eq!(
            get(&script, ASSET, Some(&token), 100).unwrap(),
            b"zip bytes"
        );
        let seen = script.seen.borrow();
        assert_eq!(seen.len(), 2);
        let auth = |headers: &[(&'static str, String)]| {
            headers
                .iter()
                .find(|(name, _)| *name == "Authorization")
                .map(|(_, v)| v.clone())
        };
        assert_eq!(auth(&seen[0].1).as_deref(), Some("Bearer ghp_SECRET"));
        assert_eq!(auth(&seen[1].1), None);
        // Both hops ask for the file itself.
        for (_, headers) in seen.iter() {
            assert!(
                headers.contains(&("Accept", ACCEPT_BINARY.to_string())),
                "{headers:?}"
            );
        }
    }

    #[test]
    fn a_redirect_off_the_allow_list_is_never_requested() {
        for (to, why) in [
            (
                "https://evil.example/crossc.zip",
                "is not one of the GitHub hosts",
            ),
            (
                "http://release-assets.githubusercontent.com/x",
                "only HTTPS",
            ),
            (
                "https://release-assets.githubusercontent.com:8443/x",
                "not the HTTPS port",
            ),
        ] {
            let script = Script::new(&[(ASSET, redirect(302, to))]);
            let why_refused = other(get(&script, ASSET, None, 100));
            assert!(why_refused.contains(why), "{to}: {why_refused}");
            // Only the first request went out.
            assert_eq!(script.urls(), [ASSET]);
        }
        // The first address itself is checked too, before anything is sent.
        let script = Script::new(&[]);
        for start in [
            "https://evil.example/x",
            "http://api.github.com/x",
            "not a url",
        ] {
            other(get(&script, start, None, 100));
        }
        assert!(script.urls().is_empty());
    }

    #[test]
    fn redirect_loops_end() {
        let script = Script::new(&[
            (ASSET, redirect(302, "https://github.com/a")),
            (
                "https://github.com/a",
                redirect(301, "https://github.com/b"),
            ),
            ("https://github.com/b", redirect(307, "/a")),
        ]);
        let why = other(get(&script, ASSET, None, 100));
        assert!(why.contains("more than 5 redirects"), "{why}");
        // The first request and five followed redirects.
        assert_eq!(script.urls().len(), 6);
    }

    #[test]
    fn size_caps_hold_for_announced_and_for_actual_bytes() {
        let body = vec![7u8; 5000];
        // Exactly the cap is fine.
        let script = Script::new(&[(ASSET, ok(&body))]);
        assert_eq!(get(&script, ASSET, None, 5000).unwrap().len(), 5000);
        // Announced too large: refused before a byte is read.
        let why = other(get(&script, ASSET, None, 4999));
        assert!(why.contains("sends more than the 4 KB allowed"), "{why}");
        // No size announced, or a small one that is a lie: stopped while reading.
        for content_length in [None, Some(10)] {
            let script = Script::new(&[(
                ASSET,
                Answer {
                    content_length,
                    ..ok(&body)
                },
            )]);
            let mut out = Vec::new();
            let result = fetch(
                &script,
                &Policy::github(),
                &binary(ASSET, None, 4999),
                &mut out,
                &mut |_, _| {},
            );
            let Err(FetchError::Other(why)) = result else {
                panic!("{result:?}");
            };
            assert!(why.contains("sends more than the 4 KB allowed"), "{why}");
            assert!(out.len() <= 4999, "{}", out.len());
        }
        // The real caps: one byte over the zip cap and over the sums cap, without allocating 200 MB.
        struct Endless;
        impl Transport for Endless {
            fn get(&self, _: &Url, _: &[(&'static str, String)]) -> Result<Response, String> {
                Ok(Response {
                    status: 200,
                    location: None,
                    content_length: None,
                    rate_remaining: None,
                    body: Box::new(io::repeat(0)),
                })
            }
        }
        for cap in [MAX_SUMS_BYTES, MAX_ZIP_BYTES] {
            let mut count = 0u64;
            struct Count<'a>(&'a mut u64);
            impl Write for Count<'_> {
                fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                    *self.0 += buf.len() as u64;
                    Ok(buf.len())
                }
                fn flush(&mut self) -> io::Result<()> {
                    Ok(())
                }
            }
            let result = fetch(
                &Endless,
                &Policy::github(),
                &binary(ASSET, None, cap),
                &mut Count(&mut count),
                &mut |_, _| {},
            );
            assert!(matches!(result, Err(FetchError::Other(_))), "{result:?}");
            assert!(count <= cap, "{count} > {cap}");
        }
    }

    #[test]
    fn a_download_that_ends_early_is_an_error() {
        let script = Script::new(&[(
            ASSET,
            Answer {
                content_length: Some(100),
                ..ok(b"short")
            },
        )]);
        let why = other(get(&script, ASSET, None, 1000));
        assert!(why.contains("is incomplete: 5 bytes of 100 bytes"), "{why}");
    }

    #[test]
    fn progress_is_reported_while_reading() {
        let body = vec![1u8; 3 * 1024 * 1024];
        let script = Script::new(&[(ASSET, ok(&body))]);
        let marks = RefCell::new(Vec::new());
        let mut out = Vec::new();
        fetch(
            &script,
            &Policy::github(),
            &binary(ASSET, None, MAX_ZIP_BYTES),
            &mut out,
            &mut |done, total| marks.borrow_mut().push((done, total)),
        )
        .unwrap();
        let marks = marks.into_inner();
        // A mark per megabyte (the step for a small file), each with the announced total.
        assert_eq!(marks.len(), 3, "{marks:?}");
        assert!(
            marks
                .iter()
                .all(|(_, total)| *total == Some(body.len() as u64))
        );
        assert_eq!(marks.last().unwrap().0, body.len() as u64);
    }

    #[test]
    fn http_errors_carry_status_rate_limit_and_message() {
        let not_found = Answer {
            status: 404,
            remaining: Some("59"),
            ..ok(br#"{"message": "Not Found", "status": "404"}"#)
        };
        let script = Script::new(&[(ASSET, not_found)]);
        let error = get(&script, ASSET, None, 100).unwrap_err();
        assert_eq!(
            error,
            FetchError::Http {
                status: 404,
                remaining: Some("59".into()),
                message: Some("Not Found".into())
            }
        );
        let text = failure_text(error, "o/r", false);
        assert!(
            text.contains("o/r was not found (HTTP 404). If the repository is private"),
            "{text}"
        );
        // An HTML error page has no message; the status still gets through.
        let script = Script::new(&[(
            ASSET,
            Answer {
                status: 502,
                ..ok(b"<html>Bad Gateway</html>")
            },
        )]);
        let text = failure_text(get(&script, ASSET, None, 100).unwrap_err(), "o/r", false);
        assert_eq!(text, "GitHub answered HTTP 502");
    }

    // ---- unpacking -------------------------------------------------------------------------------------------

    const TOP: &str = "crossc-0.2.0-x64";

    /// A zip with these entries (a name ending in `/` is a folder), deflated.
    pub fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in entries {
            if name.ends_with('/') {
                writer.add_directory(*name, options).unwrap();
            } else {
                writer.start_file(*name, options).unwrap();
                writer.write_all(data).unwrap();
            }
        }
        writer.finish().unwrap().into_inner()
    }

    /// A release bundle like the one tools/package.ps1 zips (one top folder, forward slashes), with a real
    /// executable in the place of the service and a real certificate.
    pub fn bundle_zip(top: &str) -> Vec<u8> {
        // Any complete Windows program will do for the three executables; a small one keeps the zip small.
        let small_exe = std::fs::read(r"C:\Windows\System32\whoami.exe")
            .or_else(|_| std::fs::read(std::env::current_exe().unwrap()))
            .unwrap();
        let inf = "[Version]\r\nDriverVer = 11/11/2026,8.50.53.934\r\n[Standard.NTamd64]\r\n\
                   %d% = I, BTHENUM\\{0000110b-0000-1000-8000-00805f9b34fb}_VID&00010075_PID&a013\r\n";
        let p = |rel: &str| format!("{top}/{rel}");
        zip_of(&[
            (
                &p("BUILD-INFO.txt"),
                b"crossc 0.2.0 (x64)\n\ncommit    : abc\n",
            ),
            (&p("LICENSE"), b"license text"),
            (&p("README.md"), b"# crossc"),
            (&p("crossc-cli.exe"), &small_exe),
            (&p("crossc-installer.exe"), &small_exe),
            (&p("crossc-service.exe"), &small_exe),
            (&p("crossc-tray.exe"), &small_exe),
            (&p("driver/WDRLocalTestCert.cer"), crate::cert::tests::WDR_A),
            (&p("driver/crossc_bth.cat"), b"catalog"),
            (&p("driver/crossc_bth.inf"), inf.as_bytes()),
            (&p("driver/crossc_bth.sys"), b"driver"),
            (&p("licenses/ssc-enc/NOTICE"), b"notice"),
        ])
    }

    fn unpack(zip: &[u8], dest: &Path) -> Result<(usize, u64), String> {
        extract_bundle(Cursor::new(zip), dest, TOP, MAX_UNPACKED_BYTES, MAX_ENTRIES)
    }

    /// Every file below `dir`, relative, with forward slashes, sorted.
    fn files_in(dir: &Path) -> Vec<String> {
        fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, base, out);
                } else {
                    out.push(
                        path.strip_prefix(base)
                            .unwrap()
                            .to_string_lossy()
                            .replace('\\', "/"),
                    );
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, dir, &mut out);
        out.sort();
        out
    }

    #[test]
    fn a_bundle_unpacks_without_its_top_folder() {
        let t = TempDir::new("unpack");
        let (files, bytes) = unpack(&bundle_zip(TOP), &t.0).unwrap();
        assert_eq!(files, 12);
        assert_eq!(
            files_in(&t.0),
            [
                "BUILD-INFO.txt",
                "LICENSE",
                "README.md",
                "crossc-cli.exe",
                "crossc-installer.exe",
                "crossc-service.exe",
                "crossc-tray.exe",
                "driver/WDRLocalTestCert.cer",
                "driver/crossc_bth.cat",
                "driver/crossc_bth.inf",
                "driver/crossc_bth.sys",
                "licenses/ssc-enc/NOTICE",
            ]
        );
        assert_eq!(
            std::fs::read(t.0.join(r"driver\crossc_bth.sys")).unwrap(),
            b"driver"
        );
        assert_eq!(
            std::fs::read(t.0.join(r"driver\WDRLocalTestCert.cer")).unwrap(),
            crate::cert::tests::WDR_A
        );
        let on_disk: u64 = files_in(&t.0)
            .iter()
            .map(|f| std::fs::metadata(t.0.join(f)).unwrap().len())
            .sum();
        assert_eq!(bytes, on_disk);
        // Explicit folder entries (some zip tools write them) change nothing.
        let t2 = TempDir::new("unpack-dirs");
        let zip = zip_of(&[
            ("crossc-0.2.0-x64/", b""),
            ("crossc-0.2.0-x64/driver/", b""),
            ("crossc-0.2.0-x64/driver/crossc_bth.inf", b"inf"),
            ("crossc-0.2.0-x64/empty/", b""),
        ]);
        assert_eq!(unpack(&zip, &t2.0).unwrap().0, 1);
        assert_eq!(files_in(&t2.0), ["driver/crossc_bth.inf"]);
        assert!(t2.0.join("empty").is_dir());
    }

    #[test]
    fn zip_slip_entries_stop_the_unpacking_and_write_nothing_outside() {
        for (name, why) in [
            ("../evil.exe", "\"..\" part"),
            ("crossc-0.2.0-x64/../../evil.exe", "\"..\" part"),
            ("crossc-0.2.0-x64/driver/../../evil.exe", "\"..\" part"),
            ("/evil.exe", "absolute path"),
            ("C:/Windows/Temp/evil.exe", "not allowed in a file name"),
            (
                r"crossc-0.2.0-x64\..\evil.exe",
                "not allowed in a file name",
            ),
            (
                "crossc-0.2.0-x64/x.exe:hidden",
                "not allowed in a file name",
            ),
            ("crossc-0.2.0-x64/NUL", "device name"),
            ("crossc-0.2.0-x64/x.exe.", "ends in a dot or a space"),
            ("evil.exe", "is not inside crossc-0.2.0-x64/"),
            (
                "crossc-0.1.0-x64/crossc-installer.exe",
                "is not inside crossc-0.2.0-x64/",
            ),
        ] {
            let t = TempDir::new("zip-slip");
            let dest = t.0.join("releases").join("v0.2.0.partial");
            std::fs::create_dir_all(&dest).unwrap();
            // A good entry first: the bad one must stop everything that follows, and nothing may land outside.
            let zip = zip_of(&[
                ("crossc-0.2.0-x64/README.md", b"fine"),
                (name, b"evil"),
                ("crossc-0.2.0-x64/after.txt", b"never written"),
            ]);
            let err = unpack(&zip, &dest).unwrap_err();
            assert!(err.contains(why), "{name}: {err}");
            let outside: Vec<String> = files_in(&t.0)
                .into_iter()
                .filter(|f| !f.starts_with("releases/v0.2.0.partial/"))
                .collect();
            assert!(outside.is_empty(), "{name}: {outside:?}");
            assert!(!dest.join("after.txt").exists(), "{name}");
        }
    }

    #[test]
    fn symlinks_duplicates_and_garbage_are_refused() {
        let t = TempDir::new("zip-odd");
        // A symbolic link entry (what a zip made on Linux can carry).
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .add_symlink(
                "crossc-0.2.0-x64/link",
                "C:/Windows/System32",
                SimpleFileOptions::default(),
            )
            .unwrap();
        let zip = writer.finish().unwrap().into_inner();
        let err = unpack(&zip, &t.0).unwrap_err();
        assert!(err.contains("is a symbolic link"), "{err}");
        assert!(files_in(&t.0).is_empty());

        // Two entries for one file, differing only in case: the second cannot overwrite the first.
        let t = TempDir::new("zip-dup");
        let zip = zip_of(&[
            ("crossc-0.2.0-x64/a.txt", b"first"),
            ("crossc-0.2.0-x64/A.TXT", b"second"),
        ]);
        let err = unpack(&zip, &t.0).unwrap_err();
        assert!(err.starts_with("cannot create"), "{err}");
        assert_eq!(std::fs::read(t.0.join("a.txt")).unwrap(), b"first");

        // A file where a folder is needed.
        let t = TempDir::new("zip-clash");
        let zip = zip_of(&[
            ("crossc-0.2.0-x64/driver", b"file"),
            ("crossc-0.2.0-x64/driver/x.sys", b"x"),
        ]);
        assert!(unpack(&zip, &t.0).is_err());

        // Not a zip at all; a zip cut in half; an empty zip; a zip with only the top folder.
        let t = TempDir::new("zip-garbage");
        let err = unpack(b"<html>404</html>", &t.0).unwrap_err();
        assert!(err.contains("not a usable zip file"), "{err}");
        let whole = bundle_zip(TOP);
        assert!(unpack(&whole[..whole.len() / 2], &t.0).is_err());
        assert!(
            unpack(&zip_of(&[]), &t.0)
                .unwrap_err()
                .contains("has no files")
        );
        assert!(
            unpack(&zip_of(&[("crossc-0.2.0-x64/", b"")]), &t.0)
                .unwrap_err()
                .contains("has no files")
        );
        assert!(files_in(&t.0).is_empty());
    }

    /// One stored entry whose local and central headers are patched with `patch(header, method_at, flags_at)`.
    fn patched_zip(patch: impl Fn(&mut [u8], usize, usize)) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        writer
            .start_file("crossc-0.2.0-x64/a.txt", options)
            .unwrap();
        writer.write_all(b"plain").unwrap();
        let mut zip = writer.finish().unwrap().into_inner();
        // Local file header: flags at 6, method at 8. Central directory header: flags at 8, method at 10.
        let local = zip.windows(4).position(|w| w == b"PK\x03\x04").unwrap();
        let central = zip.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
        patch(&mut zip[local..], 8, 6);
        patch(&mut zip[central..], 10, 8);
        zip
    }

    #[test]
    fn other_compression_methods_and_encrypted_entries_are_refused() {
        // Deflate64 (9), bzip2 (12), LZMA (14), zstd (93), and 77, which is no method at all. The zip crate is
        // built without the named ones, so today it prints them as `Unsupported(n)`; only the refusal is checked,
        // not that name, which a zip release with those methods built in would change.
        for method in [9u16, 12, 14, 93, 77] {
            let t = TempDir::new("zip-method");
            let zip = patched_zip(|h, at, _| h[at..at + 2].copy_from_slice(&method.to_le_bytes()));
            let err = unpack(&zip, &t.0).unwrap_err();
            assert!(
                err.contains("is compressed with ") && err.ends_with(", not stored or deflated"),
                "{err}"
            );
            assert!(files_in(&t.0).is_empty(), "{method}");
        }
        // The encrypted flag (bit 0): there is no decryption to read it with. The zip crate itself refuses to open
        // such an entry (`extract_bundle` checks it again, should that ever change).
        let t = TempDir::new("zip-encrypted");
        let zip = patched_zip(|h, _, at| h[at] |= 1);
        let err = unpack(&zip, &t.0).unwrap_err();
        assert!(
            err.ends_with("a.txt\" is encrypted") || err.contains("Password required"),
            "{err}"
        );
        assert!(files_in(&t.0).is_empty());
        // The unpatched zip is fine, so it is the patch that is refused.
        let t = TempDir::new("zip-unpatched");
        assert_eq!(unpack(&patched_zip(|_, _, _| {}), &t.0).unwrap(), (1, 5));
    }

    #[test]
    fn unpacked_size_and_entry_count_are_capped() {
        // 1 MB of zeros deflates to about 1 KB: the cap is on what comes out, not on the zip or its headers.
        let big = vec![0u8; 1024 * 1024];
        let zip = zip_of(&[
            ("crossc-0.2.0-x64/a.bin", &big),
            ("crossc-0.2.0-x64/b.bin", &big),
        ]);
        assert!(zip.len() < 10_000, "{}", zip.len());
        let t = TempDir::new("zip-cap");
        let unpack_capped = |dest: &Path, max_bytes, max_entries| {
            extract_bundle(Cursor::new(&zip), dest, TOP, max_bytes, max_entries)
        };
        assert_eq!(
            unpack_capped(&t.0.join("exact"), 2 * 1024 * 1024, 2),
            Ok((2, 2 * 1024 * 1024))
        );
        let err = unpack_capped(&t.0.join("bytes"), 2 * 1024 * 1024 - 1, 2).unwrap_err();
        assert!(
            err.contains("unpacks to more than the 2.0 MB allowed"),
            "{err}"
        );
        // What was written before the cap hit stays under the cap.
        let written: u64 = files_in(&t.0.join("bytes"))
            .iter()
            .map(|f| std::fs::metadata(t.0.join("bytes").join(f)).unwrap().len())
            .sum();
        assert!(written <= 2 * 1024 * 1024, "{written}");
        let err = unpack_capped(&t.0.join("entries"), u64::MAX / 2, 1).unwrap_err();
        assert!(err.contains("2 entries, more than the 1 allowed"), "{err}");
        assert!(!t.0.join("entries").exists());
    }

    #[test]
    fn a_link_in_place_of_a_folder_is_refused() {
        let t = TempDir::new("real-dir");
        let plain = t.0.join("plain");
        assert_eq!(real_dir(&plain), Ok(()));
        assert_eq!(real_dir(&plain), Ok(()));
        // A file where the folder should be.
        let file = t.file("file", "x");
        assert!(real_dir(&file).unwrap_err().contains("not a plain folder"));
        // A junction (needs no privilege to create, unlike a symbolic link).
        let junction = t.0.join("junction");
        let made = Command::new(r"C:\Windows\System32\cmd.exe")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&junction)
            .arg(&plain)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .is_ok_and(|o| o.status.success());
        if made {
            assert!(
                real_dir(&junction)
                    .unwrap_err()
                    .contains("not a plain folder")
            );
            // Removing it removes the link, not what it points to.
            t.file(r"plain\keep.txt", "x");
            remove_path(&junction).unwrap();
            assert!(plain.join("keep.txt").is_file());
        } else {
            eprintln!("could not create a junction: skipping that part");
        }
    }

    // ---- against GitHub itself (not part of the normal run) --------------------------------------------------

    /// The real thing, read-only: lists the releases of a public repository, downloads its smallest asset the
    /// way a crossc release would be downloaded (API asset address, `Accept: application/octet-stream`,
    /// anonymous, the production allow-list) and compares its SHA-256 with the digest GitHub reports. This is
    /// what shows that the redirect GitHub answers with still lands on a host of the allow-list.
    /// `cargo test -p crossc-installer -- --ignored real_github`; `CROSSC_TEST_REPO` names another repository.
    #[test]
    #[ignore = "talks to api.github.com"]
    fn real_github_asset_download_follows_the_redirect_and_matches_the_digest() {
        let repo = std::env::var("CROSSC_TEST_REPO").unwrap_or_else(|_| "cli/cli".into());
        let source = Source::github(Some(&repo)).unwrap();
        let http = source.transport(CHECK_TIMEOUT).unwrap();
        let mut json = Vec::new();
        let list = Get {
            url: &releases_url(&source.api_base, &source.repo),
            accept: ACCEPT_JSON,
            token: None,
            cap: MAX_JSON_BYTES,
        };
        fetch(&http, &source.policy, &list, &mut json, &mut |_, _| {}).unwrap();
        let releases = crate::release::parse_releases(&String::from_utf8_lossy(&json)).unwrap();
        let asset = releases
            .iter()
            .flat_map(|r| &r.assets)
            .filter(|a| a.sha256.is_some())
            .min_by_key(|a| a.size)
            .expect("a release asset with a digest");
        let mut hash = Sha256::new().unwrap();
        let mut bytes = Vec::new();
        let get = Get {
            url: &asset.api_url,
            accept: ACCEPT_BINARY,
            token: None,
            cap: MAX_ZIP_BYTES,
        };
        let size = fetch(&http, &source.policy, &get, &mut bytes, &mut |_, _| {}).unwrap();
        hash.update(&bytes).unwrap();
        assert_eq!(size, asset.size, "{}", asset.name);
        assert_eq!(Some(hash.finish().unwrap()), asset.sha256, "{}", asset.name);
        eprintln!(
            "{repo}: {} ({size} bytes) downloaded and verified",
            asset.name
        );
    }

    // ---- end to end, against a local HTTP server -------------------------------------------------------------

    /// What the mock server answers for a path.
    #[derive(Clone)]
    enum Route {
        Ok(Vec<u8>),
        Redirect(String),
        Status(u16, &'static str),
    }

    /// One request as the mock server saw it.
    #[derive(Debug, Clone)]
    struct Seen {
        host: &'static str,
        path: String,
        headers: Vec<(String, String)>,
    }

    impl Seen {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }
    }

    /// A tiny HTTP/1.1 server on two loopback addresses: `127.0.0.1` plays api.github.com, `127.0.0.2` the
    /// host downloads are redirected to. Plain HTTP: only `Policy::local_test`, which exists in test builds
    /// only, lets the client talk to it.
    struct Mock {
        api: String,
        files: String,
        routes: Arc<Mutex<HashMap<String, Route>>>,
        seen: Arc<Mutex<Vec<Seen>>>,
    }

    impl Mock {
        fn start() -> Mock {
            let routes = Arc::new(Mutex::new(HashMap::new()));
            let seen = Arc::new(Mutex::new(Vec::new()));
            let listen = |host: &'static str| {
                let listener = TcpListener::bind((host, 0)).unwrap();
                let base = format!("http://{host}:{}", listener.local_addr().unwrap().port());
                let (routes, seen) = (routes.clone(), seen.clone());
                std::thread::spawn(move || {
                    for stream in listener.incoming().flatten() {
                        serve(stream, host, &routes, &seen);
                    }
                });
                base
            };
            Mock {
                api: listen("127.0.0.1"),
                files: listen("127.0.0.2"),
                routes,
                seen,
            }
        }

        fn route(&self, path: &str, route: Route) {
            self.routes.lock().unwrap().insert(path.to_string(), route);
        }

        fn source(&self) -> Source {
            Source {
                api_base: self.api.clone(),
                repo: "mrquantumoff/crossc".into(),
                policy: Policy::local_test("127.0.0.1", &["127.0.0.2"]),
                direct: true,
            }
        }

        fn seen(&self) -> Vec<Seen> {
            self.seen.lock().unwrap().clone()
        }

        /// Publishes release `version` with a zip and its checksum file; returns the zip's SHA-256. The assets
        /// are served the way GitHub does it: the API address redirects to a signed address on the file host.
        fn publish(&self, version: &str, zip: &[u8], listed_sha256: Option<&str>) -> String {
            let sha256 = sha256_of(zip);
            let name = format!("crossc-{version}-x64.zip");
            let sums = format!(
                "{}  crossc-{version}-symbols.zip\n{}  {name}\n",
                "0".repeat(64),
                listed_sha256.unwrap_or(&sha256)
            );
            let assets = [
                asset_json(&self.api, 11, &name, zip.len() as u64, None),
                asset_json(&self.api, 12, SUMS_NAME, sums.len() as u64, None),
            ];
            let list = format!(
                "[{},{}]",
                release_json(&format!("v{version}"), false, true, &assets),
                release_json("v0.1.0", false, true, &[])
            );
            self.route(
                "/repos/mrquantumoff/crossc/releases?per_page=20",
                Route::Ok(list.into_bytes()),
            );
            for (id, body) in [(11, zip.to_vec()), (12, sums.into_bytes())] {
                let signed = format!("/github-production-release-asset/1/{id}?sig=SIGNATURE");
                self.route(
                    &format!("/repos/mrquantumoff/crossc/releases/assets/{id}"),
                    Route::Redirect(format!("{}{signed}", self.files)),
                );
                self.route(&signed, Route::Ok(body));
            }
            sha256
        }
    }

    fn serve(
        mut stream: std::net::TcpStream,
        host: &'static str,
        routes: &Mutex<HashMap<String, Route>>,
        seen: &Mutex<Vec<Seen>>,
    ) {
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            match stream.read(&mut byte) {
                Ok(1) => request.push(byte[0]),
                _ => return,
            }
        }
        let text = String::from_utf8_lossy(&request);
        let mut lines = text.lines();
        let path = lines
            .next()
            .unwrap_or("")
            .split(' ')
            .nth(1)
            .unwrap_or("")
            .to_string();
        let headers = lines
            .filter_map(|l| l.split_once(": "))
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect();
        seen.lock().unwrap().push(Seen {
            host,
            path: path.clone(),
            headers,
        });
        let route = routes.lock().unwrap().get(&path).cloned();
        let (status, extra, body) = match route {
            Some(Route::Ok(body)) => ("200 OK".to_string(), String::new(), body),
            Some(Route::Redirect(to)) => (
                "302 Found".to_string(),
                format!("Location: {to}\r\n"),
                Vec::new(),
            ),
            Some(Route::Status(code, body)) => (
                format!("{code} Nope"),
                String::new(),
                body.as_bytes().to_vec(),
            ),
            None => (
                "404 Not Found".to_string(),
                "X-RateLimit-Remaining: 59\r\n".to_string(),
                br#"{"message": "Not Found", "status": "404"}"#.to_vec(),
            ),
        };
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&body);
    }

    type LogLines = Arc<Mutex<Vec<(LineKind, String)>>>;

    fn collect_log() -> (LogLines, impl Fn(LineKind, String)) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = lines.clone();
        (lines, move |kind, text| {
            sink.lock().unwrap().push((kind, text))
        })
    }

    fn found(latest: Latest) -> LatestRelease {
        match latest {
            Latest::Found { release, .. } => *release,
            other => panic!("expected a release, got {other:?}"),
        }
    }

    fn auth_with(token: &str) -> Auth {
        Auth {
            token: Token::new(token),
            source: Some(TokenSource::Env("GITHUB_TOKEN")),
            note: None,
        }
    }

    #[test]
    fn end_to_end_check_download_verify_and_unpack() {
        let mock = Mock::start();
        let zip = bundle_zip(TOP);
        let sha256 = mock.publish("0.2.0", &zip, None);
        let source = mock.source();
        let auth = auth_with("ghp_TESTTOKEN");

        // The check: one request, with the token, the API version and the JSON media type.
        let latest = check_latest(&source, &auth, None);
        let Latest::Found {
            release,
            repo,
            auth: how,
            skipped,
        } = &latest
        else {
            panic!("{latest:?}");
        };
        assert_eq!(
            (repo.as_str(), how.as_str()),
            ("mrquantumoff/crossc", "token from GITHUB_TOKEN")
        );
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(release.tag, "v0.2.0");
        let seen = mock.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0].path,
            "/repos/mrquantumoff/crossc/releases?per_page=20"
        );
        assert_eq!(
            seen[0].header("Authorization"),
            Some("Bearer ghp_TESTTOKEN")
        );
        assert_eq!(seen[0].header("Accept"), Some(ACCEPT_JSON));
        assert_eq!(seen[0].header("X-GitHub-Api-Version"), Some("2022-11-28"));
        assert!(
            seen[0]
                .header("User-Agent")
                .is_some_and(|ua| ua.starts_with("crossc-installer/"))
        );

        // The download, into a temp folder standing in for %ProgramFiles%\crossc\releases.
        let t = TempDir::new("e2e");
        let releases = t.0.join("crossc").join("releases");
        // Leftovers of an interrupted earlier download, and an older unpacked copy of the same tag.
        t.file(r"crossc\releases\v0.2.0.download", "half a zip");
        t.file(r"crossc\releases\v0.2.0.partial\stale.txt", "stale");
        t.file(r"crossc\releases\v0.2.0\old.txt", "old");
        let (lines, log) = collect_log();
        let unpacked = download_release(&source, &auth, release, &releases, &log).unwrap();
        assert_eq!(unpacked.dir, releases.join("v0.2.0"));
        assert_eq!(unpacked.asset, "crossc-0.2.0-x64.zip");
        assert_eq!(unpacked.sha256, sha256);
        assert_eq!(unpacked.files, 12);

        // What is on disk: the release, unpacked without its top folder, and nothing else.
        let mut top_level: Vec<String> = std::fs::read_dir(&releases)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        top_level.sort();
        assert_eq!(top_level, ["v0.2.0"]);
        let files = files_in(&unpacked.dir);
        assert_eq!(files.len(), 12, "{files:?}");
        assert!(
            files.contains(&"driver/crossc_bth.inf".to_string())
                && !files.contains(&"old.txt".to_string())
        );
        assert_eq!(
            std::fs::read(unpacked.dir.join(r"driver\WDRLocalTestCert.cer")).unwrap(),
            crate::cert::tests::WDR_A
        );
        assert_eq!(
            crate::system::damaged_exe(&unpacked.dir.join("crossc-service.exe")),
            None
        );
        // It is a bundle the installer recognises, with the version in BUILD-INFO.txt.
        let layout = crate::layout::detect(Some(&unpacked.dir), &t.0.join("no-repo"));
        assert_eq!(layout.mode, crate::layout::Mode::Bundle);
        assert_eq!(
            crate::layout::bundle_version_of(&unpacked.dir),
            Version::parse("0.2.0")
        );

        // The requests: each asset is asked for at the API (with the token and the binary media type) and
        // fetched from the file host it redirects to (without the token).
        let seen = mock.seen();
        let downloads = &seen[1..];
        assert_eq!(
            downloads
                .iter()
                .map(|s| (s.host, s.path.as_str()))
                .collect::<Vec<_>>(),
            [
                ("127.0.0.1", "/repos/mrquantumoff/crossc/releases/assets/12"),
                (
                    "127.0.0.2",
                    "/github-production-release-asset/1/12?sig=SIGNATURE"
                ),
                ("127.0.0.1", "/repos/mrquantumoff/crossc/releases/assets/11"),
                (
                    "127.0.0.2",
                    "/github-production-release-asset/1/11?sig=SIGNATURE"
                ),
            ]
        );
        for request in downloads {
            assert_eq!(request.header("Accept"), Some(ACCEPT_BINARY));
            let expect_token = (request.host == "127.0.0.1").then_some("Bearer ghp_TESTTOKEN");
            assert_eq!(request.header("Authorization"), expect_token, "{request:?}");
            assert_eq!(request.header("Cookie"), None);
        }

        // The log: progress, the checksum, the result; never the token or the signature of the redirect.
        let lines = lines.lock().unwrap();
        let text: Vec<&str> = lines.iter().map(|(_, t)| t.as_str()).collect();
        assert!(
            text.iter()
                .any(|l| l.contains("downloading crossc-0.2.0-x64.zip")),
            "{text:?}"
        );
        assert!(
            text.iter()
                .any(|l| l.contains(&format!("SHA-256 {sha256}"))),
            "{text:?}"
        );
        assert!(
            text.iter()
                .any(|l| l.contains("the SHA-256 matches SHA256SUMS.txt")),
            "{text:?}"
        );
        assert!(
            text.iter().any(|l| l.contains("unpacked 12 files")),
            "{text:?}"
        );
        assert!(
            text.iter()
                .any(|l| l.trim_start().contains(" of ") && l.contains('%')),
            "no progress line in {text:?}"
        );
        assert!(
            !text
                .iter()
                .any(|l| l.contains("TESTTOKEN") || l.contains("SIGNATURE")),
            "{text:?}"
        );
    }

    #[test]
    fn end_to_end_a_checksum_mismatch_unpacks_nothing() {
        let mock = Mock::start();
        let zip = bundle_zip(TOP);
        // SHA256SUMS.txt lists another checksum than the zip that is served has.
        let actual = mock.publish("0.2.0", &zip, Some(&"ab".repeat(32)));
        let source = mock.source();
        let release = found(check_latest(&source, &Auth::anonymous(), None));
        let t = TempDir::new("e2e-mismatch");
        let releases = t.0.join("crossc").join("releases");
        // An earlier, good copy of that release stays as it is.
        t.file(r"crossc\releases\v0.2.0\keep.txt", "good");
        let (lines, log) = collect_log();
        let err =
            download_release(&source, &Auth::anonymous(), &release, &releases, &log).unwrap_err();
        assert!(
            err.contains("SHA-256 mismatch for crossc-0.2.0-x64.zip"),
            "{err}"
        );
        assert!(
            err.contains(&"ab".repeat(32)) && err.contains(&actual),
            "{err}"
        );
        assert_eq!(files_in(&releases), ["v0.2.0/keep.txt"]);
        assert!(
            !lines
                .lock()
                .unwrap()
                .iter()
                .any(|(_, l)| l.contains("unpacked"))
        );
        // Anonymous: no Authorization header anywhere.
        assert!(
            mock.seen()
                .iter()
                .all(|s| s.header("Authorization").is_none())
        );
    }

    #[test]
    fn end_to_end_bad_releases_are_refused_and_leave_nothing() {
        let t = TempDir::new("e2e-bad");
        let releases = t.0.join("crossc").join("releases");
        let anonymous = Auth::anonymous();
        let attempt = |mock: &Mock| {
            let source = mock.source();
            let release = found(check_latest(&source, &anonymous, None));
            let err =
                download_release(&source, &anonymous, &release, &releases, &|_, _| {}).unwrap_err();
            assert!(
                files_in(&t.0).is_empty(),
                "left behind: {:?}",
                files_in(&t.0)
            );
            err
        };

        // The zip has the right checksum but tries to write outside its folder.
        let mock = Mock::start();
        mock.publish(
            "0.2.0",
            &zip_of(&[("crossc-0.2.0-x64/a.txt", b"a"), ("../evil.exe", b"evil")]),
            None,
        );
        assert!(attempt(&mock).contains("\"..\" part"));

        // The zip is for another version than its name says (its folder is not crossc-0.2.0-x64).
        let mock = Mock::start();
        mock.publish("0.2.0", &bundle_zip("crossc-0.1.0-x64"), None);
        assert!(attempt(&mock).contains("is not inside crossc-0.2.0-x64/"));

        // The checksum file is an error page.
        let mock = Mock::start();
        mock.publish("0.2.0", &bundle_zip(TOP), None);
        mock.route(
            "/github-production-release-asset/1/12?sig=SIGNATURE",
            Route::Ok(b"<html>oops</html>".to_vec()),
        );
        assert!(attempt(&mock).contains("is not \"<sha256>  <file name>\""));

        // The asset redirects off the allow-list (here: to a host that is not the file host).
        let mock = Mock::start();
        mock.publish("0.2.0", &bundle_zip(TOP), None);
        mock.route(
            "/repos/mrquantumoff/crossc/releases/assets/11",
            Route::Redirect("http://localhost:1/evil.zip".into()),
        );
        assert!(attempt(&mock).contains("localhost is not one of the GitHub hosts"));

        // The file host fails.
        let mock = Mock::start();
        mock.publish("0.2.0", &bundle_zip(TOP), None);
        mock.route(
            "/github-production-release-asset/1/11?sig=SIGNATURE",
            Route::Status(503, "busy"),
        );
        assert_eq!(attempt(&mock), "GitHub answered HTTP 503");

        // GitHub reports another digest for the asset than SHA256SUMS.txt lists: not even downloaded.
        let mock = Mock::start();
        mock.publish("0.2.0", &bundle_zip(TOP), None);
        let source = mock.source();
        let mut release = found(check_latest(&source, &anonymous, None));
        release.assets.as_mut().unwrap().zip.sha256 = Some("cd".repeat(32));
        let before = mock.seen().len();
        let err =
            download_release(&source, &anonymous, &release, &releases, &|_, _| {}).unwrap_err();
        assert!(
            err.contains("GitHub reports") && err.contains(&"cd".repeat(32)),
            "{err}"
        );
        // Only the checksum file was fetched (two requests: the API address and its redirect).
        assert_eq!(mock.seen().len() - before, 2);

        // A tag that is not a release tag never becomes a folder name.
        let mut release = found(check_latest(&source, &anonymous, None));
        release.tag = r"v0.2.0\..\..\evil".into();
        let err =
            download_release(&source, &anonymous, &release, &releases, &|_, _| {}).unwrap_err();
        assert!(err.contains("is not a release tag"), "{err}");
        assert!(files_in(&t.0).is_empty());
    }

    #[test]
    fn end_to_end_check_outcomes() {
        // A private repository asked anonymously: GitHub says 404, the installer says what to do.
        let mock = Mock::start();
        let latest = check_latest(&mock.source(), &Auth::anonymous(), None);
        let Latest::Failed(why) = &latest else {
            panic!("{latest:?}");
        };
        assert!(
            why.contains("mrquantumoff/crossc was not found (HTTP 404)"),
            "{why}"
        );
        assert!(
            why.contains("If the repository is private") && why.contains("GITHUB_TOKEN"),
            "{why}"
        );
        assert!(why.ends_with("(anonymous)"), "{why}");
        // With a token that cannot see it, and with gh passed over in an elevated window.
        let latest = check_latest(&mock.source(), &auth_with("ghp_x"), None);
        assert!(
            matches!(&latest, Latest::Failed(why) if why.contains("the token has no access to it")),
            "{latest:?}"
        );
        let passed_over = Auth {
            note: Some("gh.exe was not asked".into()),
            ..Auth::anonymous()
        };
        let latest = check_latest(&mock.source(), &passed_over, None);
        assert!(
            matches!(&latest, Latest::Failed(why) if why.ends_with("(anonymous; gh.exe was not asked)")),
            "{latest:?}"
        );

        // A repository without releases.
        mock.route(
            "/repos/mrquantumoff/crossc/releases?per_page=20",
            Route::Ok(b"[]".to_vec()),
        );
        assert_eq!(
            check_latest(&mock.source(), &Auth::anonymous(), None),
            Latest::NoReleases {
                repo: "mrquantumoff/crossc".into(),
                auth: "anonymous".into(),
                skipped: vec![],
            }
        );
        // A release whose tag the release workflow lets through and this installer does not read: it is not
        // offered, and it is not passed over in silence either.
        let odd = format!(
            "[{}]",
            crate::release::tests::release_json("v0.2.0-rc.01", false, true, &[])
        );
        mock.route(
            "/repos/mrquantumoff/crossc/releases?per_page=20",
            Route::Ok(odd.into_bytes()),
        );
        let latest = check_latest(&mock.source(), &Auth::anonymous(), None);
        assert_eq!(
            latest,
            Latest::NoReleases {
                repo: "mrquantumoff/crossc".into(),
                auth: "anonymous".into(),
                skipped: vec!["v0.2.0-rc.01".into()],
            }
        );
        assert!(
            latest
                .skipped_note()
                .is_some_and(|note| note.starts_with("ignored v0.2.0-rc.01: not a release tag")),
            "{:?}",
            latest.skipped_note()
        );
        // An answer that is not the release list.
        mock.route(
            "/repos/mrquantumoff/crossc/releases?per_page=20",
            Route::Ok(b"<html>proxy login</html>".to_vec()),
        );
        let latest = check_latest(&mock.source(), &Auth::anonymous(), None);
        assert!(
            matches!(&latest, Latest::Failed(why) if why.contains("not valid JSON")),
            "{latest:?}"
        );
        // Rate limited.
        mock.route(
            "/repos/mrquantumoff/crossc/releases?per_page=20",
            Route::Status(500, "{}"),
        );
        let latest = check_latest(&mock.source(), &Auth::anonymous(), None);
        assert!(
            matches!(&latest, Latest::Failed(why) if why.starts_with("GitHub answered HTTP 500")),
            "{latest:?}"
        );

        // Nobody listening: a failure with a reason, soon.
        let dead = Source {
            api_base: "http://127.0.0.1:9".into(),
            ..mock.source()
        };
        let started = Instant::now();
        let latest = check_latest(&dead, &Auth::anonymous(), None);
        assert!(
            matches!(&latest, Latest::Failed(why) if why.contains("cannot reach 127.0.0.1")),
            "{latest:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(25),
            "{:?}",
            started.elapsed()
        );

        // The real source insists on a usable CROSSC_REPO and on GitHub over HTTPS.
        assert!(Source::github(Some("not a repo")).is_err());
        let github = Source::github(None).unwrap();
        assert_eq!(
            (github.api_base.as_str(), github.repo.as_str()),
            ("https://api.github.com", "mrquantumoff/crossc")
        );
        assert_eq!(github.policy, Policy::github());
        assert_eq!(
            Source::github(Some("someone/fork")).unwrap().repo,
            "someone/fork"
        );
    }
}
