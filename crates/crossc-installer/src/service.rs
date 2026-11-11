//! The `crossc` Windows service through the service control manager: a read-only query for the status (works
//! unelevated), and stop / start / delete for Install and Uninstall (admin only; the callers check).
//!
//! Registering the service is `crossc-service.exe install`'s job, not this module's. What is here is the part
//! that has to work when that exe is gone or broken.

use std::time::{Duration, Instant};

use windows::Win32::System::Services::{
    CloseServiceHandle, ControlService, DeleteService, OpenSCManagerW, OpenServiceW,
    QUERY_SERVICE_CONFIGW, QueryServiceConfigW, QueryServiceStatusEx, SC_HANDLE,
    SC_MANAGER_CONNECT, SC_STATUS_PROCESS_INFO, SERVICE_CONTROL_STOP, SERVICE_QUERY_CONFIG,
    SERVICE_QUERY_STATUS, SERVICE_START, SERVICE_STATUS, SERVICE_STATUS_PROCESS, SERVICE_STOP,
    StartServiceW,
};
use windows::core::{HRESULT, HSTRING, PCWSTR};

use crate::model::{ServiceInfo, ServiceState, service_state};
use crate::parse::image_path_exe;

const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
const ERROR_SERVICE_ALREADY_RUNNING: u32 = 1056;
const ERROR_SERVICE_DOES_NOT_EXIST: u32 = 1060;
const ERROR_SERVICE_NOT_ACTIVE: u32 = 1062;
const ERROR_SERVICE_MARKED_FOR_DELETE: u32 = 1072;
/// Standard access right `DELETE`.
const DELETE: u32 = 0x0001_0000;

/// How long the crossc service says a stop may take (its `STOP_WAIT_HINT`: a signaling exchange in flight, CLOSE,
/// the driver's channel closes). Kept in step with crates/crossc-service by hand; the installer does not link it.
const SERVICE_STOP_HINT: Duration = Duration::from_secs(40);
/// How long to wait for a stop: the service's own allowance and a margin, as `crossc-service uninstall` does.
/// Giving up earlier would abort Install at its first step while the service is still stopping in good order.
const STOP_TIMEOUT: Duration = Duration::from_secs(SERVICE_STOP_HINT.as_secs() + 5);
const START_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a service must stay up after reporting "running" before the start counts as good.
const SETTLE: Duration = Duration::from_millis(1500);
const POLL: Duration = Duration::from_millis(250);

/// A service control handle, closed on drop.
struct Handle(SC_HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseServiceHandle(self.0);
        }
    }
}

fn is_win32(e: &windows::core::Error, code: u32) -> bool {
    e.code() == HRESULT::from_win32(code)
}

/// Opens the service with `access`; `Ok(None)` when no such service is installed.
fn open(name: &str, access: u32) -> Result<Option<Handle>, String> {
    unsafe {
        let scm = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT)
            .map(Handle)
            .map_err(|e| format!("cannot connect to the service control manager: {e}"))?;
        match OpenServiceW(scm.0, &HSTRING::from(name), access) {
            Ok(h) => Ok(Some(Handle(h))),
            Err(e) if is_win32(&e, ERROR_SERVICE_DOES_NOT_EXIST) => Ok(None),
            Err(e) => Err(format!("cannot open the {name} service: {e}")),
        }
    }
}

/// Current state and Win32 exit code.
fn status(h: &Handle) -> Result<(ServiceState, u32), String> {
    let mut ssp = SERVICE_STATUS_PROCESS::default();
    let mut needed = 0u32;
    unsafe {
        let buf = std::slice::from_raw_parts_mut(
            (&raw mut ssp).cast::<u8>(),
            size_of::<SERVICE_STATUS_PROCESS>(),
        );
        QueryServiceStatusEx(h.0, SC_STATUS_PROCESS_INFO, Some(buf), &mut needed)
            .map_err(|e| format!("cannot query the service state: {e}"))?;
    }
    Ok((service_state(ssp.dwCurrentState.0), ssp.dwWin32ExitCode))
}

/// The executable the service is registered with.
fn binary_path(h: &Handle) -> Option<String> {
    let mut needed = 0u32;
    unsafe {
        match QueryServiceConfigW(h.0, None, 0, &mut needed) {
            Err(e) if is_win32(&e, ERROR_INSUFFICIENT_BUFFER) && needed > 0 => {}
            _ => return None,
        }
        // u64 elements keep the buffer aligned for the struct at its start.
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        let config = buf.as_mut_ptr().cast::<QUERY_SERVICE_CONFIGW>();
        QueryServiceConfigW(h.0, Some(config), needed, &mut needed).ok()?;
        let path = (*config).lpBinaryPathName;
        if path.is_null() {
            return None;
        }
        let path = path.to_string().ok()?;
        Some(image_path_exe(&path)).filter(|p| !p.is_empty())
    }
}

/// State and executable of the service. Read-only; works unelevated.
pub fn query(name: &str) -> Result<ServiceInfo, String> {
    let Some(h) = open(name, SERVICE_QUERY_STATUS | SERVICE_QUERY_CONFIG)? else {
        return Ok(ServiceInfo {
            state: ServiceState::NotInstalled,
            exe: None,
        });
    };
    Ok(ServiceInfo {
        state: status(&h)?.0,
        exe: binary_path(&h),
    })
}

