//! The tray app (`crossc-tray.exe`) as the installer handles it. The tray runs as the signed-in user, not
//! elevated; Install and Uninstall run elevated, perhaps as another account (UAC with an administrator's
//! credentials). So nothing here goes through this process's own `HKEY_CURRENT_USER` or starts the tray as a
//! plain child process. The user is the one at the desktop (`desktop_user`): the user of the desktop shell, or,
//! when the shell is another account's and cannot be opened, the user of this session by name.
//!
//! * autostart is the value the tray's own "Start with Windows" writes, `crossc-tray` under the Run key, written
//!   to that user's hive (`HKEY_USERS\<SID>`);
//! * the tray is started with a copy of the shell's token (`CreateProcessWithTokenW`), so it gets the rights
//!   Explorer has and no more. Without that token (another account's shell) it is not started: it starts with
//!   the next sign-in, and the log says where it is;
//! * a running tray is found by the path of its image, in any session, and ended: asked to close first (its
//!   icon then goes at once), terminated if it does not. A tray of another account (the installer was elevated
//!   with an administrator's credentials, or another user is signed in as well) is opened with the debug
//!   privilege an elevated process holds, switched on for that look only. One that still cannot be opened
//!   cannot be found that way; the log then says that there is one it could not look at;
//! * what the tray's user gets back is decided by the session: only a tray that was ended in the installer's
//!   own session (the desktop the tray is started on) is started again, and none is started while one that
//!   could not be ended is still there.
//!
//! What to do is decided by the pure functions at the top; the Win32 calls below them are thin.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_NOT_ALL_ASSIGNED, ERROR_SUCCESS, GetLastError, HANDLE,
    HLOCAL, HWND, LPARAM, LUID, LocalFree, WAIT_OBJECT_0, WIN32_ERROR, WPARAM,
};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{
    AdjustTokenPrivileges, DuplicateTokenEx, GetTokenInformation, LUID_AND_ATTRIBUTES,
    LookupAccountNameW, LookupPrivilegeValueW, PSID, SE_DEBUG_NAME, SE_PRIVILEGE_ENABLED,
    SID_NAME_USE, SecurityImpersonation, TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_PRIVILEGES,
    TOKEN_ADJUST_SESSIONID, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_PRIVILEGES,
    TOKEN_PRIVILEGES_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenPrimary, TokenUser,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::Registry::{
    HKEY, HKEY_USERS, REG_SZ, RegDeleteKeyValueW, RegEnumKeyExW, RegSetKeyValueW,
};
use windows::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTS_CURRENT_SESSION, WTS_INFO_CLASS, WTSDomainName, WTSFreeMemory,
    WTSQuerySessionInformationW, WTSUserName,
};
use windows::Win32::System::Threading::{
    CREATE_PROCESS_LOGON_FLAGS, CREATE_UNICODE_ENVIRONMENT, CreateProcessWithTokenW,
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_ACCESS_RIGHTS,
    PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    QueryFullProcessImageNameW, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, GetShellWindow, GetWindowThreadProcessId, PostMessageW, WM_CLOSE,
};
use windows::core::{HSTRING, PCWSTR, PWSTR, w};

use crate::actions::path_key;
use crate::model::TrayInfo;
use crate::system::reg_string;

/// The Run key of a user, below that user's hive. The same key and value as `crossc-tray`'s own "Start with
/// Windows" (`crates/crossc-tray/src/autostart.rs`): the tray's menu shows and changes what is written here.
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
pub const RUN_VALUE: &str = "crossc-tray";

/// How long a tray that was asked to close gets before it is terminated.
const CLOSE_GRACE: Duration = Duration::from_secs(3);
/// How long a terminated process gets to be gone (and to let go of its exe).
const EXIT_WAIT: Duration = Duration::from_secs(3);

// ---------------------------------------------------------------------------------------------------------------
// Pure
// ---------------------------------------------------------------------------------------------------------------

/// The command line stored in the Run value for `exe`: the path in quotes, no arguments. Exactly what the tray
/// writes, so its "Start with Windows" item shows as ticked.
pub fn run_command(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

/// The program a stored command line starts: the quoted part, or everything up to the first space.
fn program(command: &str) -> &str {
    let c = command.trim();
    match c.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or(rest),
        None => c.split(' ').next().unwrap_or(c),
    }
}

/// Does the stored command line start `exe`? (Compared as paths: case and the direction of the slashes do not
/// matter.)
pub fn command_starts(command: &str, exe: &Path) -> bool {
    let stored = path_key(Path::new(program(command)));
    !stored.is_empty() && stored == path_key(exe)
}

/// A subkey of `HKEY_USERS` that is the hive of a person's account (local, domain or Entra ID), not of a service
/// account, the default profile or an account's class registrations.
pub fn is_user_hive(name: &str) -> bool {
    (name.starts_with("S-1-5-21-") || name.starts_with("S-1-12-1-"))
        && !name.to_ascii_lowercase().ends_with("_classes")
}

/// What Install does about the tray once the service is dealt with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrayPlan {
    /// Write the Run value for the signed-in user.
    pub register: bool,
    /// Start the tray in the user's session.
    pub start: bool,
}

