//! The window. All decisions (checklist levels, next step, action steps) live in `model` and `actions`; this file
//! only renders them and moves work off the UI thread.

use std::ops::Range;
use std::sync::mpsc;
use std::time::Duration;

use gpui::{
    App, ClipboardItem, Context, Div, FontWeight, Hsla, IntoElement, Pixels, Render, ScrollHandle,
    ScrollStrategy, SharedString, Stateful, Task, UniformListScrollHandle, Window, div, prelude::*,
    px, rgb, uniform_list,
};

use crate::actions::{self, Action, LogLine};
use crate::layout::{Layout, Mode};
use crate::model::{self, FetchedRelease, Flavor, Latest, Level, Status};
use crate::parse::LineKind;
use crate::release::LatestRelease;
use crate::system;
use crate::update;

const MAX_LOG_LINES: usize = 5000;
const MONO: &str = "Consolas";
/// The least the first-install warning shrinks to: its heading and the start of the text stay readable.
const WARNING_MIN_HEIGHT: f32 = 64.;
/// Flex shrink factor of the status panel, next to 1 for the warning: in a short window the status gives up its
/// height first, down to its minimum, and only what is still missing then comes out of the warning. (Factors
/// below 1 would not do: flexbox hands out only that fraction of the missing space.)
const STATUS_SHRINK: f32 = 1000.;

mod color {
    pub const BG: u32 = 0x16181d;
    pub const PANEL: u32 = 0x1e2128;
    pub const BORDER: u32 = 0x2c313a;
    pub const TEXT: u32 = 0xd7dae0;
    pub const MUTED: u32 = 0x8b929e;
    pub const GOOD: u32 = 0x4cc38a;
    pub const WARN: u32 = 0xe5b04a;
    pub const WARN_BG: u32 = 0x2b2414;
    pub const BAD: u32 = 0xe5534b;
    pub const UNKNOWN: u32 = 0x6b7280;
    pub const STEP: u32 = 0x5fb3f0;
    pub const BUTTON: u32 = 0x2d3440;
    pub const BUTTON_HOVER: u32 = 0x3a4352;
    pub const ACCENT: u32 = 0x2f6fd0;
    pub const ACCENT_HOVER: u32 = 0x3b7fe6;
}

fn level_color(level: Level) -> u32 {
    match level {
        Level::Good => color::GOOD,
        Level::Warn => color::WARN,
        Level::Bad => color::BAD,
        Level::Unknown => color::UNKNOWN,
    }
}

fn line_color(kind: LineKind) -> u32 {
    match kind {
        LineKind::Output => color::TEXT,
        LineKind::Info => color::MUTED,
        LineKind::Step => color::STEP,
        LineKind::Success => color::GOOD,
        LineKind::Warning => color::WARN,
        LineKind::Error => color::BAD,
    }
}

enum WorkerMsg {
    Line(LogLine),
    /// The job ended. With a release: one that now waits for the confirmation click (a finished download, or an
    /// install of it that failed and can be tried again).
    Done(Option<FetchedRelease>),
}

/// What the worker thread runs.
enum Job {
    /// One of the plain action buttons.
    Plain(Action),
    /// "Download and install", first click: download, verify, unpack.
    Download(LatestRelease),
    /// Second click, on the confirmation: install what was downloaded.
    InstallRelease(FetchedRelease),
}

impl Job {
    fn action(&self) -> Action {
        match self {
            Job::Plain(action) => *action,
            Job::Download(_) => Action::Download,
            Job::InstallRelease(_) => Action::InstallRelease,
        }
    }
}

/// The hidden command-line switches that reach the window.
pub struct Options {
    /// Run this action as soon as the window opens (`--autostart-build`, for testing).
    pub autostart: Option<Action>,
    /// Show the first-install warning whatever the status says (`--show-warning`).
    pub force_warning: bool,
    /// Check for the latest release once the status is read (`--check-release`).
    pub check_release: bool,
    /// Show the confirmation step for a made-up release (`--show-confirm`).
    pub show_confirm: bool,
}

