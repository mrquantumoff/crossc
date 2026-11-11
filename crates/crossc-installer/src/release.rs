//! Pure rules for "install the latest release from GitHub": versions, the release list, which release and which
//! assets, `SHA256SUMS.txt`, which hosts may be talked to and how a redirect is followed, where a token comes
//! from, and which paths a zip entry may be unpacked to. Nothing here touches the network, the file system or
//! Windows, so every rule is unit-tested; `update` and `http` do the I/O.

use std::cmp::Ordering;
use std::fmt;

/// The repository whose releases are installed.
pub const DEFAULT_REPO: &str = "mrquantumoff/crossc";
/// Environment variable that names another repository (`owner/name`), for forks.
pub const REPO_ENV: &str = "CROSSC_REPO";
/// Environment variables a token is read from, in this order, before `gh auth token` is asked.
pub const TOKEN_ENVS: [&str; 2] = ["GITHUB_TOKEN", "GH_TOKEN"];
/// The checksum file every release carries next to its zips.
pub const SUMS_NAME: &str = "SHA256SUMS.txt";

/// Largest release zip that is downloaded.
pub const MAX_ZIP_BYTES: u64 = 200 * 1024 * 1024;
/// Largest `SHA256SUMS.txt` that is downloaded.
pub const MAX_SUMS_BYTES: u64 = 64 * 1024;
/// Largest release list (JSON) that is read.
pub const MAX_JSON_BYTES: u64 = 4 * 1024 * 1024;
/// Most bytes a release zip may unpack to, all entries together.
pub const MAX_UNPACKED_BYTES: u64 = 512 * 1024 * 1024;
/// Most entries a release zip may have.
pub const MAX_ENTRIES: usize = 2000;
/// Most redirects followed for one download.
pub const MAX_REDIRECTS: usize = 5;

const API_HOST: &str = "api.github.com";
/// Every host a request may go to. `api.github.com` answers the release list and the asset requests; an asset
/// request is redirected to a signed URL on `release-assets.githubusercontent.com` (seen 2026-11 for both
/// `api.github.com/.../releases/assets/<id>` and `github.com/.../releases/download/...`), which replaced
/// `objects.githubusercontent.com` in 2025; GitHub's documentation still lists both.
const GITHUB_HOSTS: [&str; 4] = [
    API_HOST,
    "github.com",
    "release-assets.githubusercontent.com",
    "objects.githubusercontent.com",
];

// ---------------------------------------------------------------------------------------------------------------
// Versions
// ---------------------------------------------------------------------------------------------------------------

/// One dot-separated part of a prerelease suffix.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ident {
    Num(u64),
    Text(String),
}

impl Ord for Ident {
    /// Semantic Versioning 2.0.0, rule 11.4: numeric parts compare as numbers and sort before text parts, text
    /// parts compare in ASCII order.
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Ident::Num(a), Ident::Num(b)) => a.cmp(b),
            (Ident::Num(_), Ident::Text(_)) => Ordering::Less,
            (Ident::Text(_), Ident::Num(_)) => Ordering::Greater,
            (Ident::Text(a), Ident::Text(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for Ident {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A semantic version: `0.2.0`, `0.2.0-rc.1`. Build metadata (`+...`) is accepted and dropped: it does not take
/// part in ordering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    pre: Vec<Ident>,
}

impl Version {
    /// `1.2.3`, `1.2.3-rc.1`, either with a leading `v`. `None` for anything else (`dev-abc1234`, `1.2`).
    pub fn parse(text: &str) -> Option<Version> {
        let text = text.strip_prefix('v').unwrap_or(text);
        let text = text.split_once('+').map_or(text, |(v, _build)| v);
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (text, None),
        };
        let mut numbers = core.split('.').map(number);
        let (major, minor, patch) = (numbers.next()??, numbers.next()??, numbers.next()??);
        if numbers.next().is_some() {
            return None;
        }
        let pre = match pre {
            None => Vec::new(),
            Some(pre) => pre.split('.').map(ident).collect::<Option<Vec<_>>>()?,
        };
        Some(Version {
            major,
            minor,
            patch,
            pre,
        })
    }

    /// A release tag: `v` followed by a version in its one canonical spelling (what the release workflow accepts:
    /// `vMAJOR.MINOR.PATCH` or `vMAJOR.MINOR.PATCH-suffix`). A tag that passes consists of `v`, ASCII letters,
    /// digits, `.` and `-`, has no empty part and does not end in a dot, so it is safe as a folder name.
    pub fn from_tag(tag: &str) -> Option<Version> {
        let version = Version::parse(tag.strip_prefix('v')?)?;
        (format!("v{version}") == tag).then_some(version)
    }

    /// The tag of this version's release.
    pub fn tag(&self) -> String {
        format!("v{self}")
    }

    /// Has a `-suffix`. This is the only definition of "prerelease" the installer uses: GitHub's own flag is set
    /// on every 0.x release by the release workflow, so it cannot tell `0.2.0` from `0.2.0-rc.1`.
    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }
}

fn number(text: &str) -> Option<u64> {
    (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

fn ident(text: &str) -> Option<Ident> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return None;
    }
    Some(match number(text) {
        Some(n) => Ident::Num(n),
        // All digits but too long for a number: keep it as text rather than refuse the version.
        None => Ident::Text(text.to_string()),
    })
}

impl Ord for Version {
    /// Semantic Versioning 2.0.0, rule 11: the numbers first; a version without a suffix is newer than the same
    /// numbers with one; suffixes compare part by part, and of two that agree as far as the shorter goes the
    /// longer is newer.
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => self.pre.cmp(&other.pre),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        for (i, part) in self.pre.iter().enumerate() {
            f.write_str(if i == 0 { "-" } else { "." })?;
            match part {
                Ident::Num(n) => write!(f, "{n}")?,
                Ident::Text(t) => f.write_str(t)?,
            }
        }
        Ok(())
    }
}

/// The version of an unpacked bundle, from the first of these that holds one:
///
/// 1. `version.txt` in the bundle folder: its first non-empty line (`0.2.0` or `v0.2.0`);
/// 2. the first line of `BUILD-INFO.txt`, `crossc <version> (x64)`;
/// 3. the folder's name, `crossc-<version>-x64` (how the zip unpacks) or `v<version>` (a release this installer
///    downloaded).
///
/// `None` for a development bundle (`dev-<commit>`) and for a folder that was renamed and has neither file.
pub fn bundle_version(
    version_txt: Option<&str>,
    build_info: Option<&str>,
    folder: Option<&str>,
) -> Option<Version> {
    let from_file = version_txt
        .and_then(|text| text.lines().map(str::trim).find(|l| !l.is_empty()))
        .and_then(Version::parse);
    let from_build_info = || {
        let mut words = build_info?.lines().next()?.split_whitespace();
        (words.next()? == "crossc")
            .then(|| words.next().and_then(Version::parse))
            .flatten()
    };
    let from_folder = || {
        let name = folder?;
        let lower = name.to_ascii_lowercase();
        match lower
            .strip_prefix("crossc-")
            .and_then(|rest| rest.strip_suffix("-x64"))
        {
            Some(inner) => Version::parse(&name[7..7 + inner.len()]),
            None => Version::from_tag(name),
        }
    };
    from_file.or_else(from_build_info).or_else(from_folder)
}

// ---------------------------------------------------------------------------------------------------------------
// The repository
// ---------------------------------------------------------------------------------------------------------------

/// The repository to ask: `CROSSC_REPO` when it is set (it must be `owner/name`), else [`DEFAULT_REPO`].
pub fn repo_slug(env_value: Option<&str>) -> Result<String, String> {
    let Some(value) = env_value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(DEFAULT_REPO.to_string());
    };
    let part_ok = |p: &str| {
        !p.is_empty()
            && p.len() <= 100
            && p != "."
            && p != ".."
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    match value.split_once('/') {
        Some((owner, name)) if part_ok(owner) && part_ok(name) => Ok(value.to_string()),
        _ => Err(format!(
            "{REPO_ENV} is \"{value}\", which is not a GitHub repository in the form owner/name"
        )),
    }
}

/// The request that lists a repository's newest releases.
pub fn releases_url(api_base: &str, repo: &str) -> String {
    format!("{api_base}/repos/{repo}/releases?per_page=20")
}

// ---------------------------------------------------------------------------------------------------------------
// The release list
// ---------------------------------------------------------------------------------------------------------------

/// One file attached to a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    /// `https://api.github.com/repos/<owner>/<name>/releases/assets/<id>`. Asked for with
    /// `Accept: application/octet-stream` it redirects to the file, in a public repository and (with a token) in
    /// a private one, where the `github.com/.../releases/download/...` address does not work.
    pub api_url: String,
    pub size: u64,
    /// The file's SHA-256 as GitHub reports it (lower-case hex), when it does.
    pub sha256: Option<String>,
}

/// One entry of the release list whose tag is a release tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    pub version: Version,
    pub draft: bool,
    pub assets: Vec<Asset>,
}

/// Reads the answer to [`releases_url`]. Entries whose tag is not a release tag (`nightly`, `V1.0.0`, `v1.0`)
/// are left out, and so are assets that are not fully uploaded. An answer that is not a list is an error; if it
/// is GitHub's error object, the error is its message.
pub fn parse_releases(json: &str) -> Result<Vec<Release>, String> {
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| format!("the release list is not valid JSON ({e})"))?;
    let Some(list) = value.as_array() else {
        return Err(match api_message(json) {
            Some(message) => format!("GitHub answered: {message}"),
            None => "the release list is not a JSON array".into(),
        });
    };
    let mut releases = Vec::new();
    for entry in list {
        let Some(tag) = entry.get("tag_name").and_then(|t| t.as_str()) else {
            continue;
        };
        let Some(version) = Version::from_tag(tag) else {
            continue;
        };
        let assets = entry
            .get("assets")
            .and_then(|a| a.as_array())
            .map(|assets| assets.iter().filter_map(asset).collect())
            .unwrap_or_default();
        releases.push(Release {
            tag: tag.to_string(),
            version,
            draft: entry
                .get("draft")
                .and_then(|d| d.as_bool())
                .unwrap_or(false),
            assets,
        });
    }
    Ok(releases)
}

