use super::*;
use crate::hub::MIN_GAP;
use crate::model::{DeviceStatus, LinkPhase, ServiceInfo, sample};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use windows::Win32::Foundation::GENERIC_WRITE;

const WAIT: Duration = Duration::from_secs(10);

/// A pipe name no other test (or test process) uses.
fn pipe_name(tag: &str) -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    format!(
        r"\\.\pipe\crossc-status-test-{}-{}-{tag}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    )
}

fn options(name: &str) -> ServerOptions {
    ServerOptions {
        pipe_name: name.to_string(),
        security: Security::CurrentUser,
        ..ServerOptions::default()
    }
}

fn hub_with(id: &str) -> Hub {
    let hub = Hub::new(ServiceInfo {
        version: "0.2.0".into(),
        ..ServiceInfo::default()
    });
    hub.set_device(DeviceStatus {
        id: id.into(),
        ..sample().devices.remove(0)
    });
    hub
}

/// The server's listener exists a moment after `start` returns.
fn connect(name: &str) -> Connection {
    let deadline = Instant::now() + WAIT;
    loop {
        match Connection::open(name, Trust::ServiceOrSelf) {
            Ok(c) => return c,
            Err(p) if Instant::now() < deadline => {
                assert!(matches!(p, Problem::NotRunning | Problem::Busy), "{p}");
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(p) => panic!("cannot connect to {name}: {p}"),
        }
    }
}

fn reports() -> (Report, Arc<Mutex<Vec<String>>>) {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let l = lines.clone();
    (
        Arc::new(move |s: &str| l.lock().unwrap().push(s.to_string())),
        lines,
    )
}

fn wait_for_report(lines: &Mutex<Vec<String>>) {
    let deadline = Instant::now() + WAIT;
    while lines.lock().unwrap().is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_client_gets_the_state_at_once_then_changes_then_keepalives() {
    let name = pipe_name("basic");
    let hub = hub_with("AABBCCDDEEFF");
    let server = Server::start(hub.clone(), options(&name)).unwrap();
    let mut c = connect(&name);
    let t0 = Instant::now();
    let first = c.read(WAIT, None).unwrap();
    assert!(
        t0.elapsed() < Duration::from_secs(1),
        "no waiting for a tick"
    );
    assert_eq!(first.version, crate::PROTOCOL_VERSION);
    assert_eq!(first.service.version, "0.2.0");
    assert_eq!(first.devices.len(), 1);
    assert!(first.seq >= 1 && first.time_unix > 1_600_000_000);

    // A change of substance arrives without waiting for the interval.
    let mut d = first.devices[0].clone();
    d.phase = LinkPhase::Reconnecting;
    d.phase_reason = "media write failed".into();
    let t0 = Instant::now();
    hub.set_device(d);
    let s = loop {
        let s = c.read(WAIT, None).unwrap();
        if s.devices[0].phase == LinkPhase::Reconnecting {
            break s;
        }
    };
    assert!(
        t0.elapsed() < Duration::from_millis(1500),
        "{:?}",
        t0.elapsed()
    );
    assert_eq!(s.devices[0].phase_reason, "media write failed");
    assert!(s.seq > first.seq);

    // With no change at all a snapshot still comes every 2 s.
    let t0 = Instant::now();
    let again = c.read(WAIT, None).unwrap();
    assert_eq!(again.devices, s.devices);
    assert!(again.seq > s.seq);
    assert!(t0.elapsed() < IDLE_INTERVAL + Duration::from_secs(1));

    // A removed device is gone from the next one.
    hub.remove_device("AABBCCDDEEFF");
    loop {
        if c.read(WAIT, None).unwrap().devices.is_empty() {
            break;
        }
    }
    server.stop();
    assert!(matches!(c.read(WAIT, None), Err(Problem::Lost(_))));
    assert_eq!(
        Connection::open(&name, Trust::ServiceOrSelf).err(),
        Some(Problem::NotRunning)
    );
}

#[test]
fn running_numbers_are_paced_while_streaming() {
    let name = pipe_name("pace");
    let hub = hub_with("AABBCCDDEEFF");
    let _server = Server::start(hub.clone(), options(&name)).unwrap();
    let mut c = connect(&name);
    let first = c.read(WAIT, None).unwrap();
    let mut d = first.devices[0].clone();
    assert_eq!(d.phase, LinkPhase::Streaming);
    // 150 updates over 1.5 s: about three snapshots, not 150.
    let t0 = Instant::now();
    let feeder = std::thread::spawn(move || {
        for _ in 0..150 {
            d.counters.packets += 1;
            hub.set_device(d.clone());
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    let mut got = 0;
    let mut last = 0;
    while t0.elapsed() < Duration::from_millis(1600) {
        match c.read(Duration::from_millis(400), None) {
            Ok(s) => {
                got += 1;
                assert!(s.devices[0].counters.packets >= last);
                last = s.devices[0].counters.packets;
            }
            Err(Problem::Silent) => {}
            Err(p) => panic!("{p}"),
        }
    }
    feeder.join().unwrap();
    assert!((1..=8).contains(&got), "{got} snapshots in 1.6 s");
    assert!(last > first.devices[0].counters.packets);
}

#[test]
fn several_clients_up_to_the_limit() {
    let name = pipe_name("limit");
    let hub = hub_with("AABBCCDDEEFF");
    let _server = Server::start(
        hub.clone(),
        ServerOptions {
            max_clients: 3,
            ..options(&name)
        },
    )
    .unwrap();
    let mut clients: Vec<Connection> = (0..3).map(|_| connect(&name)).collect();
    for c in &mut clients {
        assert_eq!(c.read(WAIT, None).unwrap().devices.len(), 1);
    }
    // The fourth is told the pipe is busy, and the three are still served.
    assert_eq!(
        Connection::open(&name, Trust::ServiceOrSelf).err(),
        Some(Problem::Busy)
    );
    hub.remove_device("AABBCCDDEEFF");
    for c in &mut clients {
        loop {
            if c.read(WAIT, None).unwrap().devices.is_empty() {
                break;
            }
        }
    }
    // One leaves; the server notices at its next snapshot and a new
    // client gets in.
    drop(clients.pop());
    hub.set_device(DeviceStatus {
        id: "X".into(),
        ..DeviceStatus::default()
    });
    let mut newcomer = connect(&name);
    assert_eq!(newcomer.read(WAIT, None).unwrap().devices[0].id, "X");
}

#[test]
fn a_client_that_does_not_read_is_dropped_and_holds_nobody_up() {
    let name = pipe_name("slow");
    let hub = hub_with("AABBCCDDEEFF");
    let _server = Server::start(
        hub.clone(),
        ServerOptions {
            // Room for about two snapshots.
            out_buffer: 2048,
            ..options(&name)
        },
    )
    .unwrap();
    let mut stuck = connect(&name);
    let mut good = connect(&name);
    good.read(WAIT, None).unwrap();
    let mut d = hub.snapshot().devices.remove(0);
    d.phase_reason = "r".repeat(280);
    // Changes of substance, each read by the good client only. Setting the
    // state never waits for anybody.
    for i in 0..12u32 {
        d.counters.reconnects = i;
        let t0 = Instant::now();
        hub.set_device(d.clone());
        assert!(
            t0.elapsed() < Duration::from_millis(50),
            "set_device blocked"
        );
        loop {
            let s = good.read(WAIT, None).unwrap();
            if s.devices[0].counters.reconnects == i {
                break;
            }
        }
    }
    // The stuck one finds what fitted the buffer, then the end.
    let mut seen = 0;
    let end = loop {
        match stuck.read(WAIT, None) {
            Ok(_) => seen += 1,
            Err(p) => break p,
        }
    };
    assert!(matches!(end, Problem::Lost(_)), "{end}");
    assert!(seen < 12, "{seen} snapshots were buffered");
}

#[test]
fn clients_cannot_write_to_the_pipe() {
    let name = pipe_name("write");
    let _server = Server::start(hub_with("A"), options(&name)).unwrap();
    drop(connect(&name));
    // Until the server has a listening instance again the pipe is busy.
    let deadline = Instant::now() + WAIT;
    loop {
        // SAFETY: plain call.
        let r = unsafe {
            CreateFileW(
                &HSTRING::from(name.as_str()),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                None,
            )
        };
        match r {
            Ok(h) => {
                drop(Handle(h));
                panic!("the pipe could be opened for writing");
            }
            Err(e) if is(&e, ERROR_ACCESS_DENIED) => break,
            Err(e) if is(&e, ERROR_PIPE_BUSY) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => panic!("{e}"),
        }
    }
}

/// Is this process elevated? Then it is an Administrator, whom the
/// service's descriptor gives everything, and the checks that need a plain
/// user are skipped. (`net session` works for administrators only.)
fn elevated() -> bool {
    std::process::Command::new("net")
        .arg("session")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn the_service_descriptor_lets_users_read_and_nothing_else() {
    let name = pipe_name("sddl");
    let descriptor = Descriptor::from_sddl(SERVICE_SDDL).unwrap();
    let first = create_instance(&name, true, Some(&descriptor), 4, 4096).unwrap();
    // The name is taken: a second "first instance" is refused, with or
    // without the descriptor (this is what stops joining a squatter).
    assert!(create_instance(&name, true, Some(&descriptor), 4, 4096).is_err());
    assert!(create_instance(&name, true, None, 4, 4096).is_err());
    let admin = elevated();
    // A plain user cannot add an instance to the service's pipe.
    let second = create_instance(&name, false, Some(&descriptor), 4, 4096);
    if admin {
        eprintln!("elevated: skipping the checks that need a plain user");
    } else {
        let e = second.err().expect("a user created a pipe instance");
        assert!(is(&e, ERROR_ACCESS_DENIED), "{e}");
    }
    // Reading is allowed.
    let reader = Connection::open(&name, Trust::ServiceOrSelf);
    assert!(reader.is_ok(), "{:?}", reader.err());
    drop(reader);
    drop(first);
    // With a real server behind it, a snapshot arrives.
    if !admin {
        let name = pipe_name("sddl-server");
        let (report, lines) = reports();
        let _server = Server::start(
            hub_with("A"),
            ServerOptions {
                pipe_name: name.clone(),
                security: Security::Service,
                report: Some(report),
                ..ServerOptions::default()
            },
        )
        .unwrap();
        let mut c = connect(&name);
        assert_eq!(c.read(WAIT, None).unwrap().devices[0].id, "A");
        // As a plain user this server cannot make its second instance:
        // it says so once and keeps serving the client it has.
        wait_for_report(&lines);
        assert_eq!(
            lines.lock().unwrap().len(),
            1,
            "{:?}",
            lines.lock().unwrap()
        );
        assert!(c.read(WAIT, None).is_ok());
    }
}

#[test]
fn a_taken_name_is_reported_and_retried_not_joined() {
    let name = pipe_name("squat");
    let squatter = raw_server(&name);
    let (report, lines) = reports();
    let server = Server::start(
        hub_with("A"),
        ServerOptions {
            report: Some(report),
            ..options(&name)
        },
    )
    .unwrap();
    let held = server.pipe_held();
    wait_for_report(&lines);
    {
        let lines = lines.lock().unwrap();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].contains("another process holds that name"),
            "{lines:?}"
        );
    }
    // And the server knows that the pipe of that name is not its own.
    assert_eq!(held.get(), Held::Taken);
    let own = Server::start(hub_with("A"), options(&pipe_name("squat-free"))).unwrap();
    let t0 = Instant::now();
    while own.pipe_held().get() == Held::NotYet && t0.elapsed() < WAIT {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(own.pipe_held().get(), Held::Ours);
    own.stop();
    // Stopping does not wait for the retry.
    let t0 = Instant::now();
    server.stop();
    assert!(t0.elapsed() < Duration::from_secs(2));
    drop(squatter);
}

/// A server that is not ours: connect one client, write `payloads`, stay
/// for `hold`, leave.
fn serve(name: String, payloads: Vec<Vec<u8>>, hold: Duration) -> std::thread::JoinHandle<()> {
    let pipe = raw_server(&name);
    std::thread::spawn(move || {
        raw_connect(&pipe);
        for p in payloads {
            // A client that only looked at who owns the pipe may have left
            // before this write: nothing more to send it then.
            if !raw_write(&pipe, &p) {
                break;
            }
        }
        std::thread::sleep(hold);
    })
}

fn frame(json: &str) -> Vec<u8> {
    let mut m = (json.len() as u32).to_le_bytes().to_vec();
    m.extend_from_slice(json.as_bytes());
    m
}

#[test]
fn a_server_that_sends_garbage_or_another_version_or_nothing() {
    let hold = Duration::from_millis(300);

    let name = pipe_name("garbage");
    let t = serve(
        name.clone(),
        vec![b"\xFF\xFF\xFF\xFFhello there".to_vec()],
        hold,
    );
    assert!(matches!(
        connect(&name).read(WAIT, None),
        Err(Problem::Malformed(_))
    ));
    t.join().unwrap();

    let name = pipe_name("notjson");
    let t = serve(name.clone(), vec![frame("<html>")], hold);
    assert!(matches!(
        connect(&name).read(WAIT, None),
        Err(Problem::Malformed(_))
    ));
    t.join().unwrap();

    let name = pipe_name("version");
    let t = serve(
        name.clone(),
        vec![frame(r#"{"version":2,"devices":"all"}"#)],
        hold,
    );
    assert_eq!(
        connect(&name).read(WAIT, None),
        Err(Problem::Version { theirs: 2 })
    );
    t.join().unwrap();

    // Half a message and then nothing: the wait ends.
    let name = pipe_name("silent");
    let good = wire::encode(&sample()).unwrap();
    let t = serve(
        name.clone(),
        vec![good[..good.len() / 2].to_vec()],
        Duration::from_millis(600),
    );
    let t0 = Instant::now();
    assert_eq!(
        connect(&name).read(Duration::from_millis(300), None),
        Err(Problem::Silent)
    );
    assert!(t0.elapsed() < Duration::from_secs(2));
    t.join().unwrap();

    // A message in two writes is one snapshot; then the server leaves.
    let name = pipe_name("split");
    let (a, b) = good.split_at(100);
    let t = serve(name.clone(), vec![a.to_vec(), b.to_vec()], Duration::ZERO);
    let mut c = connect(&name);
    assert_eq!(c.read(WAIT, None).unwrap(), sample());
    assert!(matches!(c.read(WAIT, None), Err(Problem::Lost(_))));
    t.join().unwrap();
}

#[test]
fn a_blocked_read_ends_on_stop() {
    let name = pipe_name("stop");
    let pipe = raw_server(&name);
    let mut c = connect(&name);
    let stop = Stop::new().unwrap();
    let s = stop.clone();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        s.request();
    });
    let t0 = Instant::now();
    assert_eq!(c.read(WAIT, Some(&stop)), Err(Problem::Stopped));
    assert!(t0.elapsed() < Duration::from_secs(2));
    t.join().unwrap();
    drop(pipe);
}

#[test]
fn the_client_thread_reports_reconnects_and_stops() {
    let name = pipe_name("client");
    let (tx, rx) = mpsc::channel();
    let client = Client::spawn(
        ClientOptions {
            pipe_name: name.clone(),
            silence: Duration::from_secs(5),
            trust: Trust::ServiceOrSelf,
        },
        move |e| {
            let _ = tx.send(e);
        },
    )
    .unwrap();
    // No service: said once, however many attempts follow.
    assert_eq!(
        rx.recv_timeout(WAIT).unwrap(),
        ClientEvent::Problem(Problem::NotRunning)
    );
    assert!(rx.recv_timeout(Duration::from_millis(1800)).is_err());

    // The service starts: snapshots, without anybody telling the client.
    let hub = hub_with("AABBCCDDEEFF");
    let server = Server::start(hub.clone(), options(&name)).unwrap();
    match rx.recv_timeout(WAIT).unwrap() {
        ClientEvent::Snapshot(s) => assert_eq!(s.devices[0].id, "AABBCCDDEEFF"),
        other => panic!("{other:?}"),
    }
    // The service stops: the loss, then (once) that it is not running.
    server.stop();
    let mut problems = Vec::new();
    while problems.last() != Some(&Problem::NotRunning) {
        match rx.recv_timeout(WAIT).unwrap() {
            ClientEvent::Problem(p) => problems.push(p),
            ClientEvent::Snapshot(_) => {}
        }
    }
    assert!(matches!(problems[0], Problem::Lost(_)), "{problems:?}");
    assert_eq!(problems.len(), 2, "{problems:?}");
    // And comes back.
    let _server = Server::start(hub, options(&name)).unwrap();
    loop {
        if let ClientEvent::Snapshot(_) = rx.recv_timeout(WAIT).unwrap() {
            break;
        }
    }
    let t0 = Instant::now();
    client.stop();
    assert!(t0.elapsed() < Duration::from_secs(2), "stop does not wait");
}

/// What a program that is not the server may do to the pipe: open it and
/// close it again at once. The error, if it could not be opened.
fn touch(name: &HSTRING) -> Result<(), windows::core::Error> {
    // SAFETY: plain call; the handle is closed at once.
    unsafe {
        CreateFileW(
            name,
            GENERIC_READ.0,
            FILE_SHARE_NONE,
            None,
            OPEN_EXISTING,
            windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .map(|h| drop(Handle(h)))
}

/// What a program that wants the name does: create the pipe, insisting
/// on being the first. `true`: it got the name.
fn squat(name: &HSTRING) -> bool {
    // SAFETY: plain call; the handle is closed at once.
    let h = unsafe {
        CreateNamedPipeW(
            name,
            PIPE_ACCESS_OUTBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_MESSAGE | PIPE_WAIT,
            255,
            65536,
            0,
            0,
            None,
        )
    };
    if h.is_invalid() {
        false
    } else {
        drop(Handle(h));
        true
    }
}

/// Open and close the pipe as fast as two threads can, and try to create
/// it as fast as a third can, for `time`. Returns (opens that worked,
/// times the name did not exist, times the name could be taken).
fn flood(name: &str, time: Duration) -> (u64, u64, u64) {
    let stop = Arc::new(AtomicBool::new(false));
    let opened = Arc::new(AtomicU64::new(0));
    let missing = Arc::new(AtomicU64::new(0));
    let taken = Arc::new(AtomicU64::new(0));
    let mut threads = Vec::new();
    for _ in 0..2 {
        let (stop, opened, missing) = (stop.clone(), opened.clone(), missing.clone());
        let name = HSTRING::from(name);
        threads.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match touch(&name) {
                    Ok(()) => {
                        opened.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) if is(&e, ERROR_FILE_NOT_FOUND) => {
                        missing.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => {} // busy
                }
            }
        }));
    }
    {
        let (stop, taken) = (stop.clone(), taken.clone());
        let name = HSTRING::from(name);
        threads.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if squat(&name) {
                    taken.fetch_add(1, Ordering::Relaxed);
                }
            }
        }));
    }
    std::thread::sleep(time);
    stop.store(true, Ordering::Relaxed);
    for t in threads {
        t.join().unwrap();
    }
    (
        opened.load(Ordering::Relaxed),
        missing.load(Ordering::Relaxed),
        taken.load(Ordering::Relaxed),
    )
}

