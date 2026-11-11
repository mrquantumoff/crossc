//! The service's log: timestamped lines with a level, to a size-capped file
//! that rotates (`service.log`, `service.log.1`, `service.log.2`), or to
//! stdout for a foreground run.
//!
//! The service runs as LocalSystem and its log directory lies under
//! `%ProgramData%`, where any local user may create directories. A directory
//! somebody else prepared could redirect what the service creates, renames
//! and deletes there, so the service's log is [`FileLog::guarded`]: it only
//! writes to a real directory owned by LocalSystem or the Administrators
//! group (see [`check_log_dir`]).

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};

use crossc_core::log::{Level, Log};

pub const LOG_FILE: &str = "service.log";
/// A log file is rotated before it would grow past this.
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Files kept in total: the current one and two older ones.
pub const FILES_KEPT: usize = 3;

fn level_to_u8(l: Level) -> u8 {
    match l {
        Level::Error => 0,
        Level::Warn => 1,
        Level::Info => 2,
        Level::Debug => 3,
    }
}

/// `2026-11-11 09:25:03.123 INFO  message`
pub fn format_line(timestamp: &str, level: Level, msg: &str) -> String {
    format!("{timestamp} {:<5} {msg}\r\n", level.name())
}

/// Local time as `YYYY-MM-DD HH:MM:SS.mmm`.
#[cfg(windows)]
pub fn timestamp() -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    )
}

/// Seconds since the Unix epoch (no local-time source without Windows).
#[cfg(not(windows))]
pub fn timestamp() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.{:03}", d.as_secs(), d.subsec_millis())
}

/// `%ProgramData%\crossc`, where the service keeps its log.
pub fn default_log_dir() -> PathBuf {
    let base = std::env::var_os("ProgramData")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join("crossc")
}

/// Whether the account with this SID (in string form) may own the directory
/// a LocalSystem service logs to: LocalSystem itself and the Administrators
/// group, which is also the owner of what LocalSystem or an elevated
/// installer creates.
pub fn trusted_owner(sid: &str) -> bool {
    matches!(sid, "S-1-5-18" | "S-1-5-32-544")
}

/// The owner of `path` as a SID string (`S-1-...`).
#[cfg(windows)]
pub fn owner_sid(path: &Path) -> Result<String, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Foundation::{ERROR_SUCCESS, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{
        ConvertSidToStringSidW, GetNamedSecurityInfoW, SE_FILE_OBJECT,
    };
    use windows::Win32::Security::{OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID};
    use windows::core::{PCWSTR, PWSTR};

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut owner = PSID::default();
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `wide` is NUL-terminated and outlives the call; `owner` points
    // into `sd`, which is freed below, after its last use.
    let err = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(wide.as_ptr()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            None,
            None,
            &mut sd,
        )
    };
    if err != ERROR_SUCCESS {
        return Err(format!("reading the owner failed: Win32 error {}", err.0));
    }
    let mut text = PWSTR::null();
    // SAFETY: `owner` is the valid SID the call above returned; `text` is
    // read as the NUL-terminated string the system allocated, then freed.
    unsafe {
        let r = ConvertSidToStringSidW(owner, &mut text)
            .map_err(|e| format!("formatting the owner's SID failed: {e}"))
            .and_then(|()| {
                text.to_string()
                    .map_err(|e| format!("formatting the owner's SID failed: {e}"))
            });
        if !text.is_null() {
            let _ = LocalFree(Some(HLOCAL(text.0.cast())));
        }
        let _ = LocalFree(Some(HLOCAL(sd.0)));
        r
    }
}

