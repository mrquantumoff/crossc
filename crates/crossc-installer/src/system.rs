//! Read-only Windows queries that fill a `Status`. Pure Win32 (no PowerShell), and all of them work unelevated;
//! the one child process is `pnputil /enum-drivers` for the staged packages, parsed like the script does.

use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use windows::Wdk::System::SystemInformation::{
    NtQuerySystemInformation, SystemCodeIntegrityInformation,
};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_DEVNODE_STATUS_FLAGS, CM_GETIDLIST_FILTER_ENUMERATOR, CM_Get_DevNode_PropertyW,
    CM_Get_DevNode_Status, CM_Get_Device_ID_List_SizeW, CM_Get_Device_ID_ListW,
    CM_LOCATE_DEVNODE_NORMAL, CM_LOCATE_DEVNODE_PHANTOM, CM_Locate_DevNodeW, CM_PROB,
    CR_BUFFER_SMALL, CR_SUCCESS, DN_HAS_PROBLEM,
};
use windows::Win32::Devices::Properties::{
    DEVPKEY_Device_DriverInfPath, DEVPKEY_Device_DriverVersion, DEVPKEY_Device_FriendlyName,
    DEVPKEY_Device_HardwareIds, DEVPKEY_Device_Service, DEVPKEY_Device_Stack, DEVPKEY_NAME,
    DEVPROPTYPE,
};
use windows::Win32::Foundation::{
    CloseHandle, DEVPROPKEY, ERROR_FILE_NOT_FOUND, FILETIME, HANDLE, SYSTEMTIME,
};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
    RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegDeleteValueW,
    RegGetValueW, RegOpenKeyExW, RegQueryInfoKeyW, RegSetValueExW,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows::Win32::UI::Shell::{
    FOLDERID_ProgramFiles, FOLDERID_ProgramFilesX86, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
};
use windows::core::{GUID, HSTRING, PCWSTR};

use crate::cert::{Store, TRUST_STORES, thumbprint};
use crate::layout::{self, Layout, Mode, TRAY_EXE, bundle_version_of, kept_release_dir};
use crate::model::{
    CertInfo, Flavor, InstallRecord, Latest, NodeInfo, PackageInfo, PackageKind, REQUIRED_FILES,
    Status, USER_SERVICE, hwid_matches, order_nodes, unlisted_on_driver,
};
use crate::parse::{
    EXE_HEAD_LEN, INF_NAME, StagedPackage, bt_address, bt_device_node, decode_inf, exe_problem,
    format_age, hex_upper, inf_driver_ver, inf_hardware_ids, parse_enum_drivers,
};
use crate::service;
use crate::tray;

/// Keeps child consoles from flashing up when the app runs without one (release build).
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Online mode: the layout with the release that is installed as its bundle (`Layout::with_release`). That is
/// the release the install record names, and only while its certificate is trusted on this machine. So plain
/// Install in online mode never trusts a certificate: that happens only through the confirmation of "Download
/// and install". A release that was downloaded and not confirmed is not the recorded one; one whose certificate
/// was removed again ("Remove certificate") has to be confirmed anew.
pub fn resolve_layout(layout: &Layout) -> Layout {
    if layout.mode != Mode::Online {
        return layout.clone();
    }
    let record = read_record();
    let releases = releases_dir();
    let kept = kept_release_dir(&releases, record.version.as_deref()).filter(|dir| {
        release_cert_is_trusted(
            &dir.join(crate::layout::BUNDLE_DRIVER_DIR)
                .join("WDRLocalTestCert.cer"),
        )
    });
    layout.with_release(kept)
}

/// The certificate file is a certificate that is in LocalMachine Root and TrustedPublisher already.
pub fn release_cert_is_trusted(cer: &Path) -> bool {
    cert_info(cer).is_ok_and(|c| c.trusted())
}