#[test]
fn the_name_is_never_free_while_the_server_runs() {
    // Clients that connect and leave at once, as fast as they can, while
    // another thread tries to create the pipe for itself: the server
    // keeps the name, keeps listening, and has nothing to complain about.
    let name = pipe_name("flood");
    let (report, lines) = reports();
    let hub = hub_with("AABBCCDDEEFF");
    let server = Server::start(
        hub.clone(),
        ServerOptions {
            report: Some(report),
            ..options(&name)
        },
    )
    .unwrap();
    // A real client came and went: the state the server is in afterwards
    // is the one that used to have no instance for a moment.
    connect(&name).read(WAIT, None).unwrap();
    let (opened, missing, taken) = flood(&name, Duration::from_millis(1500));
    assert!(opened > 100, "only {opened} opens: the flood did not run");
    assert_eq!(taken, 0, "another process could create the pipe");
    assert_eq!(missing, 0, "the name did not exist {missing} times");
    assert_eq!(*lines.lock().unwrap(), Vec::<String>::new());
    // And it serves as before, without waiting for any retry.
    let t0 = Instant::now();
    let mut c = connect(&name);
    assert_eq!(c.read(WAIT, None).unwrap().devices.len(), 1);
    assert!(t0.elapsed() < Duration::from_secs(1), "{:?}", t0.elapsed());

    // The same with a client that stays: it is served throughout.
    let (_, missing, taken) = {
        let reader = std::thread::spawn(move || {
            let t0 = Instant::now();
            let mut got = 0;
            while t0.elapsed() < Duration::from_millis(1200) {
                c.read(WAIT, None).unwrap();
                got += 1;
            }
            got
        });
        let feeder = {
            let hub = hub.clone();
            std::thread::spawn(move || {
                for i in 0..10u32 {
                    let mut d = hub.snapshot().devices.remove(0);
                    d.counters.reconnects = i;
                    hub.set_device(d);
                    std::thread::sleep(Duration::from_millis(100));
                }
            })
        };
        let r = flood(&name, Duration::from_millis(1000));
        feeder.join().unwrap();
        assert!(reader.join().unwrap() >= 5);
        r
    };
    assert_eq!((missing, taken), (0, 0));
    let t0 = Instant::now();
    server.stop();
    assert!(t0.elapsed() < Duration::from_secs(2), "stop under no load");
}

