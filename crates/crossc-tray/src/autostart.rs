//! "Start with Windows": a value under the current user's Run key that
//! names this exe. Per user, HKEY_CURRENT_USER only: no elevation, and
//! nothing any other user's session sees.
//!
//! The key is a parameter everywhere, so the tests run on a key of their
//! own and never touch the real Run key.

use std::path::Path;

/// The Run key of the current user.
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// The value's name.
pub const VALUE: &str = "crossc-tray";

/// The command line stored for `exe`: the path in quotes, no arguments.
pub fn command(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

/// The program a stored command line starts: the quoted part, or
/// everything up to the first space.
fn program(command: &str) -> &str {
    let c = command.trim();
    match c.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or(rest),
        None => c.split(' ').next().unwrap_or(c),
    }
}

/// Does the stored command line start `exe`? Paths compare without regard
/// to case and to the direction of the slashes.
pub fn starts(command: &str, exe: &Path) -> bool {
    let norm = |s: &str| s.replace('/', "\\").to_lowercase();
    let stored = norm(program(command));
    !stored.is_empty() && stored == norm(&exe.display().to_string())
}

#[cfg(windows)]
mod registry {
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
    };
    use windows::core::HSTRING;

    fn message(e: WIN32_ERROR) -> String {
        windows::core::Error::from_hresult(e.to_hresult()).message()
    }

    /// The string value `name` of `HKCU\key`; `None` if there is none (or
    /// it is no string).
    pub fn read(key: &str, name: &str) -> Option<String> {
        let (key, name) = (HSTRING::from(key), HSTRING::from(name));
        let mut bytes = 0u32;
        // SAFETY: a size query; no buffer is passed.
        let r = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                &key,
                &name,
                RRF_RT_REG_SZ,
                None,
                None,
                Some(&mut bytes),
            )
        };
        if r != ERROR_SUCCESS || bytes == 0 || bytes > 64 * 1024 {
            return None;
        }
        let mut buf = vec![0u16; (bytes as usize).div_ceil(2)];
        let mut bytes = (buf.len() * 2) as u32;
        // SAFETY: the buffer has `bytes` bytes.
        let r = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                &key,
                &name,
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut bytes),
            )
        };
        if r != ERROR_SUCCESS {
            return None;
        }
        let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }

    /// Set the string value `name` of `HKCU\key` (the key is created).
    pub fn write(key: &str, name: &str, value: &str) -> Result<(), String> {
        let wide: Vec<u16> = value.encode_utf16().chain([0]).collect();
        // SAFETY: the data is `wide`, its size in bytes is passed.
        let r = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
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
            Err(message(r))
        }
    }

    /// Delete the value `name` of `HKCU\key`. Not there: fine.
    pub fn delete(key: &str, name: &str) -> Result<(), String> {
        // SAFETY: plain call.
        let r = unsafe {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, &HSTRING::from(key), &HSTRING::from(name))
        };
        if r == ERROR_SUCCESS || r == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            Err(message(r))
        }
    }
}

/// Is `exe` started at logon through `HKCU\key`?
#[cfg(windows)]
pub fn enabled(key: &str, exe: &Path) -> bool {
    registry::read(key, VALUE).is_some_and(|c| starts(&c, exe))
}

/// Turn the autostart of `exe` on or off.
#[cfg(windows)]
pub fn set(key: &str, exe: &Path, on: bool) -> Result<(), String> {
    if on {
        registry::write(key, VALUE, &command(exe))
    } else {
        registry::delete(key, VALUE)
    }
}

/// Whether the system's taskbar is light (the "Windows mode" colour
/// setting). Dark when it cannot be told.
#[cfg(windows)]
pub fn taskbar_is_light() -> bool {
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    use windows::core::w;
    let mut value = 0u32;
    let mut bytes = 4u32;
    // SAFETY: the buffer is the 4 bytes of `value`.
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut value).cast()),
            Some(&mut bytes),
        )
    };
    r == ERROR_SUCCESS && value != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn the_command_is_the_quoted_path() {
        let exe = PathBuf::from(r"C:\Program Files\crossc\crossc-tray.exe");
        assert_eq!(
            command(&exe),
            r#""C:\Program Files\crossc\crossc-tray.exe""#
        );
        assert!(starts(&command(&exe), &exe));
    }

    #[test]
    fn a_stored_command_is_matched_to_the_exe() {
        let exe = PathBuf::from(r"C:\Program Files\crossc\crossc-tray.exe");
        for stored in [
            r#""C:\Program Files\crossc\crossc-tray.exe""#,
            r#""c:\program files\CROSSC\crossc-tray.exe" --minimized"#,
            r#"  "C:/Program Files/crossc/crossc-tray.exe"  "#,
        ] {
            assert!(starts(stored, &exe), "{stored}");
        }
        for stored in [
            "",
            "\"\"",
            r#""C:\Program Files\other\crossc-tray.exe""#,
            r"C:\Program Files\crossc\crossc-tray.exe", // unquoted: cut at the space
            r#""C:\Program Files\crossc\crossc-tray.exe.bak""#,
        ] {
            assert!(!starts(stored, &exe), "{stored}");
        }
        let plain = PathBuf::from(r"D:\tools\crossc-tray.exe");
        assert!(starts(r"D:\tools\crossc-tray.exe --x", &plain));
    }

    /// On a key of this test's own, deleted afterwards.
    #[cfg(windows)]
    #[test]
    fn toggling_writes_and_removes_the_value() {
        use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RegDeleteTreeW};
        use windows::core::HSTRING;
        let key = format!(r"Software\crossc-tray-test-{}", std::process::id());
        let exe = PathBuf::from(r"C:\Program Files\crossc\crossc-tray.exe");
        let other = PathBuf::from(r"C:\elsewhere\crossc-tray.exe");
        assert!(!enabled(&key, &exe), "no key yet");
        set(&key, &exe, false).unwrap(); // off when already off: fine
        set(&key, &exe, true).unwrap();
        assert!(enabled(&key, &exe));
        assert_eq!(registry::read(&key, VALUE), Some(command(&exe)));
        // The value names another copy: not "on" for this one, and turning
        // it on points it here.
        assert!(!enabled(&key, &other));
        set(&key, &other, true).unwrap();
        assert!(enabled(&key, &other) && !enabled(&key, &exe));
        set(&key, &other, false).unwrap();
        assert!(!enabled(&key, &other));
        assert_eq!(registry::read(&key, VALUE), None);
        // SAFETY: plain call; the key is this test's.
        unsafe {
            let _ = RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(key.as_str()));
        }
    }
}
