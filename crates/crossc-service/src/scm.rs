//! The Windows service side: registration with the service control manager
//! (`install`, `uninstall`, `status`) and the entry the SCM starts
//! (`service`).

use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crossc_core::log::{Level, Log};
use crossc_core::settings::Settings;
use crossc_status::pipe::Security;
use windows_service::service::{
    PowerEventParam, ServiceAccess, ServiceAction, ServiceActionType, ServiceControl,
    ServiceControlAccept, ServiceErrorControl, ServiceExitCode, ServiceFailureActions,
    ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState, ServiceStatus,
    ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

use crate::host::RealHost;
use crate::logger::{FileLog, default_log_dir};
use crate::status::{StatusLog, new_hub, start_server};
use crate::supervisor::{Command, Supervisor};

pub const SERVICE_NAME: &str = "crossc";
pub const DISPLAY_NAME: &str = "crossc SSC audio service";
pub const DESCRIPTION: &str = "Streams what Windows plays into the crossc audio endpoint to Galaxy Buds with Samsung's SSC codec (Seamless HiFi 48 kHz/24-bit, UHQ 96 kHz/24-bit).";
/// Argument the SCM passes so the executable knows it runs as the service.
pub const SERVICE_ARG: &str = "service";

/// Restart delays after the first, second and every later failure.
pub const RESTART_DELAYS: [Duration; 3] = [
    Duration::from_secs(5),
    Duration::from_secs(30),
    Duration::from_secs(60),
];
/// The failure count starts over after this long without a failure.
pub const FAILURE_RESET: Duration = Duration::from_secs(24 * 3600);
/// How long stopping may take: a signaling exchange in flight (5 s), CLOSE
/// (5 s), the driver's channel closes (up to 8 s each), with headroom.
pub const STOP_WAIT_HINT: Duration = Duration::from_secs(40);

const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
const ERROR_SERVICE_NOT_ACTIVE: i32 = 1062;
const ERROR_SERVICE_MARKED_FOR_DELETE: i32 = 1072;
const ERROR_SERVICE_ALREADY_RUNNING: i32 = 1056;

/// How the service is registered: auto-start, LocalSystem, own process,
/// `"<exe>" service` as its command line.
pub fn service_info(exe: &Path) -> ServiceInfo {
    ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe.to_path_buf(),
        launch_arguments: vec![OsString::from(SERVICE_ARG)],
        dependencies: Vec::new(),
        account_name: None, // LocalSystem
        account_password: None,
    }
}

/// Restart after 5 s, 30 s, then 60 s.
pub fn failure_actions() -> ServiceFailureActions {
    ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(FAILURE_RESET),
        reboot_msg: None,
        command: None,
        actions: Some(
            RESTART_DELAYS
                .iter()
                .map(|&delay| ServiceAction {
                    action_type: ServiceActionType::Restart,
                    delay,
                })
                .collect(),
        ),
    }
}

fn os_code(e: &windows_service::Error) -> Option<i32> {
    match e {
        windows_service::Error::Winapi(io) => io.raw_os_error(),
        _ => None,
    }
}

/// A service-manager error with the Win32 detail the crate's Display omits.
fn describe(what: &str, e: &windows_service::Error) -> String {
    match e {
        windows_service::Error::Winapi(io) => {
            let hint = if io.raw_os_error() == Some(ERROR_ACCESS_DENIED) {
                " (run from an elevated prompt)"
            } else {
                ""
            };
            format!("{what}: {io}{hint}")
        }
        other => format!("{what}: {other}"),
    }
}

fn state_name(s: ServiceState) -> &'static str {
    match s {
        ServiceState::Stopped => "stopped",
        ServiceState::StartPending => "starting",
        ServiceState::StopPending => "stopping",
        ServiceState::Running => "running",
        ServiceState::ContinuePending => "continuing",
        ServiceState::PausePending => "pausing",
        ServiceState::Paused => "paused",
    }
}