#[test]
fn the_name_is_kept_when_the_last_client_of_a_full_server_leaves() {
    // With as many clients as may be the server has no listener. When
    // they all go, their instances are the only ones: one of them must
    // become the listener, not be closed.
    let name = pipe_name("full");
    let hub = hub_with("AABBCCDDEEFF");
    let _server = Server::start(
        hub.clone(),
        ServerOptions {
            max_clients: 2,
            ..options(&name)
        },
    )
    .unwrap();
    let hname = HSTRING::from(name.as_str());
    // The client that got in at the end of a round is one of the two of
    // the next (a place it left would only be free again at the server's
    // next snapshot).
    let mut survivor: Option<Connection> = None;
    for round in 0..20u32 {
        let a = survivor.take().unwrap_or_else(|| {
            let mut a = connect(&name);
            a.read(WAIT, None).unwrap();
            a
        });
        let mut b = connect(&name);
        b.read(WAIT, None).unwrap();
        assert_eq!(
            Connection::open(&name, Trust::ServiceOrSelf).err(),
            Some(Problem::Busy)
        );
        let stop = Arc::new(AtomicBool::new(false));
        let squatter = {
            let (stop, hname) = (stop.clone(), hname.clone());
            std::thread::spawn(move || {
                let mut taken = 0;
                while !stop.load(Ordering::Relaxed) {
                    taken += u32::from(squat(&hname));
                }
                taken
            })
        };
        drop(a);
        drop(b);
        // A change makes the server write, and find both gone.
        let mut d = hub.snapshot().devices.remove(0);
        d.counters.reconnects = round + 1;
        hub.set_device(d);
        let mut c = connect(&name);
        assert_eq!(
            c.read(WAIT, None).unwrap().devices[0].counters.reconnects,
            round + 1
        );
        stop.store(true, Ordering::Relaxed);
        assert_eq!(squatter.join().unwrap(), 0, "round {round}");
        survivor = Some(c);
    }
}