/// Collects the whole status. Takes a second or so (pnputil), so the UI calls it off the main thread. The
/// latest release is not part of it (`Latest::NotChecked`): that check goes over the network and is the
/// caller's to start.
pub fn query_status(layout: &Layout, flavor: Flavor) -> Status {
    let layout = &resolve_layout(layout);
    let packages: Vec<PackageInfo> = layout
        .packages(flavor)
        .into_iter()
        .map(|(kind, dir)| package_info(kind, dir))
        .collect();
    let ci = code_integrity_options();
    // Hardware IDs and certificate come from the package Install would use, else the other one.
    let usable = packages
        .iter()
        .find(|p| p.complete())
        .unwrap_or(&packages[0]);
    let hardware_ids = packages
        .iter()
        .map(|p| package_hardware_ids(&p.dir))
        .find(|ids| !ids.is_empty())
        .unwrap_or_default();
    Status {
        admin: is_admin(),
        test_signing: ci.map(|f| f & CODEINTEGRITY_OPTION_TESTSIGN != 0),
        secure_boot: secure_boot(),
        hvci_running: ci.map(|f| f & CODEINTEGRITY_OPTION_HVCI_KMCI_ENABLED != 0),
        hvci_configured: reg_dword(
            r"SYSTEM\CurrentControlSet\Control\DeviceGuard\Scenarios\HypervisorEnforcedCodeIntegrity",
            "Enabled",
        )
        .map(|v| v != 0),
        cert: cert_info(&usable.dir.join("WDRLocalTestCert.cer")),
        nodes: find_buds_nodes(&hardware_ids),
        unlisted: find_unlisted_nodes(&hardware_ids).unwrap_or_default(),
        packages,
        hardware_ids,
        staged: staged_packages(),
        service: service::query(USER_SERVICE),
        service_exe: Some(layout.service_exe())
            .filter(|p| p.is_file() && damaged_exe(p).is_none()),
        tray: tray::query(&install_dir().join(TRAY_EXE)),
        tray_exe: Some(layout.tray_exe()).filter(|p| p.is_file()),
        online: layout.mode == Mode::Online,
        record: read_record(),
        bundle_version: match layout.mode {
            Mode::Bundle | Mode::Online => bundle_version_of(&layout.root),
            Mode::Repo => None,
        },
        latest: Latest::NotChecked,
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Token and code integrity
// ---------------------------------------------------------------------------------------------------------------

/// The process token is elevated (UAC "Run as administrator").
pub fn is_admin() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some((&raw mut elevation).cast()),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// Test signing is on in the running kernel.
pub fn test_signing() -> Option<bool> {
    code_integrity_options().map(|f| f & CODEINTEGRITY_OPTION_TESTSIGN != 0)
}

/// UEFI Secure Boot state as Windows recorded it at boot (`None` on legacy BIOS).
pub fn secure_boot() -> Option<bool> {
    reg_dword(
        r"SYSTEM\CurrentControlSet\Control\SecureBoot\State",
        "UEFISecureBootEnabled",
    )
    .map(|v| v != 0)
}

const CODEINTEGRITY_OPTION_TESTSIGN: u32 = 0x02;
const CODEINTEGRITY_OPTION_HVCI_KMCI_ENABLED: u32 = 0x400;

#[repr(C)]
struct SystemCodeIntegrityInfo {
    length: u32,
    options: u32,
}

/// CodeIntegrityOptions of the running kernel (works unelevated, unlike reading the BCD with bcdedit).
pub fn code_integrity_options() -> Option<u32> {
    let mut info = SystemCodeIntegrityInfo {
        length: size_of::<SystemCodeIntegrityInfo>() as u32,
        options: 0,
    };
    let mut ret = 0u32;
    let status = unsafe {
        NtQuerySystemInformation(
            SystemCodeIntegrityInformation,
            (&raw mut info).cast(),
            info.length,
            &mut ret,
        )
    };
    status.is_ok().then_some(info.options)
}

fn reg_dword(subkey: &str, value: &str) -> Option<u32> {
    let mut data = 0u32;
    let mut size = 4u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            &HSTRING::from(subkey),
            &HSTRING::from(value),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut data).cast()),
            Some(&mut size),
        )
    };
    rc.is_ok().then_some(data)
}

// ---------------------------------------------------------------------------------------------------------------
// The install record (HKLM\SOFTWARE\crossc)
// ---------------------------------------------------------------------------------------------------------------

/// Registry key (under HKEY_LOCAL_MACHINE) where Install writes down what it did. Administrators write it,
/// everyone reads it.
pub const RECORD_KEY: &str = r"SOFTWARE\crossc";
/// SHA-1 thumbprint of the certificate of the last install whose driver was verified (40 hex digits).
pub const RECORD_CERT: &str = "CertThumbprint";
/// Thumbprints, separated by spaces, of certificates that were trusted by an install that did not get as far as
/// a verified driver, or that could not be removed when they were replaced (`model::pending_certs`).
pub const RECORD_PENDING: &str = "PendingCerts";
/// Version of the release that is installed (`0.2.0`); absent after an install from the repo.
pub const RECORD_VERSION: &str = "Version";
/// Version number of the driver package that install staged (`8.50.53.934`).
pub const RECORD_DRIVER_VER: &str = "DriverVer";

/// What Install recorded. Works unelevated; a missing key or value is simply `None`.
pub fn read_record() -> InstallRecord {
    read_record_at(HKEY_LOCAL_MACHINE, RECORD_KEY)
}

