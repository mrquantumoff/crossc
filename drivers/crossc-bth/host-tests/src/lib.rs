//! The driver's kernel-free modules, compiled for the host. See Cargo.toml.
#![no_std]
#![forbid(unsafe_code)]

#[path = "../../src/pcm_ring.rs"]
pub mod pcm_ring;

#[path = "../../src/audio_core.rs"]
pub mod audio_core;

#[path = "../../src/audio_park.rs"]
pub mod audio_park;

#[path = "../../src/bt_name.rs"]
pub mod bt_name;

#[path = "../../src/open_park.rs"]
pub mod open_park;

#[path = "../../src/cb_refs.rs"]
pub mod cb_refs;

#[path = "../../src/rt_limits.rs"]
pub mod rt_limits;

#[path = "../../src/avrcp_vol.rs"]
pub mod avrcp_vol;

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod ring_tests;
#[cfg(test)]
mod session_tests;
#[cfg(test)]
mod park_tests;
#[cfg(test)]
mod name_tests;
#[cfg(test)]
mod open_tests;
#[cfg(test)]
mod cb_tests;
#[cfg(test)]
mod rt_tests;
#[cfg(test)]
mod volume_tests;