#[test]
fn only_the_services_pipe_is_believed() {
    for trust in [Trust::Service, Trust::ServiceOrSelf] {
        assert!(trusted(Owner::System, trust));
        assert!(trusted(Owner::Administrators, trust));
        assert!(!trusted(Owner::Other, trust));
    }
    assert!(!trusted(Owner::Me, Trust::Service));
    assert!(trusted(Owner::Me, Trust::ServiceOrSelf));
    assert_eq!(Trust::default(), Trust::Service);
    assert_eq!(ClientOptions::default().trust, Trust::Service);

    // A pipe this (plain) user made: what any program on the machine can
    // put where the service's pipe belongs while the service is not there.
    let name = pipe_name("impostor");
    let t = serve(
        name.clone(),
        vec![wire::encode(&sample()).unwrap()],
        Duration::from_millis(200),
    );
    if elevated() {
        // An elevated process's pipes are owned by Administrators.
        eprintln!("elevated: skipping the checks that need a plain user");
        drop(connect(&name));
    } else {
        // SAFETY: plain call.
        let raw = Handle(
            unsafe {
                CreateFileW(
                    &HSTRING::from(name.as_str()),
                    GENERIC_READ.0,
                    FILE_SHARE_NONE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    None,
                )
            }
            .unwrap(),
        );
        assert_eq!(pipe_owner(&raw).ok(), Some(Owner::Me));
        drop(raw);
    }
    t.join().unwrap();

    if !elevated() {
        // (A pipe for every attempt: these have one instance, which is
        // of no use once a client has opened and closed it.)
        let name = pipe_name("impostor2");
        let pipe = raw_server(&name);
        assert_eq!(
            Connection::open(&name, Trust::Service).err(),
            Some(Problem::Untrusted)
        );
        drop(pipe);
        let name = pipe_name("impostor2");
        let pipe = raw_server(&name);
        assert_eq!(
            read_once(&name, WAIT, Trust::Service).err(),
            Some(Problem::Untrusted)
        );
        drop(pipe);
        // Asked for, the user's own pipe is taken.
        let name = pipe_name("impostor2");
        let pipe = raw_server(&name);
        assert!(Connection::open(&name, Trust::ServiceOrSelf).is_ok());
        drop(pipe);
        // A real server run by this user is no more the service than that.
        let name = pipe_name("impostor3");
        let _server = Server::start(hub_with("A"), options(&name)).unwrap();
        let deadline = Instant::now() + WAIT;
        loop {
            match Connection::open(&name, Trust::Service) {
                Err(Problem::Untrusted) => break,
                Err(Problem::NotRunning | Problem::Busy) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                other => panic!("{:?}", other.err()),
            }
        }
        // The client thread says so once, however often it looks, and
        // hands on nothing of what that server sends.
        let (tx, rx) = mpsc::channel();
        let client = Client::spawn(
            ClientOptions {
                pipe_name: name.clone(),
                ..ClientOptions::default()
            },
            move |e| {
                let _ = tx.send(e);
            },
        )
        .unwrap();
        assert_eq!(
            rx.recv_timeout(WAIT).unwrap(),
            ClientEvent::Problem(Problem::Untrusted)
        );
        assert!(rx.recv_timeout(Duration::from_millis(1800)).is_err());
        client.stop();
    }
}