fn read_record_at(root: HKEY, key: &str) -> InstallRecord {
    InstallRecord {
        cert_thumbprint: reg_string(root, key, RECORD_CERT),
        pending_certs: reg_string(root, key, RECORD_PENDING),
        version: reg_string(root, key, RECORD_VERSION),
        driver_ver: reg_string(root, key, RECORD_DRIVER_VER),
    }
}

/// Sets one value of the record, or deletes it (`None`). Needs administrator rights; the callers check.
pub fn write_record(name: &str, value: Option<&str>) -> Result<(), String> {
    write_record_at(HKEY_LOCAL_MACHINE, RECORD_KEY, name, value)
        .map_err(|e| format!(r"cannot update HKLM\{RECORD_KEY}\{name}: {e}"))
}

/// A registry key handle, closed on drop.
struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

fn write_record_at(
    root: HKEY,
    key: &str,
    name: &str,
    value: Option<&str>,
) -> windows::core::Result<()> {
    let (key_name, name) = (HSTRING::from(key), HSTRING::from(name));
    unsafe {
        let mut handle = HKEY::default();
        match value {
            Some(value) => {
                RegCreateKeyExW(
                    root,
                    &key_name,
                    None,
                    PCWSTR::null(),
                    REG_OPTION_NON_VOLATILE,
                    KEY_SET_VALUE,
                    None,
                    &mut handle,
                    None,
                )
                .ok()?;
                let handle = Key(handle);
                let data: Vec<u8> = value
                    .encode_utf16()
                    .chain([0])
                    .flat_map(u16::to_le_bytes)
                    .collect();
                RegSetValueExW(handle.0, &name, None, REG_SZ, Some(&data)).ok()
            }
            None => {
                let opened = RegOpenKeyExW(
                    root,
                    &key_name,
                    None,
                    KEY_SET_VALUE | KEY_QUERY_VALUE,
                    &mut handle,
                );
                if opened == ERROR_FILE_NOT_FOUND {
                    return Ok(());
                }
                opened.ok()?;
                let handle = Key(handle);
                let deleted = RegDeleteValueW(handle.0, &name);
                if deleted != ERROR_FILE_NOT_FOUND {
                    deleted.ok()?;
                }
                // The last value gone: leave no empty key behind.
                let (mut subkeys, mut values) = (1u32, 1u32);
                let counted = RegQueryInfoKeyW(
                    handle.0,
                    None,
                    None,
                    None,
                    Some(&mut subkeys),
                    None,
                    None,
                    Some(&mut values),
                    None,
                    None,
                    None,
                    None,
                );
                drop(handle);
                if counted.is_ok() && subkeys == 0 && values == 0 {
                    let _ = RegDeleteKeyW(root, &key_name);
                }
                Ok(())
            }
        }
    }
}

/// A string value; `None` when it is missing, empty or no string.
pub fn reg_string(root: HKEY, subkey: &str, value: &str) -> Option<String> {
    let (subkey, value) = (HSTRING::from(subkey), HSTRING::from(value));
    unsafe {
        let mut bytes = 0u32;
        RegGetValueW(
            root,
            &subkey,
            &value,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut bytes),
        )
        .ok()
        .ok()?;
        if bytes == 0 || bytes > 4096 {
            return None;
        }
        let mut buf = vec![0u16; (bytes as usize).div_ceil(2)];
        let mut bytes = (buf.len() * 2) as u32;
        RegGetValueW(
            root,
            &subkey,
            &value,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
        .ok()
        .ok()?;
        let text = String::from_utf16_lossy(&buf);
        let text = text.split('\0').next().unwrap_or("").trim();
        (!text.is_empty()).then(|| text.to_string())
    }
}

/// A known folder as the system reports it (not an environment variable, which is whatever the process that
/// started this one put there).
fn known_folder(id: &GUID) -> Option<PathBuf> {
    unsafe {
        let path = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let text = path.to_string().ok();
        CoTaskMemFree(Some(path.0.cast_const().cast()));
        text.filter(|t| !t.is_empty()).map(PathBuf::from)
    }
}

/// The Program Files folder of this (64-bit) process, as the system reports it.
pub fn program_files() -> Option<PathBuf> {
    known_folder(&FOLDERID_ProgramFiles)
}

/// `<Program Files>\crossc` (`layout::install_dir`), with Program Files as the system reports it. The
/// `ProgramFiles` environment variable is never consulted: if the system does not answer, the fixed fallback of
/// `layout::install_dir` is used.
pub fn install_dir() -> PathBuf {
    layout::install_dir(program_files().as_deref().map(Path::as_os_str))
}

/// `<Program Files>\crossc\releases` (`layout::releases_dir`), on the same terms as [`install_dir`].
pub fn releases_dir() -> PathBuf {
    layout::releases_dir(program_files().as_deref().map(Path::as_os_str))
}

