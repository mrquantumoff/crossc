//! The snapshot: everything a user interface shows, as of one moment.
//!
//! Rules that keep old and new ends compatible:
//! * every struct is `#[serde(default)]`: a field the sender does not know
//!   is missing and takes its default, a field the reader does not know is
//!   ignored;
//! * enumerations travel as strings, and a string the reader does not know
//!   becomes its `Unknown` value instead of failing the snapshot;
//! * what does not exist at the moment (no link, no stream, not reported)
//!   is `None`, never a made-up zero.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// An enumeration that travels as a string. A string no variant has
/// decodes as `Unknown`.
macro_rules! wire_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident => $text:literal,)+ }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
        pub enum $name {
            $($(#[$vmeta])* $variant,)+
            /// A value this version does not know (or none at all).
            #[default]
            Unknown,
        }

        impl $name {
            /// The string on the wire.
            pub fn as_str(self) -> &'static str {
                match self {
                    $($name::$variant => $text,)+
                    $name::Unknown => "unknown",
                }
            }
            /// The value for a wire string; `Unknown` for any other.
            pub fn from_wire(s: &str) -> Self {
                match s {
                    $($text => $name::$variant,)+
                    _ => $name::Unknown,
                }
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                // Anything that is not a string is a value we do not know
                // either: it must not cost the rest of the snapshot.
                let v = serde_json::Value::deserialize(d)?;
                Ok(v.as_str().map_or($name::Unknown, $name::from_wire))
            }
        }
    };
}

wire_enum! {
    /// Where one device's session is.
    LinkPhase {
        /// The driver is not there, too old, or its audio endpoint is off.
        WaitingDriver => "waiting-driver",
        /// The driver is attached; the buds are not connected to this PC.
        WaitingBuds => "waiting-buds",
        /// The stream is open and the endpoint plugged in; nothing plays.
        Ready => "ready",
        /// Audio is being sent.
        Streaming => "streaming",
        /// The link failed and is being set up again
        /// ([`DeviceStatus::phase_reason`] says why).
        Reconnecting => "reconnecting",
        /// The session has ended (service stopping, machine going to sleep).
        Stopped => "stopped",
    }
}

wire_enum! {
    /// The codec mode of the stream.
    CodecMode {
        /// UHQ, 96 kHz / 24-bit.
        Uhq => "uhq",
        /// Seamless HiFi, 48 kHz / 24-bit.
        Hifi => "hifi",
        /// Plain SSC, 16-bit (48 or 44.1 kHz: see the stream rate).
        Ssc => "ssc",
    }
}

wire_enum! {
    /// Who applies the Windows volume slider.
    VolumeMode {
        /// The buds (AVRCP absolute volume): the hardware volume.
        Buds => "buds",
        /// The service (software gain): hardware volume without the buds
        /// answering.
        Software => "software",
        /// Windows itself scales the PCM (no hardware volume on the endpoint).
        Windows => "windows",
    }
}

wire_enum! {
    /// How bad a log line was.
    EventLevel {
        Warning => "warning",
        Error => "error",
    }
}

/// A PCM format: the Windows endpoint's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PcmFormat {
    /// Hz.
    pub sample_rate: u32,
    /// Valid bits per sample.
    pub bits: u8,
    pub channels: u8,
}

/// The stream to the buds. Present while a stream is configured
/// (ready or streaming).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LinkStatus {
    pub mode: CodecMode,
    /// The SSC configuration octet sent in SET_CONFIGURATION.
    pub config: u8,
    /// The buds' SSC capability octet.
    pub caps: u8,
    /// Rate of the audio as sent, Hz (after resampling, if any).
    pub stream_rate: u32,
    /// Bit depth of the audio as sent.
    pub stream_bits: u8,
    /// The bitrate in use right now, kbps.
    pub bitrate_kbps: u32,
    /// The configured bitrate, kbps: the top of the ladder.
    pub top_kbps: u32,
    /// The bitrate follows the link.
    pub adaptive: bool,
    /// The rates the stream may use, kbps, highest first. Empty: not known.
    pub ladder_kbps: Vec<u32>,
    /// Index of the current rate in `ladder_kbps` (0: the top).
    pub rung: u32,
    /// SSC frames per media packet.
    pub frames_per_packet: u32,
    /// Size of a media packet, bytes (RTP header included).
    pub packet_bytes: u32,
    /// Largest media packet allowed on this link, bytes.
    pub mtu_limit: u32,
    /// How `mtu_limit` was derived, short.
    pub mtu_reason: String,
    /// Delay the buds reported, in 1/10 ms. `None`: not reported.
    pub sink_delay_tenth_ms: Option<u32>,
    /// For how long audio has been streaming, ms. `None`: not streaming.
    pub streaming_ms: Option<u64>,
}

