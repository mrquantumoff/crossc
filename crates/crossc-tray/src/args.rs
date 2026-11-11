//! The command line.
//!
//! ```text
//! crossc-tray                       the tray icon (one per user session)
//! crossc-tray --print-once          connect, print one status as text, exit (0: got one)
//! crossc-tray --demo                the tray icon fed by a script of made-up states
//! crossc-tray --flyout              only the details window (what the icon starts on a click)
//!     --anchor L,T,R,B              the notification icon's rectangle, physical pixels
//!     --pin                         do not close on focus loss or Esc (for screenshots)
//!     --demo [--demo-state NAME]    made-up states; NAME holds one of them
//!     --demo-epoch MS               the demo started at this Unix time (ms): the icon's
//!                                   process passes it so both show the same step
//! crossc-tray --dump-icons FILE     write every icon state to a .bmp, to look at them
//! --pipe NAME                       another pipe than \\.\pipe\crossc-status (tests)
//! ```

use crate::view::Rect;

/// What to run as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Role {
    Tray,
    Flyout,
    PrintOnce,
    DumpIcons(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Args {
    pub role: Role,
    /// Made-up states instead of the service's.
    pub demo: bool,
    /// Hold this demo state instead of running the script.
    pub demo_state: Option<String>,
    /// When the demo started, Unix ms.
    pub demo_epoch: Option<u64>,
    pub anchor: Option<Rect>,
    pub pin: bool,
    pub pipe: Option<String>,
}

fn parse_rect(s: &str) -> Option<Rect> {
    let mut it = s.split(',').map(|p| p.trim().parse::<i32>());
    let rect = Rect {
        left: it.next()?.ok()?,
        top: it.next()?.ok()?,
        right: it.next()?.ok()?,
        bottom: it.next()?.ok()?,
    };
    (it.next().is_none() && rect.right >= rect.left && rect.bottom >= rect.top).then_some(rect)
}

/// Parse the arguments (without the program's name). Unknown ones and
/// values that make no sense are ignored: a tray app has nobody to
/// complain to.
pub fn parse(args: &[String]) -> Args {
    let mut out = Args {
        role: Role::Tray,
        demo: false,
        demo_state: None,
        demo_epoch: None,
        anchor: None,
        pin: false,
        pipe: None,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--flyout" => out.role = Role::Flyout,
            "--print-once" => out.role = Role::PrintOnce,
            "--dump-icons" => {
                if let Some(path) = it.next() {
                    out.role = Role::DumpIcons(path.clone());
                }
            }
            "--demo" => out.demo = true,
            "--demo-state" => {
                out.demo = true;
                out.demo_state = it.next().cloned();
            }
            "--demo-epoch" => out.demo_epoch = it.next().and_then(|v| v.parse().ok()),
            "--anchor" => out.anchor = it.next().and_then(|v| parse_rect(v)),
            "--pin" => out.pin = true,
            "--pipe" => {
                out.pipe = it.next().filter(|p| p.starts_with(r"\\.\pipe\")).cloned();
            }
            _ => {}
        }
    }
    out
}

/// The arguments the icon's process starts its flyout with.
pub fn flyout_args(tray: &Args, anchor: Option<Rect>, demo_epoch: u64) -> Vec<String> {
    let mut v = vec!["--flyout".to_string()];
    if let Some(r) = anchor {
        v.push("--anchor".into());
        v.push(format!("{},{},{},{}", r.left, r.top, r.right, r.bottom));
    }
    if tray.demo {
        v.push("--demo".into());
        v.push("--demo-epoch".into());
        v.push(demo_epoch.to_string());
        if let Some(s) = &tray.demo_state {
            v.push("--demo-state".into());
            v.push(s.clone());
        }
    }
    if let Some(p) = &tray.pipe {
        v.push("--pipe".into());
        v.push(p.clone());
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Args {
        parse(&s.split_whitespace().map(String::from).collect::<Vec<_>>())
    }

    #[test]
    fn roles_and_flags() {
        let a = args("");
        assert_eq!(a.role, Role::Tray);
        assert!(!a.demo && !a.pin && a.anchor.is_none() && a.pipe.is_none());
        assert_eq!(args("--print-once").role, Role::PrintOnce);
        assert_eq!(
            args("--dump-icons x.bmp").role,
            Role::DumpIcons("x.bmp".into())
        );
        assert_eq!(args("--dump-icons").role, Role::Tray);
        let a = args("--flyout --anchor 1800,1040,1824,1080 --pin --demo-state uhq");
        assert_eq!(a.role, Role::Flyout);
        assert!(a.pin && a.demo);
        assert_eq!(a.demo_state.as_deref(), Some("uhq"));
        assert_eq!(
            a.anchor,
            Some(Rect {
                left: 1800,
                top: 1040,
                right: 1824,
                bottom: 1080
            })
        );
        assert_eq!(args("--demo --demo-epoch 1234").demo_epoch, Some(1234));
    }

    #[test]
    fn nonsense_is_ignored() {
        for s in [
            "--anchor",
            "--anchor 1,2,3",
            "--anchor 1,2,3,4,5",
            "--anchor a,b,c,d",
            "--anchor 10,10,5,5",
            "--anchor 99999999999,0,0,0",
        ] {
            assert_eq!(args(&format!("--flyout {s}")).anchor, None, "{s}");
        }
        assert_eq!(args("--demo-epoch soon").demo_epoch, None);
        assert_eq!(args("--what --is this").role, Role::Tray);
        // Only a local pipe is taken as a pipe.
        assert_eq!(args(r"--pipe \\server\pipe\x").pipe, None);
        assert_eq!(args(r"--pipe C:\file").pipe, None);
        assert_eq!(
            args(r"--pipe \\.\pipe\crossc-test").pipe.as_deref(),
            Some(r"\\.\pipe\crossc-test")
        );
    }

    #[test]
    fn the_flyout_is_started_with_what_the_tray_runs_on() {
        let tray = args(r"--demo-state two --pipe \\.\pipe\t");
        let anchor = Rect {
            left: -1920,
            top: 0,
            right: -1896,
            bottom: 40,
        };
        let v = flyout_args(&tray, Some(anchor), 77);
        let back = parse(&v);
        assert_eq!(back.role, Role::Flyout);
        assert_eq!(back.anchor, Some(anchor));
        assert_eq!(back.demo_epoch, Some(77));
        assert_eq!(back.demo_state.as_deref(), Some("two"));
        assert_eq!(back.pipe.as_deref(), Some(r"\\.\pipe\t"));
        assert!(!back.pin);
        assert_eq!(flyout_args(&args(""), None, 0), ["--flyout"]);
    }
}
