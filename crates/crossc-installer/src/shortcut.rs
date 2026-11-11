//! The Start menu entry of the tray app: `crossc tray.lnk` in a `crossc` folder of the all-users Start menu
//! (`%ProgramData%\Microsoft\Windows\Start Menu\Programs\crossc`), made with the Shell's own `ShellLink`
//! object (`IShellLinkW` + `IPersistFile`), so it is the same kind of shortcut Explorer makes.
//!
//! Install writes it every time (a shortcut that is there already is written over: an update refreshes it),
//! Uninstall deletes it and the `crossc` folder if nothing else is left in it. The installer itself gets no
//! entry: Install does not copy `crossc-installer.exe` anywhere, it runs from the unpacked bundle (or the
//! checkout, or a downloaded release folder that a later release prunes), so there is no stable file to point at.
//!
//! What the shortcut holds and where it goes is decided by the pure functions at the top; the COM calls below
//! them are thin.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
    CoInitializeEx, CoUninitialize, IPersistFile, STGM_READ,
};
use windows::Win32::UI::Shell::{IShellLinkW, SLGP_RAWPATH, ShellLink};
use windows::core::{HSTRING, Interface};

/// The folder below the Start menu's Programs folder that holds crossc's entries.
pub const START_MENU_DIR: &str = "crossc";
/// The tray app's entry, as the Start menu lists it ("crossc tray").
pub const TRAY_SHORTCUT: &str = "crossc tray.lnk";
/// The shortcut's comment (the tooltip in the Start menu and Explorer).
pub const TRAY_DESCRIPTION: &str = "Shows what the crossc service is sending to the Galaxy Buds";

// ---------------------------------------------------------------------------------------------------------------
// Pure
// ---------------------------------------------------------------------------------------------------------------

/// crossc's folder in the Start menu: `<Programs>\crossc`. `common_programs` is the all-users Programs folder as
/// the system reports it (`system::common_programs`); `None` (not reported, or not an absolute path) leaves no
/// folder to use: there is no fallback to guess at, and the shortcut is then skipped with a warning.
pub fn start_menu_dir(common_programs: Option<&Path>) -> Option<PathBuf> {
    common_programs
        .filter(|p| p.is_absolute())
        .map(|p| p.join(START_MENU_DIR))
}

/// The tray's shortcut in crossc's Start menu folder.
pub fn tray_shortcut(dir: &Path) -> PathBuf {
    dir.join(TRAY_SHORTCUT)
}

/// What a shortcut holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// The program it starts.
    pub target: PathBuf,
    /// The folder that program starts in.
    pub working_dir: PathBuf,
    pub description: String,
    /// The file whose first icon the shortcut shows.
    pub icon: PathBuf,
}

impl Link {
    /// The shortcut to the installed tray at `exe`: it starts in the exe's folder and shows the exe's icon.
    pub fn tray(exe: &Path) -> Link {
        Link {
            target: exe.to_path_buf(),
            working_dir: exe.parent().map(Path::to_path_buf).unwrap_or_default(),
            description: TRAY_DESCRIPTION.to_string(),
            icon: exe.to_path_buf(),
        }
    }
}

/// What `remove` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Removed {
    /// The shortcut was there and is deleted.
    pub shortcut: bool,
    /// crossc's Start menu folder was empty afterwards and is deleted.
    pub dir: bool,
}

// ---------------------------------------------------------------------------------------------------------------
// COM
// ---------------------------------------------------------------------------------------------------------------

/// COM, initialised on this thread for as long as this lives. Only a call that succeeded is balanced with
/// `CoUninitialize`: on a thread that has COM in the other apartment model already (`RPC_E_CHANGED_MODE`) the
/// ShellLink object works all the same, and that initialisation is not this one's to end.
struct Com {
    initialised: bool,
}

impl Com {
    fn init() -> Result<Com, String> {
        // SAFETY: plain call; balanced in `drop` when it succeeded.
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        if hr.is_ok() {
            Ok(Com { initialised: true })
        } else if hr == RPC_E_CHANGED_MODE {
            Ok(Com { initialised: false })
        } else {
            Err(format!("cannot initialise COM: {}", hr.message()))
        }
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.initialised {
            // SAFETY: balances the successful CoInitializeEx of `init` on this thread.
            unsafe { CoUninitialize() };
        }
    }
}

fn shell_link() -> Result<IShellLinkW, String> {
    // SAFETY: plain call; the interface is released on drop.
    unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }
        .map_err(|e| format!("cannot create a Shell link: {}", e.message()))
}

