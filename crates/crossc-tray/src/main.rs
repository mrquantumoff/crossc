//! crossc-tray: an icon in the notification area that shows what the
//! crossc service is sending to the Galaxy Buds, and a flyout with the
//! details.
//!
//! It runs as the logged-on user, not elevated. Everything it shows comes
//! from the service over the status pipe (`crossc-status`); it never
//! touches the driver, the service or HKEY_LOCAL_MACHINE.
//!
//! * `view`: state in, text out (which icon, tooltip, cards, clipboard
//!   text, where the flyout goes). Pure.
//! * `icon`: the icon's pixels, drawn in code.
//! * `tray` (Windows): the icon, its menu, starting the flyout.
//! * `flyout` (Windows): the details window (GPUI), a process of its own
//!   that lives only while it is open.
//! * `source` (Windows): the connection to the service.
//! * `autostart`: the "Start with Windows" value under HKCU.
//! * `demo`: made-up states for looking at all of it without the service.
//! * `args`: the command line (see there for the hidden switches).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// The pure modules are complete on every platform; without the windows that use them, parts are unused there.
#![cfg_attr(not(windows), allow(dead_code))]

mod args;
mod autostart;
mod demo;
#[cfg(windows)]
mod flyout;
mod icon;
#[cfg(windows)]
mod source;
#[cfg(windows)]
mod tray;
mod view;

/// In the release build (GUI subsystem) stdout goes nowhere; reuse the console of the shell that started us.
#[cfg(windows)]
fn attach_parent_console() {
    #[cfg(not(debug_assertions))]
    // SAFETY: plain call.
    unsafe {
        use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

/// `--print-once`: one status as text. 0: there was one.
#[cfg(windows)]
fn print_once(args: &args::Args) -> i32 {
    use crossc_status::pipe::read_once;
    use std::time::Duration;
    attach_parent_console();
    let feed = if args.demo {
        let name = args.demo_state.as_deref().unwrap_or("uhq");
        demo::state(name, 0).unwrap_or(view::Feed::Connecting)
    } else {
        let trust = source::trust(args.pipe.as_deref());
        let pipe = args.pipe.as_deref().unwrap_or(crossc_status::PIPE_NAME);
        match read_once(pipe, Duration::from_secs(5), trust) {
            Ok(s) => view::Feed::Live(Box::new(s)),
            Err(p) => match source::outage(&p) {
                Some(o) => view::Feed::Down(o),
                None => view::Feed::Connecting,
            },
        }
    };
    print!("{}", view::report(&feed));
    i32::from(!matches!(feed, view::Feed::Live(_)))
}

/// `--dump-icons FILE`: every icon state at three sizes, on a dark and a
/// light strip, as a .bmp.
fn dump_icons(path: &str) -> i32 {
    use view::{Glyph, Quality};
    let glyphs = [
        Glyph::Off,
        Glyph::Problem,
        Glyph::Waiting,
        Glyph::Ready(Quality::Ssc),
        Glyph::Ready(Quality::Hifi),
        Glyph::Ready(Quality::Uhq),
        Glyph::Streaming(Quality::Ssc),
        Glyph::Streaming(Quality::Hifi),
        Glyph::Streaming(Quality::Uhq),
    ];
    let strip = |dark: bool| {
        let pics: Vec<icon::Bitmap> = [16, 24, 32]
            .into_iter()
            .flat_map(|size| glyphs.iter().map(move |g| icon::render(*g, size, dark)))
            .collect();
        let bg = if dark {
            icon::Rgba(0x1c, 0x1c, 0x1c, 255)
        } else {
            icon::Rgba(0xee, 0xee, 0xee, 255)
        };
        icon::sheet(&pics, bg, 8)
    };
    let dark = std::fs::write(path, strip(true));
    let light = std::fs::write(format!("{path}.light.bmp"), strip(false));
    i32::from(dark.is_err() || light.is_err())
}

#[cfg(windows)]
fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = args::parse(&argv);
    let code = match args.role.clone() {
        args::Role::Tray => tray::run(args),
        args::Role::Flyout => flyout::run(args),
        args::Role::PrintOnce => print_once(&args),
        args::Role::DumpIcons(path) => dump_icons(&path),
    };
    std::process::exit(code);
}

#[cfg(not(windows))]
fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if let args::Role::DumpIcons(path) = args::parse(&argv).role {
        std::process::exit(dump_icons(&path));
    }
    eprintln!("crossc-tray runs on Windows only");
    std::process::exit(1);
}
