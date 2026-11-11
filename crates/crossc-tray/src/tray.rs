//! The notification-area icon: a hidden window, Shell_NotifyIconW, a
//! context menu. Plain Win32 and nothing else, so the process that sits in
//! the tray all day is small and sleeps until the service sends a
//! snapshot. The details window is another process of the same exe
//! (`--flyout`, see `flyout`), started on a click and gone when it closes.
//!
//! Threads: the window's thread (this file) and the status client's
//! thread, which leaves the newest state in [`PENDING`] and posts one
//! message. The state of the tray lives in a thread-local; no borrow of it
//! is held across a call that runs a nested message loop (the menu, a
//! message box, ShellExecute).

use std::cell::RefCell;
use std::os::windows::io::AsRawHandle;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, FILETIME, GetLastError, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT,
    WPARAM,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Threading::{CreateMutexW, GetProcessTimes};
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION,
    NIN_SELECT, NINF_KEY, NOTIFYICON_VERSION_4, NOTIFYICONDATAW, NOTIFYICONIDENTIFIER,
    Shell_NotifyIconGetRect, Shell_NotifyIconW, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, HICON,
    KillTimer, MB_ICONINFORMATION, MB_OK, MF_CHECKED, MF_SEPARATOR, MF_STRING, MSG, MessageBoxW,
    PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SM_CXSMICON,
    SW_SHOWNORMAL, SetForegroundWindow, SetMenuDefaultItem, SetTimer, TPM_BOTTOMALIGN,
    TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTALIGN, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage,
    WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_ENDSESSION,
    WM_NULL, WM_SETTINGCHANGE, WM_THEMECHANGED, WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{HSTRING, PCWSTR, w};

use crate::args::{Args, flyout_args};
use crate::view::{self, Feed, Glyph, Rect};
use crate::{autostart, demo, icon, source};

/// The notification icon's message to the window.
const WM_TRAY: u32 = WM_APP + 1;
/// A new state waits in [`PENDING`].
const WM_FEED: u32 = WM_APP + 2;
const ICON_ID: u32 = 1;

const TIMER_DEMO: usize = 1;
const TIMER_ADD: usize = 2;
/// The demo's states change at most this often.
const DEMO_TICK_MS: u32 = 500;
/// A failed NIM_ADD (Explorer not up yet at logon) is repeated this often.
const ADD_RETRY_MS: u32 = 2_000;
/// And this many times; after that the TaskbarCreated message brings the
/// icon back whenever Explorer starts.
const ADD_RETRIES: u32 = 30;

const CMD_OPEN: usize = 1;
const CMD_COPY: usize = 2;
const CMD_LOG: usize = 3;
const CMD_AUTOSTART: usize = 4;
const CMD_EXIT: usize = 5;

/// A click this soon after the flyout closed is the click that closed it
/// (the flyout closes when it loses focus, and clicking the icon takes the
/// focus away): it must not open the flyout again.
pub const REOPEN_GUARD: Duration = Duration::from_millis(400);

/// The newest state from the client thread.
static PENDING: Mutex<Option<Feed>> = Mutex::new(None);
/// A WM_FEED is on its way.
static POSTED: AtomicBool = AtomicBool::new(false);

struct Tray {
    hwnd: HWND,
    args: Args,
    exe: PathBuf,
    feed: Feed,
    /// What the icon shows now.
    shown: Option<(Glyph, String, bool, i32)>,
    icon: Option<HICON>,
    /// NIM_ADD succeeded.
    added: bool,
    add_tries: u32,
    taskbar_created: u32,
    flyout: Option<Child>,
    /// When that process was started.
    flyout_started: Instant,
    /// When the demo started, Unix ms.
    demo_epoch: u64,
}

thread_local! {
    static TRAY: RefCell<Option<Tray>> = const { RefCell::new(None) };
}

