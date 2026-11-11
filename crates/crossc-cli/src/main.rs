//! crossc-cli - user-mode side of the crossc spike.
//!
//! ```text
//! crossc-cli info                      driver/device info
//! crossc-cli list                      list crossc device interfaces
//! crossc-cli probe [-v] [--secure]     AVDTP DISCOVER + GET_ALL_CAPABILITIES
//! crossc-cli media [--seid N]          probe, SET_CONFIGURATION (SBC) + OPEN,
//!                                      open the media channel, print its MTUs,
//!                                      CLOSE/ABORT (no START, no audio)
//! crossc-cli raw [CMD]...              hex experiments (one handle for all CMDs;
//!                                      no CMDs = read commands from stdin)
//! crossc-cli play [OPTS]               stream live-encoded SSC (tone, sweep or
//!                                      WAV) to the buds; see play::PLAY_USAGE.
//!                                      --dry-run: no device, encode as fast as
//!                                      possible and print the encoder speed
//! global: --device N | --path \\?\...  pick interface (default: first)
//!         --in-mtu N                   in_mtu requested for probe/media
//!                                      channels (default 1021; see SETUP 7)
//! ```
//! raw commands: `open <psm> [in_mtu] [secure]`, `send <ch> <hex...>`,
//! `recv <ch> [timeout_ms]`, `close <ch>`, `sleep <ms>`, `quit`.
//! `<ch>` is the slot number printed by `open` (1-based), not the driver id.

mod pcm;
mod play;
mod probe;
mod resample;
mod wav;

use std::process::ExitCode;
use std::time::Duration;

#[cfg(windows)]
use crossc_core::device;
use crossc_core::{channel, hex, media, pace};
use crossc_ioctl as ioc;

#[derive(Debug, Default, PartialEq, Eq)]
struct Global {
    device_index: usize,
    path: Option<String>,
    verbose: bool,
    secure: bool,
    /// in_mtu requested for the channels `probe` / `media` open (None = default).
    in_mtu: Option<u16>,
    /// Largest peer MTU the driver accepts in the peer's L2CAP config request (None = 0xFFFF).
    out_mtu_max: Option<u16>,
    /// `media`: SEP to configure (None = first free SBC sink).
    seid: Option<u8>,
}

/// Default in_mtu for probe/media: >= 1005 and unusual, so the reported MTUs
/// show which driver field tracks our request (unverified item 1).
const DEFAULT_PROBE_IN_MTU: u16 = 1021;

#[derive(Debug, PartialEq)]
enum Cmd {
    Info,
    List,
    Probe,
    Media,
    Raw(Vec<String>),
    Play(Box<play::PlayArgs>),
    Help,
}

fn parse_args(args: &[String]) -> Result<(Global, Cmd), String> {
    let mut g = Global::default();
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--device" => {
                g.device_index =
                    hex::parse_num(it.next().ok_or("--device needs a value")?)? as usize
            }
            "--path" => g.path = Some(it.next().ok_or("--path needs a value")?.clone()),
            "-v" | "--verbose" => g.verbose = true,
            "--secure" => g.secure = true,
            "--in-mtu" => {
                let v = hex::parse_num(it.next().ok_or("--in-mtu needs a value")?)?;
                g.in_mtu = Some(u16::try_from(v).map_err(|_| "--in-mtu out of range")?);
            }
            "--out-mtu-max" => {
                let v = hex::parse_num(it.next().ok_or("--out-mtu-max needs a value")?)?;
                g.out_mtu_max = Some(u16::try_from(v).map_err(|_| "--out-mtu-max out of range")?);
            }
            "--seid" => {
                let v = hex::parse_num(it.next().ok_or("--seid needs a value")?)?;
                g.seid = Some(
                    u8::try_from(v)
                        .ok()
                        .filter(|s| (0x01..=0x3E).contains(s))
                        .ok_or("--seid must be 0x01..0x3E")?,
                );
            }
            "-h" | "--help" => return Ok((g, Cmd::Help)),
            _ => rest.push(a.clone()),
        }
    }
    let cmd = match rest.first().map(String::as_str) {
        None | Some("help") => Cmd::Help,
        Some("info") => Cmd::Info,
        Some("list") => Cmd::List,
        Some("probe") => Cmd::Probe,
        Some("media") => Cmd::Media,
        Some("raw") => Cmd::Raw(rest[1..].to_vec()),
        Some("play") => Cmd::Play(Box::new(play::parse_play_args(&rest[1..])?)),
        Some(o) => return Err(format!("unknown command {o:?}")),
    };
    if matches!(cmd, Cmd::Play(_)) && g.seid.is_some() {
        return Err("--seid applies to media only; play picks the SSC SEP itself".into());
    }
    Ok((g, cmd))
}

