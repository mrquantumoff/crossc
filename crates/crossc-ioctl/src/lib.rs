//! IOCTL contract between `crossc_bth.sys` and user mode.
//!
//! Shared verbatim by the kernel driver (`drivers/crossc-bth`) and the user-mode
//! tools (`crates/crossc-cli`), so both sides agree on codes and wire layout.
//!
//! # Transport
//! * Device interface class: [`DEVICE_INTERFACE_GUID`]. Open it with
//!   `GENERIC_READ | GENERIC_WRITE` and preferably `FILE_FLAG_OVERLAPPED`
//!   (READ pends until data arrives; on a synchronous handle a pending READ
//!   serialises every other IOCTL behind it). The device also carries the
//!   Windows audio endpoint, so its DACL lets any logged-on user open it; the
//!   driver itself refuses every IOCTL except `GET_INFO` unless the calling
//!   process is elevated or SYSTEM (`STATUS_ACCESS_DENIED`).
//! * All IOCTLs are `METHOD_BUFFERED`. Requests/replies are little-endian
//!   packed byte layouts defined here by explicit `encode`/`decode` functions
//!   (no `repr(C)` transmutes), so they are identical on both sides.
//! * Access: `GET_INFO` is `FILE_ANY_ACCESS`; everything that touches the air
//!   link requires `FILE_READ_DATA | FILE_WRITE_DATA`.
//! * Channels belong to the file handle that opened them and are closed by the
//!   driver when that handle is cleaned up.
//!
//! # Errors
//! The driver completes failures with an NTSTATUS; user mode sees the Win32
//! translation via `GetLastError`. Notable ones:
//! * `STATUS_INVALID_HANDLE` (ERROR_INVALID_HANDLE, 6): unknown/stale channel id
//!   or channel owned by another handle.
//! * `STATUS_CONNECTION_DISCONNECTED` (ERROR_NETNAME_DELETED, 64): the remote
//!   closed the channel (pending and later READ/WRITE fail with this).
//! * `STATUS_INSUFFICIENT_RESOURCES` (ERROR_NO_SYSTEM_RESOURCES, 1450): no free
//!   channel slot.
//! * `STATUS_BUFFER_TOO_SMALL` (ERROR_INSUFFICIENT_BUFFER, 122): request/reply
//!   buffer shorter than the fixed part of the message.
//! * `STATUS_CANCELLED` (ERROR_OPERATION_ABORTED, 995): CancelIoEx or handle close.
//! * `STATUS_DEVICE_NOT_READY` (ERROR_NOT_READY, 21): device not in D0.
#![no_std]
#![forbid(unsafe_code)]

/// `{433F9EBA-036F-480D-826F-E0EC953319A9}` device interface class registered by
/// the driver on each bound A2DP-sink node. Fields as in a Windows `GUID`.
pub const DEVICE_INTERFACE_GUID: Guid = Guid {
    data1: 0x433f_9eba,
    data2: 0x036f,
    data3: 0x480d,
    data4: [0x82, 0x6f, 0xe0, 0xec, 0x95, 0x33, 0x19, 0xa9],
};

/// Plain GUID representation (field-compatible with Windows `GUID`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

/// Version of this contract, reported by GET_INFO. Bump on any wire change.
/// 1 = L2CAP pipe; 2 adds the [`audio`] IOCTLs (the pipe is unchanged); 3
/// adds the volume state to [`audio::AudioState`] (additive: the reply grows
/// behind the v2 bytes, every v2 field and code is unchanged); 4 adds
/// [`audio::IOCTL_AUDIO_SET_DEFAULT_FORMAT`] and the endpoint's default
/// format in [`audio::AudioState`] (additive again, behind the v3 bytes).
pub const PROTOCOL_VERSION: u32 = 4;

pub mod audio;

/// Max simultaneously open channels per device (AVDTP signaling + media + spare).
pub const MAX_CHANNELS: usize = 4;

/// Vendor-range device type (0x8000..=0xFFFF are for third parties).
pub const FILE_DEVICE_CROSSC: u32 = 0x8C55;
pub const METHOD_BUFFERED: u32 = 0;
pub const FILE_ANY_ACCESS: u32 = 0;
pub const FILE_READ_DATA: u32 = 1;
pub const FILE_WRITE_DATA: u32 = 2;