/// `first_install`: there was no `crossc-tray.exe` in the install folder before this Install. `was_running`:
/// this Install ended a tray that ran at this desktop (in the installer's session) to replace its exe.
/// `still_running`: a tray at this desktop could not be ended.
///
/// Autostart is only registered with the first install: afterwards it is the user's setting (the tray's menu
/// item), and an update must not switch back on what the user switched off. The tray is started when it is new,
/// and started again when the update ended it; a tray the user had closed stays closed, whatever was ended in
/// another user's session. And none is started next to one that is still there: the new process would find the
/// old one and exit at once, with nothing started for all the log says.
pub fn tray_plan(first_install: bool, was_running: bool, still_running: bool) -> TrayPlan {
    TrayPlan {
        register: first_install,
        start: (first_install || was_running) && !still_running,
    }
}

/// What became of the trays `stop` found, by where they ran. `processes`: the Windows session of each (`None`:
/// it could not be told) and whether it is gone. `here`: the installer's own session.
///
/// A process whose session is not known counts as one of this desktop: better a tray started that was closed
/// than a user left without the one they had.
fn tally(processes: &[(Option<u32>, bool)], here: Option<u32>) -> Tally {
    let mut t = Tally::default();
    for (session, gone) in processes {
        let local = session.is_none() || here.is_none() || *session == here;
        match (local, gone) {
            (true, true) => t.ended_here += 1,
            (false, true) => t.ended_elsewhere += 1,
            (true, false) => t.left_here += 1,
            (false, false) => t.left_elsewhere += 1,
        }
    }
    t
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Tally {
    ended_here: usize,
    ended_elsewhere: usize,
    left_here: usize,
    left_elsewhere: usize,
}

// ---------------------------------------------------------------------------------------------------------------
// Running trays
// ---------------------------------------------------------------------------------------------------------------

/// A handle that is closed on drop.
struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: the handle is ours and still open.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// The path of the image a process runs.
fn image_path(process: &Owned) -> Option<PathBuf> {
    let mut buf = vec![0u16; 32768];
    let mut len = buf.len() as u32;
    // SAFETY: the buffer has `len` characters.
    unsafe {
        QueryFullProcessImageNameW(
            process.0,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .ok()?;
    }
    Some(PathBuf::from(OsString::from_wide(&buf[..len as usize])))
}

/// The Windows session a process runs in.
fn session_of(pid: u32) -> Option<u32> {
    let mut session = 0u32;
    // SAFETY: plain call with a u32 to fill.
    unsafe { ProcessIdToSessionId(pid, &mut session) }
        .ok()
        .map(|()| session)
}

/// The debug privilege, switched on in this process's token for as long as this lives. With it a process of
/// another account can be opened (its tray, when the installer was elevated with an administrator's
/// credentials or another user is signed in too). An elevated administrator's token holds the privilege,
/// switched off; a plain user's does not hold it, and then there is nothing to switch on (`None`).
struct DebugPrivilege {
    token: Owned,
    luid: LUID,
    /// It was off before: switch it off again.
    restore: bool,
}

impl DebugPrivilege {
    fn set(token: &Owned, luid: LUID, on: bool, old: Option<&mut TOKEN_PRIVILEGES>) -> bool {
        let new = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: if on {
                    SE_PRIVILEGE_ENABLED
                } else {
                    TOKEN_PRIVILEGES_ATTRIBUTES(0)
                },
            }],
        };
        let mut len = 0u32;
        // SAFETY: `new` and `old` are one TOKEN_PRIVILEGES each, and that size is passed. The call succeeds
        // also when the token does not hold the privilege; the last error then says so.
        unsafe {
            AdjustTokenPrivileges(
                token.0,
                false,
                Some(&new),
                size_of::<TOKEN_PRIVILEGES>() as u32,
                old.map(std::ptr::from_mut),
                Some(&mut len),
            )
            .is_ok()
                && GetLastError() != ERROR_NOT_ALL_ASSIGNED
        }
    }

    fn enable() -> Option<DebugPrivilege> {
        // SAFETY: plain calls; the token handle is closed on drop.
        let (token, luid) = unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                &mut token,
            )
            .ok()?;
            let token = Owned(token);
            let mut luid = LUID::default();
            LookupPrivilegeValueW(PCWSTR::null(), SE_DEBUG_NAME, &mut luid).ok()?;
            (token, luid)
        };
        let mut old = TOKEN_PRIVILEGES::default();
        if !Self::set(&token, luid, true, Some(&mut old)) {
            return None;
        }
        // The previous state lists what the call changed: nothing when the privilege was on already.
        let restore = old.PrivilegeCount == 1;
        Some(DebugPrivilege {
            token,
            luid,
            restore,
        })
    }
}

impl Drop for DebugPrivilege {
    fn drop(&mut self) {
        if self.restore {
            let _ = Self::set(&self.token, self.luid, false, None);
        }
    }
}

/// A running process whose image is the exe that was looked for.
struct Process {
    pid: u32,
    handle: Owned,
    /// The Windows session it runs in.
    session: Option<u32>,
}