/// One parsed `raw` command.
#[derive(Debug, PartialEq, Eq)]
enum RawCmd {
    Open { psm: u16, in_mtu: u16, secure: bool },
    Send { ch: usize, data: Vec<u8> },
    Recv { ch: usize, timeout_ms: u64 },
    Close { ch: usize },
    Sleep { ms: u64 },
    Quit,
}

fn parse_raw(line: &str) -> Result<Option<RawCmd>, String> {
    let line = line.split('#').next().unwrap_or("").trim();
    let mut w = line.split_whitespace();
    let Some(verb) = w.next() else {
        return Ok(None);
    };
    let num = |s: Option<&str>, what: &str| -> Result<u32, String> {
        hex::parse_num(s.ok_or(format!("missing {what}"))?)
    };
    let cmd = match verb {
        "open" => {
            let psm = num(w.next(), "psm")?;
            let psm = u16::try_from(psm).map_err(|_| "psm out of range")?;
            let mut in_mtu = 0u16;
            let mut secure = false;
            for t in w {
                if t == "secure" {
                    secure = true;
                } else {
                    in_mtu = u16::try_from(hex::parse_num(t)?).map_err(|_| "mtu out of range")?;
                }
            }
            RawCmd::Open {
                psm,
                in_mtu,
                secure,
            }
        }
        "send" => {
            let ch = num(w.next(), "channel")? as usize;
            let rest: Vec<&str> = w.collect();
            RawCmd::Send {
                ch,
                data: hex::parse(&rest.join(" "))?,
            }
        }
        "recv" => {
            let ch = num(w.next(), "channel")? as usize;
            let timeout_ms = match w.next() {
                Some(t) => u64::from(hex::parse_num(t)?),
                None => 5000,
            };
            RawCmd::Recv { ch, timeout_ms }
        }
        "close" => RawCmd::Close {
            ch: num(w.next(), "channel")? as usize,
        },
        "sleep" => RawCmd::Sleep {
            ms: u64::from(num(w.next(), "ms")?),
        },
        "quit" | "exit" => RawCmd::Quit,
        o => return Err(format!("unknown raw command {o:?}")),
    };
    Ok(Some(cmd))
}

fn open_flags(secure: bool) -> u32 {
    if secure {
        ioc::OPEN_FLAG_AUTHENTICATED | ioc::OPEN_FLAG_ENCRYPTED
    } else {
        0
    }
}

fn fmt_info(i: &ioc::InfoReply) -> String {
    let mut a = [0u8; 17];
    ioc::format_bd_addr(i.remote_addr, &mut a);
    format!(
        "protocol v{}  driver {}.{}.{}\nremote     {} ({})\nchannels   {}/{}\npower      {}",
        i.protocol_version,
        i.driver_version >> 24,
        (i.driver_version >> 16) & 0xFF,
        i.driver_version & 0xFFFF,
        std::str::from_utf8(&a).unwrap_or("?"),
        if i.flags & ioc::INFO_FLAG_ADDR_VALID == 0 {
            "UNKNOWN"
        } else if i.flags & ioc::INFO_FLAG_ADDR_FROM_DEVINFO != 0 {
            "from BTHENUM devinfo"
        } else {
            "parsed from instance id"
        },
        i.channels_open,
        i.channels_max,
        if i.flags & ioc::INFO_FLAG_D0 != 0 {
            "D0"
        } else {
            "not in D0"
        },
    )
}