/// The most skipped tags that are reported, and the most characters of each.
const MAX_SKIPPED: usize = 5;
const MAX_SKIPPED_LEN: usize = 40;

/// Tags of published (non-draft) releases in the answer to [`releases_url`] that [`parse_releases`] leaves out
/// although they are meant as versions: they start like one (`v1`, `V1`, `1`) and are not a release tag
/// ([`Version::from_tag`]). The release workflow's tag rule lets some of these through (`v0.2.0-rc.01`,
/// `v01.2.3`: leading zeros; `v0.2.0-rc..1`, `v0.2.0-`: empty parts), and a release published under such a tag
/// would otherwise never be seen here, with no word about why.
///
/// Tags that are no attempt at a version (`nightly`, `latest`) are not reported. The result is for display:
/// each tag once, control and non-ASCII characters escaped, cut to 40 characters, at most 5 of them. An answer
/// that is not a list gives none (`parse_releases` reports that).
pub fn skipped_tags(json: &str) -> Vec<String> {
    let Ok(serde_json::Value::Array(list)) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let mut skipped: Vec<String> = Vec::new();
    for entry in &list {
        let Some(tag) = entry.get("tag_name").and_then(|t| t.as_str()) else {
            continue;
        };
        let draft = entry
            .get("draft")
            .and_then(|d| d.as_bool())
            .unwrap_or(false);
        let version_like = tag
            .strip_prefix(['v', 'V'])
            .unwrap_or(tag)
            .starts_with(|c: char| c.is_ascii_digit());
        if draft || !version_like || Version::from_tag(tag).is_some() {
            continue;
        }
        let mut shown: String = tag.escape_default().take(MAX_SKIPPED_LEN).collect();
        if tag.escape_default().count() > MAX_SKIPPED_LEN {
            shown.push_str("...");
        }
        if !skipped.contains(&shown) && skipped.len() < MAX_SKIPPED {
            skipped.push(shown);
        }
    }
    skipped
}

fn asset(value: &serde_json::Value) -> Option<Asset> {
    let text = |key: &str| value.get(key).and_then(|v| v.as_str());
    // "uploaded" is the only state a finished asset has; an entry without a state is taken as finished.
    if text("state").is_some_and(|s| s != "uploaded") {
        return None;
    }
    Some(Asset {
        name: text("name")?.to_string(),
        api_url: text("url")?.to_string(),
        size: value.get("size")?.as_u64()?,
        sha256: text("digest")
            .and_then(|d| d.strip_prefix("sha256:"))
            .filter(|hex| is_sha256_hex(hex))
            .map(str::to_ascii_lowercase),
    })
}

/// The `message` of a GitHub API error body (`{"message": "Not Found", ...}`).
pub fn api_message(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let message = value.get("message")?.as_str()?.trim();
    (!message.is_empty()).then(|| message.chars().take(200).collect())
}

/// The release to offer: the highest version among the published (non-draft) releases.
///
/// Prereleases (a tag with a `-suffix`, see [`Version::is_prerelease`]) only count when there is no other
/// release, or when the installed version is a prerelease itself (someone testing release candidates keeps
/// getting them).
pub fn pick_release<'a>(
    releases: &'a [Release],
    installed: Option<&Version>,
) -> Option<&'a Release> {
    let published = || releases.iter().filter(|r| !r.draft);
    let any_stable = published().any(|r| !r.version.is_prerelease());
    let with_prereleases = !any_stable || installed.is_some_and(Version::is_prerelease);
    published()
        .filter(|r| with_prereleases || !r.version.is_prerelease())
        .max_by(|a, b| a.version.cmp(&b.version))
}

/// The two assets an install needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseAssets {
    pub zip: Asset,
    pub sums: Asset,
}

/// Name of the x64 bundle of a version, as tools/package.ps1 writes it. The zip holds one folder of the same
/// name without `.zip`.
pub fn zip_asset_name(version: &Version) -> String {
    format!("crossc-{version}-x64.zip")
}

/// Finds `crossc-<version>-x64.zip` and `SHA256SUMS.txt` among a release's assets, by their exact names, and
/// refuses sizes over the download caps.
pub fn pick_assets(release: &Release) -> Result<ReleaseAssets, String> {
    let find = |name: &str, cap: u64| -> Result<Asset, String> {
        let asset = release
            .assets
            .iter()
            .find(|a| a.name == name)
            .ok_or_else(|| format!("release {} has no {name} asset", release.tag))?;
        if asset.size > cap {
            return Err(format!(
                "{name} of release {} is {}, more than the {} this installer downloads",
                release.tag,
                size_text(asset.size),
                size_text(cap)
            ));
        }
        Ok(asset.clone())
    };
    Ok(ReleaseAssets {
        zip: find(&zip_asset_name(&release.version), MAX_ZIP_BYTES)?,
        sums: find(SUMS_NAME, MAX_SUMS_BYTES)?,
    })
}

/// The newest release and what can be downloaded of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestRelease {
    pub tag: String,
    pub version: Version,
    /// `Err`: why this release cannot be installed from here (an asset is missing or too large).
    pub assets: Result<ReleaseAssets, String>,
}

/// From the API's answer to the release to offer; `Ok(None)` when nothing is published.
pub fn latest_release(
    json: &str,
    installed: Option<&Version>,
) -> Result<Option<LatestRelease>, String> {
    let releases = parse_releases(json)?;
    Ok(pick_release(&releases, installed).map(|r| LatestRelease {
        tag: r.tag.clone(),
        version: r.version.clone(),
        assets: pick_assets(r),
    }))
}

