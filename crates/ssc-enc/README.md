# ssc-enc

Encoder for the **Samsung Scalable / Seamless Codec (SSC)**, the Bluetooth A2DP vendor codec
(vendor `0x0000_0075`, codec `0x0103`) used by Galaxy phones and Galaxy Buds. It covers SSC 44.1/48 kHz,
Seamless HiFi 48 kHz/24-bit and UHQ 96 kHz/24-bit (v2 and legacy) at all 14 bitrates (88 to 886 kb/s).

This crate is a faithful port of the BSD-3-Clause C encoder
[samsung-ssc-encoder](https://github.com/WillyBilly06/samsung-ssc-encoder) (revision `94f1896`). With the
default features, its output is **byte-identical** to that C encoder built with MSVC. The crate is
`#![no_std]`, does not allocate (apart from the optional `Encoder::new_boxed`), contains no `unsafe`
(`forbid(unsafe_code)`), and has no global state.

```rust
use ssc_enc::{Bitrate, Encoder, Mode, PcmFrame, FRAME_SAMPLES, MAX_FRAME_BYTES, frame_size};

let mut enc = Encoder::new_boxed(Mode::Uhq96k, 2)?;        // 76,600 bytes of state, built directly on the heap
let pcm = vec![0i32; 2 * FRAME_SAMPLES];                   // 864 samples per channel, interleaved, 24-bit in i32
let mut frame = [0u8; MAX_FRAME_BYTES];
let n = enc.encode_frame(PcmFrame::I24(&pcm), Bitrate::K584, &mut frame)?;   // the bitrate may change every frame
assert_eq!(n, frame_size(Mode::Uhq96k, Bitrate::K584));    // 660
# Ok::<(), ssc_enc::Error>(())
```

## API

| Item | Meaning |
|---|---|
| `Mode` | `Ssc44k1`, `Ssc48k`, `Hifi48k`, `Uhq96k`, `LegacyUhq96k`. The bitstream has only two modes: 44.1/48 kHz and 96 kHz. The variants record what was negotiated over A2DP. |
| `Bitrate` | The 14 rate codes (`K88` = `0xF0` … `K152` = `0xE6`), plus `bps()`, `code()`, `is_hq()`, `from_code()` and `from_bps()`. |
| `frame_size(mode, bitrate)` | `const fn` giving the frame size in bytes, header included. |
| `PcmFrame` | `I16`, `I24` (24 bits in `i32`), `S32 { bits }` and `F32` (in [-1, 1)). Samples are interleaved; mono input (`channels == 1`) is copied to both coded channels. |
| `Encoder::new` / `Encoder::new_boxed` | Creates an encoder by value, or directly in a `Box` (feature `alloc`, on with `std`). See "Stack use". |
| `Encoder::encode_frame` | Encodes one 864-sample frame and returns the number of bytes written. |
| `Encoder::set_flags` | `Flags::{NO_TRANSIENTS, NO_POSTFILTER, FLAT_RESPONSE, SAMSUNG_HQ16_LEVEL}`, the C `SSC_ENC_FLAG_*` options. Default: none. |
| `Encoder::last_frame_info` | Sequence number, end band, transient and postfilter decisions, and range-coder position of the last frame. |
| `Encoder::reset` | Clears all history and restarts the sequence counter. |

The frame header layout is documented on the crate root (`cargo doc -p ssc-enc --open`).

The API mirrors `include/ssc_encoder.h`, with these differences:

- There is no allocator hook; the state is a plain struct.
- An invalid bitrate cannot be expressed.
- Flags are encoder state rather than a per-call argument.
- Mono input is accepted directly. The C API only takes stereo, and its tools duplicate mono themselves.

## Features

- `std` (default): `log2f`, `atan2`, `cos` and `sin` come from the platform C runtime through `std`. On
  Windows/MSVC that is the same UCRT the C reference uses, which is what makes the output byte-identical.
- `libm`: for `no_std` targets, the same functions come from the `libm` crate. `cargo build -p ssc-enc
  --no-default-features --features libm` builds. With neither feature, the build stops with a `compile_error!`.
- `alloc` (implied by `std`): `Encoder::new_boxed`. Add it to `libm` builds that have an allocator.
- `oracle`: enables the C-oracle tests (see below). Off by default, so a plain `cargo test` needs no C compiler.

## Stack use

`Encoder` is 76,600 bytes. `Encoder::new` returns it by value, so `Encoder::new` and
`Box::new(Encoder::new(..))` need several times that much stack while the encoder is built. `Encoder::new_boxed`
allocates pre-zeroed heap memory (via `bytemuck::zeroed_box`; an all-zero encoder is the blank state) and
initialises it in place, so the encoder is never on the stack, even in debug builds. `encode_frame` itself
needs little stack. Smallest thread stack that works, measured with `tests/stack.rs` on x86-64 Windows in 8 KiB
steps (`SSC_STACK_REPORT=1 cargo test -p ssc-enc [--release] --test stack -- --nocapture`):

| Construction | debug | release |
|---|---|---|
| `Box::new(Encoder::new(..))` | 328 KiB (456 KiB before `new` was reworked) | 200 KiB |
| `Encoder::new_boxed(..)` | 8 KiB | 8 KiB |
| `Encoder::new_boxed(..)` + one `encode_frame` | 72 KiB | 8 KiB |

On a small-stack thread, use `new_boxed`, or build the encoder on a thread with a normal stack and move the `Box`
over. A stack overflow aborts the process; it cannot be caught as an error.

## Bit-exactness

Every float operation is kept in the C evaluation order. That covers operand order, compound assignments, and
int-to-float conversion at the point of use. Rust never contracts to FMA. `sqrt` and `floor` are exact. C
`(int)` casts of floats use a helper that reproduces the x86-64 `cvttss2si` result (`INT_MIN` for NaN or
out-of-range values) rather than Rust's saturating `as`. Integer code that can overflow on garbage input
(NaN samples) uses wrapping arithmetic, as the compiled C does.

## Tests

| Test | Needs | What it checks |
|---|---|---|
| `tests/ref_frames.rs` | nothing | Replicates the Samsung-reference part of `tests/run_tests.py` / `ssc_conformance.c`. For every configuration of `run_tests.py` that names a reference `.ssc` (33 files), it encodes the same input with the same settings. It then compares each frame's size and its 4 header bytes against Samsung's frames. A size mismatch is accepted only where the reference frame was cut short by Samsung's capture harness, as upstream does. |
| `tests/api.rs` | nothing | Frame-size table, rate codes, argument errors and state size. Mono matches duplicated stereo, `I24` matches `S32{24}`, `reset` works, `new_boxed` gives the same encoder as `new`, and NaN/inf/huge float input encodes without panicking. |
| `tests/stack.rs` | nothing | Runs constructions on threads with small stacks, each in a child process. `new_boxed` must fit in 16 KiB, less than the encoder itself (debug and release); `new_boxed` plus `encode_frame` must fit in 256 KiB. |
| `tests/hygiene.rs` | nothing (git optional) | Every ported file carries the upstream copyright, license and C source name; NOTICE lists every source file and the adapted test code; nothing under `target/` is tracked by git and `.gitignore` ignores it. |
| `tests/oracle.rs` | `--features oracle` + the C oracle | **(a)** Rust output equals the C encoder's byte for byte, and every `FrameInfo` field matches. **(c)** Every Rust frame decodes with the BSD checker decoder (`tests/decoder`), with no decode error, and the decoder's final range-coder position equals the encoder's (the `ssc_conformance` "tell" check). The float tables are also checked bit for bit against the compiled C. |

The oracle matrix covers:

- all 5 modes × all 14 bitrates × 3 PCM front ends (`I16`, `I24` = s16·256, `F32` = s16/32768), on each of
  `mix`, `mix48k`, `mix96k` and `sine`, plus `mono` input;
- widened integer input (`wide_integer_input`): the 16-bit vectors scaled up with dither in the new low bits through
  `I24`, `S32{24}`, `S32{16}`, `S32{20}` and `S32{32}` (the whole `i32` range), in the HiFi 48 kHz and UHQ 96 kHz
  modes, stereo and mono, every rate plus switching. The oracle passes the same samples to `ssc_encode_s32`
  (`-fmt s32raw -bits B`);
- native 24-bit programme (`hires24_input`): synthesised in f64 and quantized straight to 24 bits with TPDF
  dither (no 16-bit source): a -1 dBFS log sweep to 0.45·fs, tones at -110 and -100 dBFS (below the 16-bit LSB),
  a -40 dBFS multitone including 30 kHz at 96 kHz, and harmonic decays fading through the 16-bit floor to below
  -130 dBFS. HiFi, UHQ and legacy UHQ, every rate plus switching, through `I24`, `S32{24}` and `F32`;
- every flag alone and all together, on music and tones, 16- and 24-bit, at every rate;
- per-frame bitrate switching (all 14 rates in turn, and a 584/291/152 UHQ pattern);
- a synthetic stress signal (silence, clipped full-scale noise, square wave, clicks, near-silence) at 44.1, 48
  and 96 kHz, 16- and 24-bit, every rate plus switching;
- raw float input containing NaN, ±inf, ±1e30, ±4.0 and denormals.

End-to-end 24-bit fidelity (`hires24_fidelity`): tones encoded by the Rust encoder, decoded by the C checker
decoder (`ssc_oracle dec -pcm`), level measured with a windowed single-bin DFT:

| Mode, bitrate | Input | Decoded | Neighbouring bin (+37 Hz) |
|---|---|---|---|
| HiFi 48k/24, 192 kb/s | 1 kHz, -20 dBFS | -20.01 dBFS | -110.9 dBFS |
| HiFi 48k/24, 192 kb/s | 1 kHz, -110 dBFS | -109.99 dBFS | -177.3 dBFS |
| HiFi 48k/24, 192 kb/s | 3.5 kHz, -100 dBFS | -100.00 dBFS | -169.7 dBFS |
| UHQ 96k/24, 584 kb/s | 1 kHz, -110 dBFS | -110.00 dBFS | -191.3 dBFS |
| UHQ 96k/24, 584 kb/s | 30 kHz, -20 dBFS | -20.60 dBFS | -83.8 dBFS |
| UHQ 96k/24, 584 kb/s | 40 kHz, -20 dBFS | -20.61 dBFS | -91.8 dBFS |

So detail 14 dB below the 16-bit LSB survives the codec at its level, and UHQ carries content up to at least
40 kHz. The test asserts ±0.25 dB in band, ±1 dB above 20 kHz, and the tone at least 40 dB above its neighbour.

Running the oracle tests:

```bat
crates\ssc-enc\oracle\build_oracle.cmd <path to a samsung-ssc-encoder checkout>
cargo test -p ssc-enc --release --features oracle
```

`build_oracle.cmd` compiles the C encoder, the checker decoder and `oracle/ssc_oracle.c` straight from the
checkout into `target\c-oracle`. It builds with `cl /O2 /fp:precise /MD`, with no `/arch`, so there is no FMA
and the dynamic UCRT is used, as with Rust. It also builds an `/Od` copy, and the oracle test `c_build_is_optimisation_independent` checks that
the `/O2` and `/Od` C builds produce identical streams. `SSC_ORACLE=<exe>` overrides the oracle path. `SSC_VECTORS_DIR=<checkout>\tests\vectors`
runs against the full vector set.

### Status (2026-11-10, rustc 1.99.0, MSVC cl 19.52.37029 (VS 2026, toolset 14.52), x86-64)

All tests pass. That covers `cargo test -p ssc-enc` with and without `--features oracle`, in four combinations:

- default features, debug (overflow checks on) and release, vendored vectors;
- default features, release, full vector set;
- `--no-default-features --features libm`, debug, vendored vectors;
- `--no-default-features --features libm`, release, full vector set.

The suites are 9 `api` tests, 1 `ref_frames` test, 14 oracle tests (incl. `hires24_input`: 135 runs / 49,950 frames, 0 mismatches, and `hires24_fidelity`) and 1 doctest. `cargo clippy -p ssc-enc --all-targets`
gives 0 warnings, both with the defaults and with `libm`.

| Suite | Vendored subset (48 frames per file) | Full upstream vector set |
|---|---|---|
| ref_frames: files / frames compared | 33 / 1,584: 0 size, 0 header mismatches | 33 / 5,366: 0 header mismatches; 2 size mismatches, both reference frames cut short (`uhq96_291000` and `uhq96_584000`, frame 202), same as upstream `docs/results.md` |
| oracle (a): runs / frames | 1,820 / 109,140: **0 byte-mismatching runs** | 1,820 / 336,720: **0 byte-mismatching runs** |
| oracle (c): decode errors / tell mismatches | 0 / 0 | 0 / 0 |
| oracle: C `/O2` vs `/Od` builds | 336 runs identical | 336 runs identical |
| api | 8 / 8 passed | n/a |

Oracle runs by group (vendored): 5 × 210 vector matrix, 560 flags, 90 bitrate switching, 90 stress, 30
pathological float. The `libm` build was also byte-identical to the C reference on every one of these runs.
That is not guaranteed in general (see below).

### Vendored vectors

`tests/vectors` holds about 2.0 MB: the first 48 frames of each upstream input (`mix`, `mix48k`, `mix96k`,
`sine`, `mono`) and of 33 of the 34 Samsung reference files. The full upstream set is about 7 MB, over the
3 MB budget, so these were left out:

- frames 48 and later of every file (including the two cut-short reference frames at frame 202);
- `ref_frames/mono_192000.ssc`, which upstream's `run_tests.py` never compares.

Run with `SSC_VECTORS_DIR` to use the full set.

## Known deviations

- **None in the encoder output** for the tested platform (x86-64 Windows, MSVC C build vs Rust with `std`).
- **`libm` feature:** `log2f`, `atan2`, `cos` and `sin` come from the `libm` crate instead of the platform C
  library. The bitstream is always valid, because the syntax depends only on integer state. A last-place
  difference in one of these functions could still change a float decision (an energy, the stereo angle or a
  twiddle), and with it the frame bytes. No such difference showed up on the test set.
- **Other platforms:** bit-exactness with a C build holds only when both sides use the same C math library and
  no FMA contraction. For example, glibc's `log2f` differs from the UCRT's, and GCC contracts to FMA on some
  targets by default.
- **Undefined behaviour in the C code:** float-to-int casts of NaN or out-of-range values, and the integer
  overflow that follows. The port reproduces what x86-64 compilers emit, so other C targets may differ there.

## Provenance and license

BSD-3-Clause. This crate is derived only from samsung-ssc-encoder:

- Copyright (c) 2026 William Ho (WillyBilly06);
- parts adapted from libopus 1.1 CELT, Copyright Xiph.Org Foundation and contributors.

Both notices are in `LICENSE-THIRD-PARTY`. `NOTICE` maps each Rust file to the C file it ports. Each ported file
carries the upstream copyright in its header. The test vectors come from the same repository, and the
`ref_frames` are Samsung encoder output it distributes for interoperability testing. No code from any other
SSC implementation was used.