const USAGE: &str = "usage: crossc-cli [--device N | --path PATH] [-v] [--secure] [--in-mtu N] [--out-mtu-max N] [--seid N (media only)] <info|list|probe|media|raw [CMD]...|play [OPTS]>
play OPTS: [--mode hifi|uhq|ssc48|ssc44] [--config 0xNN] [--bitrate KBPS] [--frames-per-packet N]
           [--ts packets|samples] [--seconds S] [--tone HZ [--level DBFS] | --sweep | --wav PATH]
           [--no-delay-report] [--lead N] [--force-mtu N] [--force-config] [--dry-run]
raw CMDs: \"open <psm> [in_mtu] [secure]\" \"send <ch> <hex>\" \"recv <ch> [ms]\" \"close <ch>\" \"sleep <ms>\" \"quit\"
example: crossc-cli raw \"open 0x19\" \"send 1 00 01\" \"recv 1\"   (AVDTP DISCOVER, label 0)
Run elevated (device DACL is SYSTEM+Administrators).";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (g, cmd) = match parse_args(&args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    if cmd == Cmd::Help {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if let Cmd::Play(a) = &cmd
        && a.dry_run
    {
        return match dry_run(a, g.verbose) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        };
    }
    match run(&g, cmd) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Print the play settings shared by the real and the dry run.
fn print_play_settings(a: &play::PlayArgs, source: &pcm::Source) {
    println!(
        "{}: config {:#04x}, {} kbps, {} frame(s)/packet, frame {} bytes, packet {} bytes, every {:.3} ms; RTP ts {:?}; input {} x{}ch",
        a.mode.name(),
        a.config,
        a.bitrate.bps() / 1000,
        a.fpp,
        a.frame_bytes(),
        a.packet_bytes(),
        a.packet_duration().as_secs_f64() * 1e3,
        a.ts,
        source.new_buf().name(),
        source.channels()
    );
    if let pcm::Source::Wav { wav, .. } = source {
        println!("WAV: {}", wav.describe());
    }
    for w in a.warnings() {
        println!("WARNING: {w}");
    }
}

/// `play --dry-run`: no device; encode and packetise as fast as possible.
fn dry_run(a: &play::PlayArgs, verbose: bool) -> Result<(), String> {
    let source = a.build_source()?;
    print_play_settings(a, &source);
    struct Null;
    impl channel::SduChannel for Null {
        fn send(&mut self, _: &[u8]) -> Result<(), channel::ChanError> {
            Ok(())
        }
        fn recv(&mut self, _: Duration) -> Result<Vec<u8>, channel::ChanError> {
            Err(channel::ChanError::Timeout)
        }
        fn out_mtu(&self) -> usize {
            usize::MAX
        }
    }
    let t0 = std::time::Instant::now();
    let out = play::stream_packets(
        a,
        source,
        &mut Null,
        &mut pace::VirtualClock::default(),
        &std::sync::atomic::AtomicBool::new(false),
        None,
        verbose,
        &mut |l| println!("{l}"),
    );
    let wall = t0.elapsed();
    let rate = a.mode.sample_rate();
    println!(
        "dry run: {} packets / {} frames ({:.1} s of audio) in {:.3} s wall",
        out.stats.packets,
        out.stats.frames,
        out.encode.frames as f64 * ssc_enc::FRAME_SAMPLES as f64 / f64::from(rate),
        wall.as_secs_f64()
    );
    println!(
        "encoder: {:.1}x realtime ({:.1} us/frame; real time allows {:.0} us/frame)",
        out.encode.realtime_factor(rate),
        out.encode.encode_time.as_secs_f64() * 1e6 / out.encode.frames.max(1) as f64,
        ssc_enc::FRAME_SAMPLES as f64 * 1e6 / f64::from(rate)
    );
    if out.reason.is_error() {
        return Err(play::describe_end(&out.reason, out.elapsed));
    }
    Ok(())
}

