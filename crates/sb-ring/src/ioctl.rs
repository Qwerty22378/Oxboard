//! Data-plane IOCTL ABI between the engine and the driver (`GUID_SB_STREAM`). Both sides use
//! these definitions; the driver must still treat every request as hostile (see the crate
//! docs). Draft until D6 implements the driver side.

/// `FILE_DEVICE_SOUND`.
const DEVICE_SOUND: u32 = 0x0000_001D;
const METHOD_BUFFERED: u32 = 0;
const FILE_READ_WRITE: u32 = 0x0003; // FILE_READ_DATA | FILE_WRITE_DATA

/// Windows' `CTL_CODE` macro.
pub const fn ctl_code(device: u32, function: u32, method: u32, access: u32) -> u32 {
    (device << 16) | (access << 14) | (function << 2) | method
}

/// Custom function codes start at 0x800.
pub(crate) const fn sb(function: u32) -> u32 {
    ctl_code(DEVICE_SOUND, 0x800 + function, METHOD_BUFFERED, FILE_READ_WRITE)
}

/// Hand over a ring for an endpoint. Input: [`AttachBuffer`].
pub const IOCTL_SB_ATTACH_BUFFER: u32 = sb(0x10);
/// Hand over the event the driver signals each tick. Input: [`SetEvent`].
pub const IOCTL_SB_SET_EVENT: u32 = sb(0x11);
/// Release an endpoint's ring and event. Input: [`Detach`].
pub const IOCTL_SB_DETACH: u32 = sb(0x12);

/// Which way audio flows through an endpoint, from the engine's point of view.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Apps render into the endpoint; the driver produces into the ring, the engine consumes.
    FromApps = 1,
    /// The engine produces into the ring; the driver feeds the endpoint's capture side.
    ToApps = 2,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachBuffer {
    pub endpoint_id: u32,
    /// [`Direction`] as u32 (validated by the driver; never trust an enum from user mode).
    pub direction: u32,
    /// User-mode address of the ring (header first), page-aligned.
    pub address: u64,
    /// Length in bytes; must equal `ring_bytes(capacity)` from the header.
    pub length: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetEvent {
    pub endpoint_id: u32,
    pub _reserved: u32,
    /// User-mode event `HANDLE`, widened to 64 bits.
    pub handle: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Detach {
    pub endpoint_id: u32,
}

const _: () = assert!(core::mem::size_of::<AttachBuffer>() == 24);
const _: () = assert!(core::mem::size_of::<SetEvent>() == 16);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctl_code_matches_the_windows_macro() {
        // CTL_CODE(FILE_DEVICE_SOUND, 0x810, METHOD_BUFFERED, FILE_READ_DATA|FILE_WRITE_DATA)
        assert_eq!(IOCTL_SB_ATTACH_BUFFER, 0x001D_E040);
        assert_ne!(IOCTL_SB_ATTACH_BUFFER, IOCTL_SB_SET_EVENT);
        assert_ne!(IOCTL_SB_SET_EVENT, IOCTL_SB_DETACH);
    }
}