pub struct InstallerView {
    layout: Layout,
    /// Selected build flavour (repo mode; a bundle has one package).
    flavor: Flavor,
    /// Show the first-install warning whatever the status says (hidden `--show-warning` flag).
    force_warning: bool,
    /// What the last release check found. Kept here and copied into every fresh `Status`, because a refresh
    /// does not repeat the check.
    latest: Latest,
    /// Start a release check when the next refresh has delivered a status.
    check_pending: bool,
    latest_task: Option<Task<()>>,
    /// A downloaded release that waits for the confirmation click.
    pending: Option<FetchedRelease>,
    status: Option<Status>,
    refreshing: bool,
    running: Option<Action>,
    log: Vec<LogLine>,
    log_scroll: UniformListScrollHandle,
    status_scroll: ScrollHandle,
    warning_scroll: ScrollHandle,
    action_task: Option<Task<()>>,
    refresh_task: Option<Task<()>>,
    /// The UAC prompt for "Restart as administrator" is up (ShellExecute runs on its own thread).
    restarting: bool,
    restart_task: Option<Task<()>>,
    /// A close was refused once while an action ran; the next close goes through.
    close_warned: bool,
}

impl InstallerView {
    pub fn new(
        layout: Layout,
        flavor: Flavor,
        options: Options,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let this = cx.weak_entity();
        window.on_window_should_close(cx, move |_, cx| {
            this.update(cx, |this, cx| this.should_close(cx))
                .unwrap_or(true)
        });
        let mut view = InstallerView {
            layout,
            flavor,
            force_warning: options.force_warning,
            latest: Latest::NotChecked,
            check_pending: options.check_release,
            latest_task: None,
            pending: options.show_confirm.then(model::example_fetched),
            status: None,
            refreshing: false,
            running: None,
            log: Vec::new(),
            log_scroll: UniformListScrollHandle::new(),
            status_scroll: ScrollHandle::new(),
            warning_scroll: ScrollHandle::new(),
            action_task: None,
            refresh_task: None,
            restarting: false,
            restart_task: None,
            close_warned: false,
        };
        view.push(
            LineKind::Info,
            match view.layout.mode {
                Mode::Repo => format!("repo: {}", view.layout.root.display()),
                Mode::Bundle => format!("bundle: {}", view.layout.root.display()),
                Mode::Online => format!(
                    "online: no driver package next to this installer in {}; releases come from GitHub",
                    view.layout.root.display()
                ),
            },
        );
        view.refresh(cx);
        if let Some(action) = options.autostart {
            view.start(Job::Plain(action), cx);
        }
        view
    }

    fn push(&mut self, kind: LineKind, text: impl Into<String>) {
        self.push_lines([LogLine {
            kind,
            text: text.into(),
        }]);
    }

    /// Appends lines; follows the output only if the view was already at the bottom, so reading earlier output while
    /// a build streams is not interrupted.
    fn push_lines(&mut self, lines: impl IntoIterator<Item = LogLine>) {
        let follow = self.log_metrics().is_none_or(ScrollMetrics::at_bottom);
        self.log.extend(lines);
        if self.log.len() > MAX_LOG_LINES {
            let excess = self.log.len() - MAX_LOG_LINES;
            self.log.drain(..excess);
        }
        if follow {
            self.scroll_log_to_end();
        }
    }

    fn scroll_log_to_end(&self) {
        if let Some(last) = self.log.len().checked_sub(1) {
            self.log_scroll.scroll_to_item(last, ScrollStrategy::Bottom);
        }
    }

    /// The log viewport as of the last layout; `None` before the first one.
    fn log_metrics(&self) -> Option<ScrollMetrics> {
        ScrollMetrics::of(&self.log_scroll.0.borrow().base_handle)
    }