/// `CTL_CODE` from the WDK.
pub const fn ctl_code(device_type: u32, function: u32, method: u32, access: u32) -> u32 {
    (device_type << 16) | (access << 14) | (function << 2) | method
}

const RW: u32 = FILE_READ_DATA | FILE_WRITE_DATA;

/// In: nothing. Out: [`InfoReply`].
pub const IOCTL_GET_INFO: u32 =
    ctl_code(FILE_DEVICE_CROSSC, 0x800, METHOD_BUFFERED, FILE_ANY_ACCESS);
/// In: [`OpenChannelRequest`]. Out: [`OpenChannelReply`]. Pends until the
/// L2CAP connect + configuration finishes (can take seconds: paging, auth).
pub const IOCTL_OPEN_CHANNEL: u32 = ctl_code(FILE_DEVICE_CROSSC, 0x801, METHOD_BUFFERED, RW);
/// In: [`ChannelRequest`]. Out: nothing. Completes after L2CAP disconnect.
/// The driver waits at most ~8 s. If bthport has not finished by then it
/// fails with STATUS_IO_TIMEOUT (Win32 121). The id is then invalid, and the
/// slot frees itself once bthport drains it.
pub const IOCTL_CLOSE_CHANNEL: u32 = ctl_code(FILE_DEVICE_CROSSC, 0x802, METHOD_BUFFERED, RW);
/// In: [`ChannelRequest`] header followed by the SDU payload (1..=out_mtu
/// bytes). Out: [`WriteReply`]. One WRITE = one L2CAP SDU.
pub const IOCTL_WRITE: u32 = ctl_code(FILE_DEVICE_CROSSC, 0x803, METHOD_BUFFERED, RW);
/// In: [`ChannelRequest`]. Out: raw SDU bytes; `bytes returned` = SDU length.
/// Pends until one SDU arrives. Output buffer should be >= in_mtu. Several
/// READs may be outstanding on one channel (each one SDU).
pub const IOCTL_READ: u32 = ctl_code(FILE_DEVICE_CROSSC, 0x804, METHOD_BUFFERED, RW);

/// Errors from decoding a wire message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireError {
    /// Buffer shorter than the fixed message size.
    TooShort,
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}

/// GET_INFO reply.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InfoReply {
    /// [`PROTOCOL_VERSION`] of the driver.
    pub protocol_version: u32,
    /// Driver version, `major << 24 | minor << 16 | patch`.
    pub driver_version: u32,
    /// Remote BD_ADDR (48-bit BTH_ADDR; printed MSB first).
    pub remote_addr: u64,
    /// Channel slots in use / total.
    pub channels_open: u16,
    pub channels_max: u16,
    /// `INFO_FLAG_*`.
    pub flags: u32,
}

/// Device is in D0 (BRBs can be sent).
pub const INFO_FLAG_D0: u32 = 1;
/// Remote address came from IOCTL_INTERNAL_BTHENUM_GET_DEVINFO (else from the
/// device instance id).
pub const INFO_FLAG_ADDR_FROM_DEVINFO: u32 = 2;
/// A remote address is known at all.
pub const INFO_FLAG_ADDR_VALID: u32 = 4;

impl InfoReply {
    pub const SIZE: usize = 24;
    pub fn encode(&self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        b[0..4].copy_from_slice(&self.protocol_version.to_le_bytes());
        b[4..8].copy_from_slice(&self.driver_version.to_le_bytes());
        b[8..16].copy_from_slice(&self.remote_addr.to_le_bytes());
        b[16..18].copy_from_slice(&self.channels_open.to_le_bytes());
        b[18..20].copy_from_slice(&self.channels_max.to_le_bytes());
        b[20..24].copy_from_slice(&self.flags.to_le_bytes());
        b
    }
    pub fn decode(b: &[u8]) -> Result<Self, WireError> {
        if b.len() < Self::SIZE {
            return Err(WireError::TooShort);
        }
        Ok(Self {
            protocol_version: u32_at(b, 0),
            driver_version: u32_at(b, 4),
            remote_addr: u64_at(b, 8),
            channels_open: u16_at(b, 16),
            channels_max: u16_at(b, 18),
            flags: u32_at(b, 20),
        })
    }
}