/// Writes the shortcut `lnk` (over one that is there already), creating its folder.
pub fn create(lnk: &Path, link: &Link) -> Result<(), String> {
    if let Some(dir) = lnk.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    // Declared first, so it is dropped last: after every interface.
    let _com = Com::init()?;
    let fail = |what: &str, e: windows::core::Error| format!("{what}: {}", e.message());
    let shell = shell_link()?;
    // SAFETY: plain COM calls on a live interface; every string lives until the call returns.
    unsafe {
        shell
            .SetPath(&HSTRING::from(link.target.as_os_str()))
            .map_err(|e| fail("cannot set the shortcut's target", e))?;
        shell
            .SetWorkingDirectory(&HSTRING::from(link.working_dir.as_os_str()))
            .map_err(|e| fail("cannot set the shortcut's working folder", e))?;
        shell
            .SetDescription(&HSTRING::from(link.description.as_str()))
            .map_err(|e| fail("cannot set the shortcut's description", e))?;
        shell
            .SetIconLocation(&HSTRING::from(link.icon.as_os_str()), 0)
            .map_err(|e| fail("cannot set the shortcut's icon", e))?;
        let file: IPersistFile = shell
            .cast()
            .map_err(|e| fail("the Shell link cannot be saved", e))?;
        file.Save(&HSTRING::from(lnk.as_os_str()), true)
            .map_err(|e| fail(&format!("cannot write {}", lnk.display()), e))?;
    }
    Ok(())
}

/// A string out of a buffer the Shell filled: up to the first NUL.
fn from_wide(buf: &[u16]) -> OsString {
    let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    OsString::from_wide(&buf[..len])
}

/// What the shortcut `lnk` holds, as stored (nothing is resolved); `None` when it is not there or cannot be
/// read as a shortcut.
pub fn read(lnk: &Path) -> Option<Link> {
    if !lnk.is_file() {
        return None;
    }
    let _com = Com::init().ok()?;
    let shell = shell_link().ok()?;
    let mut target = vec![0u16; 32768];
    let mut working_dir = vec![0u16; 32768];
    let mut description = vec![0u16; 1024];
    let mut icon = vec![0u16; 32768];
    let mut icon_index = 0i32;
    // SAFETY: plain COM calls on a live interface; each buffer is passed with its length, and no find data is
    // asked for.
    unsafe {
        let file: IPersistFile = shell.cast().ok()?;
        file.Load(&HSTRING::from(lnk.as_os_str()), STGM_READ).ok()?;
        shell
            .GetPath(&mut target, std::ptr::null_mut(), SLGP_RAWPATH.0 as u32)
            .ok()?;
        shell.GetWorkingDirectory(&mut working_dir).ok()?;
        shell.GetDescription(&mut description).ok()?;
        shell.GetIconLocation(&mut icon, &mut icon_index).ok()?;
    }
    Some(Link {
        target: from_wide(&target).into(),
        working_dir: from_wide(&working_dir).into(),
        description: from_wide(&description).to_string_lossy().into_owned(),
        icon: from_wide(&icon).into(),
    })
}

/// Is `lnk` a shortcut that starts `exe`? Compared as paths (case and the direction of the slashes do not
/// matter), or else as files: the Shell stores the long form of a target that was given with a short (8.3)
/// name in it, so the same exe can be spelt two ways.
pub fn starts(lnk: &Path, exe: &Path) -> bool {
    use crate::actions::{path_key, same_file};
    read(lnk)
        .is_some_and(|link| path_key(&link.target) == path_key(exe) || same_file(&link.target, exe))
}

