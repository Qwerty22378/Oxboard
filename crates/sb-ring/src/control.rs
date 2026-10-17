//! Control-plane IOCTL ABI between the controller and the driver (`GUID_SB_CONTROL`): the
//! endpoint pool. The driver has a fixed pool; the controller enables, names and disables
//! endpoints, and picks what each does with no engine attached. Fixed-size `repr(C)` records,
//! strings as NUL-terminated UTF-16 in fixed arrays. Draft until D7 implements the driver side,
//! which must validate every field (see the crate docs).

use crate::ioctl::sb;

/// The control interface's device interface class (`GUID_SB_CONTROL`), as a u128 (no_std).
pub const GUID_SB_CONTROL: u128 = 0xe2f9_3808_4ae8_45cb_9d4f_9a94_41a0_c228;

/// Bumped on any incompatible change; `DriverInfo::abi_version` must match.
pub const CONTROL_ABI: u32 = 1;
/// Endpoints `IOCTL_SB_LIST_ENDPOINTS` can return.
pub const MAX_ENDPOINTS: usize = 32;
/// UTF-16 units in a name or Windows endpoint ID, NUL included.
pub const TEXT: usize = 64;

/// Output: [`DriverInfo`].
pub const IOCTL_SB_GET_INFO: u32 = sb(0x01);
/// Output: up to [`MAX_ENDPOINTS`] [`EndpointInfo`]s (as many as fit the output buffer).
pub const IOCTL_SB_LIST_ENDPOINTS: u32 = sb(0x02);
/// Input: [`SetEndpoint`].
pub const IOCTL_SB_SET_ENDPOINT: u32 = sb(0x03);
/// Input: [`SetFallback`].
pub const IOCTL_SB_SET_FALLBACK: u32 = sb(0x04);

pub const STATE_DISABLED: u32 = 0;
pub const STATE_ENABLED: u32 = 1;

/// With no engine attached, a render endpoint swallows its audio.
pub const FALLBACK_SILENCE: u32 = 0;
/// With no engine attached, a render endpoint plays into its paired capture endpoint (a cable).
pub const FALLBACK_LOOPBACK: u32 = 1;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriverInfo {
    pub abi_version: u32,
    pub driver_version: u32,
    /// Endpoints in the pool.
    pub endpoints: u32,
    pub _reserved: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndpointInfo {
    pub id: u32,
    /// [`crate::ioctl::Direction`] as u32.
    pub direction: u32,
    /// `STATE_*`.
    pub state: u32,
    /// `FALLBACK_*`.
    pub fallback: u32,
    pub name: [u16; TEXT],
    /// The Windows endpoint ID (`{0.0.0.00000000}.{guid}`) while enabled; empty otherwise.
    pub windows_id: [u16; TEXT],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetEndpoint {
    pub id: u32,
    /// `STATE_*`.
    pub state: u32,
    /// The name Windows shows; empty keeps the current one.
    pub name: [u16; TEXT],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetFallback {
    pub id: u32,
    pub fallback: u32,
}

const _: () = assert!(core::mem::size_of::<DriverInfo>() == 16);
const _: () = assert!(core::mem::size_of::<EndpointInfo>() == 16 + 4 * TEXT);
const _: () = assert!(core::mem::size_of::<SetEndpoint>() == 8 + 2 * TEXT);
const _: () = assert!(core::mem::size_of::<SetFallback>() == 8);

/// Write `s` as NUL-terminated UTF-16. `false` (and an empty string) if it doesn't fit.
pub fn set_text(dst: &mut [u16; TEXT], s: &str) -> bool {
    *dst = [0; TEXT];
    for (n, unit) in s.encode_utf16().enumerate() {
        if n == TEXT - 1 {
            *dst = [0; TEXT];
            return false;
        }
        dst[n] = unit;
    }
    true
}

/// The units before the first NUL (all of them if there's none: never trust the terminator).
pub fn text(src: &[u16; TEXT]) -> &[u16] {
    let end = src.iter().position(|u| *u == 0).unwrap_or(TEXT);
    &src[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::String;

    #[test]
    fn codes_are_distinct_from_each_other_and_the_data_plane() {
        let codes = [
            IOCTL_SB_GET_INFO,
            IOCTL_SB_LIST_ENDPOINTS,
            IOCTL_SB_SET_ENDPOINT,
            IOCTL_SB_SET_FALLBACK,
            crate::ioctl::IOCTL_SB_ATTACH_BUFFER,
            crate::ioctl::IOCTL_SB_SET_EVENT,
            crate::ioctl::IOCTL_SB_DETACH,
        ];
        for (i, a) in codes.iter().enumerate() {
            assert!(codes[i + 1..].iter().all(|b| a != b));
        }
        // CTL_CODE(FILE_DEVICE_SOUND, 0x801, METHOD_BUFFERED, FILE_READ_DATA|FILE_WRITE_DATA)
        assert_eq!(IOCTL_SB_GET_INFO, 0x001D_E004);
    }

    #[test]
    fn text_round_trips_and_overlong_or_unterminated_text_is_safe() {
        let mut t = [0u16; TEXT];
        assert!(set_text(&mut t, "Discord — voice"));
        assert_eq!(String::from_utf16(text(&t)).unwrap(), "Discord — voice");
        let long: String = core::iter::repeat_n('x', TEXT).collect();
        assert!(!set_text(&mut t, &long));
        assert!(text(&t).is_empty());
        let full = [b'a' as u16; TEXT];
        assert_eq!(text(&full).len(), TEXT, "no NUL: bounded by the array");
    }
}