/// Set by the console Ctrl+C / Ctrl+Break handler.
#[cfg(windows)]
static STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Console control handler policy: the first Ctrl+C / Ctrl+Break
/// (`ctrl` 0 / 1) is handled (stop gracefully: play checks the flag between
/// setup steps and packets, then CLOSEs the stream); a second one, or any
/// other event, is passed on, so the default handler ends the process.
#[cfg_attr(not(windows), allow(dead_code))]
fn ctrl_c_handled(ctrl: u32, already_stopping: bool) -> bool {
    ctrl <= 1 && !already_stopping
}

#[cfg(windows)]
fn install_ctrl_c() {
    use windows::Win32::System::Console::SetConsoleCtrlHandler;
    unsafe extern "system" fn handler(ctrl: u32) -> windows::core::BOOL {
        let already = ctrl <= 1 && STOP.swap(true, std::sync::atomic::Ordering::Relaxed);
        windows::core::BOOL(i32::from(ctrl_c_handled(ctrl, already)))
    }
    if unsafe { SetConsoleCtrlHandler(Some(handler), true) }.is_err() {
        eprintln!(
            "warning: cannot install the Ctrl+C handler; Ctrl+C will kill the process without CLOSE"
        );
    }
}

#[cfg(not(windows))]
fn run(_g: &Global, _cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    Err("crossc-cli talks to a Windows driver; build it for Windows".into())
}

