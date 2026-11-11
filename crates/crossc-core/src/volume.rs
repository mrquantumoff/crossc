//! Who applies the endpoint's volume, and the software gain stage for the
//! case where nobody else does.
//!
//! The driver's endpoint can carry a hardware volume and mute (protocol 3,
//! `crossc_ioctl::audio` "Volume"). Then Windows no longer scales the PCM;
//! the inbox AVRCP stack sends the level to the buds as absolute volume.
//! [`decide`] turns an [`AudioState`] into one of three modes:
//!
//! | Mode | When | PCM |
//! |---|---|---|
//! | [`VolumeMode::Windows`] | no hardware volume (v2 driver, `DisableAvrcpVolume`) | as Windows rendered it: Windows has applied the volume |
//! | [`VolumeMode::Buds`] | hardware volume, AVRCP active | unscaled; the buds apply the level. Only balance (the quieter channel relative to the louder) and mute are applied here |
//! | [`VolumeMode::Software`] | hardware volume, AVRCP not active | the level and mute applied here, so the slider works as before |
//!
//! [`Gain`] applies per-channel linear gain to an endpoint chunk at 32-bit
//! precision (24-bit samples keep their full resolution; the encoder input
//! conversion follows) and ramps every change linearly over [`RAMP`], so a
//! slider move or a mode switch does not click (zipper noise). At unity
//! with no ramp running the chunk is not touched at all, so the Windows and
//! Buds modes are bit-exact with the pre-volume service.

use std::time::Duration;

use crossc_ioctl::audio::{AudioState, PcmFormat, VOLUME_LEVEL_MIN};

/// Length of a gain ramp.
pub const RAMP: Duration = Duration::from_millis(20);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolumeMode {
    /// No hardware volume: Windows has scaled the PCM.
    Windows,
    /// The buds apply the volume (AVRCP absolute volume).
    Buds,
    /// Hardware volume without anybody applying it: software gain.
    Software,
}

impl VolumeMode {
    pub fn describe(self) -> &'static str {
        match self {
            VolumeMode::Windows => {
                "Windows applies the volume (no hardware volume on the endpoint)"
            }
            VolumeMode::Buds => "the buds apply the volume (AVRCP absolute volume active)",
            VolumeMode::Software => "software gain (hardware volume without an AVRCP subscriber)",
        }
    }
}

/// What the service should do with the PCM of a state.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Decision {
    pub mode: VolumeMode,
    /// Linear gain for left and right.
    pub gain: [f64; 2],
}

/// 1/65536 dB -> linear amplitude; the minimum level and below is silence
/// (slider at 0 is silent, as it is with Windows' own software volume).
pub fn level_to_gain(level: i32) -> f64 {
    if level <= VOLUME_LEVEL_MIN {
        0.0
    } else {
        10f64.powf(f64::from(level) / 65536.0 / 20.0)
    }
}

/// The mode and per-channel gain for `s`.
pub fn decide(s: &AudioState) -> Decision {
    if !s.hw_volume() {
        return Decision {
            mode: VolumeMode::Windows,
            gain: [1.0, 1.0],
        };
    }
    let v = s.volume;
    let mode = if s.avrcp_volume() {
        VolumeMode::Buds
    } else {
        VolumeMode::Software
    };
    if s.muted() {
        return Decision {
            mode,
            gain: [0.0, 0.0],
        };
    }
    let gain = match mode {
        // The buds play the level; only the balance is applied here.
        // Relative to the channels themselves, not to `level`: right after a
        // buds report `level` is the buds' exact value while the channels
        // hold Windows' rounded write-back of it, and that difference must
        // not scale the PCM.
        VolumeMode::Buds => {
            let top = v.left.max(v.right);
            let rel = |ch: i32| {
                if ch <= VOLUME_LEVEL_MIN {
                    0.0
                } else {
                    level_to_gain(ch.saturating_sub(top).min(0))
                }
            };
            [rel(v.left), rel(v.right)]
        }
        _ => [level_to_gain(v.left), level_to_gain(v.right)],
    };
    Decision { mode, gain }
}

