//! `crossc-cli uhq` (Linux only): switch Galaxy Buds between SSC HiFi and SSC
//! UHQ, by hand or automatically while audio above 48 kHz plays.
//!
//! The PipeWire plugin registers two codecs on the buds' SSC endpoint, `ssc`
//! (HiFi 48 kHz/24) and `ssc_uhq` (UHQ 96 kHz/24), which PipeWire shows as the
//! profiles `a2dp-sink-ssc` and `a2dp-sink-ssc_uhq`. Switching between them
//! renegotiates the stream without restarting PipeWire, and WirePlumber keeps
//! the last profile per device. The graph is read with `pw-dump` and profiles
//! are set with `wpctl`.

use std::collections::HashMap;
use std::io::BufReader;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::Value;

pub const UHQ_USAGE: &str = "uhq [status|on|off|auto|watch]   UHQ 96 kHz on Galaxy Buds: on, off (HiFi 48 kHz), or auto (UHQ while audio above 48 kHz plays)";

const PROFILE_HIFI: &str = "a2dp-sink-ssc";
const PROFILE_UHQ: &str = "a2dp-sink-ssc_uhq";
const UNIT: &str = "crossc-uhq-auto.service";

/// Audio above this rate asks for UHQ.
const HIFI_RATE: u64 = 48_000;
/// Hi-res audio must play this long before switching up (skips a short probe of a file).
const UP_AFTER: Duration = Duration::from_secs(1);
/// Back to HiFi after this long without hi-res audio (pauses, track gaps).
const DOWN_AFTER: Duration = Duration::from_secs(10);
/// Least time between two switches: each one renegotiates the stream.
const COOLDOWN: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum UhqCmd {
    Status,
    On,
    Off,
    Auto,
    Watch,
}

pub fn parse_uhq_args(args: &[String]) -> Result<UhqCmd, String> {
    match args {
        [] => Ok(UhqCmd::Status),
        [a] => match a.as_str() {
            "status" => Ok(UhqCmd::Status),
            "on" => Ok(UhqCmd::On),
            "off" => Ok(UhqCmd::Off),
            "auto" => Ok(UhqCmd::Auto),
            "watch" => Ok(UhqCmd::Watch),
            o => Err(format!(
                "uhq: unknown mode {o:?}; expected status, on, off, auto or watch"
            )),
        },
        _ => Err("uhq takes one argument: status, on, off, auto or watch".into()),
    }
}

/// A pair of buds whose device offers the SSC profiles.
#[derive(Debug, PartialEq, Eq)]
pub struct Buds {
    pub device: u64,
    pub name: String,
    pub address: Option<String>,
    pub profile: Option<String>,
    pub hifi_index: Option<u64>,
    pub uhq_index: Option<u64>,
    /// The SSC output node, while one of the SSC profiles is active.
    pub sink: Option<Sink>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Sink {
    pub node: u64,
    pub uhq: bool,
    pub rate: Option<u64>,
    /// A running stream above 48 kHz plays into this sink.
    pub hires_playing: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Graph {
    pub buds: Vec<Buds>,
    pub allowed_rates: Option<String>,
}

fn props(o: &Value) -> &Value {
    &o["info"]["props"]
}

fn id_of(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str()?.parse().ok())
}

fn first_param<'a>(o: &'a Value, name: &str) -> Option<&'a Value> {
    o["info"]["params"][name].as_array()?.first()
}

fn is(o: &Value, kind: &str) -> bool {
    o["type"].as_str().is_some_and(|t| t.ends_with(kind))
}