    /// Window close request: refuse the first one while an action runs (closing would kill it part way, e.g. between
    /// removing the old package and adding the new one); a second close goes through, so a hung child cannot trap
    /// the user.
    fn should_close(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(action) = self.running else {
            return true;
        };
        if self.close_warned {
            return true;
        }
        self.close_warned = true;
        self.push(
            LineKind::Warning,
            format!(
                "{} is still running; closing now would stop it part way. Close again to quit anyway.",
                action.label()
            ),
        );
        cx.notify();
        false
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.refreshing = true;
        let layout = self.layout.clone();
        let flavor = self.flavor;
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let status = cx
                .background_executor()
                .spawn(async move { system::query_status(&layout, flavor) })
                .await;
            let _ = this.update(cx, |this, cx| {
                // A flavour switch while querying makes this result stale; the newer refresh replaces it.
                if status.flavor().is_none_or(|f| f == this.flavor) {
                    this.status = Some(Status {
                        latest: this.latest.clone(),
                        ..status
                    });
                    this.refreshing = false;
                    if std::mem::take(&mut this.check_pending) {
                        this.check_latest(cx);
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn set_latest(&mut self, latest: Latest) {
        if let Some(status) = &mut self.status {
            status.latest = latest.clone();
        }
        self.latest = latest;
    }

    /// Asks GitHub for the newest release, off the UI thread. Reads only; works unelevated.
    fn check_latest(&mut self, cx: &mut Context<Self>) {
        if self.latest == Latest::Checking {
            return;
        }
        self.set_latest(Latest::Checking);
        let installed = self.status.as_ref().and_then(Status::installed_version);
        self.latest_task = Some(cx.spawn(async move |this, cx| {
            let latest = cx
                .background_executor()
                .spawn(async move { update::check(installed.as_ref()) })
                .await;
            let _ = this.update(cx, |this, cx| {
                let (kind, text) = match &latest {
                    Latest::Found {
                        release,
                        repo,
                        auth,
                        ..
                    } => (
                        LineKind::Info,
                        format!("latest release of {repo}: {} ({auth})", release.tag),
                    ),
                    Latest::NoReleases { repo, auth, .. } => (
                        LineKind::Info,
                        format!("{repo} has no published release yet ({auth})"),
                    ),
                    Latest::Failed(why) => {
                        (LineKind::Warning, format!("release check failed: {why}"))
                    }
                    Latest::NotChecked | Latest::Checking => (LineKind::Info, String::new()),
                };
                if !text.is_empty() {
                    this.push(kind, text);
                }
                if let Some(note) = latest.skipped_note() {
                    this.push(LineKind::Warning, note);
                }
                this.set_latest(latest);
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// The confirmation was declined: nothing is installed, nothing is trusted, the unpacked files go.
    fn cancel_pending(&mut self, cx: &mut Context<Self>) {
        if let Some(fetched) = self.pending.take() {
            self.push(
                LineKind::Info,
                format!(
                    "Cancelled: {} was not installed and its certificate was not trusted",
                    fetched.tag
                ),
            );
            std::thread::spawn(move || actions::discard_release(&fetched));
            cx.notify();
        }
    }

    fn start(&mut self, job: Job, cx: &mut Context<Self>) {
        if self.running.is_some() {
            return;
        }
        let action = job.action();
        // Whatever starts now, a download that waited for its confirmation no longer does (the confirmation
        // itself carries it in the job).
        if let Some(stale) = self.pending.take()
            && !matches!(job, Job::InstallRelease(_))
        {
            std::thread::spawn(move || actions::discard_release(&stale));
        }
        self.running = Some(action);
        self.push(
            LineKind::Step,
            match (&job, self.layout.mode) {
                (Job::Download(release), _) => format!("=== Download {} ===", release.tag),
                (Job::InstallRelease(fetched), _) => format!("=== Install {} ===", fetched.tag),
                (Job::Plain(_), Mode::Repo) => {
                    format!("=== {} ({}) ===", action.label(), self.flavor)
                }
                (Job::Plain(_), _) => format!("=== {} ===", action.label()),
            },
        );
        let (tx, rx) = mpsc::channel::<WorkerMsg>();
        let layout = self.layout.clone();
        let flavor = self.flavor;
        std::thread::spawn(move || {
            let log = |l: LogLine| {
                let _ = tx.send(WorkerMsg::Line(l));
            };
            // The outcome is already in the log (the actions report it).
            let waiting = match job {
                Job::Plain(action) => {
                    let _ = actions::run(action, &layout, flavor, &log);
                    None
                }
                Job::Download(release) => actions::fetch_release(&release, &log).ok(),
                // A failed install leaves the confirmation up: the cause can be fixed and the click repeated
                // without downloading again.
                Job::InstallRelease(fetched) => actions::install_release(&fetched, flavor, &log)
                    .is_err()
                    .then_some(fetched),
            };
            let _ = tx.send(WorkerMsg::Done(waiting));
        });
        // Drain the worker's channel on the UI thread every 50 ms until it reports Done (or disconnects).
        self.action_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
                let mut finished = false;
                let mut waiting = None;
                let mut batch = Vec::new();
                loop {
                    match rx.try_recv() {
                        Ok(WorkerMsg::Line(l)) => batch.push(l),
                        Ok(WorkerMsg::Done(release)) => {
                            waiting = release;
                            finished = true;
                            break;
                        }
                        Err(mpsc::TryRecvError::Disconnected) => {
                            finished = true;
                            break;
                        }
                        Err(mpsc::TryRecvError::Empty) => break,
                    }
                }
                let alive = this
                    .update(cx, |this, cx| {
                        if !batch.is_empty() {
                            this.push_lines(batch);
                        }
                        if finished {
                            this.pending = waiting;
                            this.running = None;
                            this.close_warned = false;
                            this.refresh(cx);
                        }
                        cx.notify();
                    })
                    .is_ok();
                if finished || !alive {
                    break;
                }
            }
        }));
        cx.notify();
    }

    /// ShellExecute("runas") blocks until the UAC prompt is answered, so it runs on its own thread and the window keeps
    /// painting meanwhile.
    fn restart_as_admin(&mut self, cx: &mut Context<Self>) {
        if self.restarting || self.running.is_some() {
            return;
        }
        self.restarting = true;
        self.push(
            LineKind::Info,
            "Waiting for the UAC prompt (this window closes once the elevated one starts)",
        );
        let (tx, rx) = mpsc::channel();
        let layout = self.layout.clone();
        // The elevated window opens on the same flavour and, if it was forced here, with the warning.
        let mut args = Vec::new();
        if self.layout.mode == Mode::Repo && self.flavor == Flavor::Release {
            args.push("--release");
        }
        if self.force_warning {
            args.push("--show-warning");
        }
        // A check that was done here is done again there, so the release is on offer right away.
        if self.latest != Latest::NotChecked {
            args.push("--check-release");
        }
        let args = args.join(" ");
        std::thread::spawn(move || {
            let _ = tx.send(actions::restart_as_admin(&layout, &args));
        });
        self.restart_task = Some(cx.spawn(async move |this, cx| {
            let result = loop {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                match rx.try_recv() {
                    Ok(r) => break r,
                    Err(mpsc::TryRecvError::Empty) => continue,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        break Err("the elevation thread stopped".to_string());
                    }
                }
            };
            let _ = this.update(cx, |this, cx| match result {
                Ok(()) => cx.quit(),
                Err(e) => {
                    this.restarting = false;
                    this.push(LineKind::Error, format!("ERROR: {e}"));
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    fn admin(&self) -> bool {
        self.status.as_ref().is_some_and(|s| s.admin)
    }

    // -----------------------------------------------------------------------------------------------------------
    // Rendering
    // -----------------------------------------------------------------------------------------------------------

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.running.is_some() || self.restarting;
        let flavor_button = |f: Flavor, cx: &mut Context<Self>| {
            let selected = self.flavor == f;
            let mut b = div()
                .id(SharedString::from(format!("flavor-{f}")))
                .px_3()
                .py_1()
                .text_sm()
                .rounded_md()
                .child(match f {
                    Flavor::Debug => "Debug",
                    Flavor::Release => "Release",
                });
            if selected {
                b = b.bg(rgb(color::ACCENT)).text_color(rgb(0xffffff));
            } else {
                b = b.text_color(rgb(color::MUTED));
                if !busy {
                    b = b
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(color::BUTTON_HOVER)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.flavor = f;
                            this.refresh(cx);
                        }));
                }
            }
            b
        };
        div()
            .flex()
            .items_center()
            .gap_4()
            .px_4()
            .py_3()
            .border_b_1()
            .border_color(rgb(color::BORDER))
            .child(
                // Takes the leftover width and wraps its subtitle, so the controls on the right are never pushed out.
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child("crossc installer"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(color::MUTED))
                            .child(match self.layout.mode {
                                Mode::Repo => {
                                    "crossc_bth driver and crossc service for Galaxy Buds: build, install, verify, roll back"
                                }
                                Mode::Bundle => {
                                    "crossc_bth driver and crossc service for Galaxy Buds: install, verify, roll back"
                                }
                                Mode::Online => {
                                    "crossc_bth driver and crossc service for Galaxy Buds: download, install, verify, roll back"
                                }
                            }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap_3()
                    .when(!self.admin() && self.status.is_some(), |d| {
                        d.child(
                            {
                                let tip = if self.restarting {
                                    Some("Answer the UAC prompt")
                                } else {
                                    busy.then_some("Wait for the running action to finish")
                                };
                                let label = if self.restarting {
                                    "Waiting for UAC..."
                                } else {
                                    "Restart as administrator"
                                };
                                let b =
                                    button("restart-admin", label, !busy, true, tip.map(Into::into));
                                if busy {
                                    b
                                } else {
                                    b.on_click(cx.listener(|this, _, _, cx| this.restart_as_admin(cx)))
                                }
                            },
                        )
                    })
                    .child(match self.layout.mode {
                        // The Debug / Release choice is about what to build and install from the repo.
                        Mode::Repo => div()
                            .flex()
                            .gap_1()
                            .p_1()
                            .rounded_lg()
                            .bg(rgb(color::PANEL))
                            .border_1()
                            .border_color(rgb(color::BORDER))
                            .child(flavor_button(Flavor::Debug, cx))
                            .child(flavor_button(Flavor::Release, cx)),
                        // A bundle has one package: say which. Online: the release that is installed, if any.
                        Mode::Bundle | Mode::Online => div()
                            .px_3()
                            .py_1()
                            .rounded_lg()
                            .bg(rgb(color::PANEL))
                            .border_1()
                            .border_color(rgb(color::BORDER))
                            .text_sm()
                            .font_family(MONO)
                            .child({
                                let driver = self.status.as_ref().and_then(|s| s.package().version());
                                match self.layout.mode {
                                    Mode::Online => model::online_label(
                                        self.status.as_ref().and_then(|s| s.bundle_version.as_ref()),
                                        driver,
                                    ),
                                    _ => model::bundle_label(driver),
                                }
                            }),
                    }),
            )
    }

    fn render_status(&self) -> impl IntoElement {
        // Scrolls (and gives up height) when the window is short, so the log below always keeps its minimum height.
        let mut panel = div()
            .id("status")
            .track_scroll(&self.status_scroll)
            .flex()
            .flex_col()
            .flex_shrink()
            .min_h_0()
            .overflow_y_scroll()
            .gap_1()
            .p_3()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .text_xs()
                    .text_color(rgb(color::MUTED))
                    .child("STATUS")
                    .child(if self.refreshing { "refreshing..." } else { "" }),
            );
        match &self.status {
            None => {
                panel = panel.child(
                    div()
                        .text_sm()
                        .text_color(rgb(color::MUTED))
                        .child("Reading system state..."),
                )
            }
            Some(status) => {
                for item in model::checklist(status) {
                    let sub = item.sub;
                    panel = panel.child(
                        div()
                            .flex()
                            .items_start()
                            .gap_2()
                            .text_sm()
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .mt(px(5.))
                                    .size(px(10.))
                                    .rounded_full()
                                    .bg(rgb(level_color(item.level))),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .w(px(140.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(item.label),
                            )
                            .child(
                                // Wraps instead of truncating: the node instance ID is a detail people need to read in
                                // full (and gpui caches a no-wrap text's first measurement, so an ellipsis inside a
                                // flex row never appeared).
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_color(rgb(color::MUTED))
                                    .child(item.detail),
                            ),
                    );
                    // A node's driver stack, INF and version, under its row.
                    if let Some(sub) = sub {
                        panel = panel.child(
                            div()
                                .pl(px(166.))
                                .text_xs()
                                .font_family(MONO)
                                .text_color(rgb(color::MUTED))
                                .child(sub),
                        );
                    }
                }
            }
        }
        // Clipped, so a scroll thumb drawn from the previous layout (right after a resize) stays inside the panel.
        let mut frame = div()
            .relative()
            .flex()
            .flex_col()
            .min_h(px(72.))
            .overflow_hidden()
            .rounded_lg()
            .bg(rgb(color::PANEL))
            .border_1()
            .border_color(rgb(color::BORDER))
            .child(panel)
            .children(scroll_thumb(ScrollMetrics::of(&self.status_scroll)));
        frame.style().flex_shrink = Some(STATUS_SHRINK);
        frame
    }

    /// The warning a bundle shows above the buttons until crossc is installed (`model::show_install_warning`).
    /// Like the status it scrolls and gives up height in a short window: the log below is the only progress
    /// display during the first Install, so it must keep its minimum height.
    fn render_warning(&self) -> Option<impl IntoElement + use<>> {
        // The confirmation takes its place while a downloaded release waits.
        let wanted = self.confirming().is_none()
            && (self.force_warning
                || self
                    .status
                    .as_ref()
                    .is_some_and(model::show_install_warning));
        wanted.then(|| {
            let mut panel = div()
                .id("install-warning")
                .track_scroll(&self.warning_scroll)
                .flex()
                .flex_col()
                .flex_shrink()
                .min_h_0()
                .overflow_y_scroll()
                .gap_1()
                .p_3()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::BOLD)
                        .text_color(rgb(color::WARN))
                        .child(model::INSTALL_WARNING_TITLE),
                );
            for paragraph in model::INSTALL_WARNING {
                panel = panel.child(div().text_xs().child(paragraph));
            }
            div()
                .relative()
                .flex()
                .flex_col()
                .flex_shrink()
                .min_h(px(WARNING_MIN_HEIGHT))
                .overflow_hidden()
                .rounded_lg()
                .bg(rgb(color::WARN_BG))
                .border_1()
                .border_color(rgb(color::WARN))
                .child(panel)
                .children(scroll_thumb(ScrollMetrics::of(&self.warning_scroll)))
        })
    }

    /// The downloaded release that waits for its confirmation, unless something is running.
    fn confirming(&self) -> Option<&FetchedRelease> {
        self.pending.as_ref().filter(|_| self.running.is_none())
    }

    /// The confirmation step of "Download and install", shown in place of the warning once a release is
    /// downloaded: what was downloaded and which certificate the next click trusts. Not a dialog: the window
    /// stays as it is, and the two buttons that answer it are in the button row below, where they cannot scroll
    /// out of sight when this panel gives up height in a short window.
    fn render_confirm(&self) -> Option<impl IntoElement + use<>> {
        let fetched = self.confirming()?;
        let mut panel = div()
            .id("confirm-release")
            .track_scroll(&self.warning_scroll)
            .flex()
            .flex_col()
            .flex_shrink()
            .min_h_0()
            .overflow_y_scroll()
            .gap_1()
            .p_3()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::BOLD)
                    .text_color(rgb(color::WARN))
                    .child(model::confirm_title(fetched)),
            )
            .child(div().text_xs().child(model::CONFIRM_STATEMENT));
        for (label, value) in model::confirm_lines(fetched) {
            panel = panel.child(
                div()
                    .flex()
                    .items_start()
                    .gap_2()
                    .text_xs()
                    .child(
                        div()
                            .flex_shrink_0()
                            .w(px(96.))
                            .text_color(rgb(color::MUTED))
                            .child(label),
                    )
                    .child(div().flex_1().min_w_0().font_family(MONO).child(value)),
            );
        }
        Some(
            div()
                .relative()
                .flex()
                .flex_col()
                .flex_shrink()
                .min_h(px(WARNING_MIN_HEIGHT))
                .overflow_hidden()
                .rounded_lg()
                .bg(rgb(color::WARN_BG))
                .border_1()
                .border_color(rgb(color::WARN))
                .child(panel)
                .children(scroll_thumb(ScrollMetrics::of(&self.warning_scroll))),
        )
    }

    fn render_controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.running.is_some() || self.restarting;
        let admin = self.admin();
        let mut row = div()
            .flex()
            .flex_shrink_0()
            .flex_wrap()
            .items_center()
            .gap_2();
        let mode = self.layout.mode;
        const NEEDS_ADMIN: &str = "Needs administrator: use \"Restart as administrator\"";
        const BUSY: &str = "Another action is running";

        if let Some(fetched) = self.confirming() {
            // The second click. Nothing else is offered next to it: confirm, or cancel.
            let enabled = !busy && admin;
            let tip = (!admin).then_some(NEEDS_ADMIN).or(busy.then_some(BUSY));
            let mut confirm = button(
                "confirm-release",
                model::confirm_label(fetched),
                enabled,
                true,
                tip.map(Into::into),
            );
            if enabled {
                let fetched = fetched.clone();
                confirm = confirm.on_click(cx.listener(move |this, _, _, cx| {
                    this.start(Job::InstallRelease(fetched.clone()), cx)
                }));
            }
            let mut cancel = button("cancel-release", "Cancel", !busy, false, None);
            if !busy {
                cancel = cancel.on_click(cx.listener(|this, _, _, cx| this.cancel_pending(cx)));
            }
            row = row.child(confirm).child(cancel);
        } else {
            // "Download and install <tag>", when the release check found something worth installing. It needs
            // the same rights as Install (it unpacks into %ProgramFiles%), and the one Install prerequisite
            // that no download can fix.
            let offer = self
                .status
                .as_ref()
                .and_then(|s| model::download_offer(s).map(|release| (s, release.clone())));
            let offered = offer.is_some();
            if let Some((status, release)) = offer {
                let blocker = model::fetch_blocker(status);
                let tip: Option<SharedString> = if busy {
                    Some(BUSY.into())
                } else if !admin {
                    Some(
                        "Needs administrator (the release is unpacked into %ProgramFiles%\\crossc): use \
                         \"Restart as administrator\""
                            .into(),
                    )
                } else {
                    blocker.map(|why| format!("Cannot install yet: {why}").into())
                };
                let enabled = tip.is_none();
                let label = if self.running == Some(Action::Download) {
                    format!("Downloading {}...", release.tag)
                } else {
                    model::download_label(&release)
                };
                let mut b = button("download-release", label, enabled, true, tip);
                if enabled {
                    b = b.on_click(cx.listener(move |this, _, _, cx| {
                        this.start(Job::Download(release.clone()), cx)
                    }));
                }
                row = row.child(b);
            }
            for (i, action) in actions::actions_for(mode).iter().copied().enumerate() {
                let needs_admin_block = action.needs_admin() && !admin;
                let enabled = !busy && !needs_admin_block;
                let tip = if needs_admin_block {
                    Some(NEEDS_ADMIN)
                } else if busy {
                    Some(BUSY)
                } else {
                    None
                };
                // One highlighted button: the download when it is offered, else the mode's own.
                let primary = !offered && action == actions::primary_action(mode);
                let label = if self.running == Some(action) {
                    format!("{}...", action.label())
                } else {
                    action.label().into()
                };
                let mut b = button(
                    SharedString::from(format!("action-{i}")),
                    label,
                    enabled,
                    primary,
                    tip.map(Into::into),
                );
                if enabled {
                    b = b.on_click(
                        cx.listener(move |this, _, _, cx| this.start(Job::Plain(action), cx)),
                    );
                }
                row = row.child(b);
            }
        }

        // The release check reads only: no elevation needed, and it may run while nothing else does.
        let checking = self.latest == Latest::Checking;
        let can_check = !busy && !checking;
        let mut check = button(
            "check-release",
            if checking {
                "Checking..."
            } else {
                model::CHECK_LABEL
            },
            can_check,
            false,
            Some(
                "Asks api.github.com for the newest crossc release; nothing is downloaded yet"
                    .into(),
            ),
        );
        if can_check {
            check = check.on_click(cx.listener(|this, _, _, cx| this.check_latest(cx)));
        }
        let mut refresh = button("refresh", "Refresh", !busy, false, None);
        if !busy {
            refresh = refresh.on_click(cx.listener(|this, _, _, cx| this.refresh(cx)));
        }
        row.child(div().flex_1()).child(check).child(refresh)
    }

    fn render_log(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.log.len();
        let metrics = self.log_metrics();
        let scrolled_up = metrics.is_some_and(|m| !m.at_bottom());
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(150.))
            .rounded_lg()
            .bg(rgb(0x111317))
            .border_1()
            .border_color(rgb(color::BORDER))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_1()
                    .border_b_1()
                    .border_color(rgb(color::BORDER))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(color::MUTED))
                            .child(format!("LOG ({count} lines)")),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .when(scrolled_up, |d| {
                                d.child(small_button("log-end", "Jump to end").on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.scroll_log_to_end();
                                        cx.notify();
                                    }),
                                ))
                            })
                            .child(small_button("copy-log", "Copy log").on_click(cx.listener(
                                |this, _, _, cx| {
                                    let text: Vec<&str> =
                                        this.log.iter().map(|l| l.text.as_str()).collect();
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        text.join("\r\n"),
                                    ));
                                },
                            )))
                            .child(small_button("clear-log", "Clear").on_click(cx.listener(
                                |this, _, _, cx| {
                                    this.log.clear();
                                    cx.notify();
                                },
                            ))),
                    ),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        uniform_list(
                            "log",
                            count,
                            cx.processor(|this, range: Range<usize>, _window, _cx| {
                                range
                                    .map(|ix| {
                                        let l = &this.log[ix];
                                        div()
                                            .px_3()
                                            .whitespace_nowrap()
                                            .overflow_hidden()
                                            .text_ellipsis()
                                            .text_color(rgb(line_color(l.kind)))
                                            .child(l.text.clone())
                                    })
                                    .collect()
                            }),
                        )
                        .track_scroll(self.log_scroll.clone())
                        .size_full()
                        .py_1()
                        .font_family(MONO)
                        .text_xs(),
                    )
                    .children(scroll_thumb(metrics)),
            )
    }

    fn render_footer(&self) -> impl IntoElement {
        let (level, text) = match (&self.status, self.running) {
            (_, Some(a)) => (
                Level::Unknown,
                format!(
                    "{} is running; output streams into the log below",
                    a.label()
                ),
            ),
            (None, None) => (Level::Unknown, "Reading system state...".to_string()),
            (Some(s), None) => {
                let g = match self.confirming() {
                    Some(fetched) => model::confirm_step(fetched),
                    None => model::next_step(s),
                };
                (g.level, g.text)
            }
        };
        let c: Hsla = if self.running.is_some() {
            rgb(color::STEP).into()
        } else {
            rgb(level_color(level)).into()
        };
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_4()
            .py_2()
            .border_t_1()
            .border_color(rgb(color::BORDER))
            .bg(rgb(color::PANEL))
            .text_sm()
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(c)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Next step"),
            )
            .child(div().flex_1().min_w_0().child(text))
    }
}