/// Run `f` on the tray's state. `None` if there is none, or if it is in
/// use further up the stack (which the rule in the module docs prevents).
fn with<R>(f: impl FnOnce(&mut Tray) -> R) -> Option<R> {
    TRAY.with(|t| t.try_borrow_mut().ok()?.as_mut().map(f))
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// A FILETIME (100 ns since 1601) as Unix milliseconds.
pub fn filetime_to_unix_ms(ft: u64) -> u64 {
    (ft / 10_000).saturating_sub(11_644_473_600_000)
}

/// What a click on the icon does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Click {
    /// The flyout is open: close it.
    Close,
    /// The flyout closed this very moment, because of this click: nothing.
    Nothing,
    Open,
}

/// `running`: the flyout's process lives, and since when. `closed_ago`:
/// how long ago the last one ended, if one did.
pub fn click(running: Option<Duration>, closed_ago: Option<Duration>) -> Click {
    if let Some(since) = running {
        // The second click of a double click (or a key that repeats) must
        // not close what the first one just opened.
        if since < REOPEN_GUARD {
            Click::Nothing
        } else {
            Click::Close
        }
    } else if closed_ago.is_some_and(|d| d < REOPEN_GUARD) {
        Click::Nothing
    } else {
        Click::Open
    }
}

/// Is `path` a plain local directory path ("X:\...")? The log folder comes
/// from the pipe, and whatever serves the pipe must not get this program to
/// open a network path or a shell namespace.
pub fn plain_local_path(path: &str) -> bool {
    let b = path.as_bytes();
    b.len() >= 3
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && b[2] == b'\\'
        && !path.contains("..")
        && !path
            .chars()
            .any(|c| c.is_control() || "<>\"|?*".contains(c))
        && !path[2..].contains(':')
}

/// The folder "Open service log folder" opens: the one the service names,
/// else where the service logs by default.
pub fn log_folder(feed: &Feed, program_data: Option<&str>) -> Option<String> {
    let named = match feed {
        Feed::Live(s) if !s.service.log_dir.is_empty() => Some(s.service.log_dir.clone()),
        _ => None,
    };
    named
        .or_else(|| program_data.map(|p| format!(r"{}\crossc", p.trim_end_matches('\\'))))
        .filter(|p| plain_local_path(p))
}