/// What matters here, out of a `pw-dump` object list.
pub fn graph(objects: &[Value]) -> Graph {
    let nodes: Vec<&Value> = objects.iter().filter(|o| is(o, ":Node")).collect();
    let links: Vec<(u64, u64)> = objects
        .iter()
        .filter(|o| is(o, ":Link"))
        .filter_map(|o| {
            Some((
                id_of(&o["info"]["output-node-id"])?,
                id_of(&o["info"]["input-node-id"])?,
            ))
        })
        .collect();
    let hires_running = |node: u64| {
        nodes.iter().any(|n| {
            id_of(&n["id"]) == Some(node)
                && props(n)["media.class"]
                    .as_str()
                    .is_some_and(|c| c.starts_with("Stream/Output/Audio"))
                && n["info"]["state"] == "running"
                && first_param(n, "Format")
                    .and_then(|f| f["rate"].as_u64())
                    .is_some_and(|r| r > HIFI_RATE)
        })
    };

    let buds = objects
        .iter()
        .filter(|o| is(o, ":Device") && props(o)["device.api"] == "bluez5")
        .filter_map(|d| {
            let device = id_of(&d["id"])?;
            let profiles = d["info"]["params"]["EnumProfile"].as_array();
            let index = |name: &str| {
                profiles?
                    .iter()
                    .find(|p| p["name"] == name)
                    .and_then(|p| p["index"].as_u64())
            };
            let (hifi_index, uhq_index) = (index(PROFILE_HIFI), index(PROFILE_UHQ));
            if hifi_index.is_none() && uhq_index.is_none() {
                return None;
            }
            let sink = nodes
                .iter()
                .find(|n| {
                    props(n)["media.class"] == "Audio/Sink"
                        && id_of(&props(n)["device.id"]) == Some(device)
                        && matches!(
                            props(n)["api.bluez5.codec"].as_str(),
                            Some("ssc" | "ssc_uhq")
                        )
                })
                .and_then(|n| {
                    let node = id_of(&n["id"])?;
                    Some(Sink {
                        node,
                        uhq: props(n)["api.bluez5.codec"] == "ssc_uhq",
                        rate: first_param(n, "Format").and_then(|f| f["rate"].as_u64()),
                        hires_playing: links
                            .iter()
                            .any(|&(out, inp)| inp == node && hires_running(out)),
                    })
                });
            Some(Buds {
                device,
                address: props(d)["api.bluez5.address"].as_str().map(str::to_string),
                name: props(d)["device.description"]
                    .as_str()
                    .or(props(d)["device.name"].as_str())
                    .unwrap_or("Bluetooth device")
                    .to_string(),
                profile: first_param(d, "Profile")
                    .and_then(|p| p["name"].as_str())
                    .map(str::to_string),
                hifi_index,
                uhq_index,
                sink,
            })
        })
        .collect();

    let allowed_rates = objects
        .iter()
        .find(|o| is(o, ":Core"))
        .and_then(|c| props(c)["default.clock.allowed-rates"].as_str())
        .map(str::to_string);
    Graph {
        buds,
        allowed_rates,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Hifi,
    Uhq,
}

/// When to switch in auto mode: up after hi-res audio has played for a moment,
/// down after a quiet spell, never two switches within the cooldown.
#[derive(Debug, Default)]
pub struct Auto {
    hires_since: Option<Instant>,
    quiet_since: Option<Instant>,
    last_switch: Option<Instant>,
}

impl Auto {
    pub fn decide(&mut self, now: Instant, on_uhq: bool, hires_playing: bool) -> Option<Target> {
        if hires_playing {
            self.quiet_since = None;
        } else {
            self.hires_since = None;
        }
        let since = if hires_playing {
            &mut self.hires_since
        } else {
            &mut self.quiet_since
        };
        let start = *since.get_or_insert(now);
        let cooled = self
            .last_switch
            .is_none_or(|t| now.duration_since(t) >= COOLDOWN);
        let target = match (hires_playing, on_uhq) {
            (true, false) if now.duration_since(start) >= UP_AFTER => Target::Uhq,
            (false, true) if now.duration_since(start) >= DOWN_AFTER => Target::Hifi,
            _ => return None,
        };
        if !cooled {
            return None;
        }
        self.last_switch = Some(now);
        self.hires_since = None;
        self.quiet_since = None;
        Some(target)
    }
}

fn dump() -> Result<Vec<Value>, String> {
    let o = Command::new("pw-dump")
        .arg("--no-colors")
        .output()
        .map_err(|e| format!("cannot run pw-dump ({e}); is PipeWire installed?"))?;
    if !o.status.success() {
        return Err("pw-dump failed; is PipeWire running?".into());
    }
    serde_json::from_slice(&o.stdout).map_err(|e| format!("pw-dump output: {e}"))
}

fn set_profile(b: &Buds, target: Target) -> Result<(), String> {
    let (index, name) = match target {
        Target::Uhq => (b.uhq_index, PROFILE_UHQ),
        Target::Hifi => (b.hifi_index, PROFILE_HIFI),
    };
    let index = index.ok_or(format!("{} does not offer {name}", b.name))?;
    let ok = Command::new("wpctl")
        .args(["set-profile", &b.device.to_string(), &index.to_string()])
        .status()
        .map_err(|e| format!("cannot run wpctl: {e}"))?
        .success();
    if ok {
        Ok(())
    } else {
        Err(format!("wpctl could not set {name} on {}", b.name))
    }
}

fn systemctl(args: &[&str]) -> Option<String> {
    let o = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn unit_path() -> Result<PathBuf, String> {
    Ok(config_dir()?.join("systemd/user").join(UNIT))
}

fn unit_text(exe: &std::path::Path) -> String {
    format!(
        "[Unit]\n\
         Description=crossc: SSC UHQ on Galaxy Buds while audio above 48 kHz plays\n\
         After=pipewire.service wireplumber.service\n\
         \n\
         [Service]\n\
         ExecStart={} uhq watch\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exe.display()
    )
}

fn enable_auto() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find this program: {e}"))?;
    let path = unit_path()?;
    std::fs::create_dir_all(path.parent().unwrap())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    std::fs::write(&path, unit_text(&exe)).map_err(|e| format!("{}: {e}", path.display()))?;
    systemctl(&["daemon-reload"]);
    let ok = Command::new("systemctl")
        .args(["--user", "enable", "--now", UNIT])
        .status()
        .map_err(|e| format!("cannot run systemctl: {e}"))?
        .success();
    if !ok {
        return Err(format!("systemctl --user enable --now {UNIT} failed"));
    }
    systemctl(&["restart", UNIT]);
    Ok(())
}

fn disable_auto() {
    if systemctl(&["is-enabled", UNIT]).is_some_and(|s| s == "enabled") {
        systemctl(&["disable", "--now", UNIT]);
        println!("auto mode off");
    }
    if let Ok(p) = unit_path()
        && p.exists()
    {
        let _ = std::fs::remove_file(p);
        systemctl(&["daemon-reload"]);
    }
}

fn warn_rates(g: &Graph) {
    if let Some(rates) = &g.allowed_rates
        && !rates.contains("96000")
    {
        println!(
            "note: PipeWire's default.clock.allowed-rates is {rates}, so audio above 48 kHz is resampled to \
             48 kHz before it reaches the buds. Add 96000 to it (pipewire.conf.d) for real 96 kHz."
        );
    }
}

fn print_status(g: &Graph) {
    if g.buds.is_empty() {
        println!("no Galaxy Buds with SSC connected");
    }
    for b in &g.buds {
        let mode = match &b.sink {
            Some(s) if s.uhq => "UHQ 96 kHz".to_string(),
            Some(_) => "HiFi 48 kHz".to_string(),
            None => format!(
                "not on SSC (profile {})",
                b.profile.as_deref().unwrap_or("none")
            ),
        };
        let rate = b
            .sink
            .as_ref()
            .and_then(|s| s.rate)
            .map(|r| format!(", streaming at {r} Hz"))
            .unwrap_or_default();
        let uhq = if b.uhq_index.is_some() {
            ""
        } else {
            " (UHQ not offered by these buds)"
        };
        println!("{}: {mode}{rate}{uhq}", b.name);
    }
    let auto = systemctl(&["is-active", UNIT]).unwrap_or_default();
    println!("auto mode: {}", if auto == "active" { "on" } else { "off" });
    warn_rates(g);
}

const RATE_RULE_FILE: &str = "51-crossc-uhq-rate.conf";

/// PipeWire changes its graph rate only while the output is idle, so with any
/// other stream already playing (a browser keeps one open) 96 kHz audio would
/// be resampled to 48 kHz and back. This rule makes the UHQ output force its
/// graph to 96 kHz whenever it runs; the HiFi output and other devices keep
/// PipeWire's normal rate choice. WirePlumber appends rule arrays from
/// config fragments, so other `monitor.bluez.rules` stay in effect.
const RATE_RULE: &str = "# Written by crossc-cli uhq: while Galaxy Buds play SSC UHQ, run the graph at 96 kHz\n\
# so 96 kHz audio reaches them without a round trip through 48 kHz.\n\
monitor.bluez.rules = [\n\
  {\n\
    matches = [ { node.name = \"~bluez_output.*\", api.bluez5.codec = \"ssc_uhq\" } ]\n\
    actions = { update-props = { node.force-rate = 96000 } }\n\
  }\n\
]\n";

fn config_dir() -> Result<PathBuf, String> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .ok_or_else(|| "neither XDG_CONFIG_HOME nor HOME is set".to_string())
}

