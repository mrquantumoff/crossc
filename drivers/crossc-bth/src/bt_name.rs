//! Bluetooth device name (`BTH_DEVICE_INFO.name`, UTF-8) -> the UTF-16 string
//! the driver stores as the device's friendly name. Pure code, tested on the
//! host (`drivers/crossc-bth/host-tests`).

/// Longest name kept, in UTF-16 units (a Bluetooth name is at most 248 bytes).
pub const MAX_UNITS: usize = 120;

/// Buffer for a name plus its terminating NUL.
pub type Utf16Name = [u16; MAX_UNITS + 1];

/// Convert `name` (UTF-8, up to the first NUL or the end of the slice) into
/// `out` as NUL-terminated UTF-16 and return the number of units written
/// *including* the terminator; 0 if nothing usable is left.
///
/// Invalid UTF-8 ends the name at the last valid character, control
/// characters are skipped, surrounding white space is trimmed, and a name
/// that is too long is cut at a character boundary.
pub fn to_utf16(name: &[u8], out: &mut Utf16Name) -> usize {
    let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
    let Some(bytes) = name.get(..end) else {
        return 0;
    };
    let text = match core::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => match bytes.get(..e.valid_up_to()).map(core::str::from_utf8) {
            Some(Ok(s)) => s,
            _ => return 0,
        },
    };
    let mut n = 0;
    for c in text.trim().chars().filter(|c| !c.is_control()) {
        let mut units = [0u16; 2];
        let units = c.encode_utf16(&mut units);
        let Some(dst) = out.get_mut(n..n + units.len()) else {
            break;
        };
        if n + units.len() > MAX_UNITS {
            break;
        }
        dst.copy_from_slice(units);
        n += units.len();
    }
    // Skipping control characters can expose white space at the end.
    while n > 0 && out.get(n - 1).is_some_and(|&u| u == 0x20) {
        n -= 1;
    }
    if n == 0 {
        return 0;
    }
    match out.get_mut(n) {
        Some(t) => {
            *t = 0;
            n + 1
        }
        None => 0,
    }
}
