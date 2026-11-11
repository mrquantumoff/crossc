//! crossc-installer: a GPUI window that installs and uninstalls the crossc_bth driver on every paired pair of
//! supported Galaxy Buds, together with the crossc service, with live status and log.
//!
//! Three layouts (see `layout`): a release bundle (the exe next to `driver\`, `crossc-service.exe` and
//! `crossc-cli.exe`: install / uninstall only), the repo (also builds; replaces tools\install-driver.ps1), and
//! the installer on its own ("online"). In all three it can download the latest release from GitHub and
//! install that (`release`, `update`).
//!
//! Hidden command-line modes (for checking the logic without the window):
//!   --status          print the status checklist (with the latest release on GitHub) and next step, then exit
//!   --check-release   open the window and check for the latest release at once (what "Restart as
//!                     administrator" passes on when the check had been done already)
//!   --build           run the Build action headless, streaming its log to stdout (repo only)
//!   --autostart-build open the window and start Build right away (to watch the log stream; repo only)
//!   --release         select the release flavour (default: debug), for the window and both modes above (repo only)
//!   --show-warning    show the first-install warning even when crossc is installed already (to look at it)
//!   --show-confirm    show the confirmation step of "Download and install" for a made-up release (to look at
//!                     it; confirming it installs nothing, there is no such release)

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
mod cert;
mod http;
mod layout;
mod model;
mod parse;
mod release;
mod service;
mod shortcut;
mod system;
mod tray;
mod ui;
mod update;

use gpui::{
    App, AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size,
};

use layout::{Layout, Mode};
use model::{Flavor, Status};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let flavor = if has("--release") {
        Flavor::Release
    } else {
        Flavor::Debug
    };
    let layout = Layout::locate();

    if has("--status") {
        attach_parent_console();
        let mut status = system::query_status(&layout, flavor);
        // The one network request of this mode; it gives up after 10 s per step and reports why.
        status.latest = update::check(status.installed_version().as_ref());
        print!("{}", status_report(&layout, &status));
        return;
    }
    if has("--build") {
        attach_parent_console();
        let log = |l: actions::LogLine| println!("{}", l.text);
        let ok = actions::run(actions::Action::Build, &layout, flavor, &log).is_ok();
        std::process::exit(if ok { 0 } else { 1 });
    }

    let autostart =
        (has("--autostart-build") && layout.mode == Mode::Repo).then_some(actions::Action::Build);
    let force_warning = has("--show-warning");
    let check_release = has("--check-release");
    let show_confirm = has("--show-confirm");
    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(960.), px(780.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("crossc installer".into()),
                    ..Default::default()
                }),
                window_min_size: Some(size(px(720.), px(540.))),
                ..Default::default()
            },
            |window, cx| {
                cx.new(|cx| {
                    let options = ui::Options {
                        autostart,
                        force_warning,
                        check_release,
                        show_confirm,
                    };
                    ui::InstallerView::new(layout, flavor, options, window, cx)
                })
            },
        )
        .expect("open window");
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}