/// Require an authenticated ACL link (CF_LINK_AUTHENTICATED).
pub const OPEN_FLAG_AUTHENTICATED: u32 = 1;
/// Require an encrypted ACL link (CF_LINK_ENCRYPTED).
pub const OPEN_FLAG_ENCRYPTED: u32 = 2;

/// L2CAP minimum / default MTUs (Core spec Vol 3 Part A).
pub const L2CAP_MIN_MTU: u16 = 48;
pub const L2CAP_DEFAULT_MTU: u16 = 672;
/// Max MTU the driver will accept outbound.
pub const L2CAP_MAX_MTU: u16 = 0xFFFF;

/// OPEN_CHANNEL request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenChannelRequest {
    /// L2CAP PSM (0x0019 = AVDTP; both signaling and media use it).
    pub psm: u16,
    /// MTU we can receive (our inbound MTU, sent in our L2CAP config request).
    /// 0 = [`L2CAP_DEFAULT_MTU`]. Clamped to >= [`L2CAP_MIN_MTU`].
    pub in_mtu: u16,
    /// Largest outbound MTU we accept from the peer's config request
    /// (0 = [`L2CAP_MAX_MTU`]). Media wants >= ~1005.
    pub out_mtu_max: u16,
    /// `OPEN_FLAG_*`.
    pub flags: u32,
}

impl OpenChannelRequest {
    pub const SIZE: usize = 12;
    pub fn encode(&self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        b[0..2].copy_from_slice(&self.psm.to_le_bytes());
        b[2..4].copy_from_slice(&self.in_mtu.to_le_bytes());
        b[4..6].copy_from_slice(&self.out_mtu_max.to_le_bytes());
        // 6..8 reserved
        b[8..12].copy_from_slice(&self.flags.to_le_bytes());
        b
    }
    pub fn decode(b: &[u8]) -> Result<Self, WireError> {
        if b.len() < Self::SIZE {
            return Err(WireError::TooShort);
        }
        Ok(Self {
            psm: u16_at(b, 0),
            in_mtu: u16_at(b, 2),
            out_mtu_max: u16_at(b, 4),
            flags: u32_at(b, 8),
        })
    }
    /// Effective inbound MTU after defaults/clamping (what the driver requests).
    pub fn effective_in_mtu(&self) -> u16 {
        match self.in_mtu {
            0 => L2CAP_DEFAULT_MTU,
            m => m.max(L2CAP_MIN_MTU),
        }
    }
    /// Effective max outbound MTU accepted.
    pub fn effective_out_mtu_max(&self) -> u16 {
        match self.out_mtu_max {
            0 => L2CAP_MAX_MTU,
            m => m.max(L2CAP_MIN_MTU),
        }
    }
}

/// OPEN_CHANNEL reply.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpenChannelReply {
    /// Opaque id for CLOSE/READ/WRITE (slot + generation; never 0).
    pub channel_id: u32,
    /// Negotiated MTU for data we send (peer's inbound MTU).
    pub out_mtu: u16,
    /// Negotiated MTU for data we receive.
    pub in_mtu: u16,
}

impl OpenChannelReply {
    pub const SIZE: usize = 8;
    pub fn encode(&self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        b[0..4].copy_from_slice(&self.channel_id.to_le_bytes());
        b[4..6].copy_from_slice(&self.out_mtu.to_le_bytes());
        b[6..8].copy_from_slice(&self.in_mtu.to_le_bytes());
        b
    }
    pub fn decode(b: &[u8]) -> Result<Self, WireError> {
        if b.len() < Self::SIZE {
            return Err(WireError::TooShort);
        }
        Ok(Self {
            channel_id: u32_at(b, 0),
            out_mtu: u16_at(b, 4),
            in_mtu: u16_at(b, 6),
        })
    }
}

/// CLOSE_CHANNEL / READ request, and the header of a WRITE request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelRequest {
    pub channel_id: u32,
}

impl ChannelRequest {
    /// 4 bytes id + 4 reserved (keeps the WRITE payload 8-byte aligned).
    pub const SIZE: usize = 8;
    pub fn encode(&self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        b[0..4].copy_from_slice(&self.channel_id.to_le_bytes());
        b
    }
    pub fn decode(b: &[u8]) -> Result<Self, WireError> {
        if b.len() < Self::SIZE {
            return Err(WireError::TooShort);
        }
        Ok(Self {
            channel_id: u32_at(b, 0),
        })
    }
}

