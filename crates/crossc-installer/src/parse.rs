//! Pure text parsers and formatters (no Windows calls), so every rule the installer depends on is unit-tested.

/// Original INF name of the crossc driver package, as pnputil reports it.
pub const INF_NAME: &str = "crossc_bth.inf";

/// One driver package in the driver store, from `pnputil /enum-drivers`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedPackage {
    /// Published name, `oemNN.inf`.
    pub published: String,
    /// `Driver Version` line (date and version), when pnputil printed one.
    pub version: Option<String>,
}

/// Every staged package whose original name is `crossc_bth.inf` (case-insensitive), in pnputil's order.
///
/// Same rule as `Get-StagedPackages` in tools/install-driver.ps1: a `Published Name` line opens a record, the next
/// `Original Name` line decides whether it is ours. The `Driver Version` line follows `Original Name`.
pub fn parse_enum_drivers(output: &str) -> Vec<StagedPackage> {
    let mut result: Vec<StagedPackage> = Vec::new();
    let mut published: Option<String> = None;
    // Index into `result` of the record whose Driver Version line we are still waiting for.
    let mut awaiting_version: Option<usize> = None;
    for line in output.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if key.eq_ignore_ascii_case("Published Name") {
            published = first_token(value);
            awaiting_version = None;
        } else if key.eq_ignore_ascii_case("Original Name") {
            if let (Some(p), Some(orig)) = (published.take(), first_token(value))
                && orig.eq_ignore_ascii_case(INF_NAME)
            {
                result.push(StagedPackage {
                    published: p,
                    version: None,
                });
                awaiting_version = Some(result.len() - 1);
            }
        } else if key.eq_ignore_ascii_case("Driver Version")
            && let Some(i) = awaiting_version.take()
            && !value.is_empty()
        {
            result[i].version = Some(value.to_string());
        }
    }
    result
}

fn first_token(s: &str) -> Option<String> {
    s.split_whitespace().next().map(str::to_string)
}

/// Every buds-specific hardware ID on the INF's model lines (`BTHENUM\{0000110b-...}_VID&xxxxxxxx_PID&xxxx`, one
/// per supported Galaxy Buds model), in INF order, without duplicates.
///
/// Comment lines (`;`) are skipped, so the generic compatible ID quoted in the INF's header comment never matches,
/// and an ID without the `_VID&..._PID&...` suffix is never returned: the installer only ever looks at nodes that
/// carry one of these IDs. Matching is case-insensitive; the IDs are returned as written in the INF.
pub fn inf_hardware_ids(inf: &str) -> Vec<String> {
    const PREFIX: &str = "bthenum\\{0000110b-";
    let mut ids: Vec<String> = Vec::new();
    for line in inf.lines() {
        let code = line.split(';').next().unwrap_or("");
        let lower = code.to_ascii_lowercase();
        let mut from = 0;
        while let Some(pos) = lower[from..].find(PREFIX) {
            let start = from + pos;
            let end = code[start..]
                .find(|c: char| c == ',' || c.is_whitespace())
                .map_or(code.len(), |e| start + e);
            let candidate = &code[start..end];
            if is_vid_pid_id(candidate) && !ids.iter().any(|i| i.eq_ignore_ascii_case(candidate)) {
                ids.push(candidate.to_string());
            }
            from = start + PREFIX.len();
        }
    }
    ids
}

