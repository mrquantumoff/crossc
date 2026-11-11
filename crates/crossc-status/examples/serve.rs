//! A stand-in for the service's side of the status pipe, for trying a
//! client (the tray app) against a real pipe without the service:
//!
//! ```text
//! cargo run -p crossc-status --example serve -- \\.\pipe\crossc-status-try 30
//! crossc-tray --print-once --pipe \\.\pipe\crossc-status-try
//! ```
//!
//! Serves one made-up streaming device on the named pipe for the given
//! number of seconds (default 30), counting packets as it goes. The pipe
//! gets the creating user's default security, not the service's.

#[cfg(windows)]
fn main() {
    use crossc_status::pipe::{Security, Server, ServerOptions};
    use crossc_status::{
        CodecMode, DeviceStatus, Hub, LinkPhase, LinkStatus, PcmFormat, ServiceInfo,
    };
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    let mut args = std::env::args().skip(1);
    let name = args
        .next()
        .unwrap_or_else(|| r"\\.\pipe\crossc-status-try".to_string());
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(30);
    let hub = Hub::new(ServiceInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        started_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        log_dir: String::new(),
        last_event: None,
    });
    let server = Server::start(
        hub.clone(),
        ServerOptions {
            pipe_name: name.clone(),
            security: Security::CurrentUser,
            report: Some(std::sync::Arc::new(|line: &str| eprintln!("{line}"))),
            ..ServerOptions::default()
        },
    )
    .expect("start the status server");
    println!("serving {name} for {seconds} s");
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(seconds) {
        let ms = start.elapsed().as_millis() as u64;
        let mut d = DeviceStatus {
            id: "AABBCCDDEEFF".into(),
            name: "Galaxy Buds4 Pro".into(),
            phase: LinkPhase::Streaming,
            endpoint: Some(PcmFormat {
                sample_rate: 48_000,
                bits: 24,
                channels: 2,
            }),
            link: Some(LinkStatus {
                mode: CodecMode::Hifi,
                config: 0x0C,
                caps: 0x3E,
                stream_rate: 48_000,
                stream_bits: 24,
                bitrate_kbps: 192,
                top_kbps: 192,
                adaptive: true,
                ladder_kbps: vec![192, 128, 88],
                frames_per_packet: 1,
                packet_bytes: 496,
                mtu_limit: 1005,
                streaming_ms: Some(ms),
                ..LinkStatus::default()
            }),
            ..DeviceStatus::default()
        };
        d.counters.packets = ms * 48 / 864;
        d.counters.pcm_frames = ms * 48;
        hub.set_device(d);
        std::thread::sleep(Duration::from_millis(100));
    }
    server.stop();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("the status pipe exists on Windows only");
}