/// Deletes the shortcut `lnk` (one that is not there is fine), then its folder if that is empty now. Never
/// more than that one file: whatever else someone put in the folder stays, and so does the folder then.
pub fn remove(lnk: &Path) -> Result<Removed, String> {
    let mut removed = Removed::default();
    match std::fs::remove_file(lnk) {
        Ok(()) => removed.shortcut = true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("cannot delete {}: {e}", lnk.display())),
    }
    if let Some(dir) = lnk.parent()
        && std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_none())
    {
        // Not `remove_dir_all`: only a folder that is empty goes.
        std::fs::remove_dir(dir).map_err(|e| format!("cannot delete {}: {e}", dir.display()))?;
        removed.dir = true;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::path_key;
    use crate::layout::tests::TempDir;

    #[test]
    fn the_shortcut_lives_in_a_crossc_folder_of_the_all_users_start_menu() {
        let programs = Path::new(r"C:\ProgramData\Microsoft\Windows\Start Menu\Programs");
        let dir = start_menu_dir(Some(programs)).unwrap();
        assert_eq!(
            dir,
            PathBuf::from(r"C:\ProgramData\Microsoft\Windows\Start Menu\Programs\crossc")
        );
        assert_eq!(
            tray_shortcut(&dir),
            PathBuf::from(
                r"C:\ProgramData\Microsoft\Windows\Start Menu\Programs\crossc\crossc tray.lnk"
            )
        );
        // Nothing reported, or nothing usable: no folder, rather than one somewhere odd.
        for bad in [
            None,
            Some(Path::new("")),
            Some(Path::new(r"Start Menu\Programs")),
        ] {
            assert_eq!(start_menu_dir(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_tray_link_starts_the_exe_in_its_folder_with_its_icon() {
        let exe = Path::new(r"C:\Program Files\crossc\crossc-tray.exe");
        assert_eq!(
            Link::tray(exe),
            Link {
                target: exe.to_path_buf(),
                working_dir: PathBuf::from(r"C:\Program Files\crossc"),
                description: TRAY_DESCRIPTION.to_string(),
                icon: exe.to_path_buf(),
            }
        );
        assert!(!TRAY_DESCRIPTION.is_empty());
    }

    /// The real COM calls, on a folder under the temp directory: never the real Start menu.
    #[test]
    fn a_shortcut_is_written_read_back_refreshed_and_removed() {
        let t = TempDir::new("shortcut");
        let exe = t.file(r"Program Files\crossc\crossc-tray.exe", "not a real exe");
        let dir = start_menu_dir(Some(&t.0.join("Programs"))).unwrap();
        let lnk = tray_shortcut(&dir);
        assert_eq!(read(&lnk), None);
        assert!(!starts(&lnk, &exe));

        // The folder is made with it.
        create(&lnk, &Link::tray(&exe)).unwrap();
        assert!(lnk.is_file());
        let back = read(&lnk).expect("a shortcut that reads back");
        let same = |a: &Path, b: &Path| {
            let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
            path_key(&canon(a)) == path_key(&canon(b))
        };
        assert!(same(&back.target, &exe), "{back:?}");
        assert!(same(&back.working_dir, exe.parent().unwrap()), "{back:?}");
        assert!(same(&back.icon, &exe), "{back:?}");
        assert_eq!(back.description, TRAY_DESCRIPTION);
        assert!(starts(&lnk, &exe));
        assert!(!starts(&lnk, &t.0.join("other.exe")));

        // The same exe through its short (8.3) name, where the volume has them: the Shell may store the long
        // form, and it is still the same exe.
        let short = t.0.join(r"PROGRA~1\crossc\crossc-tray.exe");
        if short.is_file() {
            assert!(starts(&lnk, &short));
            create(&lnk, &Link::tray(&short)).unwrap();
            assert!(starts(&lnk, &exe) && starts(&lnk, &short));
        }

        // Written again (an update) over the one that is there, now to another exe.
        let moved = t.file(r"elsewhere\crossc-tray.exe", "moved");
        create(&lnk, &Link::tray(&moved)).unwrap();
        assert!(starts(&lnk, &moved));
        assert!(!starts(&lnk, &exe));

        // Something else in the folder: only the shortcut goes, the folder stays.
        let other = t.file(r"Programs\crossc\readme.txt", "someone's");
        assert_eq!(
            remove(&lnk).unwrap(),
            Removed {
                shortcut: true,
                dir: false
            }
        );
        assert!(!lnk.exists() && other.is_file());
        // Removing it again is fine and changes nothing.
        assert_eq!(remove(&lnk).unwrap(), Removed::default());

        // Alone in the folder: both go, and nothing above the folder.
        std::fs::remove_file(&other).unwrap();
        create(&lnk, &Link::tray(&exe)).unwrap();
        assert_eq!(
            remove(&lnk).unwrap(),
            Removed {
                shortcut: true,
                dir: true
            }
        );
        assert!(!dir.exists());
        assert!(t.0.join("Programs").is_dir());
        // No folder at all: nothing to do.
        assert_eq!(remove(&lnk).unwrap(), Removed::default());

        // A file that is not a shortcut is not read as one.
        let junk = t.file(r"Programs\junk.lnk", "not a shortcut");
        assert_eq!(read(&junk), None);
    }
}