/// Writes the rate rule; true when WirePlumber has to restart to load it.
fn ensure_rate_rule() -> Result<bool, String> {
    let path = config_dir()?
        .join("wireplumber/wireplumber.conf.d")
        .join(RATE_RULE_FILE);
    if std::fs::read_to_string(&path).is_ok_and(|t| t == RATE_RULE) {
        return Ok(false);
    }
    std::fs::create_dir_all(path.parent().unwrap())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    std::fs::write(&path, RATE_RULE).map_err(|e| format!("{}: {e}", path.display()))?;
    println!("wrote {} (96 kHz graph while UHQ plays)", path.display());
    Ok(true)
}

/// Restarts WirePlumber so it reads new rules. That drops the buds' A2DP link,
/// which they do not bring back by themselves, so they are reconnected.
fn reload_wireplumber(addresses: &[String]) -> Result<(), String> {
    println!("restarting WirePlumber to load it");
    let ok = Command::new("systemctl")
        .args(["--user", "restart", "wireplumber"])
        .status()
        .map_err(|e| format!("cannot run systemctl: {e}"))?
        .success();
    if !ok {
        return Err("systemctl --user restart wireplumber failed".into());
    }
    std::thread::sleep(Duration::from_secs(2));
    for a in addresses {
        println!("reconnecting {a}");
        let _ = Command::new("bluetoothctl")
            .args(["disconnect", a])
            .stdout(Stdio::null())
            .status();
        std::thread::sleep(Duration::from_secs(2));
        let _ = Command::new("bluetoothctl")
            .args(["connect", a])
            .stdout(Stdio::null())
            .status();
    }
    Ok(())
}

