//! API behaviour that does not need the C oracle.

mod common;

use ssc_enc::{Bitrate, Encoder, Error, FRAME_SAMPLES, Flags, MAX_FRAME_BYTES, Mode, PcmFrame, frame_size};

/// Frame sizes from the reference README (header included).
#[test]
fn frame_sizes() {
    let table: [(Bitrate, usize, usize); 14] = [
        (Bitrate::K88, 224, 104),
        (Bitrate::K96, 244, 114),
        (Bitrate::K128, 324, 150),
        (Bitrate::K192, 484, 224),
        (Bitrate::K229, 576, 266),
        (Bitrate::K256, 630, 292),
        (Bitrate::K328, 806, 372),
        (Bitrate::K152, 376, 174),
        (Bitrate::K250, 616, 284),
        (Bitrate::K291, 716, 330),
        (Bitrate::K308, 758, 350),
        (Bitrate::K442, 1086, 500),
        (Bitrate::K584, 1434, 660),
        (Bitrate::K886, 2172, 1000),
    ];
    for (b, f44, f96) in table {
        for m in [Mode::Ssc44k1, Mode::Ssc48k, Mode::Hifi48k] {
            assert_eq!(frame_size(m, b), f44, "{b:?} {m:?}");
        }
        for m in [Mode::Uhq96k, Mode::LegacyUhq96k] {
            assert_eq!(frame_size(m, b), f96, "{b:?} {m:?}");
        }
    }
    const _: () = assert!(frame_size(Mode::Ssc44k1, Bitrate::K886) == MAX_FRAME_BYTES);
}

#[test]
fn bitrate_codes() {
    for b in Bitrate::ALL {
        assert_eq!(Bitrate::from_code(b.code()), Some(b));
        assert_eq!(Bitrate::from_bps(b.bps()), Some(b));
    }
    assert_eq!(Bitrate::from_code(0xF7), None);
    assert_eq!(Bitrate::from_bps(320000), None);
    let hq: Vec<Bitrate> = Bitrate::ALL.into_iter().filter(|b| b.is_hq()).collect();
    assert_eq!(hq.len(), 9);
}

#[test]
fn argument_errors() {
    assert_eq!(Encoder::new(Mode::Ssc44k1, 0).unwrap_err(), Error::InvalidChannels(0));
    assert_eq!(Encoder::new(Mode::Ssc44k1, 3).unwrap_err(), Error::InvalidChannels(3));
    let mut enc = Box::new(Encoder::new(Mode::Ssc44k1, 2).unwrap());
    let pcm = vec![0i16; 2 * FRAME_SAMPLES];
    let mut out = [0u8; MAX_FRAME_BYTES];
    assert_eq!(
        enc.encode_frame(PcmFrame::I16(&pcm[1..]), Bitrate::K192, &mut out),
        Err(Error::InvalidSampleCount { expected: 1728, got: 1727 })
    );
    assert_eq!(
        enc.encode_frame(PcmFrame::I16(&pcm), Bitrate::K192, &mut out[..483]),
        Err(Error::BufferTooSmall { needed: 484, got: 483 })
    );
    let s32 = vec![0i32; 2 * FRAME_SAMPLES];
    assert_eq!(
        enc.encode_frame(PcmFrame::S32 { samples: &s32, bits: 15 }, Bitrate::K192, &mut out),
        Err(Error::InvalidBitDepth(15))
    );
    // errors leave the state untouched: the first real frame still has sequence number 0
    assert!(enc.last_frame_info().is_none());
    assert_eq!(enc.encode_frame(PcmFrame::I16(&pcm), Bitrate::K192, &mut out), Ok(484));
    assert_eq!(enc.last_frame_info().unwrap().seq, 0);
}

/// Mono input is the same as the stereo input with both channels equal.
#[test]
fn mono_equals_duplicated_stereo() {
    let raw = common::synthetic(44100, 1.0);
    let left: Vec<i16> = raw.as_chunks::<4>().0.iter().map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
    let mut mono = Box::new(Encoder::new(Mode::Ssc44k1, 1).unwrap());
    let mut stereo = Box::new(Encoder::new(Mode::Ssc44k1, 2).unwrap());
    let (mut a, mut b) = ([0u8; MAX_FRAME_BYTES], [0u8; MAX_FRAME_BYTES]);
    for (k, chunk) in left.as_chunks::<FRAME_SAMPLES>().0.iter().enumerate() {
        let dup: Vec<i16> = chunk.iter().flat_map(|&s| [s, s]).collect();
        let rate = Bitrate::ALL[k % 14];
        let na = mono.encode_frame(PcmFrame::I16(chunk), rate, &mut a).unwrap();
        let nb = stereo.encode_frame(PcmFrame::I16(&dup), rate, &mut b).unwrap();
        assert_eq!(a[..na], b[..nb], "frame {k}");
    }
}

