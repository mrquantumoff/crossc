//! crossc-status: what crossc-service tells its user interfaces, and how.
//!
//! The service runs as LocalSystem in session 0 and the driver's IOCTLs are
//! admin-only, so a program of the logged-on user (the tray app) can show
//! nothing it does not get from the service. This crate is that channel:
//!
//! * [`model`]: the [`Snapshot`]: the whole state at one moment, one
//!   [`DeviceStatus`] per pair of buds plus the service's own fields.
//! * [`wire`]: one snapshot per message, a 4-byte length and JSON. Unknown
//!   fields are ignored and missing ones take their default, so either end
//!   may be newer than the other; input is never trusted.
//! * [`hub`]: where the service's threads put their state ([`Hub`]), and
//!   the rule for when a snapshot goes out ([`hub::Pacer`]).
//! * [`pipe`] (Windows): the named pipe `\\.\pipe\crossc-status`. The
//!   service's [`pipe::Server`] only ever writes; [`pipe::Client`] connects,
//!   reads and reconnects.
//!
//! Nothing here depends on crossc-core: the service converts its own state
//! into the types of [`model`] (INTEGRATION.md in this crate says how).

pub mod hub;
pub mod model;
#[cfg(windows)]
pub mod pipe;
pub mod wire;

pub use hub::Hub;
pub use model::{
    CodecMode, Counters, DeviceStatus, DriverInfo, Event, EventLevel, LinkPhase, LinkStatus,
    PcmFormat, ServiceInfo, Snapshot, VolumeMode, VolumeStatus,
};

/// The protocol's major version. It changes only when an old reader would
/// misread a new snapshot; adding fields does not change it.
pub const PROTOCOL_VERSION: u32 = 1;

/// The pipe the service publishes on.
pub const PIPE_NAME: &str = r"\\.\pipe\crossc-status";