/// Installs the rate rule, reloading WirePlumber and reconnecting the buds the first time.
fn prepare_rate_rule() -> Result<(), String> {
    if ensure_rate_rule()? {
        let g = graph(&dump()?);
        let addresses: Vec<String> = g.buds.iter().filter_map(|b| b.address.clone()).collect();
        reload_wireplumber(&addresses)?;
    }
    Ok(())
}

/// The graph once buds with SSC show up again, waiting up to `wait`.
fn graph_with_buds(wait: Duration) -> Result<Graph, String> {
    let until = Instant::now() + wait;
    loop {
        let g = graph(&dump()?);
        if !g.buds.is_empty() || Instant::now() >= until {
            return Ok(g);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn switch_all(target: Target) -> Result<(), String> {
    let g = graph_with_buds(Duration::from_secs(15))?;
    let mut done = 0;
    for b in &g.buds {
        if b.sink
            .as_ref()
            .is_some_and(|s| s.uhq == (target == Target::Uhq))
        {
            println!(
                "{}: already {}",
                b.name,
                if target == Target::Uhq { "UHQ" } else { "HiFi" }
            );
            done += 1;
            continue;
        }
        match set_profile(b, target) {
            Ok(()) => {
                println!(
                    "{}: {}",
                    b.name,
                    if target == Target::Uhq {
                        "UHQ 96 kHz"
                    } else {
                        "HiFi 48 kHz"
                    }
                );
                done += 1;
            }
            Err(e) => eprintln!("{e}"),
        }
    }
    if done == 0 {
        return Err("no Galaxy Buds with SSC connected; connect them first".into());
    }
    if target == Target::Uhq {
        warn_rates(&g);
    }
    Ok(())
}

/// Recursive merge of a `pw-dump --monitor` update into the object it updates.
fn merge(into: &mut Value, update: Value) {
    match (into, update) {
        (Value::Object(a), Value::Object(b)) => {
            for (k, v) in b {
                match a.get_mut(&k) {
                    Some(old) if old.is_object() && v.is_object() => merge(old, v),
                    _ => {
                        a.insert(k, v);
                    }
                }
            }
        }
        (slot, v) => *slot = v,
    }
}

/// One `pw-dump --monitor` session: its batches arrive on the returned channel
/// until PipeWire goes away.
fn monitor() -> Result<(std::process::Child, mpsc::Receiver<Vec<Value>>), String> {
    let mut child = Command::new("pw-dump")
        .args(["--monitor", "--no-colors"])
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run pw-dump: {e}"))?;
    let stdout = child.stdout.take().ok_or("pw-dump has no output")?;
    let (tx, rx) = mpsc::channel::<Vec<Value>>();
    std::thread::spawn(move || {
        let stream =
            serde_json::Deserializer::from_reader(BufReader::new(stdout)).into_iter::<Vec<Value>>();
        for batch in stream {
            let Ok(b) = batch else { break };
            if tx.send(b).is_err() {
                break;
            }
        }
    });
    Ok((child, rx))
}

fn watch() -> Result<(), String> {
    println!(
        "watching PipeWire: UHQ while audio above {HIFI_RATE} Hz plays into Galaxy Buds on SSC"
    );
    let mut auto: HashMap<u64, Auto> = HashMap::new();
    loop {
        let (mut child, rx) = monitor()?;
        let mut objects: HashMap<u64, Value> = HashMap::new();
        loop {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(batch) => {
                    for o in batch {
                        let Some(id) = id_of(&o["id"]) else { continue };
                        if o["info"].is_null() && o.get("metadata").is_none() {
                            objects.remove(&id);
                        } else if let Some(old) = objects.get_mut(&id) {
                            merge(old, o);
                        } else {
                            objects.insert(id, o);
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            let list: Vec<Value> = objects.values().cloned().collect();
            let now = Instant::now();
            for b in graph(&list).buds {
                let Some(sink) = &b.sink else { continue };
                if b.uhq_index.is_none() {
                    continue;
                }
                if let Some(t) =
                    auto.entry(b.device)
                        .or_default()
                        .decide(now, sink.uhq, sink.hires_playing)
                {
                    match set_profile(&b, t) {
                        Ok(()) => println!(
                            "{}: {}",
                            b.name,
                            if t == Target::Uhq {
                                "audio above 48 kHz, switched to UHQ"
                            } else {
                                "back to HiFi"
                            }
                        ),
                        Err(e) => eprintln!("{e}"),
                    }
                }
            }
        }
        let _ = child.wait();
        println!("PipeWire went away; watching again when it is back");
        std::thread::sleep(Duration::from_secs(2));
    }
}

pub fn run(cmd: UhqCmd) -> Result<(), String> {
    match cmd {
        UhqCmd::Status => print_status(&graph(&dump()?)),
        UhqCmd::On => {
            disable_auto();
            prepare_rate_rule()?;
            switch_all(Target::Uhq)?;
        }
        UhqCmd::Off => {
            disable_auto();
            switch_all(Target::Hifi)?;
        }
        UhqCmd::Auto => {
            prepare_rate_rule()?;
            enable_auto()?;
            println!(
                "auto mode on ({UNIT}): UHQ while audio above 48 kHz plays, HiFi otherwise; \
                 `crossc-cli uhq on` or `off` turns it off"
            );
            warn_rates(&graph(&dump()?));
        }
        UhqCmd::Watch => watch()?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn objects(stream_rate: u64, stream_state: &str, codec: &str) -> Vec<Value> {
        vec![
            json!({"id": 0, "type": "PipeWire:Interface:Core",
                   "info": {"props": {"default.clock.allowed-rates": "[ 44100, 48000, 88200, 96000 ]"}}}),
            json!({"id": 70, "type": "PipeWire:Interface:Device",
                   "info": {"props": {"device.api": "bluez5", "device.description": "Buds4 Pro",
                                      "device.name": "bluez_card.5C_D3_3D_36_59_51"},
                            "params": {"EnumProfile": [
                                {"index": 0, "name": "off"},
                                {"index": 2, "name": "a2dp-sink"},
                                {"index": 3, "name": "a2dp-sink-ssc"},
                                {"index": 4, "name": "a2dp-sink-ssc_uhq"}],
                                       "Profile": [{"index": 3, "name": "a2dp-sink-ssc"}]}}}),
            json!({"id": 82, "type": "PipeWire:Interface:Node",
                   "info": {"state": "running",
                            "props": {"media.class": "Audio/Sink", "device.id": 70, "api.bluez5.codec": codec},
                            "params": {"Format": [{"rate": 48000}]}}}),
            json!({"id": 90, "type": "PipeWire:Interface:Node",
                   "info": {"state": stream_state,
                            "props": {"media.class": "Stream/Output/Audio"},
                            "params": {"Format": [{"rate": stream_rate}]}}}),
            json!({"id": 95, "type": "PipeWire:Interface:Link",
                   "info": {"output-node-id": 90, "input-node-id": 82}}),
        ]
    }

    #[test]
    fn reads_buds_profiles_and_hires_streams() {
        let g = graph(&objects(96000, "running", "ssc"));
        assert_eq!(g.buds.len(), 1);
        let b = &g.buds[0];
        assert_eq!(
            (b.device, b.hifi_index, b.uhq_index),
            (70, Some(3), Some(4))
        );
        assert_eq!(b.profile.as_deref(), Some("a2dp-sink-ssc"));
        let s = b.sink.as_ref().unwrap();
        assert!(!s.uhq && s.hires_playing && s.rate == Some(48000));
        assert!(g.allowed_rates.unwrap().contains("96000"));
    }

    #[test]
    fn only_running_streams_above_48k_count() {
        let s = |rate, state| {
            graph(&objects(rate, state, "ssc")).buds[0]
                .sink
                .as_ref()
                .unwrap()
                .hires_playing
        };
        assert!(!s(48000, "running"));
        assert!(!s(44100, "running"));
        assert!(!s(96000, "idle"));
        assert!(s(88200, "running"));
    }

    #[test]
    fn buds_on_another_codec_have_no_ssc_sink() {
        let g = graph(&objects(96000, "running", "aac"));
        assert!(g.buds[0].sink.is_none());
    }

    #[test]
    fn auto_switches_up_after_a_moment_and_down_after_quiet() {
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let mut a = Auto::default();
        assert_eq!(a.decide(at(0), false, true), None);
        assert_eq!(a.decide(at(500), false, true), None);
        assert_eq!(a.decide(at(1000), false, true), Some(Target::Uhq));
        // Now on UHQ: a pause shorter than DOWN_AFTER keeps it.
        assert_eq!(a.decide(at(2000), true, false), None);
        assert_eq!(a.decide(at(9000), true, true), None);
        assert_eq!(a.decide(at(10000), true, false), None);
        assert_eq!(a.decide(at(19999), true, false), None);
        assert_eq!(a.decide(at(20000), true, false), Some(Target::Hifi));
    }

    #[test]
    fn auto_respects_the_cooldown() {
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let mut a = Auto::default();
        a.decide(at(0), false, true);
        assert_eq!(a.decide(at(1000), false, true), Some(Target::Uhq));
        // Something put it back on HiFi at once; hi-res still plays.
        a.decide(at(1100), false, true);
        assert_eq!(a.decide(at(2500), false, true), None, "within the cooldown");
        assert_eq!(a.decide(at(6000), false, true), Some(Target::Uhq));
    }

    #[test]
    fn monitor_updates_merge() {
        let mut o = json!({"id": 1, "info": {"state": "idle", "props": {"a": 1}}});
        merge(&mut o, json!({"id": 1, "info": {"state": "running"}}));
        assert_eq!(o["info"]["state"], "running");
        assert_eq!(o["info"]["props"]["a"], 1);
    }

    #[test]
    fn args() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(parse_uhq_args(&s(&[])), Ok(UhqCmd::Status));
        assert_eq!(parse_uhq_args(&s(&["auto"])), Ok(UhqCmd::Auto));
        assert!(parse_uhq_args(&s(&["maybe"])).is_err());
        assert!(parse_uhq_args(&s(&["on", "off"])).is_err());
    }

    #[test]
    fn rate_rule_targets_only_the_uhq_output() {
        assert!(RATE_RULE.contains(r#"api.bluez5.codec = "ssc_uhq""#));
        assert!(RATE_RULE.contains("node.force-rate = 96000"));
        assert!(RATE_RULE.contains(r#"node.name = "~bluez_output.*""#));
    }

    #[test]
    fn unit_runs_this_program() {
        let t = unit_text(std::path::Path::new("/usr/local/bin/crossc-cli"));
        assert!(t.contains("ExecStart=/usr/local/bin/crossc-cli uhq watch"));
        assert!(t.contains("WantedBy=default.target"));
    }
}