/// The volume of the endpoint.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VolumeStatus {
    pub mode: VolumeMode,
    /// 0..=100. `None`: not known (Windows applies it and does not say).
    pub level_percent: Option<u8>,
    pub muted: bool,
}

/// Counters. All of them count since the session of the device started,
/// except the two WRITE latencies, which describe the current stream.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Counters {
    /// Media packets sent.
    pub packets: u64,
    /// PCM frames taken from the endpoint.
    pub pcm_frames: u64,
    /// PCM frames the driver dropped because the service was too slow.
    pub dropped_frames: u64,
    /// Gaps in the PCM the driver reported.
    pub discontinuities: u64,
    /// Media writes that failed.
    pub write_errors: u64,
    /// Average time a media WRITE took, microseconds.
    pub write_avg_us: u32,
    /// Longest media WRITE, microseconds.
    pub write_max_us: u32,
    /// Packets that left later than their schedule allows.
    pub late_packets: u64,
    /// Times the link was set up again after a failure.
    pub reconnects: u32,
}

/// The driver on a device.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DriverInfo {
    /// "major.minor.patch".
    pub version: String,
    /// The IOCTL protocol the driver speaks.
    pub protocol: u32,
}

/// A warning or error line of the log.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Event {
    pub level: EventLevel,
    /// When, seconds since the Unix epoch (UTC).
    pub time_unix: u64,
    pub text: String,
}

/// One pair of buds (one crossc device interface).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceStatus {
    /// Stable, short identifier (the service's log prefix). Snapshots of
    /// the same device carry the same id.
    pub id: String,
    /// The buds' Bluetooth name. Empty: not known.
    pub name: String,
    pub phase: LinkPhase,
    /// Why the device is in `phase`, when there is something to say
    /// (what the driver lacks, why the link went down).
    pub phase_reason: String,
    /// The Windows endpoint's format. `None`: no render stream seen yet.
    pub endpoint: Option<PcmFormat>,
    /// The stream to the buds. `None`: none configured.
    pub link: Option<LinkStatus>,
    /// `None`: the driver has not been asked yet.
    pub volume: Option<VolumeStatus>,
    pub counters: Counters,
    /// `None`: the driver has not answered yet.
    pub driver: Option<DriverInfo>,
    /// The last warning or error of this device.
    pub last_event: Option<Event>,
}

/// The service itself.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServiceInfo {
    /// "major.minor.patch" of crossc-service.
    pub version: String,
    /// When the service started, seconds since the Unix epoch (UTC).
    pub started_unix: u64,
    /// The directory the service writes its log to. Empty: it logs
    /// elsewhere (console).
    pub log_dir: String,
    /// The last warning or error that belongs to no device.
    pub last_event: Option<Event>,
}

/// The whole state at one moment.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    /// [`crate::PROTOCOL_VERSION`] of the sender.
    pub version: u32,
    /// Counts the snapshots a server has sent (for spotting a gap).
    pub seq: u64,
    /// The sender's clock when it sent this, seconds since the Unix epoch.
    pub time_unix: u64,
    pub service: ServiceInfo,
    pub devices: Vec<DeviceStatus>,
}

impl DeviceStatus {
    /// What to call the device: its Bluetooth name, else its id.
    pub fn display_name(&self) -> &str {
        if self.name.trim().is_empty() {
            &self.id
        } else {
            self.name.trim()
        }
    }

    /// Everything but the values that change with every packet. Two
    /// statuses that are equal here differ only in running numbers.
    pub fn steady(&self) -> DeviceStatus {
        let mut d = self.clone();
        let reconnects = d.counters.reconnects;
        d.counters = Counters {
            reconnects,
            ..Counters::default()
        };
        if let Some(l) = &mut d.link {
            // Whether it streams is steady; for how long is not.
            l.streaming_ms = l.streaming_ms.map(|_| 0);
        }
        d
    }
}

impl Snapshot {
    /// See [`DeviceStatus::steady`]. Sequence number and time are left out
    /// too.
    pub fn steady(&self) -> Snapshot {
        Snapshot {
            version: self.version,
            seq: 0,
            time_unix: 0,
            service: self.service.clone(),
            devices: self.devices.iter().map(DeviceStatus::steady).collect(),
        }
    }

    /// Audio is being sent to some device.
    pub fn streaming(&self) -> bool {
        self.devices.iter().any(|d| d.phase == LinkPhase::Streaming)
    }
}