/// Polls until `done(state)` or the timeout; returns the last state and exit code.
fn wait(
    h: &Handle,
    timeout: Duration,
    done: impl Fn(ServiceState) -> bool,
) -> Result<(ServiceState, u32), String> {
    let deadline = Instant::now() + timeout;
    loop {
        let now = status(h)?;
        if done(now.0) || Instant::now() >= deadline {
            return Ok(now);
        }
        std::thread::sleep(POLL);
    }
}

/// Stops the service and waits until it has stopped. `Ok(false)`: it is not installed.
pub fn stop(name: &str, log: &dyn Fn(String)) -> Result<bool, String> {
    let Some(h) = open(name, SERVICE_STOP | SERVICE_QUERY_STATUS)? else {
        return Ok(false);
    };
    stop_handle(name, &h, log).map(|()| true)
}

fn stop_handle(name: &str, h: &Handle, log: &dyn Fn(String)) -> Result<(), String> {
    if status(h)?.0 == ServiceState::Stopped {
        log(format!("the {name} service is stopped"));
        return Ok(());
    }
    log(format!("stopping the {name} service"));
    let mut last = SERVICE_STATUS::default();
    match unsafe { ControlService(h.0, SERVICE_CONTROL_STOP, &mut last) } {
        Ok(()) => {}
        Err(e) if is_win32(&e, ERROR_SERVICE_NOT_ACTIVE) => {}
        // A service that is already stopping refuses a second stop; the wait below settles it either way.
        Err(e) if status(h)?.0 == ServiceState::StopPending => {
            log(format!("stop already under way ({e})"));
        }
        Err(e) => return Err(format!("cannot stop the {name} service: {e}")),
    }
    match wait(h, STOP_TIMEOUT, |s| s == ServiceState::Stopped)?.0 {
        ServiceState::Stopped => {
            log(format!("the {name} service is stopped"));
            Ok(())
        }
        state => Err(format!(
            "the {name} service did not stop within {} s ({state})",
            STOP_TIMEOUT.as_secs()
        )),
    }
}

/// Starts the service and waits until it runs and stays up for a moment.
pub fn start(name: &str, log: &dyn Fn(String)) -> Result<(), String> {
    let Some(h) = open(name, SERVICE_START | SERVICE_QUERY_STATUS)? else {
        return Err(format!("the {name} service is not installed"));
    };
    match unsafe { StartServiceW(h.0, None) } {
        Ok(()) => log(format!("start requested for the {name} service")),
        Err(e) if is_win32(&e, ERROR_SERVICE_ALREADY_RUNNING) => {
            log(format!("the {name} service is already running"));
        }
        Err(e) => return Err(format!("cannot start the {name} service: {e}")),
    }
    let (state, exit) = wait(&h, START_TIMEOUT, |s| {
        matches!(s, ServiceState::Running | ServiceState::Stopped)
    })?;
    if state == ServiceState::Running {
        std::thread::sleep(SETTLE);
    }
    match (state, status(&h)?) {
        (ServiceState::Running, (ServiceState::Running, _)) => {
            log(format!("the {name} service is running"));
            Ok(())
        }
        (ServiceState::Running, (_, exit)) | (ServiceState::Stopped, (_, exit)) => Err(format!(
            "the {name} service stopped right after it started (exit code {exit})"
        )),
        (state, _) => Err(format!(
            "the {name} service did not start within {} s ({state}, exit code {exit})",
            START_TIMEOUT.as_secs()
        )),
    }
}

/// Stops and deletes the service registration (the fallback when `crossc-service.exe uninstall` cannot run).
/// `Ok(false)`: it was not installed.
pub fn delete(name: &str, log: &dyn Fn(String)) -> Result<bool, String> {
    let Some(h) = open(name, DELETE | SERVICE_STOP | SERVICE_QUERY_STATUS)? else {
        return Ok(false);
    };
    stop_handle(name, &h, log)?;
    match unsafe { DeleteService(h.0) } {
        Ok(()) => {}
        Err(e) if is_win32(&e, ERROR_SERVICE_MARKED_FOR_DELETE) => {
            log(format!("the {name} service is already marked for deletion"));
        }
        Err(e) => return Err(format!("cannot delete the {name} service: {e}")),
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_works_unelevated() {
        // A service every Windows has, running from svchost.
        let info = query("EventLog").unwrap();
        assert_eq!(info.state, ServiceState::Running);
        let exe = info.exe.expect("binary path");
        assert!(exe.to_ascii_lowercase().ends_with("svchost.exe"), "{exe}");
    }

    #[test]
    fn stop_wait_outlasts_the_time_the_service_may_take_to_stop() {
        assert!(STOP_TIMEOUT >= SERVICE_STOP_HINT + Duration::from_secs(5));
        assert_eq!(STOP_TIMEOUT, Duration::from_secs(45));
    }

    #[test]
    fn missing_service_is_not_installed_not_an_error() {
        let info = query("crossc-installer-no-such-service").unwrap();
        assert_eq!(
            info,
            ServiceInfo {
                state: ServiceState::NotInstalled,
                exe: None
            }
        );
        // The mutating calls find nothing to do, whatever rights the caller has (nothing is opened for writing
        // before the lookup fails).
        let log = |_: String| {};
        assert_eq!(stop("crossc-installer-no-such-service", &log), Ok(false));
        assert_eq!(delete("crossc-installer-no-such-service", &log), Ok(false));
        assert!(start("crossc-installer-no-such-service", &log).is_err());
    }
}