/// Every running process whose image is `exe`, opened with `access` (and the right to read its image path).
/// The path is read from the handle that is returned, so a process id that was reused in between is not taken
/// for the tray.
///
/// The second number: processes with the same file name that could not be opened, so that nothing is known
/// about where they run from. Those are processes of another account (unelevated: of any other user; elevated,
/// only what the debug privilege does not open either, see [`stop`]).
fn running(exe: &Path, access: PROCESS_ACCESS_RIGHTS) -> (Vec<Process>, usize) {
    let mut found = Vec::new();
    let mut unreadable = 0;
    let Some(name) = exe.file_name().map(|n| n.to_string_lossy().to_lowercase()) else {
        return (found, unreadable);
    };
    let want = path_key(exe);
    // SAFETY: the snapshot handle is closed on drop; `entry` carries its own size.
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return (found, unreadable);
        };
        let snapshot = Owned(snapshot);
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut more = Process32FirstW(snapshot.0, &mut entry).is_ok();
        while more {
            let len = entry
                .szExeFile
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(entry.szExeFile.len());
            if String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase() == name {
                match OpenProcess(
                    access | PROCESS_QUERY_LIMITED_INFORMATION,
                    false,
                    entry.th32ProcessID,
                ) {
                    Ok(handle) => {
                        let handle = Owned(handle);
                        if image_path(&handle).is_some_and(|p| path_key(&p) == want) {
                            found.push(Process {
                                pid: entry.th32ProcessID,
                                handle,
                                session: session_of(entry.th32ProcessID),
                            });
                        }
                    }
                    Err(_) => unreadable += 1,
                }
            }
            more = Process32NextW(snapshot.0, &mut entry).is_ok();
        }
    }
    (found, unreadable)
}

/// How many processes run `exe` (the tray's icon is one, its open details window a second), as far as this
/// process may look.
pub fn running_count(exe: &Path) -> usize {
    running(exe, PROCESS_ACCESS_RIGHTS(0)).0.len()
}

/// What `stop` did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Stopped {
    /// Processes that ran `exe` in the installer's own session (at this desktop) and are gone now.
    pub ended_here: usize,
    /// The same in other sessions: another signed-in user's tray.
    pub ended_elsewhere: usize,
    /// Processes that run `exe` at this desktop and could not be ended.
    pub left_here: usize,
    /// The same in other sessions.
    pub left_elsewhere: usize,
    /// Processes of the same file name that could not be opened: if one of them runs `exe`, it still does.
    pub unreadable: usize,
    /// Why a process that runs `exe` is still there, when one is.
    pub error: Option<String>,
}

impl Stopped {
    /// Processes that ran `exe` and are gone now.
    pub fn ended(&self) -> usize {
        self.ended_here + self.ended_elsewhere
    }

    /// What to tell the user when processes were left alone; `None` when there were none.
    pub fn left_note(&self, exe: &Path) -> Option<String> {
        (self.unreadable > 0).then(|| {
            format!(
                "{} process(es) named {} belong to another account and could not be looked at. If one of \
                 them runs {}, it is still running and keeps that file in use: close it there (its menu, \
                 Exit)",
                self.unreadable,
                exe.file_name().unwrap_or_default().to_string_lossy(),
                exe.display()
            )
        })
    }
}

fn exited(process: &Owned) -> bool {
    // SAFETY: the handle has the SYNCHRONIZE right.
    unsafe { WaitForSingleObject(process.0, 0) == WAIT_OBJECT_0 }
}

/// Waits until every one of `processes` has exited; `false` when `limit` ran out first.
fn wait_exit(processes: &[Process], limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if processes.iter().all(|p| exited(&p.handle)) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Posts WM_CLOSE to the tray windows (class `crossc-tray`) of the processes in `pids`: the tray then takes its
/// icon out of the notification area and closes its details window before it exits. Only windows of this
/// desktop can be reached; returns how many were asked.
fn ask_to_close(pids: &[u32]) -> usize {
    let mut asked = 0;
    let mut after: Option<HWND> = None;
    // SAFETY: plain calls on window handles the system just returned.
    unsafe {
        while let Ok(hwnd) = FindWindowExW(None, after, w!("crossc-tray"), PCWSTR::null()) {
            if hwnd.0.is_null() {
                break;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pids.contains(&pid)
                && PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)).is_ok()
            {
                asked += 1;
            }
            after = Some(hwnd);
        }
    }
    asked
}