/// The folders a program may be run from in an elevated window (`update::gh_to_run`): Program Files, both of
/// them, as the system reports them. Changing anything there takes administrator rights. The Windows folder is
/// deliberately not one of them: parts of it (`Tasks`, `tracing`, `System32\spool\drivers\color`) accept new
/// files from ordinary users.
pub fn protected_dirs() -> Vec<PathBuf> {
    [&FOLDERID_ProgramFiles, &FOLDERID_ProgramFilesX86]
        .into_iter()
        .filter_map(known_folder)
        .collect()
}

// ---------------------------------------------------------------------------------------------------------------
// Package and certificate
// ---------------------------------------------------------------------------------------------------------------

pub fn package_info(kind: PackageKind, dir: PathBuf) -> PackageInfo {
    let missing = REQUIRED_FILES
        .iter()
        .copied()
        .filter(|f| !dir.join(f).is_file())
        .collect();
    let modified = std::fs::metadata(dir.join("crossc_bth.sys"))
        .and_then(|m| m.modified())
        .ok();
    PackageInfo {
        kind,
        built_at: modified.and_then(local_time_text),
        built_age: modified
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .map(|d| format_age(d.as_secs())),
        driver_ver: read_inf(&dir).and_then(|text| inf_driver_ver(&text)),
        dir,
        missing,
    }
}

fn read_inf(package_dir: &Path) -> Option<String> {
    std::fs::read(package_dir.join(INF_NAME))
        .ok()
        .map(|bytes| decode_inf(&bytes))
}

/// Hardware IDs of every supported buds model, from the package's INF (empty when there is no INF).
pub fn package_hardware_ids(package_dir: &Path) -> Vec<String> {
    read_inf(package_dir)
        .map(|text| inf_hardware_ids(&text))
        .unwrap_or_default()
}

/// Why the file at `path` is not a complete Windows executable (`parse::exe_problem` on its headers and length,
/// or that it cannot be read); `None` when it is one. Install checks its `crossc-service.exe` with this before it
/// changes anything, and the status uses the same check, so both agree.
pub fn damaged_exe(path: &Path) -> Option<String> {
    let read = || -> std::io::Result<(Vec<u8>, u64)> {
        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        let mut head = Vec::new();
        file.take(EXE_HEAD_LEN as u64).read_to_end(&mut head)?;
        Ok((head, len))
    };
    match read() {
        Ok((head, len)) => exe_problem(&head, len),
        Err(e) => Some(format!("it cannot be read: {e}")),
    }
}

/// `2026-11-10 22:51` in the machine's time zone.
fn local_time_text(t: SystemTime) -> Option<String> {
    // FILETIME counts 100 ns ticks since 1601-01-01; the Unix epoch is 11644473600 s later.
    let since_unix = t.duration_since(SystemTime::UNIX_EPOCH).ok()?;
    let ticks = since_unix.as_nanos() / 100 + 11_644_473_600u128 * 10_000_000;
    let ticks = u64::try_from(ticks).ok()?;
    let ft = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    unsafe {
        FileTimeToSystemTime(&ft, &mut utc).ok()?;
        SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).ok()?;
    }
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute
    ))
}

pub fn cert_info(cer: &Path) -> Result<CertInfo, String> {
    let der = std::fs::read(cer)
        .map_err(|_| "no WDRLocalTestCert.cer in the driver package".to_string())?;
    let tp = thumbprint(&der).map_err(|e| format!("cannot parse {}: {e}", cer.display()))?;
    let in_store = |name: &str| {
        Store::local_machine(name, true)
            .map(|s| s.contains_sha1(&tp))
            .unwrap_or(false)
    };
    Ok(CertInfo {
        thumbprint: hex_upper(&tp),
        in_root: in_store(TRUST_STORES[0]),
        in_trusted_publisher: in_store(TRUST_STORES[1]),
    })
}

// ---------------------------------------------------------------------------------------------------------------
// Staged packages
// ---------------------------------------------------------------------------------------------------------------

pub fn staged_packages() -> Result<Vec<StagedPackage>, String> {
    let out = Command::new("pnputil")
        .arg("/enum-drivers")
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("pnputil failed to start: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "pnputil /enum-drivers exited {}",
            out.status.code().unwrap_or(-1)
        ));
    }
    Ok(parse_enum_drivers(&String::from_utf8_lossy(&out.stdout)))
}

// ---------------------------------------------------------------------------------------------------------------
// Device nodes (cfgmgr32)
// ---------------------------------------------------------------------------------------------------------------

const A2DP_PREFIX: &str = r"BTHENUM\{0000110B-0000-1000-8000-00805F9B34FB}";