#[test]
fn one_session_gets_only_its_share_of_the_places() {
    let name = pipe_name("session");
    let hub = hub_with("AABBCCDDEEFF");
    let _server = Server::start(
        hub.clone(),
        ServerOptions {
            max_per_session: 2,
            ..options(&name)
        },
    )
    .unwrap();
    // All of this test's clients are of one session.
    let mut a = connect(&name);
    let mut b = connect(&name);
    a.read(WAIT, None).unwrap();
    b.read(WAIT, None).unwrap();
    // The third gets in the door and is shown out; it holds no place.
    for _ in 0..5 {
        let deadline = Instant::now() + WAIT;
        let end = loop {
            match Connection::open(&name, Trust::ServiceOrSelf) {
                Ok(mut c) => break c.read(WAIT, None).err(),
                // Shown out before it had looked at the pipe.
                Err(Problem::Lost(e)) => break Some(Problem::Lost(e)),
                Err(Problem::Busy) if Instant::now() < deadline => {}
                Err(p) => panic!("{p}"),
            }
        };
        assert!(matches!(end, Some(Problem::Lost(_))), "{end:?}");
    }
    // The two are served as before.
    hub.remove_device("AABBCCDDEEFF");
    for c in [&mut a, &mut b] {
        loop {
            if c.read(WAIT, None).unwrap().devices.is_empty() {
                break;
            }
        }
    }
    // One leaves: the next one is let in, without the server having had
    // anything to send in between (it looks who is still there before it
    // turns somebody away; at most every 100 ms).
    drop(a);
    std::thread::sleep(MIN_GAP * 2);
    let mut c = connect(&name);
    assert!(c.read(WAIT, None).unwrap().devices.is_empty());
    // A flyout opened and closed a few times in a row is never refused.
    for _ in 0..6 {
        drop(c);
        std::thread::sleep(MIN_GAP * 2);
        c = connect(&name);
        c.read(WAIT, None).unwrap();
    }
    b.read(WAIT, None).unwrap();
}