/// A snapshot with every field filled in, for the tests of this crate.
#[cfg(test)]
pub(crate) fn sample() -> Snapshot {
    Snapshot {
        version: crate::PROTOCOL_VERSION,
        seq: 7,
        time_unix: 1_800_000_000,
        service: ServiceInfo {
            version: "0.2.0".into(),
            started_unix: 1_799_990_000,
            log_dir: r"C:\ProgramData\crossc".into(),
            last_event: None,
        },
        devices: vec![DeviceStatus {
            id: "AABBCCDDEEFF".into(),
            name: "Galaxy Buds4 Pro".into(),
            phase: LinkPhase::Streaming,
            phase_reason: String::new(),
            endpoint: Some(PcmFormat {
                sample_rate: 96_000,
                bits: 24,
                channels: 2,
            }),
            link: Some(LinkStatus {
                mode: CodecMode::Uhq,
                config: 0x0A,
                caps: 0x3E,
                stream_rate: 96_000,
                stream_bits: 24,
                bitrate_kbps: 291,
                top_kbps: 584,
                adaptive: true,
                ladder_kbps: vec![584, 291, 152],
                rung: 1,
                frames_per_packet: 1,
                packet_bytes: 342,
                mtu_limit: 1005,
                mtu_reason: "min of reported MTUs, capped at 1005".into(),
                sink_delay_tenth_ms: Some(1500),
                streaming_ms: Some(61_000),
            }),
            volume: Some(VolumeStatus {
                mode: VolumeMode::Buds,
                level_percent: Some(62),
                muted: false,
            }),
            counters: Counters {
                packets: 6_700,
                pcm_frames: 5_788_800,
                dropped_frames: 0,
                discontinuities: 1,
                write_errors: 0,
                write_avg_us: 310,
                write_max_us: 4_200,
                late_packets: 3,
                reconnects: 1,
            },
            driver: Some(DriverInfo {
                version: "0.2.0".into(),
                protocol: 3,
            }),
            last_event: Some(Event {
                level: EventLevel::Warning,
                time_unix: 1_799_999_000,
                text: "link down: media write failed".into(),
            }),
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enums_travel_as_strings_and_unknown_strings_decode() {
        for p in [
            LinkPhase::WaitingDriver,
            LinkPhase::WaitingBuds,
            LinkPhase::Ready,
            LinkPhase::Streaming,
            LinkPhase::Reconnecting,
            LinkPhase::Stopped,
        ] {
            let json = serde_json::to_string(&p).unwrap();
            assert_eq!(json, format!("\"{}\"", p.as_str()));
            assert_eq!(serde_json::from_str::<LinkPhase>(&json).unwrap(), p);
        }
        for json in ["\"lossless-192k\"", "7", "null", "{\"a\":1}", "[]", "true"] {
            assert_eq!(
                serde_json::from_str::<CodecMode>(json).unwrap(),
                CodecMode::Unknown,
                "{json}"
            );
        }
        assert_eq!(CodecMode::from_wire("uhq"), CodecMode::Uhq);
        assert_eq!(VolumeMode::from_wire("buds"), VolumeMode::Buds);
        assert_eq!(EventLevel::from_wire("error"), EventLevel::Error);
    }

    #[test]
    fn steady_ignores_running_numbers_only() {
        let mut a = DeviceStatus {
            id: "AABBCCDDEEFF".into(),
            phase: LinkPhase::Streaming,
            link: Some(LinkStatus {
                bitrate_kbps: 584,
                streaming_ms: Some(1000),
                ..LinkStatus::default()
            }),
            ..DeviceStatus::default()
        };
        let mut b = a.clone();
        b.counters.packets = 99;
        b.counters.write_max_us = 5;
        b.link.as_mut().unwrap().streaming_ms = Some(2000);
        assert_ne!(a, b);
        assert_eq!(a.steady(), b.steady());
        // A reconnect, a bitrate step and a stopped stream are not noise.
        b.counters.reconnects = 1;
        assert_ne!(a.steady(), b.steady());
        b.counters.reconnects = 0;
        b.link.as_mut().unwrap().bitrate_kbps = 291;
        assert_ne!(a.steady(), b.steady());
        a.link.as_mut().unwrap().bitrate_kbps = 291;
        b.link.as_mut().unwrap().streaming_ms = None;
        assert_ne!(a.steady(), b.steady());
    }

    #[test]
    fn display_name_falls_back_to_the_id() {
        let mut d = DeviceStatus {
            id: "AABBCCDDEEFF".into(),
            ..DeviceStatus::default()
        };
        assert_eq!(d.display_name(), "AABBCCDDEEFF");
        d.name = "  ".into();
        assert_eq!(d.display_name(), "AABBCCDDEEFF");
        d.name = " Galaxy Buds4 Pro ".into();
        assert_eq!(d.display_name(), "Galaxy Buds4 Pro");
    }
}