#[cfg(windows)]
fn run(g: &Global, cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    use channel::SduChannel;
    use device::Device;

    if cmd == Cmd::List {
        let l = device::list_interfaces()?;
        if l.is_empty() {
            println!("no crossc device interfaces present");
        }
        for (i, p) in l.iter().enumerate() {
            println!("#{i}: {p}");
        }
        return Ok(());
    }
    let dev = Device::find(g.device_index, g.path.as_deref())?;
    if g.verbose {
        eprintln!("device: {}", dev.path);
    }
    match cmd {
        Cmd::Info => println!("{}", fmt_info(&dev.info()?)),
        Cmd::Play(a) => play_cmd(g, &dev, &a)?,
        Cmd::Probe | Cmd::Media => {
            let info = dev.info()?;
            println!("{}\n", fmt_info(&info));
            let in_mtu = g.in_mtu.unwrap_or(DEFAULT_PROBE_IN_MTU);
            let open = |what: &str| -> Result<device::Channel, Box<dyn std::error::Error>> {
                println!("opening {what} (PSM 0x19, requested in_mtu={in_mtu})...");
                let t0 = std::time::Instant::now();
                let req = ioc::OpenChannelRequest {
                    psm: avdtp::PSM_AVDTP,
                    in_mtu,
                    out_mtu_max: g.out_mtu_max.unwrap_or(0),
                    flags: open_flags(g.secure),
                };
                let ch = dev.open_channel(req, Duration::from_secs(30))?;
                println!(
                    "{what} {:#x} open after {} ms: out_mtu={} in_mtu={}\n  MTU check: {}",
                    ch.id,
                    t0.elapsed().as_millis(),
                    ch.out_mtu,
                    ch.in_mtu,
                    media::mtu_mapping_hint(in_mtu, ch.out_mtu, ch.in_mtu)
                );
                Ok(ch)
            };
            let ch = open("signaling channel")?;
            let mut sig = probe::Signaling::new(ch);
            let res = probe::run(&mut sig);
            let media_res = match (&res, cmd == Cmd::Media) {
                (Ok(reports), true) => {
                    print!("{}", probe::render(reports));
                    Some(media_step(&mut sig, reports, g.seid, || {
                        open("media channel")
                    }))
                }
                _ => None,
            };
            if g.verbose || res.is_err() || media_res.as_ref().is_some_and(Result::is_err) {
                for t in &sig.trace {
                    eprintln!("{t}");
                }
            }
            let mut ch = sig.ch;
            let close = ch.close();
            match media_res {
                None => print!("{}", probe::render(&res?)),
                Some(r) => r?,
            }
            close?;
            println!("\nsignaling channel closed");
        }
        Cmd::Raw(cmds) => {
            let mut chans: Vec<Option<device::Channel>> = Vec::new();
            let lines: Box<dyn Iterator<Item = String>> = if cmds.is_empty() {
                eprintln!("raw mode: one command per line, 'quit' to exit");
                Box::new(std::io::stdin().lines().map_while(Result::ok))
            } else {
                Box::new(cmds.into_iter())
            };
            for line in lines {
                let c = match parse_raw(&line) {
                    Ok(Some(c)) => c,
                    Ok(None) => continue,
                    Err(e) => {
                        eprintln!("{e}");
                        continue;
                    }
                };
                let get = |chans: &mut Vec<Option<device::Channel>>,
                           ch: usize|
                 -> Result<usize, String> {
                    match ch
                        .checked_sub(1)
                        .filter(|&i| chans.get(i).is_some_and(Option::is_some))
                    {
                        Some(i) => Ok(i),
                        None => Err(format!("no open channel {ch}")),
                    }
                };
                let r: Result<(), Box<dyn std::error::Error>> = (|| {
                    match c {
                        RawCmd::Quit => return Err("quit".into()),
                        RawCmd::Sleep { ms } => std::thread::sleep(Duration::from_millis(ms)),
                        RawCmd::Open {
                            psm,
                            in_mtu,
                            secure,
                        } => {
                            let ch = dev.open_channel(
                                ioc::OpenChannelRequest {
                                    psm,
                                    in_mtu,
                                    out_mtu_max: g.out_mtu_max.unwrap_or(0),
                                    flags: open_flags(secure || g.secure),
                                },
                                Duration::from_secs(30),
                            )?;
                            println!(
                                "ch {} = driver id {:#x}, out_mtu={} in_mtu={}",
                                chans.len() + 1,
                                ch.id,
                                ch.out_mtu,
                                ch.in_mtu
                            );
                            chans.push(Some(ch));
                        }
                        RawCmd::Send { ch, data } => {
                            let i = get(&mut chans, ch)?;
                            chans[i].as_mut().ok_or("closed")?.send(&data)?;
                            println!("> {}", hex::format(&data));
                        }
                        RawCmd::Recv { ch, timeout_ms } => {
                            let i = get(&mut chans, ch)?;
                            let d = chans[i]
                                .as_mut()
                                .ok_or("closed")?
                                .recv(Duration::from_millis(timeout_ms))?;
                            println!("< {}", hex::format(&d));
                        }
                        RawCmd::Close { ch } => {
                            let i = get(&mut chans, ch)?;
                            if let Some(mut c) = chans[i].take() {
                                c.close()?;
                            }
                            println!("ch {ch} closed");
                        }
                    }
                    Ok(())
                })();
                match r {
                    Err(e) if e.to_string() == "quit" => break,
                    Err(e) => eprintln!("error: {e}"),
                    Ok(()) => {}
                }
            }
        }
        Cmd::List | Cmd::Help => unreachable!(),
    }
    Ok(())
}