impl Tray {
    fn data(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: ICON_ID,
            ..Default::default()
        }
    }

    /// Bring the icon and the tooltip in line with the state. `force`:
    /// also when nothing seems to have changed (the icon was lost).
    fn refresh(&mut self, force: bool) {
        let glyph = view::glyph(&self.feed);
        let tip = view::tooltip(&self.feed);
        let light = autostart::taskbar_is_light();
        // SAFETY: plain calls on our window.
        let px = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForWindow(self.hwnd)) }.max(16);
        let want = (glyph, tip, light, px);
        if !force && self.added && self.shown.as_ref() == Some(&want) {
            return;
        }
        let picture_changed = self
            .shown
            .as_ref()
            .is_none_or(|s| (s.0, s.2, s.3) != (want.0, want.2, want.3));
        if picture_changed || self.icon.is_none() {
            let new = icon::to_hicon(&icon::render(glyph, px as usize, !light));
            if let Some(old) = std::mem::replace(&mut self.icon, new) {
                // SAFETY: an icon this program created, no longer shown
                // once the NIM_MODIFY below has run; the shell copies it.
                unsafe {
                    let _ = DestroyIcon(old);
                }
            }
        }
        let mut data = self.data();
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        data.uCallbackMessage = WM_TRAY;
        data.hIcon = self.icon.unwrap_or_default();
        for (dst, src) in data.szTip.iter_mut().zip(want.1.encode_utf16().take(127)) {
            *dst = src;
        }
        // SAFETY: `data` is a complete NOTIFYICONDATAW for our window.
        unsafe {
            if self.added && !Shell_NotifyIconW(NIM_MODIFY, &data).as_bool() {
                // The shell forgot the icon (Explorer restarted without
                // the broadcast reaching us): add it again.
                self.added = false;
            }
            if !self.added {
                self.added = Shell_NotifyIconW(NIM_ADD, &data).as_bool();
                if self.added {
                    data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
                    let _ = Shell_NotifyIconW(NIM_SETVERSION, &data);
                    let _ = KillTimer(Some(self.hwnd), TIMER_ADD);
                    self.add_tries = 0;
                } else if self.add_tries < ADD_RETRIES {
                    self.add_tries += 1;
                    SetTimer(Some(self.hwnd), TIMER_ADD, ADD_RETRY_MS, None);
                } else {
                    let _ = KillTimer(Some(self.hwnd), TIMER_ADD);
                }
            }
        }
        self.shown = Some(want);
    }

    fn remove(&mut self) {
        if self.added {
            // SAFETY: as above.
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &self.data());
            }
            self.added = false;
        }
        if let Some(icon) = self.icon.take() {
            // SAFETY: ours, and no longer shown.
            unsafe {
                let _ = DestroyIcon(icon);
            }
        }
    }

    /// The icon's rectangle on the screen, physical pixels.
    fn anchor(&self) -> Option<Rect> {
        let id = NOTIFYICONIDENTIFIER {
            cbSize: size_of::<NOTIFYICONIDENTIFIER>() as u32,
            hWnd: self.hwnd,
            uID: ICON_ID,
            ..Default::default()
        };
        // SAFETY: `id` is complete.
        let r: RECT = unsafe { Shell_NotifyIconGetRect(&id) }.ok()?;
        Some(Rect {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        })
    }

    /// (for how long the flyout's process has lived, if it does; how long
    /// ago the last one ended)
    fn flyout_state(&mut self) -> (Option<Duration>, Option<Duration>) {
        let Some(child) = &mut self.flyout else {
            return (None, None);
        };
        match child.try_wait() {
            Ok(None) => (Some(self.flyout_started.elapsed()), None),
            _ => {
                let mut times = [FILETIME::default(); 4];
                let [created, exited, kernel, user] = &mut times;
                // SAFETY: the child's handle is open as long as `child`.
                let ok = unsafe {
                    GetProcessTimes(HANDLE(child.as_raw_handle()), created, exited, kernel, user)
                }
                .is_ok();
                let exit =
                    (u64::from(times[1].dwHighDateTime) << 32) | u64::from(times[1].dwLowDateTime);
                let ago = ok.then(|| {
                    Duration::from_millis(unix_ms().saturating_sub(filetime_to_unix_ms(exit)))
                });
                (None, ago)
            }
        }
    }

    fn close_flyout(&mut self) {
        if let Some(mut child) = self.flyout.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// A click on the icon (or "Open details").
    fn toggle_flyout(&mut self, from_menu: bool) {
        let (running, closed_ago) = self.flyout_state();
        match click(running, closed_ago) {
            Click::Close if from_menu => {} // it is open: that is what was asked for
            Click::Close => self.close_flyout(),
            // (From the menu, "nothing" with no flyout running is the
            // guard against the click that closed it: the menu means open.)
            Click::Nothing if !from_menu || running.is_some() => {}
            Click::Nothing | Click::Open => {
                self.flyout = None;
                // The icon's rectangle; if the shell does not tell, the
                // pointer is on or near the icon.
                let anchor = self.anchor().or_else(|| {
                    let mut p = POINT::default();
                    // SAFETY: plain call.
                    unsafe { GetCursorPos(&mut p) }.ok().map(|()| Rect {
                        left: p.x,
                        top: p.y,
                        right: p.x + 1,
                        bottom: p.y + 1,
                    })
                });
                let spawned = Command::new(&self.exe)
                    .args(flyout_args(&self.args, anchor, self.demo_epoch))
                    .spawn();
                if let Ok(child) = spawned {
                    // The click made this process the foreground one; hand
                    // that right on, or the flyout opens behind everything
                    // and never gets the focus it closes on losing.
                    // SAFETY: plain calls.
                    unsafe {
                        let _ = SetForegroundWindow(self.hwnd);
                        let _ = AllowSetForegroundWindow(child.id());
                    }
                    self.flyout = Some(child);
                    self.flyout_started = Instant::now();
                }
            }
        }
    }
}