/// In the release build (GUI subsystem) stdout goes nowhere; reuse the console of the shell that started us.
fn attach_parent_console() {
    #[cfg(not(debug_assertions))]
    unsafe {
        use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

fn status_report(layout: &Layout, s: &Status) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let opt = |b: Option<bool>| {
        b.map_or("unknown".to_string(), |b| {
            if b { "yes".into() } else { "no".into() }
        })
    };
    let _ = match (layout.mode, s.flavor()) {
        (Mode::Repo, Some(flavor)) => writeln!(
            out,
            "mode              : repo {} (flavor {flavor})",
            layout.root.display()
        ),
        (Mode::Online, _) => writeln!(
            out,
            "mode              : {} (the installer on its own in {})",
            model::online_label(s.bundle_version.as_ref(), s.package().version()),
            layout.root.display()
        ),
        _ => writeln!(
            out,
            "mode              : {} in {}",
            model::bundle_label(s.package().version()),
            layout.root.display()
        ),
    };
    let _ = writeln!(
        out,
        "admin             : {}",
        if s.admin { "yes" } else { "no" }
    );
    let _ = writeln!(out, "test signing      : {}", opt(s.test_signing));
    let _ = writeln!(out, "secure boot       : {}", opt(s.secure_boot));
    let _ = writeln!(out, "hvci running      : {}", opt(s.hvci_running));
    let _ = writeln!(out, "hvci configured   : {}", opt(s.hvci_configured));
    for p in &s.packages {
        let state = if p.complete() {
            "complete".to_string()
        } else {
            format!("missing {}", p.missing.join(", "))
        };
        let version = p
            .version()
            .map_or(String::new(), |v| format!(", version {v}"));
        // A bundle's file times say when it was unpacked, not when it was built.
        let built = match (&p.built_at, &p.built_age, p.kind) {
            (Some(at), Some(age), model::PackageKind::Built(_)) => {
                format!(", built {at} ({age})")
            }
            _ => String::new(),
        };
        let _ = writeln!(
            out,
            "package {:<9} : {state}{version}{built}  [{}]",
            p.kind,
            p.dir.display()
        );
    }
    if s.hardware_ids.is_empty() {
        let _ = writeln!(out, "hardware id       : -");
    }
    for id in &s.hardware_ids {
        let _ = writeln!(out, "hardware id       : {id}");
    }
    match &s.nodes {
        Ok(nodes) if nodes.is_empty() => {
            let _ = writeln!(out, "node              : none");
        }
        Ok(nodes) => {
            for n in nodes {
                let _ = writeln!(out, "node              : {}", n.instance_id);
                let _ = writeln!(
                    out,
                    "  buds            : {}",
                    n.device_name.as_deref().unwrap_or("-")
                );
                let _ = writeln!(
                    out,
                    "  present         : {}",
                    if n.present { "yes" } else { "no" }
                );
                let _ = writeln!(
                    out,
                    "  name            : {}",
                    n.name.as_deref().unwrap_or("-")
                );
                let _ = writeln!(
                    out,
                    "  service         : {}",
                    n.service.as_deref().unwrap_or("-")
                );
                let _ = writeln!(
                    out,
                    "  problem         : {}",
                    n.problem.map_or("-".into(), |p| p.to_string())
                );
                let _ = writeln!(
                    out,
                    "  inf             : {}",
                    n.inf.as_deref().unwrap_or("-")
                );
                let _ = writeln!(
                    out,
                    "  version         : {}",
                    n.version.as_deref().unwrap_or("-")
                );
                let _ = writeln!(out, "  stack           : {}", n.stack_text());
            }
        }
        Err(e) => {
            let _ = writeln!(out, "node              : error: {e}");
        }
    }
    // On crossc_bth without being a model of this package (see the checklist row).
    for n in &s.unlisted {
        let _ = writeln!(
            out,
            "unlisted node     : {}  [{}, {}, inf {}]",
            n.instance_id,
            n.label(),
            n.service.as_deref().unwrap_or("-"),
            n.inf.as_deref().unwrap_or("-")
        );
    }
    match &s.cert {
        Ok(c) => {
            let _ = writeln!(
                out,
                "cert              : {}  root={} trustedpublisher={}",
                c.thumbprint,
                if c.in_root { "yes" } else { "no" },
                if c.in_trusted_publisher { "yes" } else { "no" }
            );
        }
        Err(e) => {
            let _ = writeln!(out, "cert              : {e}");
        }
    }
    match &s.staged {
        Ok(p) if p.is_empty() => {
            let _ = writeln!(out, "staged            : none");
        }
        Ok(p) => {
            for pkg in p {
                let _ = writeln!(
                    out,
                    "staged            : {} {}",
                    pkg.published,
                    pkg.version.as_deref().unwrap_or("")
                );
            }
        }
        Err(e) => {
            let _ = writeln!(out, "staged            : error: {e}");
        }
    }
    match &s.service {
        Ok(info) => {
            let _ = writeln!(
                out,
                "service           : {}  [{}]",
                info.state,
                info.exe.as_deref().unwrap_or("-")
            );
        }
        Err(e) => {
            let _ = writeln!(out, "service           : error: {e}");
        }
    }
    let _ = writeln!(
        out,
        "service exe       : {}",
        s.service_exe
            .as_ref()
            .map_or("missing or damaged".to_string(), |p| p
                .display()
                .to_string())
    );
    let _ = writeln!(
        out,
        "tray              : {}, {}, autostart {}",
        if s.tray.installed {
            "installed"
        } else {
            "not installed"
        },
        if s.tray.running {
            "running"
        } else {
            "not running"
        },
        s.tray
            .autostart
            .map_or("unknown", |on| if on { "on" } else { "off" })
    );
    let _ = writeln!(
        out,
        "tray shortcut     : {}",
        if s.tray.shortcut {
            "Start menu, crossc\\crossc tray"
        } else {
            "none"
        }
    );
    let _ = writeln!(
        out,
        "tray exe          : {}",
        s.tray_exe
            .as_ref()
            .map_or("missing".to_string(), |p| p.display().to_string())
    );
    let latest = model::latest_item(s).detail;
    let _ = match s.latest.source_text() {
        Some(source) => writeln!(out, "latest release    : {latest}  [{source}]"),
        None => writeln!(out, "latest release    : {latest}"),
    };
    let _ = writeln!(
        out,
        "installed version : {}",
        s.installed_version()
            .map_or("unknown".to_string(), |v| v.tag())
    );
    let _ = writeln!(
        out,
        "install record    : certificate {}, version {}, driver {}, pending certificates {}",
        s.record.cert_thumbprint.as_deref().unwrap_or("-"),
        s.record.version.as_deref().unwrap_or("-"),
        s.record.driver_ver.as_deref().unwrap_or("-"),
        s.record.pending_certs.as_deref().unwrap_or("-")
    );
    let _ = writeln!(
        out,
        "download offer    : {}",
        model::download_offer(s).map_or("none".to_string(), model::download_label)
    );
    let _ = writeln!(
        out,
        "actions           : {}",
        actions::actions_for(layout.mode)
            .iter()
            .map(|a| a.label())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let _ = writeln!(
        out,
        "install warning   : {}",
        if model::show_install_warning(s) {
            "shown"
        } else {
            "not shown"
        }
    );
    let _ = writeln!(out, "\nchecklist:");
    for item in model::checklist(s) {
        let _ = writeln!(
            out,
            "  [{:<7}] {:<20} {}",
            format!("{:?}", item.level),
            item.label,
            item.detail
        );
        if let Some(sub) = &item.sub {
            let _ = writeln!(out, "  {:<9} {:<20} {sub}", "", "");
        }
    }
    let g = model::next_step(s);
    let _ = writeln!(out, "\nnext step [{:?}]: {}", g.level, g.text);
    out
}

#[cfg(test)]
mod tests {
    use crate::layout::{Layout, Mode};
    use crate::model::tests::{ready, ready_bundle};

    #[test]
    fn status_report_mentions_every_section() {
        let layout = Layout {
            mode: Mode::Repo,
            root: r"C:\x".into(),
        };
        let text = super::status_report(&layout, &ready());
        for needle in [
            r"mode              : repo C:\x (flavor debug)",
            "admin",
            "test signing",
            "hardware id       : BTHENUM",
            "node              : BTHENUM",
            "  buds            : Buds4 Pro",
            "cert",
            "staged",
            "service           : not installed  [-]",
            "tray              : not installed, not running, autostart unknown",
            "tray shortcut     : none",
            r"tray exe          : C:\x\target\release\crossc-tray.exe",
            "] Tray app             not installed (Install adds and starts it)",
            "latest release    : unknown: \"Check for update\" asks GitHub for the newest release",
            "installed version : unknown",
            "install record    : certificate -, version -, driver -, pending certificates -",
            "download offer    : none",
            "actions           : Build, Install, Build + Install, Uninstall, Remove certificate",
            "install warning   : not shown",
            "checklist:",
            "stack ksthunk -> BthA2dp -> BthEnum",
            "next step [Good]: Ready",
        ] {
            assert!(text.contains(needle), "{needle} missing in\n{text}");
        }
    }

    #[test]
    fn status_report_in_bundle_mode() {
        let layout = Layout {
            mode: Mode::Bundle,
            root: r"C:\crossc-0.1".into(),
        };
        let mut status = ready_bundle();
        status.nodes = Ok(vec![]);
        let text = super::status_report(&layout, &status);
        for needle in [
            r"mode              : bundle 8.50.53.934 in C:\crossc-0.1",
            "package bundle    : complete, version 8.50.53.934",
            "node              : none",
            "actions           : Install, Uninstall, Remove certificate",
            "install warning   : shown",
            r"service exe       : C:\crossc-0.1\crossc-service.exe",
            "tray shortcut     : none",
        ] {
            assert!(text.contains(needle), "{needle} missing in\n{text}");
        }
        assert!(!text.contains("Build"), "{text}");

        // An installed tray with its Start menu entry, in both places the status shows it.
        let mut with_tray = status.clone();
        with_tray.tray = crate::model::TrayInfo {
            installed: true,
            running: true,
            autostart: Some(true),
            shortcut: true,
        };
        let text = super::status_report(&layout, &with_tray);
        for needle in [
            "tray              : installed, running, autostart on",
            r"tray shortcut     : Start menu, crossc\crossc tray",
            "in the Start menu as \"crossc tray\"",
        ] {
            assert!(text.contains(needle), "{needle} missing in\n{text}");
        }
        assert!(!text.contains("unlisted node"), "{text}");

        // Buds on crossc_bth that this bundle does not list are named, and so is a service exe that is unusable.
        status.unlisted = status_with_bound_node();
        status.service_exe = None;
        let text = super::status_report(&layout, &status);
        for needle in [
            "unlisted node     : BTHENUM",
            "[Buds4 Pro, crossc_bth, inf oem1.inf]",
            "its model is not in this package",
            "service exe       : missing or damaged",
            "next step [Bad]: crossc-service.exe is missing from the bundle or damaged",
        ] {
            assert!(text.contains(needle), "{needle} missing in\n{text}");
        }
    }

    #[test]
    fn status_report_in_online_mode_and_with_a_release() {
        use crate::model::Latest;
        use crate::release::{LatestRelease, Version};
        let layout = Layout {
            mode: Mode::Online,
            root: r"C:\Users\x\Downloads".into(),
        };
        // The installer on its own: no package, nothing installed, no check yet.
        let mut status = ready_bundle();
        status.online = true;
        status.packages[0].missing = crate::model::REQUIRED_FILES.to_vec();
        status.packages[0].driver_ver = None;
        status.packages[0].dir = r"C:\Users\x\Downloads\driver".into();
        status.hardware_ids = vec![];
        status.nodes = Ok(vec![]);
        status.service_exe = None;
        status.cert = Err("no WDRLocalTestCert.cer in the driver package".into());
        let text = super::status_report(&layout, &status);
        for needle in [
            r"mode              : online (the installer on its own in C:\Users\x\Downloads)",
            "latest release    : unknown",
            "download offer    : none",
            "actions           : Install, Uninstall, Remove certificate",
            "next step [Warn]: This installer runs on its own, without a driver package next to it: click \
             \"Check for update\"",
        ] {
            assert!(text.contains(needle), "{needle} missing in\n{text}");
        }
        assert!(!text.contains("incomplete"), "{text}");
        assert!(!text.contains("unpack the whole release zip"), "{text}");

        // The check found a release: it is named with where the answer came from, and offered.
        let version = Version::parse("0.2.0").unwrap();
        let asset = |name: &str| crate::release::Asset {
            name: name.into(),
            api_url: "https://api.github.com/repos/mrquantumoff/crossc/releases/assets/1".into(),
            size: 1,
            sha256: None,
        };
        status.latest = Latest::Found {
            release: Box::new(LatestRelease {
                tag: version.tag(),
                assets: Ok(crate::release::ReleaseAssets {
                    zip: asset("crossc-0.2.0-x64.zip"),
                    sums: asset("SHA256SUMS.txt"),
                }),
                version,
            }),
            repo: "mrquantumoff/crossc".into(),
            auth: "token from gh auth token".into(),
            skipped: Vec::new(),
        };
        let text = super::status_report(&layout, &status);
        for needle in [
            "latest release    : v0.2.0 is available (crossc is not installed)  [mrquantumoff/crossc, token from \
             gh auth token]",
            "download offer    : Download and install v0.2.0",
            "next step [Good]: Ready: click \"Download and install v0.2.0\"",
        ] {
            assert!(text.contains(needle), "{needle} missing in\n{text}");
        }

        // No release yet, and a failed check: said plainly, nothing offered.
        status.latest = Latest::NoReleases {
            repo: "mrquantumoff/crossc".into(),
            auth: "anonymous".into(),
            skipped: Vec::new(),
        };
        let text = super::status_report(&layout, &status);
        assert!(
            text.contains(
                "latest release    : mrquantumoff/crossc has no published release yet  [mrquantumoff/crossc, \
                 anonymous]"
            ),
            "{text}"
        );
        assert!(text.contains("download offer    : none"), "{text}");
        status.latest = Latest::Failed("could not connect (anonymous)".into());
        let text = super::status_report(&layout, &status);
        assert!(
            text.contains("latest release    : check failed: could not connect (anonymous)\n"),
            "{text}"
        );
    }

    /// The buds of `ready()`, bound to crossc_bth.
    fn status_with_bound_node() -> Vec<crate::model::NodeInfo> {
        let mut nodes = ready().nodes.unwrap();
        nodes[0].service = Some(crate::model::DRIVER_SERVICE.into());
        nodes
    }
}
