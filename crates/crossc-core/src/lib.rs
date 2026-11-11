//! crossc-core: everything the service and the CLI share on top of the
//! driver's IOCTL contract.
//!
//! * [`device`] (Windows): find and open the driver's device interface,
//!   overlapped IOCTLs with timeouts, L2CAP channels, the audio IOCTLs.
//! * [`channel`]: the SDU transport abstraction the protocol code runs on.
//! * [`signaling`], [`media`], [`peer`], [`ssc`]: AVDTP signaling client,
//!   capability probe, stream setup / teardown steps, answers to commands the
//!   sink initiates, picking the SSC stream end point.
//! * [`rtp`], [`pace`]: media packets, the media MTU rule, clocks.
//! * [`pcm`], [`resample`]: encoder input formats, conversions, sample-rate
//!   conversion (offline and streaming).
//! * [`plan`], [`settings`]: which SSC mode a given endpoint format is sent
//!   in, and the registry settings that tune it.
//! * [`abr`]: the adaptive bitrate controller (which rate the next frame is
//!   encoded at, from what the sender observes per packet).
//! * [`driver`], [`stream`], [`session`]: the seam to the driver, the PCM to
//!   packets pipeline, and the state machine that turns the endpoint's audio
//!   into an SSC stream. [`real`] (Windows) is the seam's implementation on
//!   the device, with the Bluetooth connection probe.
//! * [`volume`]: who applies the endpoint volume (Windows, the buds over
//!   AVRCP, or a software gain here) and the gain stage.
//! * [`winformat`]: the format picked for the buds in the Windows sound
//!   settings (where it is, how it is matched and parsed) and the decisions
//!   that make the driver's default format follow it; [`format_watch`]
//!   (Windows) watches it in the registry.
//! * [`log`], [`stop`]: logging and shutdown seams.
//! * [`fake`] (tests, or the `testing` feature): a fake AVDTP sink and a
//!   fake driver.

pub mod abr;
pub mod channel;
#[cfg(windows)]
pub mod device;
pub mod driver;
#[cfg(any(test, feature = "testing"))]
pub mod fake;
#[cfg(windows)]
pub mod format_watch;
pub mod hex;
pub mod log;
pub mod media;
pub mod pace;
pub mod pcm;
pub mod peer;
pub mod plan;
#[cfg(windows)]
pub mod real;
pub mod resample;
pub mod rtp;
pub mod session;
pub mod settings;
pub mod signaling;
pub mod ssc;
pub mod stop;
pub mod stream;
pub mod volume;
pub mod winformat;