/// Ends every running process whose image is `exe`, so the file can be replaced or deleted: those this process
/// may open, in any session. What is still there afterwards is in the result, with the reason. The tray has no
/// state to lose.
///
/// The processes are opened with the debug privilege on, where this process holds it (elevated): a tray of
/// another account is then found and ended like the installer's own. That covers an installer elevated with an
/// administrator's credentials while a standard user's tray runs, and a second user who is signed in. The
/// privilege is off again before anything else is done; the handles keep the rights they were opened with.
pub fn stop(exe: &Path) -> Stopped {
    let (processes, unreadable) = {
        let _debug = DebugPrivilege::enable();
        running(exe, PROCESS_TERMINATE | PROCESS_SYNCHRONIZE)
    };
    let mut error = None;
    let pids: Vec<u32> = processes.iter().map(|p| p.pid).collect();
    if !processes.is_empty() && !(ask_to_close(&pids) > 0 && wait_exit(&processes, CLOSE_GRACE)) {
        let mut refused = Vec::new();
        for p in &processes {
            if !exited(&p.handle) {
                // SAFETY: the handle has the TERMINATE right.
                if let Err(e) = unsafe { TerminateProcess(p.handle.0, 1) } {
                    refused.push(format!("process {}: {}", p.pid, e.message()));
                }
            }
        }
        if !wait_exit(&processes, EXIT_WAIT) {
            error = Some(if refused.is_empty() {
                format!("{} is still running after it was ended", exe.display())
            } else {
                format!("cannot end {}: {}", exe.display(), refused.join("; "))
            });
        }
    }
    let fate: Vec<(Option<u32>, bool)> = processes
        .iter()
        .map(|p| (p.session, exited(&p.handle)))
        .collect();
    let t = tally(&fate, session_of(std::process::id()));
    Stopped {
        ended_here: t.ended_here,
        ended_elsewhere: t.ended_elsewhere,
        left_here: t.left_here,
        left_elsewhere: t.left_elsewhere,
        unreadable,
        error,
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The signed-in user
// ---------------------------------------------------------------------------------------------------------------

/// The user at the desktop this installer runs on: the SID as text, and the token of the desktop shell
/// (Explorer) when it can be had.
///
/// The token is there when the installer runs as the same account, elevated or not: the usual case. When it was
/// elevated with another account's credentials, the shell's token is that other user's and Windows does not
/// hand it out; the user is then still known by name (the session's user), which is enough for the autostart
/// value but not to start anything as that user.
pub struct DesktopUser {
    pub sid: String,
    token: Option<Owned>,
}

/// A SID as text (`S-1-5-21-...`).
///
/// # Safety
/// `sid` points to a valid SID.
unsafe fn sid_text(sid: PSID) -> Option<String> {
    // SAFETY: the caller's SID; the string is freed with LocalFree, as the function asks.
    unsafe {
        let mut text = PWSTR::null();
        ConvertSidToStringSidW(sid, &mut text).ok()?;
        let out = text.to_string().ok();
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
        out
    }
}

/// The SID of a token's user.
fn token_sid(token: &Owned) -> Option<String> {
    // SAFETY: the buffer is as large as the first call asked for and aligned for TOKEN_USER; the SID it
    // points into lives as long as the buffer.
    unsafe {
        let mut bytes = 0u32;
        let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut bytes);
        if bytes == 0 {
            return None;
        }
        let mut buf = vec![0u64; (bytes as usize).div_ceil(8)];
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            bytes,
            &mut bytes,
        )
        .ok()?;
        let user = &*buf.as_ptr().cast::<TOKEN_USER>();
        sid_text(user.User.Sid)
    }
}

/// The token of the process that owns the desktop shell's window. `None` when no shell runs on this desktop
/// (Explorer crashed or was replaced, a service session) or it is another account's and cannot be opened.
fn shell_token() -> Option<Owned> {
    // SAFETY: plain calls; every handle is closed on drop.
    unsafe {
        let shell = GetShellWindow();
        if shell.0.is_null() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(shell, Some(&mut pid));
        if pid == 0 {
            return None;
        }
        let process = Owned(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?);
        let mut token = HANDLE::default();
        OpenProcessToken(process.0, TOKEN_DUPLICATE | TOKEN_QUERY, &mut token).ok()?;
        Some(Owned(token))
    }
}

/// One text property of the session this process runs in; `None` when it is empty or cannot be read.
fn session_text(class: WTS_INFO_CLASS) -> Option<String> {
    let mut text = PWSTR::null();
    let mut bytes = 0u32;
    // SAFETY: the buffer is the system's and is freed with WTSFreeMemory, as the function asks.
    unsafe {
        WTSQuerySessionInformationW(None, WTS_CURRENT_SESSION, class, &mut text, &mut bytes)
            .ok()?;
        if text.is_null() {
            return None;
        }
        let out = text.to_string().ok();
        WTSFreeMemory(text.0.cast());
        out.filter(|s| !s.is_empty())
    }
}

/// The SID of the user who is signed in to this session, looked up by name. Needs no access to any of that
/// user's processes.
fn session_user_sid() -> Option<String> {
    let name = session_text(WTSUserName)?;
    let account = match session_text(WTSDomainName) {
        Some(domain) => format!(r"{domain}\{name}"),
        None => name,
    };
    let account = HSTRING::from(account);
    // SAFETY: both buffers are as large as the first call asked for; the SID buffer is aligned for a SID.
    unsafe {
        let (mut sid_bytes, mut domain_chars) = (0u32, 0u32);
        let mut kind = SID_NAME_USE::default();
        let _ = LookupAccountNameW(
            PCWSTR::null(),
            &account,
            None,
            &mut sid_bytes,
            None,
            &mut domain_chars,
            &mut kind,
        );
        if sid_bytes == 0 {
            return None;
        }
        let mut sid = vec![0u64; (sid_bytes as usize).div_ceil(8)];
        let mut domain = vec![0u16; domain_chars as usize + 1];
        let sid_ptr = PSID(sid.as_mut_ptr().cast());
        LookupAccountNameW(
            PCWSTR::null(),
            &account,
            Some(sid_ptr),
            &mut sid_bytes,
            Some(PWSTR(domain.as_mut_ptr())),
            &mut domain_chars,
            &mut kind,
        )
        .ok()?;
        sid_text(sid_ptr)
    }
}