/// Every paired pair of supported buds: the BTHENUM A2DP instances that carry one of the INF's hardware IDs
/// (`hwid_matches`), present ones first. Nodes of any other device are never looked at beyond their hardware IDs.
pub fn find_buds_nodes(inf_ids: &[String]) -> Result<Vec<NodeInfo>, String> {
    if inf_ids.is_empty() {
        return Ok(Vec::new());
    }
    a2dp_nodes(|devinst| hwid_matches(&prop_strings(devinst, &DEVPKEY_Device_HardwareIds), inf_ids))
}

/// The A2DP nodes that run crossc_bth although `inf_ids` does not list their model (`unlisted_on_driver`): buds
/// bound by another version of the package, or every node on the driver when there is no INF. Read-only, like
/// everything here; of other devices' nodes only the hardware IDs and the service name are read.
pub fn find_unlisted_nodes(inf_ids: &[String]) -> Result<Vec<NodeInfo>, String> {
    a2dp_nodes(|devinst| {
        unlisted_on_driver(
            &prop_strings(devinst, &DEVPKEY_Device_HardwareIds),
            prop_string(devinst, &DEVPKEY_Device_Service).as_deref(),
            inf_ids,
        )
    })
}

/// The BTHENUM A2DP instances `wanted` picks (it gets the devnode handle), present ones first.
fn a2dp_nodes(wanted: impl Fn(u32) -> bool) -> Result<Vec<NodeInfo>, String> {
    let ids = device_ids("BTHENUM")?;
    let mut nodes = Vec::new();
    for id in ids
        .iter()
        .filter(|id| id.to_ascii_uppercase().starts_with(A2DP_PREFIX))
    {
        let Some(devinst) = locate(id, true) else {
            continue;
        };
        if !wanted(devinst) {
            continue;
        }
        let mut node = node_info(id, devinst);
        node.device_name = device_name(&ids, id);
        nodes.push(node);
    }
    Ok(order_nodes(nodes))
}

/// The name the buds were paired under: the name of the `BTHENUM\DEV_<address>` node next to the service node.
fn device_name(bthenum_ids: &[String], instance_id: &str) -> Option<String> {
    let address = bt_address(instance_id)?;
    let devinst = locate(bt_device_node(bthenum_ids, &address)?, true)?;
    prop_string(devinst, &DEVPKEY_Device_FriendlyName)
        .or_else(|| prop_string(devinst, &DEVPKEY_NAME))
}

/// The current state of nodes found earlier (to verify after install / uninstall). A node that is gone (buds
/// unpaired meanwhile) is left out.
pub fn refresh_nodes(nodes: &[NodeInfo]) -> Vec<NodeInfo> {
    nodes
        .iter()
        .filter_map(|old| {
            let devinst = locate(&old.instance_id, true)?;
            let mut node = node_info(&old.instance_id, devinst);
            node.device_name = old.device_name.clone();
            Some(node)
        })
        .collect()
}

fn node_info(instance_id: &str, devinst: u32) -> NodeInfo {
    let present = locate(instance_id, false).is_some();
    let problem = if present { problem_code(devinst) } else { None };
    NodeInfo {
        instance_id: instance_id.to_string(),
        present,
        name: prop_string(devinst, &DEVPKEY_Device_FriendlyName)
            .or_else(|| prop_string(devinst, &DEVPKEY_NAME)),
        device_name: None,
        service: prop_string(devinst, &DEVPKEY_Device_Service),
        problem,
        inf: prop_string(devinst, &DEVPKEY_Device_DriverInfPath),
        version: prop_string(devinst, &DEVPKEY_Device_DriverVersion),
        stack: prop_strings(devinst, &DEVPKEY_Device_Stack),
    }
}

fn device_ids(enumerator: &str) -> Result<Vec<String>, String> {
    let filter = HSTRING::from(enumerator);
    // The list can grow between the two calls; retry a few times.
    for _ in 0..4 {
        let mut len = 0u32;
        let cr = unsafe {
            CM_Get_Device_ID_List_SizeW(&mut len, &filter, CM_GETIDLIST_FILTER_ENUMERATOR)
        };
        if cr != CR_SUCCESS {
            return Err(format!(
                "CM_Get_Device_ID_List_Size failed (CONFIGRET {})",
                cr.0
            ));
        }
        let mut buf = vec![0u16; len as usize + 1];
        let cr =
            unsafe { CM_Get_Device_ID_ListW(&filter, &mut buf, CM_GETIDLIST_FILTER_ENUMERATOR) };
        if cr == CR_BUFFER_SMALL {
            continue;
        }
        if cr != CR_SUCCESS {
            return Err(format!("CM_Get_Device_ID_List failed (CONFIGRET {})", cr.0));
        }
        return Ok(split_multi_sz(&buf));
    }
    Err("device list kept changing".into())
}