impl Render for InstallerView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(color::BG))
            .text_color(rgb(color::TEXT))
            .font_family("Segoe UI")
            .child(self.render_header(cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .gap_3()
                    .p_4()
                    .child(self.render_status())
                    .children(self.render_confirm())
                    .children(self.render_warning())
                    .child(self.render_controls(cx))
                    .child(self.render_log(cx)),
            )
            .child(self.render_footer())
    }
}

/// gpui has no built-in scrollbar; this thumb shows where a scrolled view is and that there is more to see.
fn scroll_thumb(metrics: Option<ScrollMetrics>) -> Option<Div> {
    metrics.and_then(ScrollMetrics::thumb).map(|(top, height)| {
        div()
            .absolute()
            .right(px(2.))
            .top(top)
            .w(px(6.))
            .h(height)
            .rounded_full()
            .bg(rgb(color::BUTTON_HOVER))
    })
}

/// Scroll viewport geometry (logical px) from the last layout.
#[derive(Clone, Copy)]
struct ScrollMetrics {
    viewport: f32,
    max: f32,
    offset: f32,
}

impl ScrollMetrics {
    /// `None` before the first layout.
    fn of(h: &ScrollHandle) -> Option<Self> {
        let viewport = f32::from(h.bounds().size.height);
        (viewport > 0.).then(|| ScrollMetrics {
            viewport,
            max: f32::from(h.max_offset().height).max(0.),
            offset: -f32::from(h.offset().y),
        })
    }