fn copy_to_clipboard(hwnd: HWND, text: &str) {
    const CF_UNICODETEXT: u32 = 13;
    let wide: Vec<u16> = text
        .replace('\n', "\r\n")
        .encode_utf16()
        .chain([0])
        .collect();
    // SAFETY: the block is allocated with room for `wide`, filled while
    // locked, and handed to the clipboard, which owns it from then on.
    unsafe {
        if OpenClipboard(Some(hwnd)).is_err() {
            return;
        }
        let _ = EmptyClipboard();
        if let Ok(mem) = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2) {
            let p = GlobalLock(mem).cast::<u16>();
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
                let _ = GlobalUnlock(mem);
                let _ = SetClipboardData(CF_UNICODETEXT, Some(HANDLE(mem.0)));
            }
        }
        let _ = CloseClipboard();
    }
}

fn open_log_folder(hwnd: HWND, feed: &Feed) {
    let program_data = std::env::var("ProgramData").ok();
    let folder = log_folder(feed, program_data.as_deref());
    match folder.filter(|f| std::path::Path::new(f).is_dir()) {
        // "explore" opens a folder window and nothing else: a path that is
        // a program is not run.
        // SAFETY: plain call with strings that outlive it.
        Some(f) => unsafe {
            ShellExecuteW(
                Some(hwnd),
                w!("explore"),
                &HSTRING::from(f),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            );
        },
        // SAFETY: plain call.
        None => unsafe {
            MessageBoxW(
                Some(hwnd),
                w!(
                    "The crossc service's log folder does not exist (yet). The service creates it when it first runs."
                ),
                w!("crossc"),
                MB_OK | MB_ICONINFORMATION,
            );
        },
    }
}