/// Split a WRITE input buffer into header and payload.
pub fn split_write(b: &[u8]) -> Result<(ChannelRequest, &[u8]), WireError> {
    let h = ChannelRequest::decode(b)?;
    Ok((h, &b[ChannelRequest::SIZE..]))
}

/// WRITE reply.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WriteReply {
    pub bytes_written: u32,
}

impl WriteReply {
    pub const SIZE: usize = 4;
    pub fn encode(&self) -> [u8; Self::SIZE] {
        self.bytes_written.to_le_bytes()
    }
    pub fn decode(b: &[u8]) -> Result<Self, WireError> {
        if b.len() < Self::SIZE {
            return Err(WireError::TooShort);
        }
        Ok(Self {
            bytes_written: u32_at(b, 0),
        })
    }
}

/// Channel id helpers (driver side): `generation << 8 | slot`, never 0 as
/// long as generation starts at 1.
pub mod channel_id {
    pub fn make(slot: usize, generation: u32) -> u32 {
        ((generation & 0x00FF_FFFF) << 8) | (slot as u32 & 0xFF)
    }
    pub fn slot(id: u32) -> usize {
        (id & 0xFF) as usize
    }
    pub fn generation(id: u32) -> u32 {
        id >> 8
    }
}

/// Format a BTH_ADDR as `AA:BB:CC:DD:EE:FF` (MSB first).
pub fn format_bd_addr(addr: u64, out: &mut [u8; 17]) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for i in 0..6 {
        let byte = (addr >> (8 * (5 - i))) as u8;
        out[i * 3] = HEX[(byte >> 4) as usize];
        out[i * 3 + 1] = HEX[(byte & 0xF) as usize];
        if i < 5 {
            out[i * 3 + 2] = b':';
        }
    }
}