/// Register the service, or update its configuration if it exists.
/// With `start`, also start it if it is not running.
pub fn install(start: bool) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find my own path: {e}"))?;
    let info = service_info(&exe);
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .map_err(|e| describe("opening the service manager", &e))?;
    let access = ServiceAccess::QUERY_CONFIG
        | ServiceAccess::CHANGE_CONFIG
        | ServiceAccess::QUERY_STATUS
        | ServiceAccess::START;
    let (service, created) = match manager.open_service(SERVICE_NAME, access) {
        Ok(s) => {
            s.change_config(&info)
                .map_err(|e| describe("updating the service configuration", &e))?;
            (s, false)
        }
        Err(e) if os_code(&e) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => (
            manager
                .create_service(&info, access)
                .map_err(|e| describe("creating the service", &e))?,
            true,
        ),
        Err(e) => return Err(describe("opening the service", &e)),
    };
    service
        .set_description(DESCRIPTION)
        .map_err(|e| describe("setting the description", &e))?;
    service
        .update_failure_actions(failure_actions())
        .map_err(|e| describe("setting the failure actions", &e))?;
    service
        .set_failure_actions_on_non_crash_failures(true)
        .map_err(|e| describe("setting the failure actions flag", &e))?;
    let mut report = format!(
        "service \"{SERVICE_NAME}\" {}: auto-start, LocalSystem, \"{}\" {SERVICE_ARG}",
        if created { "installed" } else { "updated" },
        exe.display()
    );
    let running = service
        .query_status()
        .map(|s| s.current_state != ServiceState::Stopped)
        .unwrap_or(false);
    if start {
        if running {
            report.push_str("\nalready running (restart it to pick up a new executable)");
        } else {
            match service.start::<&str>(&[]) {
                Ok(()) => report.push_str("\nstarted"),
                Err(e) if os_code(&e) == Some(ERROR_SERVICE_ALREADY_RUNNING) => {
                    report.push_str("\nalready running")
                }
                Err(e) => return Err(describe("starting the service", &e)),
            }
        }
    } else if !running {
        report.push_str(
            "\nnot started: it starts at the next boot, or now with `crossc-service install --start`",
        );
    }
    Ok(report)
}

/// Stop and delete the service. Succeeds if it is not installed.
pub fn uninstall() -> Result<String, String> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|e| describe("opening the service manager", &e))?;
    let access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
    let service = match manager.open_service(SERVICE_NAME, access) {
        Ok(s) => s,
        Err(e) if os_code(&e) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => {
            return Ok(format!("service \"{SERVICE_NAME}\" is not installed"));
        }
        Err(e) => return Err(describe("opening the service", &e)),
    };
    let stopped = |s: &windows_service::service::Service| {
        s.query_status()
            .map(|st| st.current_state == ServiceState::Stopped)
            .unwrap_or(false)
    };
    let mut note = "";
    if !stopped(&service) {
        match service.stop() {
            Ok(_) => {}
            Err(e) if os_code(&e) == Some(ERROR_SERVICE_NOT_ACTIVE) => {}
            Err(e) => return Err(describe("stopping the service", &e)),
        }
        let t0 = Instant::now();
        while !stopped(&service) && t0.elapsed() < STOP_WAIT_HINT + Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(250));
        }
        if !stopped(&service) {
            note = " (it has not stopped yet; it is removed once it has)";
        }
    }
    match service.delete() {
        Ok(()) => {}
        Err(e) if os_code(&e) == Some(ERROR_SERVICE_MARKED_FOR_DELETE) => {}
        Err(e) => return Err(describe("deleting the service", &e)),
    }
    Ok(format!("service \"{SERVICE_NAME}\" removed{note}"))
}

/// One line about the service's registration and state.
pub fn status_line() -> String {
    let manager = match ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
    {
        Ok(m) => m,
        Err(e) => return describe("service: cannot open the service manager", &e),
    };
    let service = match manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG,
    ) {
        Ok(s) => s,
        Err(e) if os_code(&e) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => {
            return "service: not installed".into();
        }
        Err(e) => return describe("service: cannot open it", &e),
    };
    let state = match service.query_status() {
        Ok(s) => state_name(s.current_state).to_string(),
        Err(e) => describe("state unknown", &e),
    };
    let config = match service.query_config() {
        Ok(c) => format!(
            "{}, {}",
            match c.start_type {
                ServiceStartType::AutoStart => "auto-start",
                ServiceStartType::OnDemand => "manual start",
                ServiceStartType::Disabled => "disabled",
                _ => "driver start type",
            },
            c.executable_path.display()
        ),
        Err(e) => describe("configuration unknown", &e),
    };
    format!("service: installed, {state} ({config})")
}