fn locate(instance_id: &str, phantom: bool) -> Option<u32> {
    let id = HSTRING::from(instance_id);
    let flags = if phantom {
        CM_LOCATE_DEVNODE_PHANTOM
    } else {
        CM_LOCATE_DEVNODE_NORMAL
    };
    let mut devinst = 0u32;
    let cr = unsafe { CM_Locate_DevNodeW(&mut devinst, PCWSTR(id.as_ptr()), flags) };
    (cr == CR_SUCCESS).then_some(devinst)
}

fn problem_code(devinst: u32) -> Option<u32> {
    let mut status = CM_DEVNODE_STATUS_FLAGS(0);
    let mut problem = CM_PROB(0);
    let cr = unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, devinst, 0) };
    if cr != CR_SUCCESS {
        return None;
    }
    Some(if status.0 & DN_HAS_PROBLEM.0 != 0 {
        problem.0
    } else {
        0
    })
}

fn prop_raw(devinst: u32, key: &DEVPROPKEY) -> Option<Vec<u16>> {
    let mut ty = DEVPROPTYPE(0);
    let mut size = 0u32;
    let cr = unsafe { CM_Get_DevNode_PropertyW(devinst, key, &mut ty, None, &mut size, 0) };
    if cr != CR_BUFFER_SMALL || size == 0 {
        return None;
    }
    let mut buf = vec![0u16; (size as usize).div_ceil(2)];
    let cr = unsafe {
        CM_Get_DevNode_PropertyW(
            devinst,
            key,
            &mut ty,
            Some(buf.as_mut_ptr().cast()),
            &mut size,
            0,
        )
    };
    (cr == CR_SUCCESS).then_some(buf)
}

fn prop_string(devinst: u32, key: &DEVPROPKEY) -> Option<String> {
    prop_strings(devinst, key)
        .into_iter()
        .next()
        .filter(|s| !s.is_empty())
}

fn prop_strings(devinst: u32, key: &DEVPROPKEY) -> Vec<String> {
    prop_raw(devinst, key)
        .map(|b| split_multi_sz(&b))
        .unwrap_or_default()
}