/// `None` when nobody can be made out: no desktop shell whose token can be read, and no user of this session.
pub fn desktop_user() -> Option<DesktopUser> {
    if let Some(token) = shell_token()
        && let Some(sid) = token_sid(&token)
    {
        return Some(DesktopUser {
            sid,
            token: Some(token),
        });
    }
    session_user_sid().map(|sid| DesktopUser { sid, token: None })
}

/// Starts `exe` as `user`, with the shell's token: not elevated, whatever this process is. Needs the
/// impersonate privilege, which an elevated process has; unelevated it fails (and is not needed: nothing
/// unelevated installs).
pub fn start_as(user: &DesktopUser, exe: &Path) -> Result<(), String> {
    let fail = |what: &str, e: windows::core::Error| format!("{what}: {}", e.message());
    let Some(token) = &user.token else {
        return Err(format!(
            "cannot start {} from here: the desktop belongs to another account than the one this installer \
             runs as",
            exe.display()
        ));
    };
    let application = HSTRING::from(exe.as_os_str());
    let directory = exe.parent().map(|d| HSTRING::from(d.as_os_str()));
    // The function may write to the command line, so it gets a buffer of its own.
    let mut command: Vec<u16> = run_command(exe).encode_utf16().chain([0]).collect();
    // SAFETY: every pointer is to a live local; the handles and the environment block are released below.
    unsafe {
        let mut primary = HANDLE::default();
        DuplicateTokenEx(
            token.0,
            TOKEN_QUERY
                | TOKEN_ASSIGN_PRIMARY
                | TOKEN_DUPLICATE
                | TOKEN_ADJUST_DEFAULT
                | TOKEN_ADJUST_SESSIONID,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        )
        .map_err(|e| fail("cannot copy the desktop shell's token", e))?;
        let primary = Owned(primary);

        // The user's own environment, not this (elevated, perhaps another account's) process's.
        let mut environment = std::ptr::null_mut();
        let has_environment =
            CreateEnvironmentBlock(&mut environment, Some(primary.0), false).is_ok();
        let startup = STARTUPINFOW {
            cb: size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        };
        let mut info = PROCESS_INFORMATION::default();
        let created = CreateProcessWithTokenW(
            primary.0,
            CREATE_PROCESS_LOGON_FLAGS(0),
            &application,
            Some(PWSTR(command.as_mut_ptr())),
            if has_environment {
                CREATE_UNICODE_ENVIRONMENT
            } else {
                PROCESS_CREATION_FLAGS(0)
            },
            has_environment.then_some(environment.cast_const()),
            directory
                .as_ref()
                .map_or(PCWSTR::null(), |d| PCWSTR(d.as_ptr())),
            &startup,
            &mut info,
        );
        if has_environment {
            let _ = DestroyEnvironmentBlock(environment);
        }
        created.map_err(|e| fail(&format!("cannot start {}", exe.display()), e))?;
        let _ = CloseHandle(info.hThread);
        let _ = CloseHandle(info.hProcess);
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------
// Autostart
// ---------------------------------------------------------------------------------------------------------------

fn win32_text(e: WIN32_ERROR) -> String {
    windows::core::Error::from_hresult(e.to_hresult()).message()
}

/// The Run key of the user with this SID, below `HKEY_USERS`.
fn run_key_of(sid: &str) -> String {
    format!(r"{sid}\{RUN_KEY}")
}

fn write_value(root: HKEY, key: &str, name: &str, value: &str) -> Result<(), String> {
    let wide: Vec<u16> = value.encode_utf16().chain([0]).collect();
    // SAFETY: the data is `wide`, its size in bytes is passed.
    let r = unsafe {
        RegSetKeyValueW(
            root,
            &HSTRING::from(key),
            &HSTRING::from(name),
            REG_SZ.0,
            Some(wide.as_ptr().cast()),
            (wide.len() * 2) as u32,
        )
    };
    if r == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(win32_text(r))
    }
}

/// Deletes a value; one that is not there is fine.
fn delete_value(root: HKEY, key: &str, name: &str) -> Result<(), String> {
    // SAFETY: plain call.
    let r = unsafe { RegDeleteKeyValueW(root, &HSTRING::from(key), &HSTRING::from(name)) };
    if r == ERROR_SUCCESS || r == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(win32_text(r))
    }
}