// ---------------------------------------------------------------------------
// Running as the service
// ---------------------------------------------------------------------------

define_windows_service!(ffi_service_main, service_main);

/// Hand the process over to the service control dispatcher. Returns when
/// the service has stopped. Fails at once when not started by the SCM.
pub fn dispatch() -> Result<(), String> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main).map_err(|e| {
        describe(
            "cannot connect to the service control manager (this subcommand is for the SCM; use `crossc-service run` in a console)",
            &e,
        )
    })
}

fn service_main(_arguments: Vec<OsString>) {
    let log = Arc::new(FileLog::guarded(&default_log_dir(), Level::Info));
    if let Err(e) = run_service(log.clone()) {
        log.log(Level::Error, &format!("service failed: {e}"));
    }
}

fn status(state: ServiceState, wait_hint: Duration, exit: u32) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: if state == ServiceState::Running {
            ServiceControlAccept::STOP
                | ServiceControlAccept::SHUTDOWN
                | ServiceControlAccept::POWER_EVENT
        } else {
            ServiceControlAccept::empty()
        },
        exit_code: ServiceExitCode::Win32(exit),
        checkpoint: u32::from(wait_hint > Duration::ZERO),
        wait_hint,
        process_id: None,
    }
}

/// What a control code means for the supervisor. The handler only passes
/// this on; it never waits for the sessions.
fn command_for(control: &ServiceControl) -> Option<Command> {
    match control {
        ServiceControl::Stop | ServiceControl::Shutdown => Some(Command::Stop),
        ServiceControl::PowerEvent(PowerEventParam::Suspend) => Some(Command::Suspend),
        ServiceControl::PowerEvent(
            PowerEventParam::ResumeAutomatic
            | PowerEventParam::ResumeSuspend
            | PowerEventParam::ResumeCritical,
        ) => Some(Command::Resume),
        _ => None,
    }
}