/// `play` on a real device.
#[cfg(windows)]
fn play_cmd(
    g: &Global,
    dev: &device::Device,
    a: &play::PlayArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let source = a.build_source()?;
    print_play_settings(a, &source);
    let info = dev.info()?;
    println!("{}\n", fmt_info(&info));
    let in_mtu = g.in_mtu.unwrap_or(DEFAULT_PROBE_IN_MTU);
    let open = |what: &str| -> Result<device::Channel, String> {
        let t0 = std::time::Instant::now();
        let ch = dev
            .open_channel(
                ioc::OpenChannelRequest {
                    psm: avdtp::PSM_AVDTP,
                    in_mtu,
                    out_mtu_max: g.out_mtu_max.unwrap_or(0),
                    flags: open_flags(g.secure),
                },
                Duration::from_secs(30),
            )
            .map_err(|e| format!("opening the {what}: {e}"))?;
        println!(
            "{what} {:#x} open after {} ms: out_mtu={} in_mtu={} (requested in_mtu={in_mtu})",
            ch.id,
            t0.elapsed().as_millis(),
            ch.out_mtu,
            ch.in_mtu
        );
        Ok(ch)
    };
    let ch = open("signaling channel")?;
    install_ctrl_c();
    let mut sig = probe::Signaling::new(ch);
    let t0 = std::time::Instant::now();
    let res = play::play(
        &mut sig,
        a,
        source,
        in_mtu,
        || {
            open("media channel").map(|ch| play::OpenedMedia {
                out_mtu: ch.out_mtu,
                in_mtu: ch.in_mtu,
                ch,
            })
        },
        &mut pace::WallClock::new(),
        &STOP,
        g.verbose,
        &mut |l| println!("{l}"),
    );
    if g.verbose || res.is_err() {
        eprintln!("--- signaling trace ---");
        for t in &sig.trace {
            eprintln!("{t}");
        }
    }
    let mut ch = sig.ch;
    let close = ch.close();
    println!(
        "signaling channel closed ({:.1} s total)",
        t0.elapsed().as_secs_f64()
    );
    let out = res?;
    close?;
    if out.reason.is_error() {
        return Err(play::describe_end(&out.reason, out.elapsed).into());
    }
    Ok(())
}

