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