    fn at_bottom(self) -> bool {
        self.offset >= self.max - 4.
    }

    /// Scroll thumb (top, height), or `None` when everything fits.
    fn thumb(self) -> Option<(Pixels, Pixels)> {
        if self.max < 1. {
            return None;
        }
        let h = (self.viewport * self.viewport / (self.viewport + self.max)).max(20.);
        let top = (self.viewport - h) * (self.offset / self.max).clamp(0., 1.);
        Some((px(top), px(h)))
    }
}

/// A tooltip bubble.
struct Tip(SharedString);

impl Render for Tip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(rgb(0x2a2f38))
            .border_1()
            .border_color(rgb(color::BORDER))
            .text_xs()
            .text_color(rgb(color::TEXT))
            .child(self.0.clone())
    }
}

fn button(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    enabled: bool,
    primary: bool,
    tooltip: Option<SharedString>,
) -> Stateful<Div> {
    let (bg, hover) = if primary && enabled {
        (color::ACCENT, color::ACCENT_HOVER)
    } else {
        (color::BUTTON, color::BUTTON_HOVER)
    };
    let mut b = div()
        .id(id.into())
        .px_3()
        .py(px(6.))
        .rounded_md()
        .text_sm()
        .border_1()
        .border_color(rgb(color::BORDER))
        .bg(rgb(bg))
        .child(label.into());
    if enabled {
        b = b.cursor_pointer().hover(move |s| s.bg(rgb(hover)));
    } else {
        b = b.opacity(0.4);
    }
    if let Some(tip) = tooltip {
        b = b.tooltip(move |_, cx: &mut App| cx.new(|_| Tip(tip.clone())).into());
    }
    b
}