/// The names of the subkeys of `root`.
fn subkeys(root: HKEY) -> Vec<String> {
    let mut names = Vec::new();
    for index in 0.. {
        let mut buf = [0u16; 256];
        let mut len = buf.len() as u32;
        // SAFETY: the buffer has `len` characters.
        let r = unsafe {
            RegEnumKeyExW(
                root,
                index,
                Some(PWSTR(buf.as_mut_ptr())),
                &mut len,
                None,
                None,
                None,
                None,
            )
        };
        if r != ERROR_SUCCESS {
            break;
        }
        names.push(String::from_utf16_lossy(&buf[..len as usize]));
    }
    names
}

/// Does `user` have the tray at `exe` started at sign-in?
pub fn autostart_enabled(user: &DesktopUser, exe: &Path) -> bool {
    reg_string(HKEY_USERS, &run_key_of(&user.sid), RUN_VALUE)
        .is_some_and(|command| command_starts(&command, exe))
}

/// Has the tray at `exe` started when `user` signs in.
pub fn enable_autostart(user: &DesktopUser, exe: &Path) -> Result<(), String> {
    write_value(
        HKEY_USERS,
        &run_key_of(&user.sid),
        RUN_VALUE,
        &run_command(exe),
    )
}

/// Removes the autostart of the tray at `exe` for every user whose hive is loaded (everyone who is signed in),
/// and only where the value starts that exe: a tray that someone runs from another folder keeps its autostart.
/// Returns how many values were removed and what could not be removed. A value in the hive of a user who is
/// not signed in stays; Windows skips a Run value whose program is gone.
pub fn remove_autostart(exe: &Path) -> (usize, Vec<String>) {
    let (mut removed, mut failed) = (0, Vec::new());
    for sid in subkeys(HKEY_USERS)
        .into_iter()
        .filter(|name| is_user_hive(name))
    {
        let key = run_key_of(&sid);
        if reg_string(HKEY_USERS, &key, RUN_VALUE).is_some_and(|c| command_starts(&c, exe)) {
            match delete_value(HKEY_USERS, &key, RUN_VALUE) {
                Ok(()) => removed += 1,
                Err(e) => failed.push(format!("{sid}: {e}")),
            }
        }
    }
    (removed, failed)
}