/// Whether the LocalSystem service may log to `dir`. `Err` says why not: it
/// is a junction or a link, not a directory, or owned by an account other
/// than LocalSystem / Administrators (see the module docs). A directory
/// that does not exist yet is fine: the service creates it, and checks
/// again what it then finds. Changes nothing.
#[cfg(windows)]
pub fn check_log_dir(dir: &Path) -> Result<(), String> {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

    // Of the path itself, not of what a link there points to.
    let meta = match fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("{} cannot be examined: {e}", dir.display())),
    };
    if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(format!(
            "{} is a junction or link, not a directory of its own",
            dir.display()
        ));
    }
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }
    let owner = owner_sid(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if !trusted_owner(&owner) {
        return Err(format!(
            "{} is owned by {owner}, not by SYSTEM or Administrators",
            dir.display()
        ));
    }
    Ok(())
}

/// Nothing to check where there is no LocalSystem.
#[cfg(not(windows))]
pub fn check_log_dir(_dir: &Path) -> Result<(), String> {
    Ok(())
}

struct FileState {
    dir: PathBuf,
    /// Only write to a directory that passes [`check_log_dir`].
    guarded: bool,
    file: Option<File>,
    size: u64,
    max_bytes: u64,
    keep: usize,
}

impl FileState {
    fn path(&self, index: usize) -> PathBuf {
        if index == 0 {
            self.dir.join(LOG_FILE)
        } else {
            self.dir.join(format!("{LOG_FILE}.{index}"))
        }
    }

    fn open(&mut self) -> std::io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        if self.guarded {
            // After creating it: whoever made the directory, it is the one
            // that exists now that counts.
            check_log_dir(&self.dir).map_err(std::io::Error::other)?;
        }
        let f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path(0))?;
        self.size = f.metadata()?.len();
        self.file = Some(f);
        Ok(())
    }

    /// `service.log` becomes `.1`, `.1` becomes `.2`, the oldest goes.
    fn rotate(&mut self) {
        self.file = None; // close before renaming
        if self.guarded && check_log_dir(&self.dir).is_err() {
            return; // `open` fails for the same reason
        }
        let oldest = self.keep.saturating_sub(1);
        if oldest == 0 {
            let _ = fs::remove_file(self.path(0));
        } else {
            let _ = fs::remove_file(self.path(oldest));
            for i in (0..oldest).rev() {
                let _ = fs::rename(self.path(i), self.path(i + 1));
            }
        }
        self.size = 0;
    }

    fn write(&mut self, line: &str) -> std::io::Result<()> {
        if self.file.is_none() {
            self.open()?;
        }
        if self.size > 0 && self.size + line.len() as u64 > self.max_bytes {
            self.rotate();
            self.open()?;
        }
        if let Some(f) = &mut self.file {
            f.write_all(line.as_bytes())?;
            self.size += line.len() as u64;
        }
        Ok(())
    }
}

/// Log to `<dir>\service.log` with rotation. Never fails and never panics:
/// a line that cannot be written is dropped and the file is reopened for
/// the next one.
pub struct FileLog {
    state: Mutex<FileState>,
    level: AtomicU8,
}

impl FileLog {
    pub fn new(dir: &Path, level: Level) -> Self {
        Self::with_limits(dir, level, MAX_FILE_BYTES, FILES_KEPT)
    }

    /// The service's own log: like [`Self::new`], but nothing is created,
    /// renamed or deleted in a directory that fails [`check_log_dir`] (its
    /// lines are dropped until the directory is put right).
    pub fn guarded(dir: &Path, level: Level) -> Self {
        let log = Self::new(dir, level);
        log.state.lock().unwrap_or_else(|p| p.into_inner()).guarded = true;
        log
    }

    pub fn with_limits(dir: &Path, level: Level, max_bytes: u64, keep: usize) -> Self {
        FileLog {
            state: Mutex::new(FileState {
                dir: dir.to_path_buf(),
                guarded: false,
                file: None,
                size: 0,
                max_bytes: max_bytes.max(1),
                keep: keep.max(1),
            }),
            level: AtomicU8::new(level_to_u8(level)),
        }
    }

    pub fn set_level(&self, level: Level) {
        self.level.store(level_to_u8(level), Ordering::Relaxed);
    }

    fn write_line(&self, line: &str) {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if st.write(line).is_err() {
            st.file = None; // reopen next time
        }
    }
}