/// INF text from the file's bytes: UTF-16 LE when it starts with that byte-order mark (INFs may be saved as
/// "Unicode"), otherwise UTF-8 / ANSI read lossily (everything the installer looks for is ASCII).
pub fn decode_inf(bytes: &[u8]) -> String {
    match bytes {
        [0xFF, 0xFE, rest @ ..] => {
            let units: Vec<u16> = rest
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .collect();
            String::from_utf16_lossy(&units)
        }
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// The value of the INF's `DriverVer` directive, e.g. `11/11/2026,8.50.53.934`. `None` when the directive is
/// missing or empty (the `.inx` template before stampinf). Comments are skipped: the header mentions "DriverVer".
pub fn inf_driver_ver(inf: &str) -> Option<String> {
    inf.lines().find_map(|line| {
        let code = line.split(';').next().unwrap_or("");
        let (key, value) = code.split_once('=')?;
        let value = value.trim();
        (key.trim().eq_ignore_ascii_case("DriverVer") && !value.is_empty())
            .then(|| value.to_string())
    })
}

/// The version number in a `DriverVer` value (`11/11/2026,8.50.53.934`) or in pnputil's `Driver Version` line
/// (`11/11/2026 8.50.53.934`): the last token, when it is dotted digits. The date is not compared anywhere,
/// because pnputil prints it in its own format.
pub fn version_number(text: &str) -> Option<&str> {
    let last = text
        .split(|c: char| c == ',' || c.is_whitespace())
        .rfind(|t| !t.is_empty())?;
    (last.contains('.') && last.chars().all(|c| c.is_ascii_digit() || c == '.')).then_some(last)
}

/// Bluetooth address of the remote device a BTHENUM service node belongs to, uppercase: the 12 hex digits before
/// `_C00000000` in `BTHENUM\{...}_VID&..._PID&...\b&1a2b3c4d&0&AABBCCDDEEFF_C00000000`.
pub fn bt_address(instance_id: &str) -> Option<String> {
    let last = instance_id.rsplit('\\').next()?;
    let addr = last.rsplit('&').next()?.split('_').next()?;
    (addr.len() == 12 && addr.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| addr.to_ascii_uppercase())
}

/// Instance id of the Bluetooth device node itself (`BTHENUM\DEV_<address>\...`, whose name is the name the buds
/// were paired under) among `ids`.
pub fn bt_device_node<'a>(ids: &'a [String], address: &str) -> Option<&'a str> {
    let prefix = format!(r"BTHENUM\DEV_{address}\").to_ascii_uppercase();
    ids.iter()
        .map(String::as_str)
        .find(|id| id.to_ascii_uppercase().starts_with(&prefix))
}

/// The executable in a service's binary path (`"C:\Program Files\crossc\crossc-service.exe" run` or the same
/// without quotes): the quoted part, else everything up to the first `.exe`, else the first word.
pub fn image_path_exe(image_path: &str) -> String {
    let s = image_path.trim();
    if let Some(rest) = s.strip_prefix('"') {
        return rest.split('"').next().unwrap_or(rest).to_string();
    }
    if let Some(pos) = s.to_ascii_lowercase().find(".exe") {
        return s[..pos + 4].to_string();
    }
    s.split_whitespace().next().unwrap_or("").to_string()
}

/// `..._VID&<hex>_PID&<hex>` suffix check (the regex `_VID&[0-9a-f]+_PID&[0-9a-f]+` from the script).
fn is_vid_pid_id(id: &str) -> bool {
    let lower = id.to_ascii_lowercase();
    let Some(v) = lower.find("_vid&") else {
        return false;
    };
    let rest = &lower[v + 5..];
    let vid_len = rest.chars().take_while(char::is_ascii_hexdigit).count();
    if vid_len == 0 {
        return false;
    }
    let Some(after) = rest[vid_len..].strip_prefix("_pid&") else {
        return false;
    };
    after.chars().take_while(char::is_ascii_hexdigit).count() > 0
}

/// What a device-manager problem code means for this driver (texts from tools/install-driver.ps1).
pub fn problem_text(code: u32) -> String {
    match code {
        0 => "no problem".into(),
        52 => "problem 52: signature not trusted (test signing off, Secure Boot on, or cert not trusted)".into(),
        10 => "problem 10: start failed; see the driver trace in DebugView (docs/SETUP.md section 6)".into(),
        31 => "problem 31: driver failed to load; see C:\\Windows\\INF\\setupapi.dev.log".into(),
        39 => "problem 39: driver failed to load (often HVCI / Memory integrity); see setupapi.dev.log".into(),
        28 => "problem 28: no driver installed on the node".into(),
        22 => "problem 22: device is disabled".into(),
        45 => "problem 45: device is not connected (buds off or out of range)".into(),
        n => format!("problem code {n}"),
    }
}

/// How pnputil's exit code is read (as in the script: 3010 = reboot needed, other nonzero = warn and verify anyway).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PnpExit {
    Ok,
    /// ERROR_NO_MORE_ITEMS: the package was handled, but no device matched it (no supported buds paired) or the
    /// device already runs a driver that ranks at least as well.
    NothingToDo,
    RebootRequired,
    Failed(i32),
}