#[test]
fn a_stopped_server_leaves_nothing_in_the_hub() {
    let hub = hub_with("A");
    for _ in 0..20 {
        let name = pipe_name("leak");
        let server = Server::start(hub.clone(), options(&name)).unwrap();
        assert_eq!(hub.waker_count(), 1);
        connect(&name).read(WAIT, None).unwrap();
        server.stop();
        assert_eq!(hub.waker_count(), 0);
    }
    // Dropping it is stopping it.
    let server = Server::start(hub.clone(), options(&pipe_name("leak"))).unwrap();
    drop(server);
    assert_eq!(hub.waker_count(), 0);
}

#[test]
fn the_services_pipe_name_is_a_pipe_name() {
    // The name every program uses unless told otherwise must be one
    // Windows takes: a server on a name that starts like it comes up, and
    // a client finds it. (Not the name itself: a service may be running.)
    assert_eq!(crate::PIPE_NAME, r"\\.\pipe\crossc-status");
    assert_eq!(ServerOptions::default().pipe_name, crate::PIPE_NAME);
    assert_eq!(ClientOptions::default().pipe_name, crate::PIPE_NAME);
    let name = format!("{}-test-{}", crate::PIPE_NAME, std::process::id());
    let (report, lines) = reports();
    let _server = Server::start(
        hub_with("A"),
        ServerOptions {
            report: Some(report),
            ..options(&name)
        },
    )
    .unwrap();
    assert_eq!(connect(&name).read(WAIT, None).unwrap().devices[0].id, "A");
    assert_eq!(*lines.lock().unwrap(), Vec::<String>::new());
    // A name that is no pipe's is not mistaken for "not running".
    // (With one backslash too few this is a path on the current drive.)
    let wrong = name.strip_prefix('\\').unwrap();
    assert!(wrong.starts_with(r"\.\pipe\"));
    let r = Connection::open(wrong, Trust::ServiceOrSelf).err();
    assert!(matches!(r, Some(Problem::Lost(_))), "{r:?}");
}

#[test]
fn retry_delays() {
    let p = Problem::NotRunning;
    let secs: Vec<f32> = (0..6).map(|a| retry_delay(a, &p).as_secs_f32()).collect();
    assert_eq!(secs, [0.5, 1.0, 2.0, 5.0, 5.0, 5.0]);
    assert_eq!(
        retry_delay(u32::MAX, &Problem::Busy),
        Duration::from_secs(5)
    );
    assert_eq!(
        retry_delay(0, &Problem::Version { theirs: 2 }),
        Duration::from_secs(30)
    );
}

#[test]
fn problems_read_well() {
    assert_eq!(
        Problem::NotRunning.to_string(),
        "the crossc service is not running"
    );
    assert_eq!(
        Problem::Untrusted.to_string(),
        "the status pipe belongs to another program, not to the crossc service"
    );
    let v = Problem::Version { theirs: 3 }.to_string();
    assert!(v.contains("version 3") && v.contains("version 1"), "{v}");
}