/// `1.5 MB`, `12 KB`, `300 bytes`.
pub fn size_text(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    match bytes {
        b if b >= MB => format!("{:.1} MB", b as f64 / MB as f64),
        b if b >= KB => format!("{} KB", b / KB),
        b => format!("{b} bytes"),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// SHA256SUMS.txt
// ---------------------------------------------------------------------------------------------------------------

fn is_sha256_hex(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The SHA-256 (lower-case hex) that `SHA256SUMS.txt` lists for exactly `file_name`.
///
/// The file is in `sha256sum` format, one `<64 hex digits>  <name>` per line (`*` instead of the second space
/// marks binary mode). Any other non-empty line is an error, and so are two different checksums for the name:
/// a file this installer does not fully understand is not one to trust a download by.
pub fn expected_sha256(sums: &str, file_name: &str) -> Result<String, String> {
    let mut found: Option<String> = None;
    for (number, line) in sums.trim_start_matches('\u{feff}').lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let bad = || {
            format!(
                "line {} of {SUMS_NAME} is not \"<sha256>  <file name>\"",
                number + 1
            )
        };
        let (hash, rest) = line.split_at_checked(64).ok_or_else(bad)?;
        let name = rest
            .strip_prefix("  ")
            .or_else(|| rest.strip_prefix(" *"))
            .ok_or_else(bad)?;
        if !is_sha256_hex(hash) || name.is_empty() {
            return Err(bad());
        }
        if name != file_name {
            continue;
        }
        let hash = hash.to_ascii_lowercase();
        match &found {
            Some(earlier) if *earlier != hash => {
                return Err(format!(
                    "{SUMS_NAME} lists two different checksums for {file_name}"
                ));
            }
            _ => found = Some(hash),
        }
    }
    found.ok_or_else(|| format!("{SUMS_NAME} has no entry for {file_name}"))
}

/// `Ok` when the SHA-256 computed over the downloaded bytes is the expected one (hex, compared without case).
pub fn check_sha256(what: &str, expected: &str, actual: &str) -> Result<(), String> {
    if is_sha256_hex(actual) && expected.eq_ignore_ascii_case(actual) {
        Ok(())
    } else {
        Err(format!(
            "SHA-256 mismatch for {what}: expected {}, the download is {}. Nothing was unpacked",
            expected.to_ascii_lowercase(),
            actual.to_ascii_lowercase()
        ))
    }
}

// ---------------------------------------------------------------------------------------------------------------
// URLs, the host allow-list and redirects
// ---------------------------------------------------------------------------------------------------------------

/// An absolute http(s) URL, taken apart as far as a request needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub secure: bool,
    /// Lower case.
    pub host: String,
    pub port: u16,
    /// Path and query, starting with `/`.
    pub path: String,
}

impl Url {
    /// Accepts `http://` and `https://` URLs with a plain host name or IPv4 address. Refuses user names, IPv6
    /// literals, backslashes, spaces and control characters; a fragment is dropped.
    pub fn parse(text: &str) -> Result<Url, String> {
        let bad = |why: &str| format!("not a usable URL ({why})");
        if text.len() > 8192 {
            return Err(bad("too long"));
        }
        if text
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
        {
            return Err(bad("it has a space, a backslash or a control character"));
        }
        let (scheme, rest) = text.split_once("://").ok_or_else(|| bad("no scheme"))?;
        let secure = match scheme.to_ascii_lowercase().as_str() {
            "https" => true,
            "http" => false,
            _ => return Err(bad("the scheme is not http or https")),
        };
        let rest = rest
            .split_once('#')
            .map_or(rest, |(before, _fragment)| before);
        let end = rest.find(['/', '?']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(end);
        if authority.contains(['@', '[', ']']) {
            return Err(bad("user names and IPv6 addresses are not accepted"));
        }
        let (host, port) = match authority.split_once(':') {
            Some((host, port)) => {
                let port = number(port)
                    .and_then(|p| u16::try_from(p).ok())
                    .filter(|p| *p != 0)
                    .ok_or_else(|| bad("the port is not a number"))?;
                (host, port)
            }
            None => (authority, if secure { 443 } else { 80 }),
        };
        let host = host.to_ascii_lowercase();
        let label_ok = |l: &str| {
            !l.is_empty()
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        };
        if host.is_empty() || host.len() > 253 || !host.split('.').all(label_ok) {
            return Err(bad("the host name is not valid"));
        }
        let path = match tail {
            "" => "/".to_string(),
            t if t.starts_with('?') => format!("/{t}"),
            t => t.to_string(),
        };
        Ok(Url {
            secure,
            host,
            port,
            path,
        })
    }

    /// `https://host/path` without the query: what may appear in the log. The query of a redirect target is a
    /// signature that grants access to the file, so it is never shown.
    pub fn redacted(&self) -> String {
        let path = self.path.split('?').next().unwrap_or("/");
        format!("{}{path}", self.origin())
    }

    fn origin(&self) -> String {
        let scheme = if self.secure { "https" } else { "http" };
        let default_port = if self.secure { 443 } else { 80 };
        if self.port == default_port {
            format!("{scheme}://{}", self.host)
        } else {
            format!("{scheme}://{}:{}", self.host, self.port)
        }
    }
}

/// Which servers a request may go to, and which of them get the token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    hosts: Vec<String>,
    /// Hosts the `Authorization` header is sent to. Never the host a download is redirected to: the redirect
    /// carries its own signature, and the token is no business of a file server.
    auth_hosts: Vec<String>,
    /// Tests only: allow `http://` and any port, for a mock server on localhost.
    #[cfg(test)]
    plain_http: bool,
}

impl Policy {
    /// GitHub over HTTPS on port 443, nothing else.
    pub fn github() -> Policy {
        Policy {
            hosts: GITHUB_HOSTS.iter().map(|h| h.to_string()).collect(),
            auth_hosts: vec![API_HOST.to_string()],
            #[cfg(test)]
            plain_http: false,
        }
    }

    /// A mock server: `api_host` is the API (and gets the token), `other_hosts` are redirect targets.
    #[cfg(test)]
    pub fn local_test(api_host: &str, other_hosts: &[&str]) -> Policy {
        let mut hosts = vec![api_host.to_string()];
        hosts.extend(other_hosts.iter().map(|h| h.to_string()));
        Policy {
            hosts,
            auth_hosts: vec![api_host.to_string()],
            plain_http: true,
        }
    }

    fn plain_http(&self) -> bool {
        #[cfg(test)]
        {
            self.plain_http
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    /// `Ok` when a request to `url` is allowed: HTTPS on port 443 to a host on the list.
    pub fn check(&self, url: &Url) -> Result<(), String> {
        if !self.plain_http() {
            if !url.secure {
                return Err(format!("refusing {}: only HTTPS is used", url.redacted()));
            }
            if url.port != 443 {
                return Err(format!("refusing {}: not the HTTPS port", url.redacted()));
            }
        }
        if self.hosts.contains(&url.host) {
            Ok(())
        } else {
            Err(format!(
                "refusing {}: {} is not one of the GitHub hosts this installer talks to ({})",
                url.redacted(),
                url.host,
                self.hosts.join(", ")
            ))
        }
    }

    /// The token may be sent with a request to `url`.
    pub fn sends_token_to(&self, url: &Url) -> bool {
        self.check(url).is_ok() && self.auth_hosts.contains(&url.host)
    }
}

/// What to do with a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// 200: read the body.
    Body,
    /// A redirect to an allowed URL: ask there.
    Redirect(Url),
}

/// Decides how to go on after a response to a request for `current`: `followed` redirects have been followed
/// already. A redirect is only followed to a URL that passes [`Policy::check`] itself (so HTTPS never turns
/// into HTTP and no hop leaves the allow-list), and only [`MAX_REDIRECTS`] times. Every status other than 200
/// and the redirects is `Err(None)`; a redirect that is not followed is `Err(Some(reason))`.
pub fn next_hop(
    policy: &Policy,
    current: &Url,
    status: u32,
    location: Option<&str>,
    followed: usize,
) -> Result<Next, Option<String>> {
    match status {
        200 => Ok(Next::Body),
        301 | 302 | 303 | 307 | 308 => {
            if followed >= MAX_REDIRECTS {
                return Err(Some(format!(
                    "more than {MAX_REDIRECTS} redirects, the last from {}",
                    current.redacted()
                )));
            }
            let location = location
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .ok_or_else(|| {
                    Some(format!(
                        "{} redirects (HTTP {status}) without saying where to",
                        current.redacted()
                    ))
                })?;
            let target = if location.starts_with('/') && !location.starts_with("//") {
                // The same server, another path.
                let path = location.split_once('#').map_or(location, |(p, _)| p);
                Url::parse(&format!("{}{path}", current.origin()))
            } else {
                Url::parse(location)
            }
            .map_err(|e| {
                Some(format!(
                    "{} redirects to something that is {e}",
                    current.redacted()
                ))
            })?;
            policy.check(&target).map_err(Some)?;
            Ok(Next::Redirect(target))
        }
        _ => Err(None),
    }
}

/// Media type asked for when a release asset's bytes are wanted from the API.
pub const ACCEPT_BINARY: &str = "application/octet-stream";
/// Media type of the release list.
pub const ACCEPT_JSON: &str = "application/vnd.github+json";

/// The headers of one request. `Authorization` goes only to the hosts the policy names (`api.github.com`), so a
/// redirect to the file server is asked without it.
pub fn request_headers(
    policy: &Policy,
    url: &Url,
    accept: &str,
    token: Option<&Token>,
) -> Vec<(&'static str, String)> {
    let mut headers = vec![("Accept", accept.to_string())];
    if policy.sends_token_to(url) {
        headers.push(("X-GitHub-Api-Version", "2022-11-28".to_string()));
        if let Some(token) = token {
            headers.push(("Authorization", format!("Bearer {}", token.0)));
        }
    }
    headers
}

/// What to tell the user when GitHub does not answer 200. `authenticated`: a token was sent. `remaining`: the
/// `X-RateLimit-Remaining` header. `message`: the `message` of the error body.
pub fn http_failure(
    status: u32,
    repo: &str,
    authenticated: bool,
    remaining: Option<&str>,
    message: Option<&str>,
) -> String {
    let token_hint = format!(
        "set {} or {} to a token that can read it, or sign in with the GitHub CLI (gh auth login)",
        TOKEN_ENVS[0], TOKEN_ENVS[1]
    );
    match status {
        401 => "GitHub rejected the token (HTTP 401): it is invalid or has expired".into(),
        407 => "the proxy between this PC and GitHub asks for a login (HTTP 407), which this installer does \
                not do"
            .into(),
        403 | 429 if remaining.map(str::trim) == Some("0") => {
            if authenticated {
                format!(
                    "GitHub's request limit for this token is used up (HTTP {status}): try again later"
                )
            } else {
                format!(
                    "GitHub's request limit for anonymous requests from this address is used up (HTTP \
                     {status}): try again in an hour, or {token_hint}"
                )
            }
        }
        404 if authenticated => format!(
            "{repo} was not found (HTTP 404): it does not exist, or the token has no access to it"
        ),
        404 => {
            format!("{repo} was not found (HTTP 404). If the repository is private, {token_hint}")
        }
        _ => match message {
            Some(message) => format!("GitHub answered HTTP {status}: {message}"),
            None => format!("GitHub answered HTTP {status}"),
        },
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The token
// ---------------------------------------------------------------------------------------------------------------

/// A GitHub token. It is only ever put into an `Authorization` header; `Debug` does not show it and there is no
/// other way to read it, so it cannot end up in the log.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    /// The trimmed text when it can be a token: not empty, printable ASCII, no space (so it cannot break out of
    /// a header line).
    pub fn new(raw: &str) -> Option<Token> {
        let text = raw.trim();
        (!text.is_empty() && text.len() <= 1024 && text.bytes().all(|b| b.is_ascii_graphic()))
            .then(|| Token(text.to_string()))
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(hidden)")
    }
}

/// Where the token came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    /// One of [`TOKEN_ENVS`].
    Env(&'static str),
    /// `gh auth token`.
    GhCli,
}

impl fmt::Display for TokenSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenSource::Env(name) => write!(f, "token from {name}"),
            TokenSource::GhCli => f.write_str("token from gh auth token"),
        }
    }
}

/// How a request is authenticated, for the status line: `anonymous` or where the token is from.
pub fn auth_text(source: Option<TokenSource>) -> String {
    source.map_or("anonymous".to_string(), |s| s.to_string())
}

/// Looks for a token: `GITHUB_TOKEN`, then `GH_TOKEN`, then whatever `gh` returns (`gh auth token`, only asked
/// when neither variable holds a usable token). `None`: ask anonymously.
pub fn discover_token(
    env: &dyn Fn(&str) -> Option<String>,
    gh: &dyn Fn() -> Option<String>,
) -> Option<(Token, TokenSource)> {
    TOKEN_ENVS
        .iter()
        .find_map(|name| Some((Token::new(&env(name)?)?, TokenSource::Env(name))))
        .or_else(|| Some((Token::new(&gh()?)?, TokenSource::GhCli)))
}

// ---------------------------------------------------------------------------------------------------------------
// Unpacking
// ---------------------------------------------------------------------------------------------------------------

/// Where a zip entry goes, relative to the folder the release is unpacked to: the entry's path below `top` (the
/// one folder the zip must consist of, `crossc-<version>-x64`), as its parts. `Ok(None)` for the entry of `top`
/// itself.
///
/// Refused: an entry outside `top`; an absolute path; `.` and `..`; an empty part; a backslash, a colon (drive
/// letters, alternate data streams), other characters Windows does not allow in names, control characters and
/// non-ASCII; a part that ends in a dot or a space (Windows drops them, so the name written would not be the
/// name checked) or is a device name (`NUL`, `COM1`, also with an extension); more than 16 levels.
pub fn entry_path<'a>(name: &'a str, top: &str) -> Result<Option<Vec<&'a str>>, String> {
    let bad = |why: &str| format!("zip entry \"{}\" {why}", name.escape_default());
    if name
        .chars()
        .any(|c| !c.is_ascii() || c.is_ascii_control() || r#"\:<>"|?*"#.contains(c))
    {
        return Err(bad(
            "has a character that is not allowed in a file name here",
        ));
    }
    if name.starts_with('/') {
        return Err(bad("is an absolute path"));
    }
    let trimmed = name.strip_suffix('/').unwrap_or(name);
    let parts: Vec<&str> = trimmed.split('/').collect();
    if parts.len() > 16 {
        return Err(bad("is nested too deeply"));
    }
    for part in &parts {
        if part.is_empty() || *part == "." || *part == ".." {
            return Err(bad("has an empty, \".\" or \"..\" part"));
        }
        if part.ends_with(['.', ' ']) || part.starts_with(' ') {
            return Err(bad(
                "has a part that starts with a space or ends in a dot or a space",
            ));
        }
        if is_device_name(part) {
            return Err(bad("uses a Windows device name"));
        }
    }
    if parts[0] != top {
        return Err(bad(&format!("is not inside {top}/")));
    }
    Ok((parts.len() > 1).then(|| parts[1..].to_vec()))
}

