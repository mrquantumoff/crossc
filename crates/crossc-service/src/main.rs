//! crossc-service: turns what Windows plays into the crossc endpoint into an
//! SSC stream to the Galaxy Buds.
//!
//! ```text
//! crossc-service run [-v] [--for SECONDS]   foreground, logs to stdout (needs admin)
//! crossc-service install [--start]          register the auto-start service "crossc"
//! crossc-service uninstall                  stop and remove the service
//! crossc-service status                     service and per-device state
//! crossc-service service                    entry for the service control manager
//! ```
//!
//! One session ([`crossc_core::session`]) runs per crossc device interface;
//! the interfaces are rescanned every 5 s. As a service it logs to
//! `%ProgramData%\crossc\service.log`.

// The logger and the supervisor are portable (and tested everywhere), but
// only the Windows build has a driver to run them against.
#[cfg(windows)]
mod host;
#[cfg_attr(not(windows), allow(dead_code))]
mod logger;
#[cfg(windows)]
mod scm;
#[cfg_attr(not(windows), allow(dead_code))]
mod supervisor;

use std::process::ExitCode;

#[derive(Debug, PartialEq)]
#[cfg_attr(not(windows), allow(dead_code))]
enum Cmd {
    Run {
        verbose: bool,
        /// Stop by itself after this many seconds.
        seconds: Option<f64>,
    },
    Service,
    Install {
        start: bool,
    },
    Uninstall,
    Status,
    Help,
}

const USAGE: &str = "usage: crossc-service <command>
  run [-v] [--for SECONDS]  run in the foreground and log to stdout (Ctrl+C stops); needs an
                            elevated prompt. -v: debug log. --for: stop after SECONDS.
  install [--start]         register the auto-start LocalSystem service \"crossc\", or update it
                            if it exists; --start also starts it (elevated)
  uninstall                 stop and remove the service (elevated)
  status                    show whether the service is installed and running, and each
                            device's state
  service                   entry point used by the service control manager
Log of the service: %ProgramData%\\crossc\\service.log. Settings: HKLM\\SOFTWARE\\crossc
(UhqConfig, HifiBitrate, UhqBitrate, LogLevel).";