/// `media` after a successful probe: SET_CONFIGURATION + OPEN, open the media
/// transport channel with `open_media`, report it, then CLOSE (or ABORT) and
/// close the media channel. Never STARTs.
#[cfg(windows)]
fn media_step<C: channel::SduChannel>(
    sig: &mut probe::Signaling<C>,
    reports: &[probe::SepReport],
    seid: Option<u8>,
    open_media: impl FnOnce() -> Result<device::Channel, Box<dyn std::error::Error>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let choice = media::choose(reports, seid)?;
    println!(
        "\nconfiguring SEP {:#04x} (INT SEID {:#04x}): {:?}",
        choice.acp_seid,
        media::INT_SEID,
        choice.config
    );
    media::configure_and_open(sig, &choice)?;
    println!("SET_CONFIGURATION + OPEN accepted; opening the media transport channel");
    let media_ch = open_media();
    let released = media::release(sig, choice.acp_seid);
    match &released {
        Ok(how) => println!("stream released with {how}"),
        Err(e) => eprintln!("releasing the stream failed: {e}"),
    }
    let mut media_ch = media_ch?;
    if media_ch.out_mtu < 1005 {
        println!(
            "NOTE: media out_mtu {} < 1005 (what SSC media packets want)",
            media_ch.out_mtu
        );
    }
    media_ch.close()?;
    println!("media channel closed");
    released.map(|_| ()).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn args() {
        assert_eq!(parse_args(&s(&["info"])).unwrap().1, Cmd::Info);
        let (g, c) = parse_args(&s(&["--device", "1", "-v", "probe", "--secure"])).unwrap();
        assert_eq!(c, Cmd::Probe);
        assert_eq!(g.device_index, 1);
        assert!(g.verbose && g.secure);
        assert_eq!(
            parse_args(&s(&["raw", "open 0x19", "recv 1"])).unwrap().1,
            Cmd::Raw(s(&["open 0x19", "recv 1"]))
        );
        assert_eq!(parse_args(&s(&[])).unwrap().1, Cmd::Help);
        assert!(parse_args(&s(&["bogus"])).is_err());
        assert!(parse_args(&s(&["--device"])).is_err());
        let (g, c) = parse_args(&s(&["--in-mtu", "1013", "--seid", "0x03", "media"])).unwrap();
        assert_eq!(c, Cmd::Media);
        assert_eq!((g.in_mtu, g.seid), (Some(1013), Some(3)));
        assert!(parse_args(&s(&["--in-mtu", "70000", "probe"])).is_err());
        assert!(parse_args(&s(&["--seid", "0", "media"])).is_err());
        assert!(parse_args(&s(&["--seid", "0x3F", "media"])).is_err());
        let (g, _) = parse_args(&s(&["--path", r"\\?\X", "info"])).unwrap();
        assert_eq!(g.path.as_deref(), Some(r"\\?\X"));
        // play picks the SSC SEP itself; --seid would be silently ignored.
        assert!(
            parse_args(&s(&["--seid", "3", "play"]))
                .unwrap_err()
                .contains("media only")
        );
        assert!(matches!(
            parse_args(&s(&["--in-mtu", "900", "play", "--mode", "uhq"]))
                .unwrap()
                .1,
            Cmd::Play(_)
        ));
    }

    #[test]
    fn ctrl_c_policy() {
        assert!(ctrl_c_handled(0, false), "first Ctrl+C: graceful stop");
        assert!(ctrl_c_handled(1, false), "first Ctrl+Break: graceful stop");
        assert!(
            !ctrl_c_handled(0, true),
            "second Ctrl+C: default handler exits"
        );
        assert!(!ctrl_c_handled(2, false), "CTRL_CLOSE_EVENT: not ours");
    }

    #[test]
    fn raw_commands() {
        assert_eq!(
            parse_raw("open 0x19").unwrap(),
            Some(RawCmd::Open {
                psm: 0x19,
                in_mtu: 0,
                secure: false
            })
        );
        assert_eq!(
            parse_raw("open 25 1005 secure").unwrap(),
            Some(RawCmd::Open {
                psm: 25,
                in_mtu: 1005,
                secure: true
            })
        );
        assert_eq!(
            parse_raw("send 1 00 01").unwrap(),
            Some(RawCmd::Send {
                ch: 1,
                data: vec![0, 1]
            })
        );
        assert_eq!(
            parse_raw("recv 2").unwrap(),
            Some(RawCmd::Recv {
                ch: 2,
                timeout_ms: 5000
            })
        );
        assert_eq!(
            parse_raw("recv 2 100").unwrap(),
            Some(RawCmd::Recv {
                ch: 2,
                timeout_ms: 100
            })
        );
        assert_eq!(parse_raw("close 1").unwrap(), Some(RawCmd::Close { ch: 1 }));
        assert_eq!(parse_raw("  # comment").unwrap(), None);
        assert_eq!(parse_raw("quit").unwrap(), Some(RawCmd::Quit));
        assert!(parse_raw("open").is_err());
        assert!(parse_raw("open 0x10000").is_err());
        assert!(parse_raw("send 1 0").is_err());
        assert!(parse_raw("frobnicate").is_err());
    }

    #[test]
    fn info_format() {
        let i = ioc::InfoReply {
            protocol_version: 1,
            driver_version: 1 << 16,
            remote_addr: 0xA0B1_C2D3_E4F5,
            channels_open: 1,
            channels_max: 4,
            flags: ioc::INFO_FLAG_D0 | ioc::INFO_FLAG_ADDR_VALID | ioc::INFO_FLAG_ADDR_FROM_DEVINFO,
        };
        let t = fmt_info(&i);
        assert!(
            t.contains("A0:B1:C2:D3:E4:F5 (from BTHENUM devinfo)"),
            "{t}"
        );
        assert!(t.contains("driver 0.1.0"), "{t}");
        assert!(t.contains("channels   1/4"));
        assert!(t.contains("D0"));
    }

    #[cfg(windows)]
    #[test]
    fn multi_sz() {
        let v: Vec<u16> = "a\0bc\0\0".encode_utf16().collect();
        assert_eq!(
            device::split_multi_sz(&v),
            vec!["a".to_string(), "bc".to_string()]
        );
        assert!(device::split_multi_sz(&[0, 0]).is_empty());
    }
}