pub fn pnputil_exit(code: i32) -> PnpExit {
    match code {
        0 => PnpExit::Ok,
        259 => PnpExit::NothingToDo,
        3010 => PnpExit::RebootRequired,
        n => PnpExit::Failed(n),
    }
}

/// Removes ANSI escape sequences (CSI `ESC [ ... final` and two-byte `ESC x`) from tool output.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                // Parameters and intermediates, then one final byte in 0x40..=0x7e.
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                // OSC: up to BEL or ESC \.
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Severity of a log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// Child-process output with nothing notable in it.
    Output,
    /// Installer progress (`==> ...`).
    Step,
    Info,
    Success,
    Warning,
    Error,
}

/// Colours a child-process line by its content (cargo writes normal progress to stderr, so the stream says nothing).
pub fn classify_output(line: &str) -> LineKind {
    let t = line.trim_start();
    let lower = t.to_ascii_lowercase();
    if lower.starts_with("error")
        || lower.contains("[error]")
        || lower.contains("] error ")
        || lower.starts_with("failed")
        || lower.contains("build failed")
        || lower.contains("could not compile")
        || lower.starts_with("fatal")
    {
        LineKind::Error
    } else if lower.starts_with("warning")
        || lower.contains("] warn ")
        || lower.contains("reboot is required")
    {
        LineKind::Warning
    } else if lower.starts_with("finished")
        || lower.contains("build done")
        || lower.contains("successfully")
        || lower.contains("driver package added")
    {
        LineKind::Success
    } else {
        LineKind::Output
    }
}

/// "just now", "5 min ago", "3 h ago", "2 days ago".
pub fn format_age(secs: u64) -> String {
    match secs {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", secs / 60),
        3600..=86_399 => format!("{} h ago", secs / 3600),
        86_400..=172_799 => "1 day ago".into(),
        _ => format!("{} days ago", secs / 86_400),
    }
}