/// REG_MULTI_SZ / DEVPROP_TYPE_STRING_LIST (a single string parses as a one-element list).
fn split_multi_sz(buf: &[u16]) -> Vec<String> {
    buf.split(|&c| c == 0)
        .take_while(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn multi_sz() {
        assert_eq!(split_multi_sz(&w("a\0bc\0\0")), vec!["a", "bc"]);
        assert_eq!(split_multi_sz(&w("single\0")), vec!["single"]);
        assert_eq!(split_multi_sz(&w("\0")), Vec::<String>::new());
        assert_eq!(split_multi_sz(&[]), Vec::<String>::new());
    }

    #[test]
    fn queries_do_not_fail_unelevated() {
        // Smoke test on the build machine: the code integrity query works without elevation.
        assert!(code_integrity_options().is_some());
        let _ = is_admin();
        assert!(device_ids("BTHENUM").is_ok());
    }

    #[test]
    fn nodes_are_found_only_for_inf_ids() {
        // No INF IDs: nothing is enumerated, nothing matches.
        assert_eq!(find_buds_nodes(&[]), Ok(Vec::new()));
        // An ID no device has, and the generic A2DP compatible ID (which the INF parser never returns, and which
        // is not a hardware ID of any node anyway): no node, whatever is paired with this machine.
        let ids = [
            r"BTHENUM\{0000110b-0000-1000-8000-00805f9b34fb}_VID&0001ffff_PID&ffff".to_string(),
            r"BTHENUM\{0000110b-0000-1000-8000-00805f9b34fb}".to_string(),
        ];
        assert_eq!(find_buds_nodes(&ids), Ok(Vec::new()));
        assert!(refresh_nodes(&[NodeInfo {
            instance_id: r"BTHENUM\{0000110b-0000-1000-8000-00805f9b34fb}_VID&0001ffff_PID&ffff\0&0&0_C00000000"
                .into(),
            ..NodeInfo::default()
        }])
        .is_empty());
    }

    #[test]
    fn unlisted_nodes_never_overlap_the_listed_ones() {
        // Whatever is paired with this machine: a node is either one of the INF's models or (when it runs
        // crossc_bth without being one) unlisted, never both; and every unlisted node is on the driver.
        let ids =
            [r"BTHENUM\{0000110b-0000-1000-8000-00805f9b34fb}_VID&00010075_PID&a013".to_string()];
        let listed = find_buds_nodes(&ids).unwrap();
        let unlisted = find_unlisted_nodes(&ids).unwrap();
        for n in &unlisted {
            assert!(n.runs(crate::model::DRIVER_SERVICE), "{n:?}");
            assert!(
                !listed.iter().any(|l| l.instance_id == n.instance_id),
                "{n:?}"
            );
        }
        // With no INF, the nodes on the driver are exactly the unlisted ones.
        let all = find_unlisted_nodes(&[]).unwrap();
        let on_driver = |nodes: &[NodeInfo]| {
            nodes
                .iter()
                .filter(|n| n.runs(crate::model::DRIVER_SERVICE))
                .count()
        };
        assert_eq!(all.len(), on_driver(&listed) + unlisted.len());
    }

    #[test]
    fn damaged_exe_is_told_from_a_whole_one() {
        use crate::layout::tests::TempDir;
        let t = TempDir::new("damaged-exe");
        assert_eq!(damaged_exe(&std::env::current_exe().unwrap()), None);
        // The zero-byte placeholder, a text file, a real exe cut in half, a missing file.
        let empty = t.file("crossc-service.exe", "");
        assert_eq!(damaged_exe(&empty).as_deref(), Some("the file is empty"));
        let text = t.file("text.exe", "not a program");
        assert!(damaged_exe(&text).unwrap().contains("no MZ header"));
        let bytes = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let half = t.0.join("half.exe");
        std::fs::write(&half, &bytes[..bytes.len() / 2]).unwrap();
        assert!(damaged_exe(&half).unwrap().starts_with("it is cut short"));
        assert!(
            damaged_exe(&t.0.join("none.exe"))
                .unwrap()
                .starts_with("it cannot be read")
        );
    }

    #[test]
    fn package_info_reads_the_inf() {
        use crate::layout::tests::TempDir;
        let t = TempDir::new("package-info");
        let dir = t.0.join("driver");
        let info = package_info(PackageKind::Bundle, dir.clone());
        assert_eq!(info.missing, REQUIRED_FILES.to_vec());
        assert_eq!((info.driver_ver, info.built_at), (None, None));
        assert!(package_hardware_ids(&dir).is_empty());

        t.file(
            r"driver\crossc_bth.inf",
            "[Version]\r\nDriverVer = 11/11/2026,0.1.0.0\r\n[Standard.NTamd64]\r\n\
             %d% = I, BTHENUM\\{0000110b-0000-1000-8000-00805f9b34fb}_VID&00010075_PID&a013\r\n\
             %d% = I, BTHENUM\\{0000110b-0000-1000-8000-00805f9b34fb}_VID&00010075_PID&a00f\r\n",
        );
        t.file(r"driver\crossc_bth.sys", "");
        let info = package_info(PackageKind::Bundle, dir.clone());
        assert_eq!(info.missing, vec!["crossc_bth.cat", "WDRLocalTestCert.cer"]);
        assert_eq!(info.version(), Some("0.1.0.0"));
        assert!(info.built_at.is_some() && info.built_age.is_some());
        assert_eq!(package_hardware_ids(&dir).len(), 2);
    }

    #[test]
    fn install_record_round_trip() {
        use windows::Win32::System::Registry::HKEY_CURRENT_USER;
        // The same code as for HKLM\SOFTWARE\crossc, on a throwaway key of the current user (no elevation, and
        // nothing of the machine is touched). The key is gone again when the test ends.
        let key = format!(r"Software\crossc-installer-test-{}", std::process::id());
        let read = || read_record_at(HKEY_CURRENT_USER, &key);
        let write = |name, value| write_record_at(HKEY_CURRENT_USER, &key, name, value).unwrap();
        assert_eq!(read(), InstallRecord::default());
        // Deleting from a key that does not exist is not an error.
        write(RECORD_CERT, None);

        let tp = "00E56600170648F8D7B71EACA4392DBFCFC615FD";
        write(RECORD_CERT, Some(tp));
        write(RECORD_VERSION, Some("0.2.0"));
        write(RECORD_DRIVER_VER, Some("8.50.53.934"));
        assert_eq!(
            read(),
            InstallRecord {
                cert_thumbprint: Some(tp.into()),
                pending_certs: None,
                version: Some("0.2.0".into()),
                driver_ver: Some("8.50.53.934".into()),
            }
        );
        // The pending list: written, read back as written, deleted.
        let pending = format!("{tp} BF82EE9838ED6EE917234D4EF9319BC4EF8C39FF");
        write(RECORD_PENDING, Some(&pending));
        assert_eq!(read().pending_certs.as_deref(), Some(pending.as_str()));
        assert_eq!(
            crate::model::pending_certs(read().pending_certs.as_deref()).len(),
            2
        );
        write(RECORD_PENDING, None);
        assert_eq!(read().pending_certs, None);
        // Overwrite one, delete one (twice: the second time there is nothing to delete).
        write(RECORD_VERSION, Some("0.3.0-rc.1"));
        write(RECORD_DRIVER_VER, None);
        write(RECORD_DRIVER_VER, None);
        assert_eq!(
            read(),
            InstallRecord {
                cert_thumbprint: Some(tp.into()),
                pending_certs: None,
                version: Some("0.3.0-rc.1".into()),
                driver_ver: None,
            }
        );
        // An empty value reads as no value.
        write(RECORD_VERSION, Some(""));
        assert_eq!(read().version, None);
        // The last value gone: the key goes with it.
        write(RECORD_VERSION, None);
        write(RECORD_CERT, None);
        assert_eq!(read(), InstallRecord::default());
        let mut handle = HKEY::default();
        let opened = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                &HSTRING::from(key.as_str()),
                None,
                KEY_QUERY_VALUE,
                &mut handle,
            )
        };
        assert_eq!(opened, ERROR_FILE_NOT_FOUND);
    }

    #[test]
    fn the_machine_record_is_readable_unelevated() {
        // Whatever it holds on this machine (usually nothing): reading never fails, and never needs elevation.
        let record = read_record();
        if let Some(tp) = &record.cert_thumbprint {
            assert!(!tp.is_empty());
        }
    }

    #[test]
    fn protected_folders_are_the_systems() {
        let dirs = protected_dirs();
        assert!(!dirs.is_empty(), "{dirs:?}");
        assert!(
            dirs.iter().all(|d| d.is_absolute() && d.is_dir()),
            "{dirs:?}"
        );
        // Program Files is one of them; the Windows folder (parts of which ordinary users can write to) and
        // the temp folder are not.
        let program_files = program_files().unwrap();
        assert!(dirs.contains(&program_files), "{dirs:?}");
        let inside = |p: &Path| dirs.iter().any(|d| crate::actions::is_inside(p, d));
        let windows = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap();
        assert!(inside(&program_files.join("GitHub CLI").join("gh.exe")));
        assert!(!inside(&windows.join("System32").join("cmd.exe")));
        assert!(!inside(&windows.join("Tasks").join("gh.exe")));
        assert!(!inside(&std::env::temp_dir().join("gh.exe")));
    }

    #[test]
    fn the_install_folder_comes_from_the_system_not_the_environment() {
        // This test changes no environment variable (tests share the process); it shows the two things the
        // guarantee rests on. One: the folder is built on what the system reports.
        let program_files = program_files().unwrap();
        assert!(program_files.is_absolute() && program_files.is_dir());
        assert_eq!(install_dir(), program_files.join("crossc"));
        assert_eq!(
            releases_dir(),
            program_files.join("crossc").join(layout::RELEASES_DIR)
        );
        // Two: nothing in this crate reads the variable any more. Every use of the folder goes through the two
        // functions above (the sources are searched, the tests' own lines apart).
        let needle = ["var_os(\"Program", "Files\")"].concat();
        let needle_var = ["var(\"Program", "Files\")"].concat();
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        for entry in std::fs::read_dir(&src).unwrap().flatten() {
            let text = std::fs::read_to_string(entry.path()).unwrap();
            assert!(
                !text.contains(&needle) && !text.contains(&needle_var),
                "{} reads the ProgramFiles environment variable",
                entry.path().display()
            );
        }
    }

    #[test]
    fn a_kept_release_counts_only_while_its_certificate_is_trusted() {
        use crate::cert::tests::WDR_A;
        use crate::layout::tests::TempDir;
        let t = TempDir::new("kept-cert");
        let cer = t.0.join("WDRLocalTestCert.cer");
        // A certificate made for the tests: no machine trusts it. So a release carrying it (downloaded, never
        // confirmed; or confirmed once and untrusted again by "Remove certificate") is not used by Install.
        std::fs::write(&cer, WDR_A).unwrap();
        assert!(!release_cert_is_trusted(&cer));
        // No file, a file that is not a certificate.
        assert!(!release_cert_is_trusted(&t.0.join("missing.cer")));
        std::fs::write(&cer, b"junk").unwrap();
        assert!(!release_cert_is_trusted(&cer));
        // It agrees with what the status shows for a package's certificate (on a development machine that has
        // installed its own build, this is the trusted case).
        for flavor in [Flavor::Debug, Flavor::Release] {
            let built = Layout::locate()
                .package_dir(flavor)
                .join("WDRLocalTestCert.cer");
            assert_eq!(
                release_cert_is_trusted(&built),
                cert_info(&built).is_ok_and(|c| c.in_root && c.in_trusted_publisher)
            );
        }
        // Any mode but online is left alone, without looking at the record.
        let repo = Layout::locate();
        assert_eq!(resolve_layout(&repo), repo);
    }

    #[test]
    fn local_time_is_formatted() {
        let t = local_time_text(SystemTime::now()).unwrap();
        assert_eq!(t.len(), 16, "{t}");
        assert_eq!(&t[4..5], "-");
    }
}