/// The context menu at (x, y). Runs a nested message loop: the tray's
/// state is not borrowed while it is up.
fn show_menu(hwnd: HWND, x: i32, y: i32) {
    let Some((exe, feed)) = with(|t| (t.exe.clone(), t.feed.clone())) else {
        return;
    };
    let auto = autostart::enabled(autostart::RUN_KEY, &exe);
    // SAFETY: the menu is created, shown and destroyed here; the strings
    // are static.
    let cmd = unsafe {
        let Ok(menu) = CreatePopupMenu() else {
            return;
        };
        let _ = AppendMenuW(menu, MF_STRING, CMD_OPEN, w!("Open details"));
        let _ = AppendMenuW(menu, MF_STRING, CMD_COPY, w!("Copy details"));
        let _ = AppendMenuW(menu, MF_STRING, CMD_LOG, w!("Open service log folder"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(
            menu,
            MF_STRING | if auto { MF_CHECKED } else { Default::default() },
            CMD_AUTOSTART,
            w!("Start with Windows"),
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, CMD_EXIT, w!("Exit"));
        let _ = SetMenuDefaultItem(menu, CMD_OPEN as u32, 0);
        // Without this the menu does not close when the user clicks
        // elsewhere; the WM_NULL afterwards is the other half of that.
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON | TPM_RIGHTALIGN | TPM_BOTTOMALIGN,
            x,
            y,
            None,
            hwnd,
            None,
        );
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        cmd.0 as usize
    };
    match cmd {
        CMD_OPEN => {
            with(|t| t.toggle_flyout(true));
        }
        CMD_COPY => copy_to_clipboard(hwnd, &view::report(&feed)),
        CMD_LOG => open_log_folder(hwnd, &feed),
        CMD_AUTOSTART => {
            if let Err(e) = autostart::set(autostart::RUN_KEY, &exe, !auto) {
                let text = format!("Could not change the autostart setting: {e}");
                // SAFETY: plain call.
                unsafe {
                    MessageBoxW(
                        Some(hwnd),
                        &HSTRING::from(text),
                        w!("crossc"),
                        MB_OK | MB_ICONINFORMATION,
                    );
                }
            }
        }
        CMD_EXIT => {
            // SAFETY: our window.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
        _ => {}
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_FEED => {
            POSTED.store(false, Ordering::SeqCst);
            let feed = PENDING.lock().ok().and_then(|mut p| p.take());
            if let Some(feed) = feed {
                with(|t| {
                    t.feed = feed;
                    t.refresh(false);
                });
            }
            LRESULT(0)
        }
        WM_TRAY => {
            // Version 4: the event in the low word of lParam, the anchor
            // point in wParam.
            let event = (lparam.0 as u32) & 0xFFFF;
            if event == NIN_SELECT || event == (NIN_SELECT | NINF_KEY) {
                with(|t| t.toggle_flyout(false));
            } else if event == WM_CONTEXTMENU {
                let mut p = POINT {
                    x: i32::from((wparam.0 & 0xFFFF) as u16 as i16),
                    y: i32::from(((wparam.0 >> 16) & 0xFFFF) as u16 as i16),
                };
                if p.x == -1 && p.y == -1 {
                    // SAFETY: plain call.
                    unsafe {
                        let _ = GetCursorPos(&mut p);
                    }
                }
                show_menu(hwnd, p.x, p.y);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            match wparam.0 {
                TIMER_DEMO => {
                    with(|t| {
                        let elapsed = unix_ms().saturating_sub(t.demo_epoch);
                        let feed = match &t.args.demo_state {
                            Some(name) => demo::state(name, elapsed),
                            None => Some(demo::feed_at(elapsed)),
                        };
                        if let Some(feed) = feed {
                            t.feed = feed;
                            t.refresh(false);
                        }
                    });
                }
                TIMER_ADD => {
                    with(|t| t.refresh(true));
                }
                _ => {}
            }
            LRESULT(0)
        }
        // The colours, the DPI or the theme changed: draw the icon anew.
        WM_SETTINGCHANGE | WM_DPICHANGED | WM_DISPLAYCHANGE | WM_THEMECHANGED => {
            with(|t| t.refresh(false));
            // SAFETY: default handling of a message for our window.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_ENDSESSION => {
            if wparam.0 != 0 {
                with(|t| {
                    t.close_flyout();
                    t.remove();
                });
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            // SAFETY: our window.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            with(|t| {
                t.close_flyout();
                t.remove();
            });
            // SAFETY: plain call.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => {
            // Explorer (re)started: every icon is gone, add ours again.
            let created = with(|t| t.taskbar_created).unwrap_or(0);
            if created != 0 && msg == created {
                with(|t| {
                    t.added = false;
                    t.add_tries = 0;
                    t.refresh(true);
                });
                return LRESULT(0);
            }
            // SAFETY: default handling of a message for our window.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
    }
}

/// Take the single-instance mutex. `None`: another tray of this kind runs
/// in this session.
fn single_instance(demo: bool) -> Option<HANDLE> {
    let name = if demo {
        w!("Local\\crossc-tray-demo")
    } else {
        w!("Local\\crossc-tray")
    };
    // SAFETY: plain calls. The handle is kept until the process ends.
    unsafe {
        let h = CreateMutexW(None, false, name).ok()?;
        (GetLastError() != ERROR_ALREADY_EXISTS).then_some(h)
    }
}

/// Run the tray icon until "Exit" (or the session ends). Returns the
/// process's exit code.
pub fn run(args: Args) -> i32 {
    let Some(_instance) = single_instance(args.demo) else {
        return 0; // already running: nothing to do
    };
    let Ok(exe) = std::env::current_exe() else {
        return 1;
    };
    // SAFETY: the usual window setup; the class and the window live until
    // the process ends, `wndproc` is the function above.
    let hwnd = unsafe {
        let Ok(module) = GetModuleHandleW(None) else {
            return 1;
        };
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: module.into(),
            lpszClassName: w!("crossc-tray"),
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            return 1;
        }
        // A top-level window that is never shown (a message-only window
        // would not get the TaskbarCreated broadcast).
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            w!("crossc-tray"),
            w!("crossc"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(module.into()),
            None,
        ) else {
            return 1;
        };
        hwnd
    };
    let demo_epoch = args.demo_epoch.unwrap_or_else(unix_ms);
    let demo = args.demo;
    let pipe = args.pipe.clone();
    TRAY.with(|t| {
        *t.borrow_mut() = Some(Tray {
            hwnd,
            args,
            exe,
            feed: Feed::Connecting,
            shown: None,
            icon: None,
            added: false,
            add_tries: 0,
            // SAFETY: plain call.
            taskbar_created: unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) },
            flyout: None,
            flyout_started: Instant::now(),
            demo_epoch,
        });
    });
    with(|t| t.refresh(true));

    // Where the states come from.
    let _client = if demo {
        // SAFETY: a timer on our window.
        unsafe { SetTimer(Some(hwnd), TIMER_DEMO, DEMO_TICK_MS, None) };
        None
    } else {
        let target = hwnd.0 as isize;
        source::connect(pipe, move |feed| {
            if let Ok(mut p) = PENDING.lock() {
                *p = Some(feed);
            }
            // One message however many states arrive before it is handled.
            if !POSTED.swap(true, Ordering::SeqCst) {
                // SAFETY: posting to a window handle; if the window is gone
                // the call fails and nothing happens.
                unsafe {
                    let _ = PostMessageW(
                        Some(HWND(target as *mut core::ffi::c_void)),
                        WM_FEED,
                        WPARAM(0),
                        LPARAM(0),
                    );
                }
            }
        })
    };

    let mut msg = MSG::default();
    // SAFETY: the message loop of this thread.
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    TRAY.with(|t| t.borrow_mut().take());
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossc_status::{ServiceInfo, Snapshot};

    #[test]
    fn a_click_opens_closes_or_is_the_click_that_closed() {
        let long = Some(Duration::from_secs(5));
        assert_eq!(click(None, None), Click::Open);
        assert_eq!(click(long, None), Click::Close);
        assert_eq!(click(long, Some(Duration::ZERO)), Click::Close);
        // A double click: the second one finds the flyout just started.
        assert_eq!(
            click(Some(Duration::from_millis(120)), None),
            Click::Nothing
        );
        assert_eq!(click(Some(REOPEN_GUARD), None), Click::Close);
        assert_eq!(click(None, Some(Duration::from_millis(30))), Click::Nothing);
        assert_eq!(click(None, Some(REOPEN_GUARD)), Click::Open);
        assert_eq!(click(None, Some(Duration::from_secs(60))), Click::Open);
    }

    #[test]
    fn filetime_conversion() {
        // 1970-01-01 in FILETIME units.
        assert_eq!(filetime_to_unix_ms(116_444_736_000_000_000), 0);
        assert_eq!(
            filetime_to_unix_ms(116_444_736_000_000_000 + 15_000_000),
            1_500
        );
        assert_eq!(
            filetime_to_unix_ms(0),
            0,
            "an unset time does not underflow"
        );
    }

    fn live(log_dir: &str) -> Feed {
        Feed::Live(Box::new(Snapshot {
            version: 1,
            service: ServiceInfo {
                log_dir: log_dir.into(),
                ..ServiceInfo::default()
            },
            ..Snapshot::default()
        }))
    }

    #[test]
    fn only_a_plain_local_folder_is_opened() {
        for ok in [r"C:\ProgramData\crossc", r"D:\logs", r"c:\a b\c"] {
            assert!(plain_local_path(ok), "{ok}");
        }
        for bad in [
            "",
            "C:",
            r"\\server\share\logs",
            r"\\?\C:\ProgramData\crossc",
            r"\\.\pipe\x",
            "shell:startup",
            "https://example.com/",
            r"C:\ProgramData\..\Windows",
            r"C:\x\file.txt:stream",
            r"relative\path",
            "C:/forward/slashes",
            "C:\\a\nb",
            r#"C:\a"b"#,
        ] {
            assert!(!plain_local_path(bad), "{bad:?}");
        }
    }

    #[test]
    fn the_log_folder_is_the_services_or_the_default() {
        let pd = Some(r"C:\ProgramData");
        assert_eq!(
            log_folder(&live(r"D:\crossc-logs"), pd).as_deref(),
            Some(r"D:\crossc-logs")
        );
        assert_eq!(
            log_folder(&live(""), pd).as_deref(),
            Some(r"C:\ProgramData\crossc")
        );
        assert_eq!(
            log_folder(&Feed::Connecting, Some(r"C:\ProgramData\")).as_deref(),
            Some(r"C:\ProgramData\crossc")
        );
        // What the pipe names is not trusted.
        assert_eq!(log_folder(&live(r"\\evil\share"), pd), None);
        assert_eq!(log_folder(&live("calc.exe"), pd), None);
        assert_eq!(log_folder(&Feed::Connecting, None), None);
    }
}