/// The volume part of an [`AudioState`] as one line (CLI `info`, service
/// status).
pub fn describe(s: &AudioState) -> String {
    if !s.hw_volume() {
        return "no hardware volume (Windows scales the PCM)".into();
    }
    let v = s.volume;
    let dbs = |l: i32| f64::from(l) / 65536.0;
    let who = match decide(s).mode {
        VolumeMode::Buds => "the buds apply it (AVRCP absolute volume active)",
        _ => "software gain in the service (no AVRCP subscriber answering)",
    };
    format!(
        "hardware, {who}; level {:.1} dB = AVRCP {}{}; left {:.1} / right {:.1} dB; AVRCP subscriber: {}; AVRCP requests: {} get, {} set, {} events",
        dbs(v.level),
        v.avrcp,
        if s.muted() { ", muted" } else { "" },
        dbs(v.left),
        dbs(v.right),
        if s.avrcp_subscribed() { "yes" } else { "no" },
        v.gets,
        v.sets,
        v.events
    )
}

/// Per-channel gain with linear ramps. Starts at unity.
#[derive(Clone, Debug, PartialEq)]
pub struct Gain {
    cur: [f64; 2],
    target: [f64; 2],
    step: [f64; 2],
    /// Frames left in the running ramp.
    left: u64,
}

impl Default for Gain {
    fn default() -> Self {
        Self::unity()
    }
}

const FS: f64 = 2_147_483_648.0;

impl Gain {
    pub fn unity() -> Self {
        Gain {
            cur: [1.0, 1.0],
            target: [1.0, 1.0],
            step: [0.0, 0.0],
            left: 0,
        }
    }

    /// Move to `target`, ramping over [`RAMP`] at `rate` frames per second,
    /// or at once with `immediate`. A target already in effect or already
    /// being ramped to changes nothing.
    pub fn set(&mut self, target: [f64; 2], rate: u32, immediate: bool) {
        let target = target.map(|g| if g.is_finite() { g.max(0.0) } else { 0.0 });
        if target == self.target {
            if immediate && self.left > 0 {
                self.cur = target;
                self.left = 0;
            }
            return;
        }
        self.target = target;
        let frames = (u128::from(rate) * RAMP.as_nanos() / 1_000_000_000) as u64;
        if immediate || frames == 0 {
            self.cur = target;
            self.left = 0;
            return;
        }
        self.left = frames;
        self.step = [0, 1].map(|c| (target[c] - self.cur[c]) / frames as f64);
    }

    /// Unity and no ramp: [`Self::apply`] would not change a sample.
    pub fn is_unity(&self) -> bool {
        self.left == 0 && self.cur == [1.0, 1.0]
    }

    pub fn current(&self) -> [f64; 2] {
        self.cur
    }

    pub fn target(&self) -> [f64; 2] {
        self.target
    }