/// Parse the BD_ADDR out of a BTHENUM instance id, e.g.
/// `BTHENUM\{...}_VID&..._PID&...\8&2a1b&0&AABBCCDDEEFF_C00000000`.
/// Fallback for when IOCTL_INTERNAL_BTHENUM_GET_DEVINFO fails. UTF-16 input
/// (optionally NUL-terminated) so the driver can pass the property buffer.
pub fn bd_addr_from_instance_id(id: &[u16]) -> Option<u64> {
    let id = match id.iter().position(|&c| c == 0) {
        Some(n) => &id[..n],
        None => id,
    };
    let start = id
        .iter()
        .rposition(|&c| c == u16::from(b'\\'))
        .map_or(0, |p| p + 1);
    let last = &id[start..];
    let fstart = last
        .iter()
        .rposition(|&c| c == u16::from(b'&'))
        .map_or(0, |p| p + 1);
    let field = &last[fstart..];
    if field.len() < 12 {
        return None;
    }
    let mut v: u64 = 0;
    for &c in &field[..12] {
        let d = match c {
            0x30..=0x39 => c - 0x30,
            0x41..=0x46 => c - 0x41 + 10,
            0x61..=0x66 => c - 0x61 + 10,
            _ => return None,
        };
        v = (v << 4) | u64::from(d);
    }
    match field.get(12) {
        None | Some(0x5F) => Some(v),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    #[test]
    fn ctl_codes_are_stable() {
        assert_eq!(IOCTL_GET_INFO, 0x8C55_2000);
        assert_eq!(IOCTL_OPEN_CHANNEL, 0x8C55_E004);
        assert_eq!(IOCTL_CLOSE_CHANNEL, 0x8C55_E008);
        assert_eq!(IOCTL_WRITE, 0x8C55_E00C);
        assert_eq!(IOCTL_READ, 0x8C55_E010);
        for c in [
            IOCTL_GET_INFO,
            IOCTL_OPEN_CHANNEL,
            IOCTL_CLOSE_CHANNEL,
            IOCTL_WRITE,
            IOCTL_READ,
        ] {
            assert_eq!(c & 3, METHOD_BUFFERED);
        }
    }

    #[test]
    fn wdk_ctl_code_matches_cl_exe() {
        // Values printed by drivers/crossc-bth/tools/bthcheck (MSVC + WDK headers).
        assert_eq!(ctl_code(0x41, 0, 3, 0), 0x0041_0003); // IOCTL_INTERNAL_BTH_SUBMIT_BRB
        assert_eq!(ctl_code(0x41, 2, 3, 0), 0x0041_000B); // IOCTL_INTERNAL_BTHENUM_GET_DEVINFO
    }

    #[test]
    fn roundtrips() {
        let i = InfoReply {
            protocol_version: 1,
            driver_version: 0x0001_0000,
            remote_addr: 0x1122_3344_5566,
            channels_open: 2,
            channels_max: 4,
            flags: 3,
        };
        assert_eq!(InfoReply::decode(&i.encode()).unwrap(), i);
        let o = OpenChannelRequest {
            psm: 0x19,
            in_mtu: 1005,
            out_mtu_max: 0,
            flags: 3,
        };
        assert_eq!(OpenChannelRequest::decode(&o.encode()).unwrap(), o);
        let r = OpenChannelReply {
            channel_id: 0x101,
            out_mtu: 895,
            in_mtu: 1005,
        };
        assert_eq!(OpenChannelReply::decode(&r.encode()).unwrap(), r);
        let w = WriteReply { bytes_written: 7 };
        assert_eq!(WriteReply::decode(&w.encode()).unwrap(), w);
    }

    #[test]
    fn short_buffers_rejected() {
        assert_eq!(InfoReply::decode(&[0; 23]), Err(WireError::TooShort));
        assert_eq!(
            OpenChannelRequest::decode(&[0; 11]),
            Err(WireError::TooShort)
        );
        assert_eq!(OpenChannelReply::decode(&[0; 7]), Err(WireError::TooShort));
        assert_eq!(ChannelRequest::decode(&[0; 7]), Err(WireError::TooShort));
        assert_eq!(WriteReply::decode(&[0; 3]), Err(WireError::TooShort));
        assert_eq!(split_write(&[]), Err(WireError::TooShort));
    }

    #[test]
    fn split_write_works() {
        let mut b = Vec::from(ChannelRequest { channel_id: 9 }.encode());
        b.extend_from_slice(&[1, 2, 3]);
        let (h, p) = split_write(&b).unwrap();
        assert_eq!(h.channel_id, 9);
        assert_eq!(p, &[1, 2, 3]);
    }

    #[test]
    fn mtu_defaults() {
        let o = OpenChannelRequest {
            psm: 0x19,
            in_mtu: 0,
            out_mtu_max: 0,
            flags: 0,
        };
        assert_eq!(o.effective_in_mtu(), 672);
        assert_eq!(o.effective_out_mtu_max(), 0xFFFF);
        let o = OpenChannelRequest {
            psm: 0x19,
            in_mtu: 10,
            out_mtu_max: 10,
            flags: 0,
        };
        assert_eq!(o.effective_in_mtu(), 48);
        assert_eq!(o.effective_out_mtu_max(), 48);
    }

    #[test]
    fn channel_ids() {
        let id = channel_id::make(3, 0x1234);
        assert_ne!(id, 0);
        assert_eq!(channel_id::slot(id), 3);
        assert_eq!(channel_id::generation(id), 0x1234);
    }

    #[test]
    fn bd_addr_format_and_parse() {
        let mut s = [0u8; 17];
        format_bd_addr(0x0011_22AA_BBCC, &mut s);
        assert_eq!(&s, b"00:11:22:AA:BB:CC");
        let mut id: Vec<u16> = "BTHENUM\\{0000110b-0000-1000-8000-00805f9b34fb}_VID&00010075_PID&A015\\8&2a1b3c&0&A0B1C2D3E4F5_C00000000"
            .encode_utf16()
            .collect();
        assert_eq!(bd_addr_from_instance_id(&id), Some(0xA0B1_C2D3_E4F5));
        id.push(0);
        id.push(0x41);
        assert_eq!(bd_addr_from_instance_id(&id), Some(0xA0B1_C2D3_E4F5));
        let bad: Vec<u16> = "BTHENUM\\x\\8&2a&0&XYZ".encode_utf16().collect();
        assert_eq!(bd_addr_from_instance_id(&bad), None);
        assert_eq!(bd_addr_from_instance_id(&[]), None);
    }
}