impl Log for FileLog {
    fn log(&self, level: Level, msg: &str) {
        if self.enabled(level) {
            self.write_line(&format_line(&timestamp(), level, msg));
        }
    }
    fn enabled(&self, level: Level) -> bool {
        level_to_u8(level) <= self.level.load(Ordering::Relaxed)
    }
}

/// Log to stdout (the foreground `run`).
pub struct StdoutLog {
    level: AtomicU8,
}

impl StdoutLog {
    pub fn new(level: Level) -> Self {
        StdoutLog {
            level: AtomicU8::new(level_to_u8(level)),
        }
    }
}

impl Log for StdoutLog {
    fn log(&self, level: Level, msg: &str) {
        if self.enabled(level) {
            // A closed stdout must not take the process down.
            let line = format_line(&timestamp(), level, msg);
            let _ = std::io::stdout().lock().write_all(line.as_bytes());
        }
    }
    fn enabled(&self, level: Level) -> bool {
        level_to_u8(level) <= self.level.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("crossc-log-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn line_format() {
        assert_eq!(
            format_line("2026-11-11 09:25:03.123", Level::Info, "hello"),
            "2026-11-11 09:25:03.123 INFO  hello\r\n"
        );
        assert_eq!(
            format_line("t", Level::Error, "x"),
            "t ERROR x\r\n",
            "levels are padded to one width"
        );
        let ts = timestamp();
        assert!(ts.len() >= 10 && ts.contains('.'), "{ts}");
    }

    #[test]
    fn writes_lines_and_creates_the_directory() {
        let dir = temp_dir("basic").join("nested");
        let log = FileLog::new(&dir, Level::Info);
        log.log(Level::Info, "one");
        log.log(Level::Debug, "hidden");
        log.log(Level::Error, "two");
        let text = fs::read_to_string(dir.join(LOG_FILE)).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(lines[0].ends_with(" INFO  one") && lines[1].ends_with(" ERROR two"));
        assert!(!log.enabled(Level::Debug) && log.enabled(Level::Warn));
        log.set_level(Level::Debug);
        log.log(Level::Debug, "shown");
        assert!(
            fs::read_to_string(dir.join(LOG_FILE))
                .unwrap()
                .contains("DEBUG shown")
        );
        // A second logger appends to the same file.
        drop(log);
        FileLog::new(&dir, Level::Info).log(Level::Warn, "again");
        assert_eq!(
            fs::read_to_string(dir.join(LOG_FILE))
                .unwrap()
                .lines()
                .count(),
            4
        );
        let _ = fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn rotates_at_the_size_cap_and_keeps_three_files() {
        let dir = temp_dir("rotate");
        // Each line is 100 bytes; 1000-byte files hold 10 lines.
        let log = FileLog::with_limits(&dir, Level::Info, 1000, 3);
        let line_len = format_line(&timestamp(), Level::Info, "").len();
        let pad = "x".repeat(100 - line_len - 4);
        for i in 0..45 {
            log.log(Level::Info, &format!("{i:03} {pad}"));
        }
        let read = |name: &str| fs::read_to_string(dir.join(name)).unwrap_or_default();
        let first = |s: &str| -> String {
            s.lines()
                .next()
                .and_then(|l| l.split("INFO  ").nth(1))
                .map(|m| m[..3].to_string())
                .unwrap_or_default()
        };
        let (cur, one, two) = (
            read("service.log"),
            read("service.log.1"),
            read("service.log.2"),
        );
        // 45 lines: [0..10) and [10..20) rotated out, [20..30) in .2,
        // [30..40) in .1, [40..45) in the current file.
        assert_eq!((cur.lines().count(), first(&cur)), (5, "040".into()));
        assert_eq!((one.lines().count(), first(&one)), (10, "030".into()));
        assert_eq!((two.lines().count(), first(&two)), (10, "020".into()));
        assert!(!dir.join("service.log.3").exists());
        for f in ["service.log", "service.log.1", "service.log.2"] {
            assert!(fs::metadata(dir.join(f)).unwrap().len() <= 1000);
        }
        // A line longer than the cap still gets written, alone in its file.
        log.log(Level::Info, &"y".repeat(3000));
        assert_eq!(read("service.log").lines().count(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unwritable_location_is_survived() {
        // A path under a regular file cannot be created.
        let dir = temp_dir("bad");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("not-a-dir");
        fs::write(&file, b"x").unwrap();
        let log = FileLog::new(&file.join("sub"), Level::Info);
        log.log(Level::Info, "dropped");
        log.log(Level::Info, "dropped too");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_system_and_administrators_are_trusted_owners() {
        assert!(trusted_owner("S-1-5-18"), "LocalSystem");
        assert!(trusted_owner("S-1-5-32-544"), "the Administrators group");
        for sid in [
            "S-1-5-32-545", // the Users group
            "S-1-5-11",     // Authenticated Users
            "S-1-1-0",      // Everyone
            "S-1-5-21-1004336348-1177238915-682003330-1001",
            // Not even the built-in Administrator account: only the group.
            "S-1-5-21-1004336348-1177238915-682003330-500",
            "S-1-5-180",
            "",
        ] {
            assert!(!trusted_owner(sid), "{sid}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_guarded_log_refuses_a_directory_owned_by_an_ordinary_account() {
        // What the service would find had a local user made the log
        // directory before it: here, a directory this test makes.
        let dir = temp_dir("planted");
        fs::create_dir_all(&dir).unwrap();
        let owner = owner_sid(&dir).unwrap();
        assert!(owner.starts_with("S-1-"), "{owner}");
        let log = FileLog::guarded(&dir, Level::Info);
        log.log(Level::Info, "one");
        log.log(Level::Info, "two");
        if trusted_owner(&owner) {
            // An elevated run: the directory is the Administrators', and used.
            assert_eq!(check_log_dir(&dir), Ok(()));
            let text = fs::read_to_string(dir.join(LOG_FILE)).unwrap();
            assert_eq!(text.lines().count(), 2);
        } else {
            let why = check_log_dir(&dir).unwrap_err();
            assert!(why.contains(&owner) && why.contains("owned by"), "{why}");
            assert!(
                fs::read_dir(&dir).unwrap().next().is_none(),
                "nothing was created, renamed or deleted in it"
            );
            // The same directory without the guard (a tool run by its user).
            FileLog::new(&dir, Level::Info).log(Level::Info, "plain");
            assert!(dir.join(LOG_FILE).exists());
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn a_guarded_log_refuses_a_junction() {
        let base = temp_dir("junction");
        let target = base.join("elsewhere");
        let link = base.join("crossc");
        fs::create_dir_all(&target).unwrap();
        // Junctions need no privilege to create.
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output();
        if !made.is_ok_and(|o| o.status.success()) || !link.exists() {
            eprintln!("skipped: could not create a junction");
            let _ = fs::remove_dir_all(&base);
            return;
        }
        let why = check_log_dir(&link).unwrap_err();
        assert!(why.contains("junction or link"), "{why}");
        let log = FileLog::guarded(&link, Level::Info);
        log.log(Level::Info, "must not land elsewhere");
        assert!(fs::read_dir(&target).unwrap().next().is_none());
        // Remove the junction itself first, so that nothing follows it.
        let _ = fs::remove_dir(&link);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_directory_that_does_not_exist_yet_is_not_a_problem() {
        let dir = temp_dir("absent");
        assert_eq!(check_log_dir(&dir), Ok(()));
        assert!(!dir.exists(), "checking creates nothing");
        // A file where the directory should be.
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("file");
        fs::write(&file, b"x").unwrap();
        #[cfg(windows)]
        assert!(check_log_dir(&file).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_location() {
        let d = default_log_dir();
        assert!(d.ends_with("crossc"), "{d:?}");
    }
}