    /// Apply the gain to an endpoint chunk (`data` in `fmt`) and write the
    /// result to `out` as interleaved stereo, full 32-bit integer samples;
    /// returns that format. Mono is duplicated; a trailing partial frame is
    /// ignored.
    pub fn apply(&mut self, data: &[u8], fmt: PcmFormat, out: &mut Vec<u8>) -> PcmFormat {
        out.clear();
        for (l, r) in crate::pcm::frames_i32(data, fmt) {
            let g = self.cur;
            for (x, gc) in [(l, g[0]), (r, g[1])] {
                let y = (f64::from(x) * gc).round().clamp(-FS, FS - 1.0) as i32;
                out.extend_from_slice(&y.to_le_bytes());
            }
            if self.left > 0 {
                self.left -= 1;
                if self.left == 0 {
                    self.cur = self.target;
                } else {
                    for c in 0..2 {
                        self.cur[c] += self.step[c];
                    }
                }
            }
        }
        PcmFormat {
            sample_rate: fmt.sample_rate,
            channels: 2,
            container_bits: 32,
            valid_bits: 32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossc_ioctl::audio::{
        AUDIO_FLAG_ACX, AUDIO_FLAG_AVRCP, AUDIO_FLAG_HW_VOLUME, AUDIO_FLAG_MUTED, VolumeState,
    };

    const F24: PcmFormat = PcmFormat {
        sample_rate: 48000,
        channels: 2,
        container_bits: 32,
        valid_bits: 24,
    };
    const F16: PcmFormat = PcmFormat {
        sample_rate: 48000,
        channels: 2,
        container_bits: 16,
        valid_bits: 16,
    };

    fn db(x: f64) -> i32 {
        (x * 65536.0).round() as i32
    }

    fn state(flags: u32, level: i32, left: i32, right: i32) -> AudioState {
        AudioState {
            flags: AUDIO_FLAG_ACX | flags,
            volume: VolumeState {
                level,
                left,
                right,
                ..VolumeState::default()
            },
            ..AudioState::default()
        }
    }

    fn close(a: [f64; 2], b: [f64; 2]) -> bool {
        (a[0] - b[0]).abs() < 1e-9 && (a[1] - b[1]).abs() < 1e-9
    }

    #[test]
    fn describe_lines() {
        assert_eq!(
            describe(&AudioState::default()),
            "no hardware volume (Windows scales the PCM)"
        );
        let mut s = state(
            AUDIO_FLAG_HW_VOLUME | AUDIO_FLAG_AVRCP,
            db(-10.0),
            db(-10.5),
            db(-10.0),
        );
        s.flags |= crossc_ioctl::audio::AUDIO_FLAG_AVRCP_SUBSCRIBED;
        s.volume.avrcp = 64;
        s.volume.gets = 3;
        assert_eq!(
            describe(&s),
            "hardware, the buds apply it (AVRCP absolute volume active); level -10.0 dB = AVRCP 64; left -10.5 / right -10.0 dB; AVRCP subscriber: yes; AVRCP requests: 3 get, 0 set, 0 events"
        );
        let s = state(AUDIO_FLAG_HW_VOLUME | AUDIO_FLAG_MUTED, 0, 0, 0);
        assert!(describe(&s).starts_with("hardware, software gain in the service"));
        assert!(describe(&s).contains("AVRCP 0, muted"));
    }

    #[test]
    fn modes() {
        // v2 driver / kill switch: unity, whatever the volume bytes say.
        let d = decide(&state(AUDIO_FLAG_MUTED, db(-30.0), db(-30.0), db(-30.0)));
        assert_eq!(
            d,
            Decision {
                mode: VolumeMode::Windows,
                gain: [1.0, 1.0]
            }
        );
        // Buds: unscaled; balance relative to the louder channel only.
        let d = decide(&state(
            AUDIO_FLAG_HW_VOLUME | AUDIO_FLAG_AVRCP,
            db(-10.0),
            db(-10.0),
            db(-10.0),
        ));
        assert_eq!(
            d,
            Decision {
                mode: VolumeMode::Buds,
                gain: [1.0, 1.0]
            }
        );
        let d = decide(&state(
            AUDIO_FLAG_HW_VOLUME | AUDIO_FLAG_AVRCP,
            db(-10.0),
            db(-16.0),
            db(-10.0),
        ));
        assert_eq!(d.mode, VolumeMode::Buds);
        assert!(close(d.gain, [level_to_gain(db(-6.0)), 1.0]));
        // After a buds report the driver keeps their exact level (-10.19 dB,
        // AVRCP 64) while the channels hold audiosrv's 0.5 dB write-back
        // (-10.5 dB): still unity, exactly.
        let d = decide(&state(
            AUDIO_FLAG_HW_VOLUME | AUDIO_FLAG_AVRCP,
            -667_751,
            db(-10.5),
            db(-10.5),
        ));
        assert_eq!(
            d,
            Decision {
                mode: VolumeMode::Buds,
                gain: [1.0, 1.0]
            }
        );
        // Software: the level itself.
        let d = decide(&state(
            AUDIO_FLAG_HW_VOLUME,
            db(-20.0),
            db(-20.0),
            db(-26.0),
        ));
        assert_eq!(d.mode, VolumeMode::Software);
        assert!(close(d.gain, [0.1, level_to_gain(db(-26.0))]));
        // Mute silences in both hardware modes.
        for f in [
            AUDIO_FLAG_HW_VOLUME,
            AUDIO_FLAG_HW_VOLUME | AUDIO_FLAG_AVRCP,
        ] {
            let d = decide(&state(f | AUDIO_FLAG_MUTED, 0, 0, 0));
            assert_eq!(d.gain, [0.0, 0.0]);
        }
        // Slider at the bottom is silence.
        assert_eq!(level_to_gain(VOLUME_LEVEL_MIN), 0.0);
        assert_eq!(level_to_gain(0), 1.0);
        assert!((level_to_gain(db(-6.0206)) - 0.5).abs() < 1e-6);
    }

    fn tone24(frames: usize) -> Vec<u8> {
        let mut v = Vec::new();
        for i in 0..frames {
            let x = (0.5 * (i as f64 * 0.05).sin() * 8_388_607.0) as i32;
            v.extend_from_slice(&(x << 8).to_le_bytes());
            v.extend_from_slice(&((-x) << 8).to_le_bytes());
        }
        v
    }

    fn samples(b: &[u8]) -> Vec<i32> {
        b.as_chunks::<4>()
            .0
            .iter()
            .map(|c| i32::from_le_bytes(*c))
            .collect()
    }

    #[test]
    fn ramp_is_linear_and_lands_exactly() {
        let mut g = Gain::unity();
        assert!(g.is_unity());
        g.set([0.0, 0.5], 48000, false);
        assert!(!g.is_unity());
        // A constant full-scale-ish input shows the ramp itself.
        let one = (0x4000_0000i32).to_le_bytes();
        let mut data = Vec::new();
        for _ in 0..2000 {
            data.extend_from_slice(&one);
            data.extend_from_slice(&one);
        }
        let mut out = Vec::new();
        let f = g.apply(&data, F24, &mut out);
        assert_eq!((f.container_bits, f.valid_bits, f.channels), (32, 32, 2));
        let s = samples(&out);
        let ramp = 960; // 20 ms at 48 kHz
        // Monotonic, small steps (no zipper), ends exactly on target.
        let max_step = s
            .as_chunks::<2>()
            .0
            .iter()
            .zip(s.as_chunks::<2>().0.iter().skip(1))
            .map(|(a, b)| (i64::from(b[0]) - i64::from(a[0])).abs())
            .max()
            .unwrap();
        assert!(
            max_step <= i64::from(0x4000_0000i32) / ramp as i64 + 2,
            "{max_step}"
        );
        assert_eq!(s[2 * ramp], 0);
        assert_eq!(s[2 * ramp + 1], 0x2000_0000);
        assert_eq!(s[2 * 1999], 0);
        assert_eq!(g.current(), [0.0, 0.5]);
        // Retarget mid-ramp: continues from where it is, no jump.
        g.set([1.0, 1.0], 48000, false);
        let mut out = Vec::new();
        g.apply(&data[..8 * 100], F24, &mut out);
        let s = samples(&out);
        assert_eq!(s[0], 0, "first frame still at the old gain");
        g.set([0.25, 0.25], 48000, false);
        let before = g.current();
        let mut out2 = Vec::new();
        g.apply(&data[..8], F24, &mut out2);
        let first = samples(&out2)[0];
        assert!((f64::from(first) - before[0] * f64::from(0x4000_0000i32)).abs() < 2.0);
        // Same target again is a no-op; immediate jumps.
        g.set([0.25, 0.25], 48000, false);
        g.set([0.7, 0.7], 48000, true);
        assert_eq!(g.current(), [0.7, 0.7]);
        g.set([1.0, 1.0], 48000, true);
        assert!(g.is_unity());
    }

    #[test]
    fn sixteen_bit_input_keeps_24_bit_precision() {
        let mut g = Gain::unity();
        g.set([0.3, 0.3], 48000, true);
        let x: i16 = 12345;
        let mut data = Vec::new();
        for _ in 0..4 {
            data.extend_from_slice(&x.to_le_bytes());
            data.extend_from_slice(&(-x).to_le_bytes());
        }
        let mut out = Vec::new();
        g.apply(&data, F16, &mut out);
        let s = samples(&out);
        let want = (f64::from(i32::from(x) << 16) * 0.3).round() as i32;
        assert_eq!(s[0], want);
        // Below the 16-bit grid: the low bits carry information.
        assert_ne!(s[0] & 0xFFFF, 0);
        assert_ne!(
            (s[0] >> 8) & 0xFF,
            0,
            "24-bit value is not a widened 16-bit one"
        );
    }

    #[test]
    fn unity_is_untouched_and_mute_is_silence() {
        let data = tone24(500);
        let mut g = Gain::unity();
        let mut out = Vec::new();
        g.apply(&data, F24, &mut out);
        assert_eq!(out, data, "unity at 32 bits reproduces 24-in-32 exactly");
        g.set([0.0, 0.0], 48000, false);
        g.apply(&data, F24, &mut out);
        g.apply(&data, F24, &mut out); // frames 500..1000: the 960-frame ramp ends
        g.apply(&data, F24, &mut out);
        assert!(samples(&out).iter().all(|&x| x == 0));
        // Mono in, stereo out.
        let mono = PcmFormat { channels: 1, ..F24 };
        g.set([1.0, 0.5], 48000, true);
        let mut o = Vec::new();
        g.apply(&(0x1000_0000i32).to_le_bytes(), mono, &mut o);
        assert_eq!(samples(&o), vec![0x1000_0000, 0x0800_0000]);
    }
}