/// 16-bit, 24-bit-in-i32 and f32 input of the same signal give the same frames where the reference does
/// (I24 = I16 * 256 and S32{bits: 24} are the same front end).
#[test]
fn i24_equals_s32_24() {
    let raw = common::synthetic(96000, 0.5);
    let s: Vec<i32> = raw.as_chunks::<2>().0.iter().map(|&b| i16::from_le_bytes(b) as i32 * 256).collect();
    let mut e1 = Box::new(Encoder::new(Mode::Uhq96k, 2).unwrap());
    let mut e2 = Box::new(Encoder::new(Mode::Uhq96k, 2).unwrap());
    let (mut a, mut b) = ([0u8; MAX_FRAME_BYTES], [0u8; MAX_FRAME_BYTES]);
    for chunk in s.as_chunks::<{ 2 * FRAME_SAMPLES }>().0.iter() {
        let na = e1.encode_frame(PcmFrame::I24(chunk), Bitrate::K584, &mut a).unwrap();
        let nb = e2.encode_frame(PcmFrame::S32 { samples: chunk, bits: 24 }, Bitrate::K584, &mut b).unwrap();
        assert_eq!(a[..na], b[..nb]);
    }
}

#[test]
fn reset_restarts_stream() {
    let raw = common::synthetic(48000, 0.3);
    let s: Vec<i16> = raw.as_chunks::<2>().0.iter().map(|&b| i16::from_le_bytes(b)).collect();
    let mut enc = Box::new(Encoder::new(Mode::Hifi48k, 2).unwrap());
    enc.set_flags(Flags::FLAT_RESPONSE);
    let mut out = [0u8; MAX_FRAME_BYTES];
    let encode_all = |enc: &mut Encoder, out: &mut [u8]| -> Vec<Vec<u8>> {
        s.as_chunks::<{ 2 * FRAME_SAMPLES }>()
            .0
            .iter()
            .map(|c| {
                let n = enc.encode_frame(PcmFrame::I16(c), Bitrate::K192, out).unwrap();
                out[..n].to_vec()
            })
            .collect()
    };
    let first = encode_all(&mut enc, &mut out);
    enc.reset();
    assert_eq!(enc.flags(), Flags::FLAT_RESPONSE);
    let second = encode_all(&mut enc, &mut out);
    assert_eq!(first, second);
    // header: sync, F-family marker, sequence nibble, rate code
    assert_eq!(first[1][..2], [0xFF, 0xEE]);
    assert_eq!(first[1][2] >> 4, 1);
    assert_eq!(first[1][3], 0xF3);
}

/// Non-finite and out-of-range float input must not panic (the C reference has undefined behaviour there; the
/// port mirrors what x86-64 C builds do and keeps producing well-formed frames).
#[test]
fn pathological_float_input() {
    let specials = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1e30, -1e30, 4.0, -4.0, 1e-40, 0.0];
    for mode in [Mode::Ssc44k1, Mode::Uhq96k] {
        let mut enc = Box::new(Encoder::new(mode, 2).unwrap());
        let mut out = [0u8; MAX_FRAME_BYTES];
        for (k, &v) in specials.iter().enumerate() {
            for rate in Bitrate::ALL {
                let mut pcm = vec![0.25f32; 2 * FRAME_SAMPLES];
                for (i, s) in pcm.iter_mut().enumerate() {
                    if (i + k) % 7 == 0 {
                        *s = v;
                    }
                }
                let n = enc.encode_frame(PcmFrame::F32(&pcm), rate, &mut out).unwrap();
                assert_eq!(n, frame_size(mode, rate));
                assert_eq!(out[0], 0xFF);
            }
        }
    }
}

#[test]
fn state_size() {
    let s = std::mem::size_of::<Encoder>();
    println!("size_of::<Encoder>() = {s}");
    assert!(s < 100_000);
}

/// `Encoder::new_boxed` gives the same encoder as `Encoder::new` (both codec modes, mono and stereo, every
/// bitrate) and rejects the same channel counts.
#[cfg(feature = "alloc")]
#[test]
fn new_boxed_equals_new() {
    assert_eq!(Encoder::new_boxed(Mode::Uhq96k, 3).unwrap_err(), Error::InvalidChannels(3));
    assert_eq!(Encoder::new_boxed(Mode::Uhq96k, 0).unwrap_err(), Error::InvalidChannels(0));
    for (mode, rate) in [(Mode::Hifi48k, 48000), (Mode::Uhq96k, 96000)] {
        let raw = common::synthetic(rate, 0.5);
        let s: Vec<i16> = raw.as_chunks::<2>().0.iter().map(|&b| i16::from_le_bytes(b)).collect();
        for ch in [1, 2] {
            let mut a = Box::new(Encoder::new(mode, ch).unwrap());
            let mut b = Encoder::new_boxed(mode, ch).unwrap();
            assert_eq!((b.mode(), b.channels(), b.flags()), (mode, ch, Flags::NONE));
            let (mut fa, mut fb) = ([0u8; MAX_FRAME_BYTES], [0u8; MAX_FRAME_BYTES]);
            for (k, c) in s.chunks_exact(ch * FRAME_SAMPLES).enumerate() {
                let r = Bitrate::ALL[k % 14];
                let na = a.encode_frame(PcmFrame::I16(c), r, &mut fa).unwrap();
                let nb = b.encode_frame(PcmFrame::I16(c), r, &mut fb).unwrap();
                assert_eq!(fa[..na], fb[..nb], "{mode:?} ch {ch} frame {k}");
                assert_eq!(a.last_frame_info(), b.last_frame_info());
            }
        }
    }
}