fn small_button(id: &'static str, label: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .px_2()
        .rounded_sm()
        .text_xs()
        .text_color(rgb(color::MUTED))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(color::BUTTON_HOVER)).text_color(rgb(color::TEXT)))
        .child(label)
}

#[cfg(test)]
mod tests {
    use super::ScrollMetrics;
    use gpui::px;

    #[test]
    fn log_follows_only_at_bottom() {
        let m = |offset| ScrollMetrics {
            viewport: 100.,
            max: 300.,
            offset,
        };
        assert!(m(300.).at_bottom());
        assert!(m(297.).at_bottom());
        assert!(!m(250.).at_bottom());
        assert!(!m(0.).at_bottom());
        // Everything fits: always at the bottom, no thumb.
        let fits = ScrollMetrics {
            viewport: 100.,
            max: 0.,
            offset: 0.,
        };
        assert!(fits.at_bottom());
        assert!(fits.thumb().is_none());
    }

    #[test]
    fn thumb_geometry() {
        let m = |offset| ScrollMetrics {
            viewport: 100.,
            max: 300.,
            offset,
        };
        assert_eq!(m(0.).thumb(), Some((px(0.), px(25.))));
        assert_eq!(m(300.).thumb(), Some((px(75.), px(25.))));
        // Tiny thumbs keep a grabbable minimum.
        let long = ScrollMetrics {
            viewport: 100.,
            max: 100_000.,
            offset: 0.,
        };
        assert_eq!(long.thumb().unwrap().1, px(20.));
    }
}