fn parse_args(args: &[String]) -> Result<Cmd, String> {
    let mut it = args.iter().map(String::as_str);
    let Some(verb) = it.next() else {
        return Ok(Cmd::Help);
    };
    let rest: Vec<&str> = it.collect();
    let no_args = |cmd: Cmd| -> Result<Cmd, String> {
        match rest.first() {
            None => Ok(cmd),
            Some(a) => Err(format!("{verb}: unexpected argument {a:?}")),
        }
    };
    match verb {
        "run" => {
            let mut verbose = false;
            let mut seconds = None;
            let mut i = 0;
            while i < rest.len() {
                match rest[i] {
                    "-v" | "--verbose" => verbose = true,
                    "--for" => {
                        i += 1;
                        let s = rest.get(i).ok_or("--for needs a number of seconds")?;
                        let v: f64 = s.parse().map_err(|_| format!("--for: bad number {s:?}"))?;
                        if !v.is_finite() || v <= 0.0 || v > 366.0 * 86400.0 {
                            return Err("--for must be a positive number of seconds".into());
                        }
                        seconds = Some(v);
                    }
                    a => return Err(format!("run: unexpected argument {a:?}")),
                }
                i += 1;
            }
            Ok(Cmd::Run { verbose, seconds })
        }
        "install" => match rest.as_slice() {
            [] => Ok(Cmd::Install { start: false }),
            ["--start"] => Ok(Cmd::Install { start: true }),
            [a, ..] => Err(format!("install: unexpected argument {a:?}")),
        },
        "service" => no_args(Cmd::Service),
        "uninstall" => no_args(Cmd::Uninstall),
        "status" => no_args(Cmd::Status),
        "help" | "-h" | "--help" => Ok(Cmd::Help),
        o => Err(format!("unknown command {o:?}")),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = match parse_args(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    if cmd == Cmd::Help {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    match execute(cmd) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn execute(_cmd: Cmd) -> Result<(), String> {
    Err("crossc-service talks to a Windows driver; build it for Windows".into())
}

#[cfg(windows)]
fn execute(cmd: Cmd) -> Result<(), String> {
    match cmd {
        Cmd::Run { verbose, seconds } => {
            win::run_foreground(verbose, seconds);
            Ok(())
        }
        Cmd::Service => scm::dispatch(),
        Cmd::Install { start } => scm::install(start).map(|report| println!("{report}")),
        Cmd::Uninstall => scm::uninstall().map(|report| println!("{report}")),
        Cmd::Status => {
            println!("{}", scm::status_line());
            for line in host::device_status() {
                println!("{line}");
            }
            let log_dir = logger::default_log_dir();
            println!("log: {}", log_dir.join(logger::LOG_FILE).display());
            if let Err(why) = logger::check_log_dir(&log_dir) {
                println!(
                    "     NOT WRITTEN: {why}. Remove it (as administrator); the service makes its own"
                );
            }
            Ok(())
        }
        Cmd::Help => Ok(()),
    }
}

#[cfg(windows)]
mod win {
    use std::sync::mpsc::{self, Sender};
    use std::sync::{Arc, OnceLock};
    use std::time::Duration;

    use crossc_core::log::{Level, Log};
    use crossc_core::settings::Settings;
    use windows::Win32::System::Console::SetConsoleCtrlHandler;

    use crate::host::RealHost;
    use crate::logger::StdoutLog;
    use crate::supervisor::{Command, Supervisor};

    /// Where the console control handler sends the stop.
    static STOP_TX: OnceLock<Sender<Command>> = OnceLock::new();
    static STOPPING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    /// Console control policy: the first Ctrl+C / Ctrl+Break (0 / 1) stops
    /// gracefully; a second one, or any other event, goes to the default
    /// handler, which ends the process.
    pub fn ctrl_handled(ctrl: u32, already_stopping: bool) -> bool {
        ctrl <= 1 && !already_stopping
    }

    unsafe extern "system" fn on_ctrl(ctrl: u32) -> windows::core::BOOL {
        let already = ctrl <= 1 && STOPPING.swap(true, std::sync::atomic::Ordering::SeqCst);
        let handled = ctrl_handled(ctrl, already);
        if handled && let Some(tx) = STOP_TX.get() {
            let _ = tx.send(Command::Stop);
        }
        windows::core::BOOL(i32::from(handled))
    }

    /// `run`: the service's work in this console, until Ctrl+C.
    pub fn run_foreground(verbose: bool, seconds: Option<f64>) {
        let (settings, warnings) = Settings::load();
        let level = if verbose {
            Level::Debug
        } else {
            settings.log_level
        };
        let log: Arc<dyn Log> = Arc::new(StdoutLog::new(level));
        log.log(
            Level::Info,
            &format!(
                "crossc-service {} in the foreground (Ctrl+C stops); settings: {}",
                env!("CARGO_PKG_VERSION"),
                settings.describe()
            ),
        );
        for w in warnings {
            log.log(Level::Warn, &w);
        }
        let (tx, rx) = mpsc::channel();
        let _ = STOP_TX.set(tx.clone());
        if unsafe { SetConsoleCtrlHandler(Some(on_ctrl), true) }.is_err() {
            log.log(
                Level::Warn,
                "cannot install the Ctrl+C handler; Ctrl+C will end the process without a clean stop",
            );
        }
        if let Some(s) = seconds {
            let tx = tx.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs_f64(s));
                let _ = tx.send(Command::Stop);
            });
        }
        let mut supervisor = Supervisor::new(RealHost::new(settings, log.clone()), log.clone());
        supervisor.run(&rx);
        log.log(Level::Info, "stopping");
        supervisor.stop_all();
        log.log(Level::Info, "stopped");
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn ctrl_c_policy() {
            assert!(ctrl_handled(0, false), "first Ctrl+C: graceful stop");
            assert!(ctrl_handled(1, false), "first Ctrl+Break: graceful stop");
            assert!(!ctrl_handled(0, true), "second Ctrl+C: default handler");
            assert!(!ctrl_handled(2, false), "CTRL_CLOSE_EVENT: not ours");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(v: &[&str]) -> Result<Cmd, String> {
        parse_args(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn commands() {
        assert_eq!(p(&[]), Ok(Cmd::Help));
        assert_eq!(p(&["--help"]), Ok(Cmd::Help));
        assert_eq!(
            p(&["run"]),
            Ok(Cmd::Run {
                verbose: false,
                seconds: None
            })
        );
        assert_eq!(
            p(&["run", "-v", "--for", "2.5"]),
            Ok(Cmd::Run {
                verbose: true,
                seconds: Some(2.5)
            })
        );
        assert_eq!(p(&["service"]), Ok(Cmd::Service));
        assert_eq!(p(&["install"]), Ok(Cmd::Install { start: false }));
        assert_eq!(p(&["install", "--start"]), Ok(Cmd::Install { start: true }));
        assert_eq!(p(&["uninstall"]), Ok(Cmd::Uninstall));
        assert_eq!(p(&["status"]), Ok(Cmd::Status));
    }

    #[test]
    fn bad_arguments() {
        assert!(p(&["start"]).unwrap_err().contains("unknown command"));
        assert!(p(&["run", "--fast"]).unwrap_err().contains("unexpected"));
        assert!(p(&["run", "--for"]).unwrap_err().contains("needs"));
        assert!(
            p(&["run", "--for", "x"])
                .unwrap_err()
                .contains("bad number")
        );
        assert!(p(&["run", "--for", "0"]).unwrap_err().contains("positive"));
        assert!(p(&["install", "--now"]).unwrap_err().contains("unexpected"));
        assert!(p(&["status", "all"]).unwrap_err().contains("unexpected"));
        assert!(p(&["service", "x"]).unwrap_err().contains("unexpected"));
        assert!(p(&["uninstall", "x"]).unwrap_err().contains("unexpected"));
    }

    #[test]
    fn usage_names_every_command_and_setting() {
        for w in [
            "run",
            "install",
            "uninstall",
            "status",
            "service",
            "service.log",
            "UhqConfig",
            "HifiBitrate",
            "UhqBitrate",
            "LogLevel",
        ] {
            assert!(USAGE.contains(w), "{w}");
        }
    }
}