fn run_service(file_log: Arc<FileLog>) -> Result<(), String> {
    // The status feed's state first, then the log that feeds it its last
    // warning, then everything that logs.
    let hub = new_hub(default_log_dir().display().to_string());
    let plain: Arc<dyn Log> = file_log.clone();
    let log: Arc<dyn Log> = Arc::new(StatusLog::new(plain.clone(), hub.clone()));
    let (tx, rx) = mpsc::channel::<Command>();
    let handler = move |control: ServiceControl| -> ServiceControlHandlerResult {
        match (command_for(&control), &control) {
            (Some(c), _) => {
                let _ = tx.send(c);
                ServiceControlHandlerResult::NoError
            }
            // Always answered, even though there is nothing to do.
            (None, ServiceControl::Interrogate | ServiceControl::PowerEvent(_)) => {
                ServiceControlHandlerResult::NoError
            }
            (None, _) => ServiceControlHandlerResult::NotImplemented,
        }
    };
    let handle = service_control_handler::register(SERVICE_NAME, handler)
        .map_err(|e| describe("registering the control handler", &e))?;
    let (settings, warnings) = Settings::load();
    file_log.set_level(settings.log_level);
    log.log(
        Level::Info,
        &format!(
            "crossc-service {} starting as a service; settings: {}",
            env!("CARGO_PKG_VERSION"),
            settings.describe()
        ),
    );
    for w in warnings {
        log.log(Level::Warn, &w);
    }
    handle
        .set_service_status(status(ServiceState::Running, Duration::ZERO, 0))
        .map_err(|e| describe("reporting the running state", &e))?;
    // The pipe the tray app reads. Without it the service works as before.
    let status_server = start_server(&hub, Security::Service, plain);
    let host = RealHost::new(settings, log.clone(), hub.clone());
    let mut supervisor = Supervisor::new(host, log.clone());
    supervisor.run(&rx);
    // Tell the SCM how long the teardown may take before it starts.
    let _ = handle.set_service_status(status(ServiceState::StopPending, STOP_WAIT_HINT, 0));
    log.log(Level::Info, "stopping");
    supervisor.stop_all();
    hub.clear_devices();
    // Disconnects the clients and ends the pipe's thread.
    drop(status_server);
    log.log(Level::Info, "stopped");
    handle
        .set_service_status(status(ServiceState::Stopped, Duration::ZERO, 0))
        .map_err(|e| describe("reporting the stopped state", &e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn registration_is_auto_start_local_system_with_the_service_argument() {
        let exe = PathBuf::from(r"C:\Program Files\crossc\crossc-service.exe");
        let i = service_info(&exe);
        assert_eq!(i.name, OsString::from("crossc"));
        assert_eq!(i.display_name, OsString::from("crossc SSC audio service"));
        assert_eq!(i.service_type, ServiceType::OWN_PROCESS);
        assert_eq!(i.start_type, ServiceStartType::AutoStart);
        assert_eq!(i.account_name, None, "LocalSystem");
        // The service manager crate quotes the path and appends the
        // arguments: "C:\Program Files\crossc\crossc-service.exe" service
        assert_eq!(i.executable_path, exe);
        assert_eq!(i.launch_arguments, vec![OsString::from("service")]);
        assert!(i.dependencies.is_empty());
    }

    #[test]
    fn failure_actions_restart_after_5_30_60_seconds() {
        let f = failure_actions();
        let actions = f.actions.unwrap();
        assert_eq!(actions.len(), 3);
        assert!(
            actions
                .iter()
                .all(|a| a.action_type == ServiceActionType::Restart)
        );
        let delays: Vec<u64> = actions.iter().map(|a| a.delay.as_secs()).collect();
        assert_eq!(delays, [5, 30, 60]);
        assert_eq!(
            f.reset_period,
            ServiceFailureResetPeriod::After(Duration::from_secs(86400))
        );
        assert!(f.command.is_none() && f.reboot_msg.is_none());
    }

    #[test]
    fn control_codes_map_to_commands() {
        assert_eq!(command_for(&ServiceControl::Stop), Some(Command::Stop));
        assert_eq!(command_for(&ServiceControl::Shutdown), Some(Command::Stop));
        assert_eq!(
            command_for(&ServiceControl::PowerEvent(PowerEventParam::Suspend)),
            Some(Command::Suspend)
        );
        for p in [
            PowerEventParam::ResumeAutomatic,
            PowerEventParam::ResumeSuspend,
            PowerEventParam::ResumeCritical,
        ] {
            assert_eq!(
                command_for(&ServiceControl::PowerEvent(p)),
                Some(Command::Resume)
            );
        }
        assert_eq!(
            command_for(&ServiceControl::PowerEvent(
                PowerEventParam::PowerStatusChange
            )),
            None
        );
        assert_eq!(command_for(&ServiceControl::Interrogate), None);
        assert_eq!(command_for(&ServiceControl::Pause), None);
    }

    #[test]
    fn reported_states() {
        let run = status(ServiceState::Running, Duration::ZERO, 0);
        assert!(run.controls_accepted.contains(
            ServiceControlAccept::STOP
                | ServiceControlAccept::SHUTDOWN
                | ServiceControlAccept::POWER_EVENT
        ));
        assert_eq!((run.checkpoint, run.wait_hint), (0, Duration::ZERO));
        let stopping = status(ServiceState::StopPending, STOP_WAIT_HINT, 0);
        assert!(stopping.controls_accepted.is_empty());
        assert_eq!(stopping.checkpoint, 1);
        assert!(stopping.wait_hint >= Duration::from_secs(30));
        assert_eq!(state_name(ServiceState::Running), "running");
    }

    #[test]
    fn status_query_needs_no_privilege() {
        // Whatever is registered on this machine, the query answers.
        let line = status_line();
        assert!(line.starts_with("service: "), "{line}");
    }

    #[test]
    fn the_service_entry_refuses_a_console() {
        // Not started by the SCM: the dispatcher fails instead of hanging.
        let e = dispatch().unwrap_err();
        assert!(e.contains("crossc-service run"), "{e}");
    }
}