/// Uppercase hex, no separators (the format PowerShell shows for certificate thumbprints).
pub fn hex_upper(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

/// How much of an executable `exe_problem` needs: the DOS header, the PE headers and the section table.
pub const EXE_HEAD_LEN: usize = 8192;

/// Why a file cannot be a complete Windows executable, from its first bytes (`EXE_HEAD_LEN` are plenty) and its
/// length: `None` when the DOS and PE headers are there and every section's data lies inside the file. This is
/// what catches an empty placeholder or a download / unzip that was cut short; it says nothing about what the
/// program does.
pub fn exe_problem(head: &[u8], file_len: u64) -> Option<String> {
    let u16_at = |at: usize| {
        head.get(at..at + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]) as usize)
    };
    let u32_at = |at: usize| {
        head.get(at..at + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
    };
    if file_len == 0 {
        return Some("the file is empty".into());
    }
    let cut_short = || Some(format!("its headers are cut short ({file_len} bytes)"));
    if head.get(..2) != Some(b"MZ") {
        return Some("it is not a Windows executable (no MZ header)".into());
    }
    // IMAGE_DOS_HEADER.e_lfanew, then "PE\0\0" and the 20-byte COFF header.
    let Some(pe) = u32_at(0x3c) else {
        return cut_short();
    };
    match head.get(pe..pe + 4) {
        Some(b"PE\0\0") => {}
        Some(_) => return Some("it is not a Windows executable (no PE header)".into()),
        None => return cut_short(),
    }
    let (Some(sections), Some(optional_len)) = (u16_at(pe + 6), u16_at(pe + 20)) else {
        return cut_short();
    };
    // IMAGE_SECTION_HEADER is 40 bytes; SizeOfRawData is at +16 and PointerToRawData at +20.
    let table = pe + 24 + optional_len;
    let mut end = 0u64;
    for i in 0..sections {
        let at = table + i * 40;
        let (Some(size), Some(start)) = (u32_at(at + 16), u32_at(at + 20)) else {
            return cut_short();
        };
        if size > 0 {
            end = end.max(start as u64 + size as u64);
        }
    }
    (end > file_len).then(|| format!("it is cut short ({file_len} bytes; its sections need {end})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENUM_DRIVERS: &str = "Microsoft PnP Utility\r\n\
\r\n\
Published Name:     oem47.inf\r\n\
Original Name:      amdafd.inf\r\n\
Provider Name:      AMD\r\n\
Class Name:         System\r\n\
Driver Version:     08/21/2025 25.30.0.1\r\n\
Signer Name:        Microsoft Windows Hardware Compatibility Publisher\r\n\
\r\n\
Published Name:     oem131.inf\r\n\
Original Name:      crossc_bth.inf\r\n\
Provider Name:      crossc\r\n\
Class Name:         Sound, video and game controllers\r\n\
Class GUID:         {4d36e96c-e325-11ce-bfc1-08002be10318}\r\n\
Driver Version:     11/10/2026 22.51.13.530\r\n\
Signer Name:        WDRLocalTestCert\r\n\
\r\n\
Published Name:     oem9.inf\r\n\
Original Name:      other.inf\r\n\
Driver Version:     01/01/2020 1.0.0.0\r\n\
\r\n\
Published Name : oem132.inf\r\n\
Original Name : CROSSC_BTH.INF\r\n";

    #[test]
    fn enum_drivers_finds_only_crossc_packages() {
        let pkgs = parse_enum_drivers(ENUM_DRIVERS);
        assert_eq!(
            pkgs,
            vec![
                StagedPackage {
                    published: "oem131.inf".into(),
                    version: Some("11/10/2026 22.51.13.530".into())
                },
                StagedPackage {
                    published: "oem132.inf".into(),
                    version: None
                },
            ]
        );
    }

    #[test]
    fn enum_drivers_empty_and_unrelated() {
        assert!(parse_enum_drivers("").is_empty());
        assert!(
            parse_enum_drivers(
                "Microsoft PnP Utility\n\nPublished Name: oem1.inf\nOriginal Name: a.inf\n"
            )
            .is_empty()
        );
        // Original Name without a preceding Published Name is ignored.
        assert!(parse_enum_drivers("Original Name: crossc_bth.inf\n").is_empty());
    }

    #[test]
    fn enum_drivers_version_belongs_to_its_own_record() {
        // A crossc record without a Driver Version line must not pick up the next record's version.
        let text = "Published Name: oem5.inf\nOriginal Name: crossc_bth.inf\n\n\
                    Published Name: oem6.inf\nOriginal Name: x.inf\nDriver Version: 1/1/2000 1.0\n";
        assert_eq!(
            parse_enum_drivers(text),
            vec![StagedPackage {
                published: "oem5.inf".into(),
                version: None
            }]
        );
    }

    const INF: &str = r#";
; matches the compatible ID BTHENUM\{0000110b-0000-1000-8000-00805f9b34fb}.
; replace ..._VID&xxxxxxxx_PID&xxxx below; old: BTHENUM\{0000110b-0000-1000-8000-00805f9b34fb}_VID&00010075_PID&ffff
[Standard.NTamd64.10.0...16299]
; Galaxy Buds4 Pro
%DeviceDesc% = crossc_bth_Install, BTHENUM\{0000110b-0000-1000-8000-00805f9b34fb}_VID&00010075_PID&a013
"#;

    const A013: &str = r"BTHENUM\{0000110b-0000-1000-8000-00805f9b34fb}_VID&00010075_PID&a013";

    #[test]
    fn inf_hardware_ids_from_model_line_not_comments() {
        assert_eq!(inf_hardware_ids(INF), vec![A013.to_string()]);
    }

    #[test]
    fn inf_hardware_ids_lists_every_model_once() {
        let inf = r#"; header: BTHENUM\{0000110b-0000-1000-8000-00805f9b34fb}_VID&00010075_PID&dead
[Version]
DriverVer = 11/11/2026,8.50.53.934
[Standard.NTamd64.10.0...16299]
; Galaxy Buds4 Pro
%CrossC.DeviceDesc% = CrossC_Device, BTHENUM\{0000110b-0000-1000-8000-00805f9b34fb}_VID&00010075_PID&a013
; Galaxy Buds3 Pro
%CrossC.DeviceDesc% = CrossC_Device, BTHENUM\{0000110b-0000-1000-8000-00805f9b34fb}_VID&00010075_PID&A00F ; trailing
%CrossC.DeviceDesc% = CrossC_Device,BTHENUM\{0000110B-0000-1000-8000-00805F9B34FB}_VID&00010075_PID&a00a,BTHENUM\{0000110b-0000-1000-8000-00805f9b34fb}_VID&0001000f_PID&1200
; the same model again, in another case, and the generic compatible ID
%CrossC.DeviceDesc% = CrossC_Device, bthenum\{0000110b-0000-1000-8000-00805f9b34fb}_vid&00010075_pid&A013
%CrossC.DeviceDesc% = CrossC_Device, BTHENUM\{0000110b-0000-1000-8000-00805f9b34fb}
; another profile of the same buds is not an A2DP sink node
%CrossC.DeviceDesc% = CrossC_Device, BTHENUM\{0000111e-0000-1000-8000-00805f9b34fb}_VID&00010075_PID&a013
"#;
        assert_eq!(
            inf_hardware_ids(inf),
            vec![
                A013.to_string(),
                r"BTHENUM\{0000110b-0000-1000-8000-00805f9b34fb}_VID&00010075_PID&A00F".to_string(),
                r"BTHENUM\{0000110B-0000-1000-8000-00805F9B34FB}_VID&00010075_PID&a00a".to_string(),
                r"BTHENUM\{0000110b-0000-1000-8000-00805f9b34fb}_VID&0001000f_PID&1200".to_string(),
            ]
        );
    }

    #[test]
    fn inf_hardware_ids_rejects_generic_id_and_handles_case() {
        let none: Vec<String> = Vec::new();
        assert_eq!(inf_hardware_ids(""), none);
        assert_eq!(
            inf_hardware_ids("x = y, BTHENUM\\{0000110b-0000-1000-8000-00805f9b34fb}\n"),
            none
        );
        assert_eq!(
            inf_hardware_ids(
                "x = y,bthenum\\{0000110B-0000-1000-8000-00805F9B34FB}_VID&0001000F_PID&1200 ; c\n"
            ),
            vec![r"bthenum\{0000110B-0000-1000-8000-00805F9B34FB}_VID&0001000F_PID&1200"]
        );
        // Generic ID first on the line, specific one second.
        assert_eq!(
            inf_hardware_ids("x = y, BTHENUM\\{0000110b-0}, BTHENUM\\{0000110b-0}_VID&1_PID&2\n"),
            vec![r"BTHENUM\{0000110b-0}_VID&1_PID&2"]
        );
        assert_eq!(
            inf_hardware_ids("x = y, BTHENUM\\{0000110b-0}_VID&_PID&2\n"),
            none
        );
    }

    #[test]
    fn inf_hardware_ids_of_built_package_include_the_buds() {
        // The real build output, when present (not required for the test to pass). The INF may list more models.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../drivers/crossc-bth/target");
        for flavor in ["debug", "release"] {
            let inf = root.join(flavor).join("crossc_bth_package").join(INF_NAME);
            if let Ok(text) = std::fs::read_to_string(&inf) {
                let ids = inf_hardware_ids(&text);
                assert!(
                    ids.iter().any(|id| id.eq_ignore_ascii_case(A013)),
                    "{ids:?}"
                );
                assert!(
                    ids.iter()
                        .all(|id| id.to_ascii_uppercase().contains("_VID&")),
                    "{ids:?}"
                );
            }
        }
    }

    #[test]
    fn inf_bytes_are_decoded_whatever_the_encoding() {
        let text = "DriverVer = 1/2/2026,1.2.3.4\r\n";
        assert_eq!(decode_inf(text.as_bytes()), text);
        let mut utf16 = vec![0xFF, 0xFE];
        utf16.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(decode_inf(&utf16), text);
        let mut bom = vec![0xEF, 0xBB, 0xBF];
        bom.extend(text.as_bytes());
        assert_eq!(decode_inf(&bom), text);
        // ANSI bytes in a comment do not lose the rest.
        assert_eq!(
            inf_driver_ver(&decode_inf(
                b"; caf\xe9\r\nDriverVer = 1/2/2026,1.2.3.4\r\n"
            ))
            .as_deref(),
            Some("1/2/2026,1.2.3.4")
        );
        assert_eq!(decode_inf(&[]), "");
    }

    #[test]
    fn driver_ver_from_the_directive_not_the_comment() {
        let inf = ";   identifier scores tie, and the newer DriverVer wins - so crossc = bad\r\n\
                   [Version]\r\n\
                   Class       = MEDIA\r\n\
                   DriverVer = 11/11/2026,8.50.53.934 ; stamped\r\n";
        assert_eq!(
            inf_driver_ver(inf).as_deref(),
            Some("11/11/2026,8.50.53.934")
        );
        assert_eq!(
            inf_driver_ver("driverver=01/02/2026,1.2.3.4").as_deref(),
            Some("01/02/2026,1.2.3.4")
        );
        // The .inx template leaves it empty for stampinf.
        assert_eq!(inf_driver_ver("[Version]\nDriverVer   =\n"), None);
        assert_eq!(inf_driver_ver("[Version]\nClass = MEDIA\n"), None);
    }

    #[test]
    fn version_numbers() {
        assert_eq!(
            version_number("11/11/2026,8.50.53.934"),
            Some("8.50.53.934")
        );
        assert_eq!(
            version_number("11/10/2026 22.51.13.530"),
            Some("22.51.13.530")
        );
        assert_eq!(version_number("11/11/2026, 0.1.0.0 "), Some("0.1.0.0"));
        assert_eq!(version_number("1.2.3.4"), Some("1.2.3.4"));
        assert_eq!(version_number("11/11/2026"), None);
        assert_eq!(version_number("10.11.2026 1.2.3.4"), Some("1.2.3.4"));
        assert_eq!(version_number(""), None);
    }

    #[test]
    fn bluetooth_address_and_device_node() {
        let a2dp = r"BTHENUM\{0000110B-0000-1000-8000-00805F9B34FB}_VID&00010075_PID&A013\B&1A2B3C4D&0&aabbccddeeff_C00000000";
        assert_eq!(bt_address(a2dp).as_deref(), Some("AABBCCDDEEFF"));
        assert_eq!(
            bt_address(r"BTHENUM\{X}_LOCALMFG&0000\B&1&0&000000000000_00000000").as_deref(),
            Some("000000000000")
        );
        assert_eq!(bt_address(r"USB\VID_1234&PID_5678\6&1&0&2"), None);
        assert_eq!(
            bt_address(r"BTHENUM\DEV_AABBCCDDEEFF\B&1A2B3C4D&0&BLUETOOTHDEVICE_AABBCCDDEEFF"),
            None
        );
        assert_eq!(bt_address(""), None);

        let ids = vec![
            a2dp.to_string(),
            r"BTHENUM\DEV_665544332211\B&1A2B3C4D&0&BLUETOOTHDEVICE_665544332211".to_string(),
            r"BTHENUM\Dev_AABBCCDDEEFF\B&1A2B3C4D&0&BLUETOOTHDEVICE_AABBCCDDEEFF".to_string(),
        ];
        assert_eq!(bt_device_node(&ids, "AABBCCDDEEFF"), Some(ids[2].as_str()));
        assert_eq!(bt_device_node(&ids, "665544332211"), Some(ids[1].as_str()));
        assert_eq!(bt_device_node(&ids, "000000000000"), None);
    }

    #[test]
    fn service_image_paths() {
        assert_eq!(
            image_path_exe(r#""C:\Program Files\crossc\crossc-service.exe" run"#),
            r"C:\Program Files\crossc\crossc-service.exe"
        );
        assert_eq!(
            image_path_exe(r"C:\Program Files\crossc\crossc-service.exe run --x"),
            r"C:\Program Files\crossc\crossc-service.exe"
        );
        assert_eq!(
            image_path_exe(r"C:\WINDOWS\System32\svchost.exe -k LocalServiceNetworkRestricted -p"),
            r"C:\WINDOWS\System32\svchost.exe"
        );
        assert_eq!(image_path_exe(r"  C:\x\Y.EXE  "), r"C:\x\Y.EXE");
        assert_eq!(
            image_path_exe(r"\SystemRoot\System32\drivers\x.sys"),
            r"\SystemRoot\System32\drivers\x.sys"
        );
        assert_eq!(
            image_path_exe(r#""C:\unterminated\a.exe"#),
            r"C:\unterminated\a.exe"
        );
        assert_eq!(image_path_exe(""), "");
    }

    #[test]
    fn problem_texts() {
        assert!(problem_text(52).contains("signature not trusted"));
        assert!(problem_text(10).contains("DebugView"));
        assert!(problem_text(31).contains("setupapi.dev.log"));
        assert!(problem_text(39).contains("Memory integrity"));
        assert_eq!(problem_text(0), "no problem");
        assert_eq!(problem_text(1234), "problem code 1234");
    }

    #[test]
    fn pnputil_exit_codes() {
        assert_eq!(pnputil_exit(0), PnpExit::Ok);
        assert_eq!(pnputil_exit(3010), PnpExit::RebootRequired);
        assert_eq!(pnputil_exit(259), PnpExit::NothingToDo);
        assert_eq!(pnputil_exit(1), PnpExit::Failed(1));
        assert_eq!(pnputil_exit(-1), PnpExit::Failed(-1));
    }

    #[test]
    fn ansi_is_stripped() {
        assert_eq!(
            strip_ansi("\u{1b}[1m\u{1b}[32m   Compiling\u{1b}[0m foo"),
            "   Compiling foo"
        );
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}plain"), "plain");
        assert_eq!(strip_ansi("no escapes"), "no escapes");
    }

    #[test]
    fn output_classification() {
        assert_eq!(
            classify_output("error[E0425]: cannot find value"),
            LineKind::Error
        );
        assert_eq!(
            classify_output("   error: could not compile `x`"),
            LineKind::Error
        );
        assert_eq!(
            classify_output("[cargo-make] ERROR - Error while executing command"),
            LineKind::Error
        );
        assert_eq!(
            classify_output("warning: unused variable"),
            LineKind::Warning
        );
        assert_eq!(
            classify_output("[cargo-make] WARN - Build Done"),
            LineKind::Warning
        );
        assert_eq!(
            classify_output("    Finished `release` profile [optimized]"),
            LineKind::Success
        );
        assert_eq!(
            classify_output("Driver package added successfully."),
            LineKind::Success
        );
        assert_eq!(
            classify_output("   Compiling gpui v0.2.2"),
            LineKind::Output
        );
    }

    #[test]
    fn ages() {
        assert_eq!(format_age(5), "just now");
        assert_eq!(format_age(125), "2 min ago");
        assert_eq!(format_age(7200), "2 h ago");
        assert_eq!(format_age(90_000), "1 day ago");
        assert_eq!(format_age(3 * 86_400 + 5), "3 days ago");
    }

    #[test]
    fn hex() {
        assert_eq!(hex_upper(&[0x0a, 0xff, 0x10]), "0AFF10");
    }

    /// A minimal PE image head: DOS header, PE signature, COFF header without an optional header, and one section
    /// header per `(PointerToRawData, SizeOfRawData)`.
    fn pe_head(sections: &[(u32, u32)]) -> Vec<u8> {
        let mut v = vec![0u8; 0x80];
        v[..2].copy_from_slice(b"MZ");
        v[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        v.extend_from_slice(b"PE\0\0");
        let mut coff = [0u8; 20];
        coff[2..4].copy_from_slice(&(sections.len() as u16).to_le_bytes());
        v.extend_from_slice(&coff);
        for (start, size) in sections {
            let mut h = [0u8; 40];
            h[16..20].copy_from_slice(&size.to_le_bytes());
            h[20..24].copy_from_slice(&start.to_le_bytes());
            v.extend_from_slice(&h);
        }
        v
    }

    #[test]
    fn exe_check_accepts_a_whole_image_only() {
        let head = pe_head(&[(0x200, 0x400), (0x600, 0x200), (0, 0)]);
        assert_eq!(exe_problem(&head, 0x800), None);
        assert_eq!(exe_problem(&head, 0x900), None);
        // A section with no data in the file (.bss) does not count, whatever its offset says.
        assert_eq!(exe_problem(&pe_head(&[(0x9000, 0)]), 0x200), None);
        assert_eq!(exe_problem(&pe_head(&[]), 0x100), None);

        // The placeholder a broken packaging step leaves, and other things that are not executables.
        assert_eq!(exe_problem(&[], 0).as_deref(), Some("the file is empty"));
        for junk in [&b"M"[..], b"PK\x03\x04 a zip", b"#!/bin/sh\n"] {
            let why = exe_problem(junk, junk.len() as u64).unwrap();
            assert!(why.contains("no MZ header"), "{why}");
        }
        let mut no_pe = head.clone();
        no_pe[0x80..0x84].copy_from_slice(b"NE\0\0");
        assert!(exe_problem(&no_pe, 0x800).unwrap().contains("no PE header"));

        // Cut short: in the data, in the section table, in the COFF header, before the PE signature.
        assert_eq!(
            exe_problem(&head, 0x7ff).as_deref(),
            Some("it is cut short (2047 bytes; its sections need 2048)")
        );
        for len in [head.len() - 1, 0x80 + 24 + 10, 0x80 + 10, 0x80 + 2, 0x3e, 2] {
            let why = exe_problem(&head[..len], len as u64).unwrap();
            assert!(why.contains("cut short"), "{len}: {why}");
        }
        // e_lfanew pointing far outside what was read.
        let mut far = head.clone();
        far[0x3c..0x40].copy_from_slice(&0xffff_fff0u32.to_le_bytes());
        assert!(exe_problem(&far, 0x800).unwrap().contains("cut short"));
    }

    #[test]
    fn exe_check_accepts_a_real_executable() {
        let bytes = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let head = &bytes[..bytes.len().min(EXE_HEAD_LEN)];
        assert_eq!(exe_problem(head, bytes.len() as u64), None);
        // The same file with its tail missing (an interrupted download or unzip).
        let why = exe_problem(head, bytes.len() as u64 / 2).unwrap();
        assert!(why.starts_with("it is cut short"), "{why}");
    }
}