fn is_device_name(part: &str) -> bool {
    let stem = part.split('.').next().unwrap_or(part).trim_end();
    let upper = stem.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || (upper.len() == 4
        && (upper.starts_with("COM") || upper.starts_with("LPT"))
        && upper.as_bytes()[3].is_ascii_digit())
}

/// The entries of the releases folder to delete after `keep` has been installed: everything else (older
/// releases, leftovers of an interrupted download), except what the running installer is started from.
pub fn releases_to_prune(
    entries: &[String],
    keep: &str,
    running_from: Option<&str>,
) -> Vec<String> {
    let same = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
    entries
        .iter()
        .filter(|e| !same(e, keep) && !running_from.is_some_and(|r| same(e, r)))
        .cloned()
        .collect()
}

/// How many bytes lie between two progress lines of a download: a tenth of the file, at least 1 MB; 8 MB when
/// the size is not known.
pub fn progress_step(total: Option<u64>) -> u64 {
    const MB: u64 = 1024 * 1024;
    total.map_or(8 * MB, |t| (t / 10).max(MB))
}

/// `12.0 MB of 48.1 MB (25%)`, or `12.0 MB` when the size is not known.
pub fn progress_text(done: u64, total: Option<u64>) -> String {
    match total.filter(|t| *t > 0) {
        Some(total) => format!(
            "{} of {} ({}%)",
            size_text(done),
            size_text(total),
            (done.min(total) * 100) / total
        ),
        None => size_text(done),
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::cell::Cell;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap_or_else(|| panic!("{text} should parse"))
    }

    // ---- versions --------------------------------------------------------------------------------------------

    #[test]
    fn versions_parse_and_print() {
        for text in [
            "0.1.0",
            "1.2.3",
            "10.20.30",
            "1.0.0-rc.1",
            "1.0.0-alpha",
            "1.0.0-0.3.7",
            "1.0.0-x-y.z",
        ] {
            assert_eq!(v(text).to_string(), text);
            assert_eq!(v(&format!("v{text}")).to_string(), text);
        }
        // Build metadata is dropped.
        assert_eq!(v("1.2.3+build.5").to_string(), "1.2.3");
        assert_eq!(v("1.2.3-rc.1+build.5"), v("1.2.3-rc.1"));
        for bad in [
            "",
            "v",
            "1",
            "1.2",
            "1.2.3.4",
            "1.2.x",
            "a.b.c",
            "1.2.3-",
            "1.2.3-rc..1",
            "1.2.3-rc.",
            "1.2.3-r c",
            "1.2.3-rc_1",
            "dev-abc1234",
            "-1.2.3",
            "1.-2.3",
            "1..3",
            " 1.2.3",
            "99999999999999999999.0.0",
            "1.2.3-é",
        ] {
            assert_eq!(Version::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn tags_are_canonical_versions_with_a_v() {
        assert_eq!(Version::from_tag("v0.1.0"), Some(v("0.1.0")));
        assert_eq!(Version::from_tag("v0.2.0-rc.1"), Some(v("0.2.0-rc.1")));
        assert_eq!(v("0.2.0-rc.1").tag(), "v0.2.0-rc.1");
        // No v, a capital V (git refs are case-sensitive), leading zeros, build metadata, junk: not release tags,
        // so none of them is ever used as a folder name.
        for bad in [
            "0.1.0",
            "V0.1.0",
            "v01.0.0",
            "v0.1.0+b1",
            "v0.1",
            "nightly",
            "v0.1.0-",
            "v0.1.0-a..b",
            "v0.1.0.",
            r"v0.1.0-..\x",
            "v0.1.0/x",
            "vv0.1.0",
            "v0.1.0-rc.01",
            "v0.1.0 ",
        ] {
            assert_eq!(Version::from_tag(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn versions_order_as_semver_says() {
        // The example chain from semver.org, rule 11.
        let chain = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
            "1.0.1",
            "1.1.0",
            "1.10.0",
            "2.0.0",
            "10.0.0",
        ];
        for pair in chain.windows(2) {
            assert!(v(pair[0]) < v(pair[1]), "{} < {}", pair[0], pair[1]);
            assert!(v(pair[1]) > v(pair[0]));
        }
        // Numbers are compared as numbers, not as text.
        assert!(v("0.9.0") < v("0.10.0"));
        assert!(v("1.0.0-rc.9") < v("1.0.0-rc.10"));
        // A number sorts before text in a suffix.
        assert!(v("1.0.0-1") < v("1.0.0-a"));
        assert_eq!(v("1.2.3").cmp(&v("v1.2.3")), Ordering::Equal);
        assert!(v("1.0.0-rc.1").is_prerelease());
        assert!(!v("0.1.0").is_prerelease());
    }

    #[test]
    fn bundle_version_sources_in_order() {
        let build_info = "crossc 0.2.0 (x64)\n\ncommit    : abc\n";
        // version.txt wins, then BUILD-INFO.txt, then the folder name.
        assert_eq!(
            bundle_version(Some("0.3.0\n"), Some(build_info), Some("crossc-0.1.0-x64")),
            Some(v("0.3.0"))
        );
        assert_eq!(
            bundle_version(Some("\n  v0.3.0-rc.1  \n"), None, None),
            Some(v("0.3.0-rc.1"))
        );
        assert_eq!(
            bundle_version(None, Some(build_info), Some("crossc-0.1.0-x64")),
            Some(v("0.2.0"))
        );
        assert_eq!(
            bundle_version(None, None, Some("crossc-0.1.0-x64")),
            Some(v("0.1.0"))
        );
        assert_eq!(
            bundle_version(None, None, Some("CROSSC-0.1.0-rc.2-X64")),
            Some(v("0.1.0-rc.2"))
        );
        assert_eq!(bundle_version(None, None, Some("v0.4.0")), Some(v("0.4.0")));
        // A file that does not hold a version does not hide the next source.
        assert_eq!(
            bundle_version(Some("unknown"), Some(build_info), None),
            Some(v("0.2.0"))
        );
        assert_eq!(
            bundle_version(
                Some(""),
                Some("something else 1.0.0"),
                Some("crossc-0.1.0-x64")
            ),
            Some(v("0.1.0"))
        );
        // A development bundle, a renamed folder, nothing at all.
        assert_eq!(
            bundle_version(
                None,
                Some("crossc dev-abc1234 (x64)"),
                Some("crossc-dev-abc1234-x64")
            ),
            None
        );
        assert_eq!(bundle_version(None, None, Some("Downloads")), None);
        assert_eq!(bundle_version(None, None, Some("0.1.0")), None);
        assert_eq!(bundle_version(None, None, None), None);
    }

    // ---- the repository --------------------------------------------------------------------------------------

    #[test]
    fn repo_comes_from_the_environment_or_the_default() {
        assert_eq!(repo_slug(None).unwrap(), DEFAULT_REPO);
        assert_eq!(repo_slug(Some("")).unwrap(), DEFAULT_REPO);
        assert_eq!(repo_slug(Some("  ")).unwrap(), DEFAULT_REPO);
        assert_eq!(
            repo_slug(Some("someone/crossc-fork")).unwrap(),
            "someone/crossc-fork"
        );
        assert_eq!(repo_slug(Some(" a.b/c_d ")).unwrap(), "a.b/c_d");
        for bad in [
            "crossc",
            "a/b/c",
            "/b",
            "a/",
            "a/..",
            "../b",
            "a b/c",
            "a/b?x=1",
            "https://github.com/a/b",
            "a/b#c",
            "a\\b",
            "a/b\r\nHost: evil",
        ] {
            let err = repo_slug(Some(bad)).unwrap_err();
            assert!(
                err.contains("CROSSC_REPO") && err.contains("owner/name"),
                "{err}"
            );
        }
        assert_eq!(
            releases_url("https://api.github.com", DEFAULT_REPO),
            "https://api.github.com/repos/mrquantumoff/crossc/releases?per_page=20"
        );
    }

    // ---- the release list ------------------------------------------------------------------------------------

    /// One asset the way the API lists it (fields as captured from api.github.com in 2026-11, the uploader object
    /// shortened).
    pub fn asset_json(api: &str, id: u64, name: &str, size: u64, digest: Option<&str>) -> String {
        let digest = digest.map_or("null".to_string(), |d| format!("\"sha256:{d}\""));
        format!(
            r#"{{
      "url": "{api}/repos/mrquantumoff/crossc/releases/assets/{id}",
      "id": {id},
      "node_id": "RA_kwDODKw3uc4oe7N8",
      "name": "{name}",
      "label": "",
      "uploader": {{ "login": "github-actions[bot]", "id": 41898282, "type": "Bot", "site_admin": false }},
      "content_type": "application/zip",
      "state": "uploaded",
      "size": {size},
      "digest": {digest},
      "download_count": 12,
      "created_at": "2026-10-26T19:19:59Z",
      "updated_at": "2026-10-26T19:20:00Z",
      "browser_download_url": "https://github.com/mrquantumoff/crossc/releases/download/x/{name}"
    }}"#
        )
    }

    /// One release the way the API lists it.
    pub fn release_json(tag: &str, draft: bool, prerelease: bool, assets: &[String]) -> String {
        format!(
            r###"{{
    "url": "https://api.github.com/repos/mrquantumoff/crossc/releases/424663522",
    "assets_url": "https://api.github.com/repos/mrquantumoff/crossc/releases/424663522/assets",
    "upload_url": "https://uploads.github.com/repos/mrquantumoff/crossc/releases/424663522/assets{{?name,label}}",
    "html_url": "https://github.com/mrquantumoff/crossc/releases/tag/{tag}",
    "id": 424663522,
    "author": {{ "login": "github-actions[bot]", "id": 41898282, "type": "Bot", "site_admin": false }},
    "node_id": "RE_kwDODKw3uc4ZT9Ti",
    "tag_name": "{tag}",
    "target_commitish": "63837fb778d6a39f02be0ce76ff89f2bc8d74bdc",
    "name": "crossc {tag}",
    "draft": {draft},
    "immutable": false,
    "prerelease": {prerelease},
    "created_at": "2026-10-26T18:02:47Z",
    "updated_at": "2026-10-26T19:20:08Z",
    "published_at": "2026-10-26T19:20:08Z",
    "assets": [{}],
    "tarball_url": "https://api.github.com/repos/mrquantumoff/crossc/tarball/{tag}",
    "zipball_url": "https://api.github.com/repos/mrquantumoff/crossc/zipball/{tag}",
    "body": "## Test-signed driver: read before installing\r\n\r\nText with \"quotes\" and a [link](https://example.com).",
    "mentions_count": 1
  }}"###,
            assets.join(",")
        )
    }

    /// The three assets of a release as the release workflow uploads them.
    pub fn full_assets(api: &str, version: &str, first_id: u64) -> Vec<String> {
        vec![
            asset_json(
                api,
                first_id,
                &format!("crossc-{version}-x64.zip"),
                9_500_000,
                Some(&"ab".repeat(32)),
            ),
            asset_json(
                api,
                first_id + 1,
                &format!("crossc-{version}-symbols.zip"),
                30_000_000,
                None,
            ),
            asset_json(api, first_id + 2, SUMS_NAME, 190, None),
        ]
    }

    fn list(releases: &[String]) -> String {
        format!("[{}]", releases.join(","))
    }

    const API: &str = "https://api.github.com";

    #[test]
    fn release_list_is_read() {
        let json = list(&[
            release_json("v0.2.0", false, true, &full_assets(API, "0.2.0", 100)),
            release_json("v0.1.0", false, true, &full_assets(API, "0.1.0", 200)),
        ]);
        let releases = parse_releases(&json).unwrap();
        assert_eq!(releases.len(), 2);
        let r = &releases[0];
        assert_eq!(
            (r.tag.as_str(), &r.version, r.draft),
            ("v0.2.0", &v("0.2.0"), false)
        );
        assert_eq!(r.assets.len(), 3);
        assert_eq!(
            r.assets[0],
            Asset {
                name: "crossc-0.2.0-x64.zip".into(),
                api_url: "https://api.github.com/repos/mrquantumoff/crossc/releases/assets/100"
                    .into(),
                size: 9_500_000,
                sha256: Some("ab".repeat(32)),
            }
        );
        assert_eq!(r.assets[2].sha256, None);
        // Nothing published: an empty list, not an error.
        assert_eq!(parse_releases("[]").unwrap(), vec![]);
    }

    #[test]
    fn release_list_leaves_out_what_is_not_a_release() {
        let unfinished = asset_json(API, 7, "crossc-0.3.0-x64.zip", 5, None)
            .replace(r#""state": "uploaded""#, r#""state": "open""#);
        let json = list(&[
            release_json("nightly", false, true, &[]),
            release_json("V0.9.0", false, false, &[]),
            release_json(
                "v0.3.0",
                false,
                true,
                &[unfinished, "{\"name\": 5}".into(), "17".into()],
            ),
            "{\"name\": \"no tag\"}".into(),
            "null".into(),
        ]);
        let releases = parse_releases(&json).unwrap();
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].tag, "v0.3.0");
        // The asset that is still uploading and the malformed ones are not assets.
        assert_eq!(releases[0].assets, vec![]);
        // A digest that is not a SHA-256 is ignored, the asset stays.
        let odd = asset_json(API, 8, "x.zip", 5, Some("abc"));
        let releases =
            parse_releases(&list(&[release_json("v0.3.0", false, true, &[odd])])).unwrap();
        assert_eq!(releases[0].assets[0].sha256, None);
    }

    #[test]
    fn tags_the_release_workflow_accepts_and_this_installer_does_not_are_reported() {
        // The workflow's rule is ^v\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$ : every one of these passes it, and none is
        // a release tag here.
        let odd = ["v0.2.0-rc.01", "v01.2.3", "v0.2.0-rc..1", "v0.2.0-"];
        for tag in odd {
            assert_eq!(Version::from_tag(tag), None, "{tag}");
        }
        let mut entries: Vec<String> = odd
            .iter()
            .map(|tag| release_json(tag, false, true, &[]))
            .collect();
        entries.push(release_json(
            "v0.1.0",
            false,
            true,
            &full_assets(API, "0.1.0", 1),
        ));
        let json = list(&entries);
        // They are not releases (the installer offers v0.1.0) ...
        let latest = latest_release(&json, None).unwrap().unwrap();
        assert_eq!(latest.tag, "v0.1.0");
        // ... but they are named.
        assert_eq!(skipped_tags(&json), odd);

        // Not reported: a proper tag, a draft, tags that are no attempt at a version, entries without a tag.
        let json = list(&[
            release_json("v0.3.0", false, true, &[]),
            release_json("v0.4.0-rc.01", true, true, &[]),
            release_json("nightly", false, true, &[]),
            release_json("latest", false, true, &[]),
            release_json("version-2", false, true, &[]),
            "{\"name\": \"no tag\"}".into(),
            "null".into(),
        ]);
        assert_eq!(skipped_tags(&json), Vec::<String>::new());
        // Reported too: a capital V, no v at all, two numbers only.
        let json = list(&[
            release_json("V0.9.0", false, false, &[]),
            release_json("0.9.0", false, false, &[]),
            release_json("v1.0", false, false, &[]),
        ]);
        assert_eq!(skipped_tags(&json), ["V0.9.0", "0.9.0", "v1.0"]);

        // For display: each once, at most five, escaped and cut short.
        let many: Vec<String> = (0..9)
            .map(|n| release_json(&format!("v0{n}.0.0"), false, true, &[]))
            .chain([release_json("v00.0.0", false, true, &[])])
            .collect();
        assert_eq!(
            skipped_tags(&list(&many)),
            ["v00.0.0", "v01.0.0", "v02.0.0", "v03.0.0", "v04.0.0"]
        );
        let long = format!("v01.0.0-{}", "x".repeat(200));
        let shown = skipped_tags(&format!(
            r#"[{{"tag_name": "{long}"}}, {{"tag_name": "v1.0.0-\u001b[31mé\n"}}]"#
        ));
        assert_eq!(shown[0], format!("v01.0.0-{}...", "x".repeat(32)));
        assert_eq!(shown[1], r"v1.0.0-\u{1b}[31m\u{e9}\n");
        assert!(
            shown.iter().all(|t| t.is_ascii() && t.len() <= 43),
            "{shown:?}"
        );

        // An answer that is not a list: nothing (the error is `parse_releases`'s to report).
        for bad in ["", "{}", "<html>", r#"{"message": "Not Found"}"#] {
            assert_eq!(skipped_tags(bad), Vec::<String>::new(), "{bad:?}");
        }
    }

    #[test]
    fn an_answer_that_is_not_a_list_is_an_error() {
        let not_found = r#"{"message": "Not Found", "documentation_url": "https://docs.github.com/rest", "status": "404"}"#;
        assert_eq!(
            parse_releases(not_found).unwrap_err(),
            "GitHub answered: Not Found"
        );
        assert_eq!(api_message(not_found).as_deref(), Some("Not Found"));
        assert_eq!(api_message("<html>"), None);
        assert_eq!(api_message("{\"message\": \"\"}"), None);
        assert!(
            parse_releases("{}")
                .unwrap_err()
                .contains("not a JSON array")
        );
        assert!(
            parse_releases("<html>502</html>")
                .unwrap_err()
                .contains("not valid JSON")
        );
        assert!(parse_releases("").unwrap_err().contains("not valid JSON"));
    }

    fn rel(tag: &str, draft: bool) -> Release {
        Release {
            tag: tag.into(),
            version: Version::from_tag(tag).unwrap(),
            draft,
            assets: vec![],
        }
    }

    fn picked(releases: &[Release], installed: Option<&str>) -> Option<String> {
        let installed = installed.map(v);
        pick_release(releases, installed.as_ref()).map(|r| r.tag.clone())
    }

    #[test]
    fn the_highest_published_version_is_picked() {
        // The API lists by creation date, which is not version order (a 0.1.1 fix after 0.2.0).
        let releases = [
            rel("v0.1.1", false),
            rel("v0.2.0", false),
            rel("v0.1.0", false),
            rel("v0.10.0", false),
        ];
        assert_eq!(picked(&releases, None).as_deref(), Some("v0.10.0"));
        // Drafts never count, whatever their version.
        let releases = [rel("v9.0.0", true), rel("v0.2.0", false)];
        assert_eq!(picked(&releases, None).as_deref(), Some("v0.2.0"));
        assert_eq!(picked(&[rel("v9.0.0", true)], None), None);
        assert_eq!(picked(&[], None), None);
    }

    #[test]
    fn prereleases_count_only_without_a_stable_release_or_for_a_prerelease_user() {
        let releases = [
            rel("v0.3.0-rc.1", false),
            rel("v0.2.0", false),
            rel("v0.2.0-rc.2", false),
        ];
        // Someone on a plain version (or with nothing installed) is not offered the release candidate.
        assert_eq!(picked(&releases, None).as_deref(), Some("v0.2.0"));
        assert_eq!(picked(&releases, Some("0.1.0")).as_deref(), Some("v0.2.0"));
        // Someone on a release candidate is.
        assert_eq!(
            picked(&releases, Some("0.2.0-rc.2")).as_deref(),
            Some("v0.3.0-rc.1")
        );
        // Only release candidates exist: the newest of them.
        let only = [
            rel("v0.1.0-rc.1", false),
            rel("v0.1.0-rc.2", false),
            rel("v0.1.0", true),
        ];
        assert_eq!(picked(&only, None).as_deref(), Some("v0.1.0-rc.2"));
    }

    #[test]
    fn githubs_prerelease_flag_is_not_what_makes_a_prerelease() {
        // The release workflow sets "prerelease": true on every 0.x release. v0.2.0 is still the release to
        // offer, and v0.3.0-rc.1 is still not.
        let json = list(&[
            release_json("v0.3.0-rc.1", false, true, &[]),
            release_json("v0.2.0", false, true, &[]),
            release_json("v0.1.0", false, true, &[]),
        ]);
        let latest = latest_release(&json, Some(&v("0.1.0"))).unwrap().unwrap();
        assert_eq!(latest.tag, "v0.2.0");
        let latest = latest_release(&json, None).unwrap().unwrap();
        assert_eq!(latest.tag, "v0.2.0");
    }

    #[test]
    fn assets_are_found_by_their_exact_names() {
        let json = list(&[release_json(
            "v0.2.0",
            false,
            true,
            &full_assets(API, "0.2.0", 100),
        )]);
        let latest = latest_release(&json, None).unwrap().unwrap();
        assert_eq!(
            (latest.tag.as_str(), &latest.version),
            ("v0.2.0", &v("0.2.0"))
        );
        let assets = latest.assets.unwrap();
        assert_eq!(assets.zip.name, "crossc-0.2.0-x64.zip");
        assert!(assets.zip.api_url.ends_with("/releases/assets/100"));
        assert_eq!(assets.sums.name, "SHA256SUMS.txt");
        assert!(assets.sums.api_url.ends_with("/releases/assets/102"));
        assert_eq!(
            zip_asset_name(&v("0.2.0-rc.1")),
            "crossc-0.2.0-rc.1-x64.zip"
        );
    }

    #[test]
    fn a_release_without_its_assets_is_reported_not_skipped() {
        // The symbols zip and another version's zip are not the bundle.
        let wrong = vec![
            asset_json(API, 1, "crossc-0.2.0-symbols.zip", 5, None),
            asset_json(API, 2, "crossc-0.1.0-x64.zip", 5, None),
            asset_json(API, 3, "CROSSC-0.2.0-X64.ZIP", 5, None),
            asset_json(API, 4, SUMS_NAME, 5, None),
        ];
        let json = list(&[
            release_json("v0.2.0", false, true, &wrong),
            release_json("v0.1.0", false, true, &full_assets(API, "0.1.0", 200)),
        ]);
        let latest = latest_release(&json, None).unwrap().unwrap();
        assert_eq!(latest.tag, "v0.2.0");
        assert_eq!(
            latest.assets.unwrap_err(),
            "release v0.2.0 has no crossc-0.2.0-x64.zip asset"
        );

        let no_sums = vec![asset_json(API, 1, "crossc-0.2.0-x64.zip", 5, None)];
        let json = list(&[release_json("v0.2.0", false, true, &no_sums)]);
        let latest = latest_release(&json, None).unwrap().unwrap();
        assert_eq!(
            latest.assets.unwrap_err(),
            "release v0.2.0 has no SHA256SUMS.txt asset"
        );

        // Nothing published at all, or only a draft.
        assert_eq!(latest_release("[]", None).unwrap(), None);
        let json = list(&[release_json(
            "v0.2.0",
            true,
            true,
            &full_assets(API, "0.2.0", 1),
        )]);
        assert_eq!(latest_release(&json, None).unwrap(), None);
    }

    #[test]
    fn assets_over_the_size_caps_are_refused_before_any_download() {
        let assets = |zip: u64, sums: u64| {
            vec![
                asset_json(API, 1, "crossc-0.2.0-x64.zip", zip, None),
                asset_json(API, 2, SUMS_NAME, sums, None),
            ]
        };
        let latest = |zip, sums| {
            let json = list(&[release_json("v0.2.0", false, true, &assets(zip, sums))]);
            latest_release(&json, None).unwrap().unwrap().assets
        };
        assert!(latest(MAX_ZIP_BYTES, MAX_SUMS_BYTES).is_ok());
        let err = latest(MAX_ZIP_BYTES + 1, 100).unwrap_err();
        assert!(
            err.contains("crossc-0.2.0-x64.zip") && err.contains("200.0 MB"),
            "{err}"
        );
        let err = latest(100, MAX_SUMS_BYTES + 1).unwrap_err();
        assert!(
            err.contains("SHA256SUMS.txt") && err.contains("64 KB"),
            "{err}"
        );
    }

    #[test]
    fn sizes_are_readable() {
        assert_eq!(size_text(0), "0 bytes");
        assert_eq!(size_text(1023), "1023 bytes");
        assert_eq!(size_text(64 * 1024), "64 KB");
        assert_eq!(size_text(1024 * 1024), "1.0 MB");
        assert_eq!(size_text(9_500_000), "9.1 MB");
    }

    // ---- SHA256SUMS.txt --------------------------------------------------------------------------------------

    const HASH_A: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
    const HASH_B: &str = "2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae";

    #[test]
    fn sums_file_is_read_for_the_exact_name() {
        // What tools/package.ps1 writes: LF line ends, two spaces, sorted names.
        let sums = format!("{HASH_A}  crossc-0.2.0-symbols.zip\n{HASH_B}  crossc-0.2.0-x64.zip\n");
        assert_eq!(
            expected_sha256(&sums, "crossc-0.2.0-x64.zip").unwrap(),
            HASH_B
        );
        assert_eq!(
            expected_sha256(&sums, "crossc-0.2.0-symbols.zip").unwrap(),
            HASH_A
        );
        // CRLF, a byte-order mark, upper-case hex, binary-mode marker, blank lines: the same content.
        let sums = format!(
            "\u{feff}{}  a.zip\r\n\r\n{} *b.zip\r\n",
            HASH_A.to_uppercase(),
            HASH_B
        );
        assert_eq!(expected_sha256(&sums, "a.zip").unwrap(), HASH_A);
        assert_eq!(expected_sha256(&sums, "b.zip").unwrap(), HASH_B);
        // The same line twice is fine.
        let sums = format!("{HASH_A}  a.zip\n{HASH_A}  a.zip\n");
        assert_eq!(expected_sha256(&sums, "a.zip").unwrap(), HASH_A);
    }

    #[test]
    fn sums_file_must_name_the_file_exactly() {
        let sums = format!("{HASH_A}  crossc-0.2.0-x64.zip\n");
        for other in [
            "crossc-0.2.0-x64.ZIP",
            "crossc-0.2.0-x64.zip ",
            "0.2.0-x64.zip",
            "./crossc-0.2.0-x64.zip",
            "crossc-0.2.1-x64.zip",
        ] {
            let err = expected_sha256(&sums, other).unwrap_err();
            assert!(err.contains("has no entry for"), "{err}");
        }
        assert!(
            expected_sha256("", "a.zip")
                .unwrap_err()
                .contains("has no entry")
        );
        // A name with a directory in the file is a different name.
        let sums = format!("{HASH_A}  dist/a.zip\n");
        assert!(expected_sha256(&sums, "a.zip").is_err());
    }

    #[test]
    fn sums_file_that_is_not_understood_is_refused() {
        for bad in [
            format!("{HASH_A} a.zip\n"),            // one space
            format!("{HASH_A}\ta.zip\n"),           // a tab
            format!("{}  a.zip\n", &HASH_A[..63]),  // short hash
            format!("{}g  a.zip\n", &HASH_A[..63]), // not hex
            format!("{HASH_A}  \n"),                // no name
            format!("SHA256 (a.zip) = {HASH_A}\n"), // BSD style
            "<html>Not Found</html>\n".to_string(),
            format!("{HASH_A}  a.zip\ngarbage\n"),
            format!("é{HASH_A}  a.zip\n"), // a multi-byte character where the hash should be
        ] {
            let err = expected_sha256(&bad, "a.zip").unwrap_err();
            assert!(
                err.contains("is not \"<sha256>  <file name>\""),
                "{bad:?}: {err}"
            );
        }
        // Two different checksums for the file.
        let sums = format!("{HASH_A}  a.zip\n{HASH_B}  a.zip\n");
        assert!(
            expected_sha256(&sums, "a.zip")
                .unwrap_err()
                .contains("two different checksums")
        );
    }

    #[test]
    fn a_checksum_mismatch_is_refused() {
        assert_eq!(check_sha256("a.zip", HASH_A, HASH_A), Ok(()));
        assert_eq!(
            check_sha256("a.zip", &HASH_A.to_uppercase(), HASH_A),
            Ok(())
        );
        let err = check_sha256("a.zip", HASH_A, HASH_B).unwrap_err();
        assert!(err.contains("SHA-256 mismatch for a.zip"), "{err}");
        assert!(err.contains(HASH_A) && err.contains(HASH_B), "{err}");
        assert!(err.contains("Nothing was unpacked"), "{err}");
        // One digit off, a truncated hash, an empty one.
        let mut off = HASH_A.to_string();
        off.replace_range(63.., "9");
        assert!(check_sha256("a.zip", HASH_A, &off).is_err());
        assert!(check_sha256("a.zip", &HASH_A[..32], &HASH_A[..32]).is_err());
        assert!(check_sha256("a.zip", "", "").is_err());
    }

    // ---- URLs, allow-list, redirects -------------------------------------------------------------------------

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"))
    }

    #[test]
    fn urls_are_taken_apart() {
        assert_eq!(
            url("https://api.github.com/repos/a/b/releases?per_page=20"),
            Url {
                secure: true,
                host: "api.github.com".into(),
                port: 443,
                path: "/repos/a/b/releases?per_page=20".into()
            }
        );
        assert_eq!(url("HTTPS://API.GitHub.com").path, "/");
        assert_eq!(url("https://API.GitHub.com").host, "api.github.com");
        assert_eq!(url("https://example.com?x=1").path, "/?x=1");
        assert_eq!(url("https://example.com/a?x=1#frag").path, "/a?x=1");
        assert_eq!(url("http://127.0.0.1:8080/x").port, 8080);
        assert_eq!(
            (
                url("http://localhost/x").port,
                url("http://localhost/x").secure
            ),
            (80, false)
        );
        for bad in [
            "",
            "api.github.com/x",
            "ftp://github.com/x",
            "file:///C:/x",
            "https://",
            "https:///x",
            "https://user@github.com/x",
            "https://github.com@evil.example/x",
            "https://user:pw@github.com/",
            "https://[::1]/x",
            "https://github.com:0/x",
            "https://github.com:99999/x",
            "https://github.com:x/",
            "https://git hub.com/x",
            "https://github.com/a b",
            "https://github.com/a\r\nHost: evil",
            r"https://github.com\@evil.example/",
            "https://-github.com/",
            "https://github..com/",
            "https://github.com./x",
            "//github.com/x",
            "https://exa_mple.com/",
        ] {
            assert!(Url::parse(bad).is_err(), "{bad:?}");
        }
        assert!(Url::parse(&format!("https://github.com/{}", "a".repeat(9000))).is_err());
    }

    #[test]
    fn the_log_never_shows_a_query() {
        let signed = url(
            "https://release-assets.githubusercontent.com/github-production-release-asset/1/2?sp=r&sig=SECRET&jwt=abc",
        );
        assert_eq!(
            signed.redacted(),
            "https://release-assets.githubusercontent.com/github-production-release-asset/1/2"
        );
        assert_eq!(
            url("http://127.0.0.1:8080/a?b").redacted(),
            "http://127.0.0.1:8080/a"
        );
    }

    #[test]
    fn only_github_over_https_is_allowed() {
        let policy = Policy::github();
        for ok in [
            "https://api.github.com/repos/a/b/releases",
            "https://github.com/a/b/releases/download/v1/x.zip",
            "https://release-assets.githubusercontent.com/github-production-release-asset/1/2?sig=x",
            "https://objects.githubusercontent.com/github-production-release-asset-2e65be/1/2",
            "https://API.GITHUB.COM/x",
        ] {
            assert_eq!(policy.check(&url(ok)), Ok(()), "{ok}");
        }
        for (bad, why) in [
            ("http://api.github.com/repos/a/b/releases", "only HTTPS"),
            ("https://api.github.com:8443/x", "not the HTTPS port"),
            ("https://evil.example/x", "is not one of the GitHub hosts"),
            (
                "https://api.github.com.evil.example/x",
                "is not one of the GitHub hosts",
            ),
            (
                "https://evil-github.com/x",
                "is not one of the GitHub hosts",
            ),
            ("https://xgithub.com/x", "is not one of the GitHub hosts"),
            (
                "https://raw.githubusercontent.com/a/b/main/x",
                "is not one of the GitHub hosts",
            ),
            (
                "https://githubusercontent.com/x",
                "is not one of the GitHub hosts",
            ),
            (
                "https://uploads.github.com/x",
                "is not one of the GitHub hosts",
            ),
            ("https://127.0.0.1/x", "is not one of the GitHub hosts"),
            ("https://localhost/x", "is not one of the GitHub hosts"),
        ] {
            let err = policy.check(&url(bad)).unwrap_err();
            assert!(err.contains(why), "{bad}: {err}");
        }
        // The refusal does not repeat a query string.
        let err = policy
            .check(&url("https://evil.example/x?token=SECRET"))
            .unwrap_err();
        assert!(!err.contains("SECRET"), "{err}");
    }

    #[test]
    fn plain_http_exists_only_for_the_test_policy() {
        let local = Policy::local_test("127.0.0.1", &["localhost"]);
        assert_eq!(local.check(&url("http://127.0.0.1:5000/x")), Ok(()));
        assert_eq!(local.check(&url("http://localhost:5000/x")), Ok(()));
        assert!(local.check(&url("http://example.com/x")).is_err());
        // The policy the program uses never has it.
        assert!(!Policy::github().plain_http());
        assert!(Policy::github().check(&url("http://github.com/x")).is_err());
    }

    #[test]
    fn redirects_are_followed_only_inside_the_allow_list() {
        let policy = Policy::github();
        let asset = url("https://api.github.com/repos/a/b/releases/assets/1");
        assert_eq!(next_hop(&policy, &asset, 200, None, 0), Ok(Next::Body));
        // What GitHub answers for an asset: 302 to the signed file URL.
        let signed = "https://release-assets.githubusercontent.com/github-production-release-asset/1/2?sig=S";
        for status in [301, 302, 303, 307, 308] {
            assert_eq!(
                next_hop(&policy, &asset, status, Some(signed), 0),
                Ok(Next::Redirect(url(signed))),
                "{status}"
            );
        }
        // A path on the same server.
        assert_eq!(
            next_hop(
                &policy,
                &asset,
                302,
                Some("/repos/a/b/releases/assets/2?x=1#f"),
                1
            ),
            Ok(Next::Redirect(url(
                "https://api.github.com/repos/a/b/releases/assets/2?x=1"
            )))
        );
        // Off the list, down to HTTP, to another port, to nowhere, to something relative or protocol-relative.
        for (location, why) in [
            (
                Some("https://evil.example/crossc.zip"),
                "is not one of the GitHub hosts",
            ),
            (
                Some("http://release-assets.githubusercontent.com/x"),
                "only HTTPS",
            ),
            (Some("https://github.com:444/x"), "not the HTTPS port"),
            (Some("//evil.example/x"), "not a usable URL"),
            (Some("assets/2"), "not a usable URL"),
            (Some("file:///C:/Windows/System32/x"), "not a usable URL"),
            (Some("https://user@github.com/x"), "not a usable URL"),
            (Some(""), "without saying where to"),
            (Some("   "), "without saying where to"),
            (None, "without saying where to"),
        ] {
            let err = next_hop(&policy, &asset, 302, location, 0)
                .unwrap_err()
                .unwrap();
            assert!(err.contains(why), "{location:?}: {err}");
            assert!(!err.contains("sig="), "{err}");
        }
        // The fifth redirect is followed, a sixth is not.
        assert!(next_hop(&policy, &asset, 302, Some(signed), MAX_REDIRECTS - 1).is_ok());
        let err = next_hop(&policy, &asset, 302, Some(signed), MAX_REDIRECTS)
            .unwrap_err()
            .unwrap();
        assert!(err.contains("more than 5 redirects"), "{err}");
        // Anything else is an HTTP failure for the caller to word.
        for status in [201, 204, 206, 300, 304, 400, 401, 403, 404, 429, 500, 503] {
            assert_eq!(
                next_hop(&policy, &asset, status, Some(signed), 0),
                Err(None),
                "{status}"
            );
        }
    }

    #[test]
    fn the_token_goes_to_the_api_and_nowhere_else() {
        let policy = Policy::github();
        let token = Token::new("ghp_SECRET").unwrap();
        let api = url("https://api.github.com/repos/a/b/releases/assets/1");
        let headers = request_headers(&policy, &api, ACCEPT_BINARY, Some(&token));
        assert_eq!(
            headers,
            vec![
                ("Accept", "application/octet-stream".to_string()),
                ("X-GitHub-Api-Version", "2022-11-28".to_string()),
                ("Authorization", "Bearer ghp_SECRET".to_string()),
            ]
        );
        // The redirect target, github.com itself, an API look-alike, the API over HTTP or on another port.
        for other in [
            "https://release-assets.githubusercontent.com/x?sig=S",
            "https://objects.githubusercontent.com/x",
            "https://github.com/a/b/releases/download/v1/x.zip",
            "https://api.github.com.evil.example/x",
            "http://api.github.com/x",
            "https://api.github.com:8443/x",
        ] {
            let headers = request_headers(&policy, &url(other), ACCEPT_BINARY, Some(&token));
            assert_eq!(
                headers,
                vec![("Accept", "application/octet-stream".to_string())],
                "{other}"
            );
        }
        // Anonymous: no Authorization header at all.
        let headers = request_headers(&policy, &api, ACCEPT_JSON, None);
        assert!(
            headers.iter().all(|(name, _)| *name != "Authorization"),
            "{headers:?}"
        );
        assert_eq!(
            headers[0],
            ("Accept", "application/vnd.github+json".to_string())
        );
    }

    #[test]
    fn http_failures_say_what_to_do() {
        let repo = "mrquantumoff/crossc";
        // A private repository asked anonymously looks like a missing one: say so.
        let text = http_failure(404, repo, false, Some("58"), Some("Not Found"));
        assert!(
            text.contains("mrquantumoff/crossc was not found (HTTP 404)"),
            "{text}"
        );
        assert!(text.contains("If the repository is private"), "{text}");
        assert!(
            text.contains("GITHUB_TOKEN")
                && text.contains("GH_TOKEN")
                && text.contains("gh auth login"),
            "{text}"
        );
        let text = http_failure(404, repo, true, None, Some("Not Found"));
        assert!(text.contains("the token has no access to it"), "{text}");
        assert!(!text.contains("GITHUB_TOKEN"), "{text}");
        assert!(
            http_failure(401, repo, true, None, Some("Bad credentials"))
                .contains("rejected the token")
        );
        // Rate limit: 403 or 429 with nothing remaining.
        let text = http_failure(
            403,
            repo,
            false,
            Some("0"),
            Some("API rate limit exceeded for 203.0.113.9."),
        );
        assert!(
            text.contains("request limit for anonymous requests") && text.contains("GITHUB_TOKEN"),
            "{text}"
        );
        assert!(!text.contains("203.0.113.9"), "{text}");
        let text = http_failure(429, repo, true, Some(" 0 "), None);
        assert!(text.contains("request limit for this token"), "{text}");
        assert!(http_failure(407, repo, false, None, None).contains("the proxy"));
        // Any other status: the number and GitHub's message.
        assert_eq!(
            http_failure(403, repo, true, Some("10"), Some("Forbidden")),
            "GitHub answered HTTP 403: Forbidden"
        );
        assert_eq!(
            http_failure(503, repo, false, None, None),
            "GitHub answered HTTP 503"
        );
    }

    // ---- the token -------------------------------------------------------------------------------------------

    #[test]
    fn tokens_are_validated_and_never_printed() {
        assert!(Token::new("ghp_abcDEF123").is_some());
        assert!(Token::new("  github_pat_11ABC_xyz\r\n").is_some());
        for bad in [
            "",
            "   ",
            "\r\n",
            "two words",
            "line\nbreak",
            "tab\tbed",
            "x\r\nX-Evil: 1",
            "naïve",
            "\u{0}",
        ] {
            assert!(Token::new(bad).is_none(), "{bad:?}");
        }
        assert!(Token::new(&"a".repeat(1025)).is_none());
        let token = Token::new("ghp_SECRET").unwrap();
        assert_eq!(format!("{token:?}"), "Token(hidden)");
        assert_eq!(
            format!("{:?}", Some((token, TokenSource::GhCli))),
            "Some((Token(hidden), GhCli))"
        );
        assert_eq!(auth_text(None), "anonymous");
        assert_eq!(
            auth_text(Some(TokenSource::Env("GH_TOKEN"))),
            "token from GH_TOKEN"
        );
        assert_eq!(
            auth_text(Some(TokenSource::GhCli)),
            "token from gh auth token"
        );
    }

    #[test]
    fn token_is_looked_for_in_order() {
        let gh_calls = Cell::new(0);
        let gh = |answer: Option<&'static str>| {
            let gh_calls = &gh_calls;
            move || {
                gh_calls.set(gh_calls.get() + 1);
                answer.map(str::to_string)
            }
        };
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, v)| v.to_string())
            }
        };
        let found = |pairs, answer| discover_token(&env(pairs), &gh(answer));

        // GITHUB_TOKEN first, then GH_TOKEN; gh is not even started when a variable has a token.
        let both = found(
            &[
                ("GH_TOKEN", "from-gh-token"),
                ("GITHUB_TOKEN", "from-github-token"),
            ],
            Some("from-cli"),
        );
        assert_eq!(
            both,
            Some((
                Token::new("from-github-token").unwrap(),
                TokenSource::Env("GITHUB_TOKEN")
            ))
        );
        let second = found(&[("GH_TOKEN", "from-gh-token")], Some("from-cli"));
        assert_eq!(
            second,
            Some((
                Token::new("from-gh-token").unwrap(),
                TokenSource::Env("GH_TOKEN")
            ))
        );
        assert_eq!(gh_calls.get(), 0);

        // Neither variable: gh, once.
        let cli = found(&[], Some("from-cli\n"));
        assert_eq!(
            cli,
            Some((Token::new("from-cli").unwrap(), TokenSource::GhCli))
        );
        assert_eq!(gh_calls.get(), 1);

        // A variable that is set but empty or not a token is passed over, not used.
        let skipped = found(
            &[("GITHUB_TOKEN", "  "), ("GH_TOKEN", "bad token")],
            Some("from-cli"),
        );
        assert_eq!(skipped.map(|(_, source)| source), Some(TokenSource::GhCli));
        let skipped = found(
            &[("GITHUB_TOKEN", ""), ("GH_TOKEN", "good")],
            Some("from-cli"),
        );
        assert_eq!(
            skipped.map(|(_, source)| source),
            Some(TokenSource::Env("GH_TOKEN"))
        );

        // No gh, gh not signed in (no output), gh printing something that is not a token: anonymous.
        assert_eq!(found(&[], None), None);
        assert_eq!(found(&[], Some("")), None);
        assert_eq!(
            found(&[], Some("no oauth token found for github.com")),
            None
        );
        // Other variables are not looked at.
        assert_eq!(found(&[("GITHUB_PAT", "x"), ("TOKEN", "y")], None), None);
    }

    // ---- unpacking -------------------------------------------------------------------------------------------

    const TOP: &str = "crossc-0.2.0-x64";

    #[test]
    fn entries_inside_the_top_folder_are_accepted() {
        assert_eq!(entry_path("crossc-0.2.0-x64/", TOP), Ok(None));
        assert_eq!(entry_path("crossc-0.2.0-x64", TOP), Ok(None));
        assert_eq!(
            entry_path("crossc-0.2.0-x64/crossc-installer.exe", TOP),
            Ok(Some(vec!["crossc-installer.exe"]))
        );
        assert_eq!(
            entry_path("crossc-0.2.0-x64/driver/", TOP),
            Ok(Some(vec!["driver"]))
        );
        assert_eq!(
            entry_path("crossc-0.2.0-x64/driver/crossc_bth.inf", TOP),
            Ok(Some(vec!["driver", "crossc_bth.inf"]))
        );
        assert_eq!(
            entry_path("crossc-0.2.0-x64/licenses/ssc-enc/LICENSE-THIRD-PARTY", TOP),
            Ok(Some(vec!["licenses", "ssc-enc", "LICENSE-THIRD-PARTY"]))
        );
        assert_eq!(
            entry_path("crossc-0.2.0-x64/THIRD-PARTY-LICENSES.html", TOP),
            Ok(Some(vec!["THIRD-PARTY-LICENSES.html"]))
        );
        // Dots inside a name are ordinary.
        assert_eq!(
            entry_path("crossc-0.2.0-x64/a..b", TOP),
            Ok(Some(vec!["a..b"]))
        );
        assert_eq!(
            entry_path("crossc-0.2.0-x64/.hidden", TOP),
            Ok(Some(vec![".hidden"]))
        );
    }

    #[test]
    fn zip_slip_and_windows_path_tricks_are_refused() {
        for (name, why) in [
            // Out of the folder.
            ("../evil.exe", "\"..\" part"),
            ("crossc-0.2.0-x64/../evil.exe", "\"..\" part"),
            ("crossc-0.2.0-x64/driver/../../evil.exe", "\"..\" part"),
            ("crossc-0.2.0-x64/..", "\"..\" part"),
            ("crossc-0.2.0-x64/./x", "\".\" or"),
            ("crossc-0.2.0-x64//x", "empty"),
            // Absolute, drive-relative, UNC, device paths.
            ("/crossc-0.2.0-x64/x", "absolute path"),
            ("/Windows/System32/drivers/x.sys", "absolute path"),
            ("C:/Windows/x", "character that is not allowed"),
            ("C:evil.exe", "character that is not allowed"),
            (r"crossc-0.2.0-x64\x", "character that is not allowed"),
            (r"..\..\Windows\x", "character that is not allowed"),
            (r"\\server\share\x", "character that is not allowed"),
            (r"\\?\C:\x", "character that is not allowed"),
            ("//server/share/x", "absolute path"),
            // Alternate data streams and other reserved characters.
            (
                "crossc-0.2.0-x64/x.exe:stream",
                "character that is not allowed",
            ),
            ("crossc-0.2.0-x64/x*.exe", "character that is not allowed"),
            ("crossc-0.2.0-x64/x?.exe", "character that is not allowed"),
            ("crossc-0.2.0-x64/\"x\"", "character that is not allowed"),
            ("crossc-0.2.0-x64/a|b", "character that is not allowed"),
            ("crossc-0.2.0-x64/a<b>", "character that is not allowed"),
            ("crossc-0.2.0-x64/a\nb", "character that is not allowed"),
            ("crossc-0.2.0-x64/a\u{0}b", "character that is not allowed"),
            (
                "crossc-0.2.0-x64/caf\u{e9}",
                "character that is not allowed",
            ),
            (
                "crossc-0.2.0-x64/x\u{202e}exe.txt",
                "character that is not allowed",
            ),
            // Names Windows would silently change, and devices.
            ("crossc-0.2.0-x64/x.exe.", "ends in a dot or a space"),
            ("crossc-0.2.0-x64/x.exe ", "ends in a dot or a space"),
            ("crossc-0.2.0-x64/driver./x", "ends in a dot or a space"),
            ("crossc-0.2.0-x64/ x", "starts with a space"),
            ("crossc-0.2.0-x64/.../x", "ends in a dot or a space"),
            ("crossc-0.2.0-x64/NUL", "device name"),
            ("crossc-0.2.0-x64/nul.txt", "device name"),
            ("crossc-0.2.0-x64/CON", "device name"),
            ("crossc-0.2.0-x64/aux.inf", "device name"),
            ("crossc-0.2.0-x64/COM1", "device name"),
            ("crossc-0.2.0-x64/lpt9.sys", "device name"),
            ("crossc-0.2.0-x64/driver/PRN", "device name"),
            // Another top folder, a file next to it, a look-alike.
            ("evil.exe", "is not inside crossc-0.2.0-x64/"),
            (
                "other/crossc-installer.exe",
                "is not inside crossc-0.2.0-x64/",
            ),
            ("crossc-0.2.0-x64-evil/x", "is not inside crossc-0.2.0-x64/"),
            ("CROSSC-0.2.0-X64/x", "is not inside crossc-0.2.0-x64/"),
            ("crossc-0.1.0-x64/x", "is not inside crossc-0.2.0-x64/"),
            ("", "empty"),
        ] {
            let err = entry_path(name, TOP).unwrap_err();
            assert!(err.contains(why), "{name:?}: {err}");
        }
        let deep = format!("{TOP}/{}x", "a/".repeat(16));
        assert!(
            entry_path(&deep, TOP)
                .unwrap_err()
                .contains("nested too deeply")
        );
        // Not everything that starts like a device name is one.
        for fine in [
            "COM",
            "COM10",
            "console.txt",
            "nullable.rs",
            "LPT",
            "auxiliary",
        ] {
            assert!(entry_path(&format!("{TOP}/{fine}"), TOP).is_ok(), "{fine}");
        }
    }

    #[test]
    fn older_releases_are_pruned_and_the_current_one_kept() {
        let entries: Vec<String> = [
            "v0.1.0",
            "v0.2.0",
            "v0.2.0.partial",
            "v0.3.0",
            "v0.3.0.download",
            "notes.txt",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            releases_to_prune(&entries, "v0.3.0", None),
            vec![
                "v0.1.0",
                "v0.2.0",
                "v0.2.0.partial",
                "v0.3.0.download",
                "notes.txt"
            ]
        );
        // The folder this installer runs from is left alone (it could not be deleted anyway).
        assert_eq!(
            releases_to_prune(&entries, "V0.3.0", Some("V0.1.0")),
            vec!["v0.2.0", "v0.2.0.partial", "v0.3.0.download", "notes.txt"]
        );
        assert_eq!(
            releases_to_prune(&["v0.3.0".to_string()], "v0.3.0", None),
            Vec::<String>::new()
        );
        assert_eq!(releases_to_prune(&[], "v0.3.0", None), Vec::<String>::new());
    }

    #[test]
    fn progress_lines() {
        const MB: u64 = 1024 * 1024;
        assert_eq!(progress_step(Some(48 * MB)), 48 * MB / 10);
        assert_eq!(progress_step(Some(100)), MB);
        assert_eq!(progress_step(None), 8 * MB);
        assert_eq!(
            progress_text(12 * MB, Some(48 * MB)),
            "12.0 MB of 48.0 MB (25%)"
        );
        assert_eq!(
            progress_text(48 * MB, Some(48 * MB)),
            "48.0 MB of 48.0 MB (100%)"
        );
        assert_eq!(progress_text(12 * MB, None), "12.0 MB");
        assert_eq!(progress_text(5, Some(0)), "5 bytes");
    }
}