/// The state of the tray installed at `exe`, for the status. Works unelevated (it then sees the trays of the
/// same user only, which is the one that matters).
pub fn query(exe: &Path) -> TrayInfo {
    TrayInfo {
        installed: exe.is_file(),
        running: running_count(exe) > 0,
        autostart: desktop_user().map(|user| autostart_enabled(&user, exe)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::tests::TempDir;
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RegDeleteKeyW};
    use windows::Win32::System::Threading::GetCurrentProcess;

    #[test]
    fn the_run_value_is_the_one_the_tray_writes() {
        // crossc-tray's autostart::command: the path in quotes, nothing else. Its menu item is ticked only
        // for exactly this exe.
        let exe = Path::new(r"C:\Program Files\crossc\crossc-tray.exe");
        assert_eq!(
            run_command(exe),
            r#""C:\Program Files\crossc\crossc-tray.exe""#
        );
        assert_eq!(RUN_VALUE, "crossc-tray");
        assert_eq!(RUN_KEY, r"Software\Microsoft\Windows\CurrentVersion\Run");
        assert!(command_starts(&run_command(exe), exe));
    }

    #[test]
    fn a_stored_command_is_matched_by_its_program() {
        let exe = Path::new(r"C:\Program Files\crossc\crossc-tray.exe");
        for stored in [
            r#""C:\Program Files\crossc\crossc-tray.exe""#,
            r#""c:\program files\CROSSC\crossc-tray.exe" --demo"#,
            r#"  "C:/Program Files/crossc/crossc-tray.exe"  "#,
        ] {
            assert!(command_starts(stored, exe), "{stored}");
        }
        for stored in [
            "",
            "\"\"",
            r#""C:\src\crossc\target\release\crossc-tray.exe""#,
            r#""C:\Program Files\crossc\crossc-tray.exe.bak""#,
            // Unquoted with a space in it: Windows would not start our exe with this either.
            r"C:\Program Files\crossc\crossc-tray.exe",
            r#""C:\Program Files\crossc\other.exe" "C:\Program Files\crossc\crossc-tray.exe""#,
        ] {
            assert!(!command_starts(stored, exe), "{stored}");
        }
        assert!(command_starts(
            r"C:\crossc\crossc-tray.exe --demo",
            Path::new(r"C:\crossc\crossc-tray.exe")
        ));
    }

    #[test]
    fn only_accounts_of_people_are_swept() {
        for name in [
            "S-1-5-21-1111111111-2222222222-3333333333-1001",
            "S-1-12-1-1111111111-2222222222-3333333333-4444444444",
        ] {
            assert!(is_user_hive(name), "{name}");
            assert!(!is_user_hive(&format!("{name}_Classes")), "{name}");
            assert!(!is_user_hive(&format!("{name}_classes")), "{name}");
        }
        for name in [".DEFAULT", "S-1-5-18", "S-1-5-19", "S-1-5-20", "S-1-5-80-1"] {
            assert!(!is_user_hive(name), "{name}");
        }
    }

    #[test]
    fn autostart_is_registered_once_and_the_tray_comes_back_after_an_update() {
        let plan = |register, start| TrayPlan { register, start };
        // New on this PC: starts with Windows, and starts now.
        assert_eq!(tray_plan(true, false, false), plan(true, true));
        // An update that ended the running tray: it comes back; the user's autostart setting is not touched.
        assert_eq!(tray_plan(false, true, false), plan(false, true));
        // An update while the tray was closed: stays closed.
        assert_eq!(tray_plan(false, false, false), plan(false, false));
        assert_eq!(tray_plan(true, true, false), plan(true, true));
    }

    #[test]
    fn no_tray_is_started_next_to_one_that_could_not_be_ended() {
        // The new process would find the old one (the tray runs once per user) and exit at once: "started"
        // would be said of nothing. So: not started, whatever else was ended.
        let plan = |register, start| TrayPlan { register, start };
        assert_eq!(tray_plan(false, true, true), plan(false, false));
        assert_eq!(tray_plan(false, false, true), plan(false, false));
        // (Autostart is another matter: it is about the next sign-in.)
        assert_eq!(tray_plan(true, false, true), plan(true, false));
    }

    #[test]
    fn only_a_tray_ended_at_this_desktop_counts_as_having_run_here() {
        let t = |ended_here, ended_elsewhere, left_here, left_elsewhere| Tally {
            ended_here,
            ended_elsewhere,
            left_here,
            left_elsewhere,
        };
        // The installer runs in session 1. Another user's tray (session 2) was ended; the user at this
        // desktop had closed theirs: nothing ran here, so nothing is started here.
        let fate = [(Some(2), true)];
        assert_eq!(tally(&fate, Some(1)), t(0, 1, 0, 0));
        let stopped = Stopped {
            ended_elsewhere: 1,
            ..Stopped::default()
        };
        assert_eq!(stopped.ended(), 1);
        assert!(!tray_plan(false, stopped.ended_here > 0, stopped.left_here > 0).start);
        // Both ran: the one here comes back.
        let fate = [(Some(2), true), (Some(1), true), (Some(1), true)];
        assert_eq!(tally(&fate, Some(1)), t(2, 1, 0, 0));
        // One that would not go, here and elsewhere.
        let fate = [(Some(1), false), (Some(2), false), (Some(1), true)];
        assert_eq!(tally(&fate, Some(1)), t(1, 0, 1, 1));
        // A session that cannot be told counts as this desktop.
        assert_eq!(tally(&[(None, true)], Some(1)), t(1, 0, 0, 0));
        assert_eq!(tally(&[(Some(3), false)], None), t(0, 0, 1, 0));
        assert_eq!(tally(&[], Some(1)), Tally::default());
        // This test's own session is one Windows can tell.
        assert!(session_of(std::process::id()).is_some());
    }

    #[test]
    fn the_debug_privilege_is_only_had_elevated_and_is_given_back() {
        // A plain user's token (and an administrator's unelevated one) does not hold it: nothing is switched
        // on, and `stop` goes on with what it may open anyway.
        if !crate::system::is_admin() {
            assert!(DebugPrivilege::enable().is_none());
            return;
        }
        // Usually off until asked for; some elevated tokens (CI runners) have it on already, and then
        // nothing is to be given back.
        let first = DebugPrivilege::enable().expect("elevated: the token holds it");
        let was_off = first.restore;
        let second = DebugPrivilege::enable().expect("still there");
        assert!(!second.restore, "the inner one leaves it to the outer one");
        drop(second);
        drop(first);
        assert!(
            DebugPrivilege::enable().is_some_and(|p| p.restore == was_off),
            "given back to the state it was found in"
        );
    }

    /// The registry code on a throwaway key of the current user, never the real Run key.
    #[test]
    fn a_run_value_is_written_read_and_removed() {
        let key = format!(
            r"Software\crossc-installer-test-tray-{}",
            std::process::id()
        );
        let exe = Path::new(r"C:\Program Files\crossc\crossc-tray.exe");
        let read = || reg_string(HKEY_CURRENT_USER, &key, RUN_VALUE);
        assert_eq!(read(), None);
        // Deleting what is not there (not even the key) is fine.
        delete_value(HKEY_CURRENT_USER, &key, RUN_VALUE).unwrap();

        write_value(HKEY_CURRENT_USER, &key, RUN_VALUE, &run_command(exe)).unwrap();
        let stored = read().unwrap();
        assert_eq!(stored, r#""C:\Program Files\crossc\crossc-tray.exe""#);
        assert!(command_starts(&stored, exe));

        // The same value through HKEY_USERS\<SID>, the way Install and the status reach it. Only where a
        // desktop shell runs as the user the tests run as (not on a build server).
        let me = {
            let mut token = HANDLE::default();
            // SAFETY: plain call; the handle is closed on drop.
            unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).unwrap() };
            token_sid(&Owned(token)).unwrap()
        };
        assert!(me.starts_with("S-1-"), "{me}");
        // Both ways to the user agree: the shell's token, and the session's user looked up by name (what is
        // left when the shell is another account's).
        match (shell_token(), session_user_sid()) {
            (Some(token), Some(by_name)) => assert_eq!(token_sid(&token), Some(by_name)),
            (Some(_), None) => eprintln!("skipped: the session's user cannot be looked up"),
            (None, _) => eprintln!("skipped: no desktop shell here"),
        }
        match desktop_user() {
            Some(user) if user.sid == me => {
                assert!(is_user_hive(&user.sid), "{}", user.sid);
                assert!(subkeys(HKEY_USERS).contains(&user.sid));
                assert_eq!(
                    reg_string(HKEY_USERS, &format!(r"{}\{key}", user.sid), RUN_VALUE),
                    Some(stored)
                );
                // Reading the real Run key changes nothing.
                let _ = autostart_enabled(&user, exe);
            }
            Some(_) => eprintln!("skipped: the desktop belongs to another user"),
            None => eprintln!("skipped: no desktop user here"),
        }

        delete_value(HKEY_CURRENT_USER, &key, RUN_VALUE).unwrap();
        assert_eq!(read(), None);
        // SAFETY: plain call; the key is this test's own.
        unsafe {
            let _ = RegDeleteKeyW(HKEY_CURRENT_USER, &HSTRING::from(&key));
        }
    }

    #[test]
    fn a_process_is_found_by_the_path_of_its_image() {
        let me = std::env::current_exe().unwrap();
        let (found, _) = running(&me, PROCESS_ACCESS_RIGHTS(0));
        let this = found
            .iter()
            .find(|p| p.pid == std::process::id())
            .unwrap_or_else(|| panic!("{} not found", me.display()));
        assert_eq!(this.session, session_of(std::process::id()));
        // The same file name in another folder is another program.
        let t = TempDir::new("tray-running");
        let elsewhere = t.0.join(me.file_name().unwrap());
        assert_eq!(running_count(&elsewhere), 0);
        assert_eq!(stop(&elsewhere), Stopped::default());
        let info = query(&elsewhere);
        assert!(!info.installed && !info.running);
    }

    /// The real start needs an elevated process (the impersonate privilege) and would put a tray on the
    /// desktop, so it is not run here. Unelevated, everything up to the call itself runs (the shell's token is
    /// copied, the user's environment built and released) and Windows then refuses: nothing is started.
    #[test]
    fn unelevated_the_start_is_refused() {
        if crate::system::is_admin() {
            eprintln!("skipped: elevated");
            return;
        }
        let Some(user) = desktop_user().filter(|user| user.token.is_some()) else {
            eprintln!("skipped: no desktop shell here");
            return;
        };
        // Known by name only (the shell is another account's): said, and nothing is tried.
        let by_name = DesktopUser {
            sid: user.sid.clone(),
            token: None,
        };
        let e = start_as(&by_name, Path::new(r"C:\x\crossc-tray.exe")).unwrap_err();
        assert!(e.contains("another account"), "{e}");

        let t = TempDir::new("tray-start");
        let exe = t.0.join("no-such-tray.exe");
        let e = start_as(&user, &exe).unwrap_err();
        assert!(e.starts_with("cannot start "), "{e}");
        eprintln!("{e}");
    }

    /// Not a test: the process `a_running_copy_is_ended` starts and ends. It only sleeps when it is asked to.
    #[test]
    fn sleeper_for_the_stop_test() {
        if std::env::var_os("CROSSC_TEST_SLEEP").is_some() {
            std::thread::sleep(Duration::from_secs(60));
        }
    }

    #[test]
    fn a_running_copy_is_ended() {
        // A copy of this test program under a name of its own stands in for the tray: it has no window, so it
        // is terminated, and afterwards its file can be replaced.
        let t = TempDir::new("tray-stop");
        let exe = t.0.join("crossc-tray-stand-in.exe");
        std::fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
        let mut child = std::process::Command::new(&exe)
            .args(["--exact", "tray::tests::sleeper_for_the_stop_test"])
            .env("CROSSC_TEST_SLEEP", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while running_count(&exe) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(running_count(&exe), 1);
        assert!(query(&exe).running);

        let started = Instant::now();
        let stopped = stop(&exe);
        // It ran in this session, so it is one to start again.
        assert_eq!(
            stopped,
            Stopped {
                ended_here: 1,
                ..Stopped::default()
            }
        );
        assert_eq!(stopped.left_note(&exe), None);
        // A process of that name that could not be opened is said, with what to do about it.
        let unreadable = Stopped {
            unreadable: 2,
            ..Stopped::default()
        };
        let note = unreadable
            .left_note(Path::new(r"C:\Program Files\crossc\crossc-tray.exe"))
            .unwrap();
        assert!(
            note.starts_with("2 process(es) named crossc-tray.exe belong to another account"),
            "{note}"
        );
        let status = child.wait().unwrap();
        assert_eq!(status.code(), Some(1), "ended by stop, not by itself");
        assert!(started.elapsed() < Duration::from_secs(30));
        assert_eq!(running_count(&exe), 0);
        std::fs::write(&exe, "replaced").unwrap();
    }
}
