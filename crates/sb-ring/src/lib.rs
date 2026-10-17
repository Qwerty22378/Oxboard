//! The shared-memory ring between the soundboard engine (user mode) and its virtual audio
//! driver (kernel), the data-plane IOCTL ABI that hands it over, and the control-plane ABI the
//! controller manages the endpoint pool with ([`control`]). `#![no_std]` and dependency-free,
//! so the driver and user mode build the same layout from the same source.
//!
//! **The memory is untrusted on both sides.** It lives in user-mode pages that any process
//! holding the stream interface could rewrite at any moment. So:
//! - [`attach`] validates the header once and snapshots it; nothing in the header is read
//!   again afterwards.
//! - Each side keeps **its own index locally** and only *publishes* it; the other side's
//!   index is read once per operation and clamped against the snapshot capacity. A corrupt
//!   index yields silence (consumer) or dropped frames (producer), never an out-of-bounds
//!   access.
//!
//! Layout (see `Driver Notes` → Ring buffer layout):
//! ```text
//! 0    magic version sample_rate channels format capacity_frames   (64 B line)
//! 64   write_index: u64                                            (own cache line)
//! 128  read_index: u64                                             (own cache line)
//! 192  overruns: u64, underruns: u64                               (own cache line)
//! 256  PCM: capacity_frames × channels × f32, interleaved
//! ```

#![no_std]

#[cfg(test)]
extern crate std;

pub mod control;
pub mod ioctl;

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// "SBRG", little-endian.
pub const MAGIC: u32 = u32::from_le_bytes(*b"SBRG");
pub const VERSION: u32 = 1;
/// The only format in v1: 32-bit float.
pub const FORMAT_F32: u32 = 1;
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;
pub const HEADER_BYTES: usize = 256;
/// Largest ring accepted: 1 s. Bigger requests are rejected at attach.
pub const MAX_CAPACITY_FRAMES: u32 = SAMPLE_RATE;

/// One interleaved stereo frame.
pub type Frame = [f32; 2];

/// The header, exactly as it sits in shared memory.
#[repr(C, align(64))]
pub struct Header {
    pub magic: AtomicU32,
    pub version: AtomicU32,
    pub sample_rate: AtomicU32,
    pub channels: AtomicU32,
    pub format: AtomicU32,
    pub capacity_frames: AtomicU32,
    _pad0: [u8; 40],
    /// Frames written so far (monotonic; wraps with `% capacity`). Published by the producer.
    pub write_index: AtomicU64,
    _pad1: [u8; 56],
    /// Frames read so far. Published by the consumer.
    pub read_index: AtomicU64,
    _pad2: [u8; 56],
    /// Frames the producer dropped because the ring was full.
    pub overruns: AtomicU64,
    /// Frames the consumer filled with silence because the ring was empty.
    pub underruns: AtomicU64,
    _pad3: [u8; 48],
}

const _: () = assert!(core::mem::size_of::<Header>() == HEADER_BYTES);
const _: () = assert!(core::mem::offset_of!(Header, write_index) == 64);
const _: () = assert!(core::mem::offset_of!(Header, read_index) == 128);
const _: () = assert!(core::mem::offset_of!(Header, overruns) == 192);

/// Bytes needed for a ring of `capacity_frames`.
pub const fn ring_bytes(capacity_frames: u32) -> usize {
    HEADER_BYTES + capacity_frames as usize * CHANNELS as usize * core::mem::size_of::<f32>()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutError {
    /// Null, misaligned, or shorter than a header.
    BadBuffer,
    BadMagic(u32),
    BadVersion(u32),
    /// Rate, channel count or sample format other than v1's 48 kHz stereo f32.
    Unsupported,
    /// Capacity of 0 or above [`MAX_CAPACITY_FRAMES`].
    BadCapacity(u32),
    /// Buffer length doesn't match the header's capacity.
    LengthMismatch {
        expected: usize,
        actual: usize,
    },
}

/// The validated, snapshotted shape of a ring. Copies, never re-read from shared memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    base: *mut u8,
    capacity: u64,
}

// The layout is just an address and a number; the handles built on it do the synchronizing.
unsafe impl Send for Layout {}
unsafe impl Sync for Layout {}

impl Layout {
    pub fn capacity_frames(&self) -> u32 {
        self.capacity as u32
    }

    fn header(&self) -> &Header {
        // SAFETY: attach/init checked the buffer holds a header, aligned for it.
        unsafe { &*(self.base as *const Header) }
    }

    fn data(&self) -> *mut f32 {
        // SAFETY: the PCM area follows the header and holds `capacity` frames (checked).
        unsafe { self.base.add(HEADER_BYTES) as *mut f32 }
    }

    /// Overrun and underrun counters as last published (for reporting; untrusted).
    pub fn counters(&self) -> (u64, u64) {
        let h = self.header();
        (h.overruns.load(Ordering::Relaxed), h.underruns.load(Ordering::Relaxed))
    }
}

fn check_buffer(base: *mut u8, len: usize) -> Result<(), LayoutError> {
    if base.is_null() || (base as usize) % core::mem::align_of::<Header>() != 0 || len < HEADER_BYTES {
        return Err(LayoutError::BadBuffer);
    }
    Ok(())
}

/// Write a fresh header for `capacity_frames` into `base..base+len` (the side that allocates
/// the memory does this) and return its layout.
///
/// # Safety
/// `base` must point to `len` writable bytes that stay valid while the layout is used, and
/// no one else may be using them yet.
pub unsafe fn init(base: *mut u8, len: usize, capacity_frames: u32) -> Result<Layout, LayoutError> {
    check_buffer(base, len)?;
    if capacity_frames == 0 || capacity_frames > MAX_CAPACITY_FRAMES {
        return Err(LayoutError::BadCapacity(capacity_frames));
    }
    let expected = ring_bytes(capacity_frames);
    if len != expected {
        return Err(LayoutError::LengthMismatch { expected, actual: len });
    }
    // SAFETY: caller guarantees exclusive access to `len` writable bytes.
    unsafe { core::ptr::write_bytes(base, 0, len) };
    let layout = Layout {
        base,
        capacity: capacity_frames as u64,
    };
    let h = layout.header();
    h.sample_rate.store(SAMPLE_RATE, Ordering::Relaxed);
    h.channels.store(CHANNELS, Ordering::Relaxed);
    h.format.store(FORMAT_F32, Ordering::Relaxed);
    h.capacity_frames.store(capacity_frames, Ordering::Relaxed);
    h.version.store(VERSION, Ordering::Relaxed);
    h.magic.store(MAGIC, Ordering::Release);
    Ok(layout)
}

/// Validate a ring someone else initialized and snapshot its shape.
///
/// # Safety
/// `base` must point to `len` bytes that stay readable and writable while the layout is used
/// (their *contents* may be anything: that's what this checks).
pub unsafe fn attach(base: *mut u8, len: usize) -> Result<Layout, LayoutError> {
    check_buffer(base, len)?;
    // SAFETY: at least a header's worth of aligned bytes (checked above).
    let h = unsafe { &*(base as *const Header) };
    let magic = h.magic.load(Ordering::Acquire);
    if magic != MAGIC {
        return Err(LayoutError::BadMagic(magic));
    }
    let version = h.version.load(Ordering::Relaxed);
    if version != VERSION {
        return Err(LayoutError::BadVersion(version));
    }
    if h.sample_rate.load(Ordering::Relaxed) != SAMPLE_RATE
        || h.channels.load(Ordering::Relaxed) != CHANNELS
        || h.format.load(Ordering::Relaxed) != FORMAT_F32
    {
        return Err(LayoutError::Unsupported);
    }
    let capacity = h.capacity_frames.load(Ordering::Relaxed);
    if capacity == 0 || capacity > MAX_CAPACITY_FRAMES {
        return Err(LayoutError::BadCapacity(capacity));
    }
    let expected = ring_bytes(capacity);
    if len != expected {
        return Err(LayoutError::LengthMismatch { expected, actual: len });
    }
    Ok(Layout {
        base,
        capacity: capacity as u64,
    })
}

/// Writing side. Keeps its own write index; reads the consumer's index only to bound free space.
pub struct Producer {
    layout: Layout,
    write: u64,
}

/// Reading side. Keeps its own read index; reads the producer's index only to bound data.
pub struct Consumer {
    layout: Layout,
    read: u64,
}

impl Producer {
    /// Start writing where the published write index says (a fresh ring: 0).
    pub fn new(layout: Layout) -> Self {
        let write = layout.header().write_index.load(Ordering::Relaxed);
        Self { layout, write }
    }

    /// Frames that can be written now. A consumer index that makes no sense (ahead of us, or
    /// more than a capacity behind) counts as a full ring: nothing is written.
    pub fn free(&self) -> usize {
        let read = self.layout.header().read_index.load(Ordering::Acquire);
        let used = self.write.wrapping_sub(read);
        if used > self.layout.capacity {
            0
        } else {
            (self.layout.capacity - used) as usize
        }
    }

    /// Frames in the ring as this side sees it (0 when the consumer's index is bogus).
    pub fn fill(&self) -> usize {
        self.layout.capacity as usize - self.free()
    }

    /// Write as much of `frames` as fits; the rest is dropped and counted. Returns frames written.
    pub fn write(&mut self, frames: &[Frame]) -> usize {
        let n = frames.len().min(self.free());
        copy_in(&self.layout, self.write, &frames[..n]);
        self.write = self.write.wrapping_add(n as u64);
        let h = self.layout.header();
        h.write_index.store(self.write, Ordering::Release);
        if n < frames.len() {
            h.overruns.fetch_add((frames.len() - n) as u64, Ordering::Relaxed);
        }
        n
    }
}

impl Consumer {
    /// Start reading where the published read index says (a fresh ring: 0).
    pub fn new(layout: Layout) -> Self {
        let read = layout.header().read_index.load(Ordering::Relaxed);
        Self { layout, read }
    }

    /// Frames available now. A producer index that makes no sense (behind us, or more than a
    /// capacity ahead) means the ring was tampered with: skip to it and report nothing.
    pub fn available(&mut self) -> usize {
        let write = self.layout.header().write_index.load(Ordering::Acquire);
        let avail = write.wrapping_sub(self.read);
        if avail > self.layout.capacity {
            self.read = write;
            self.layout.header().read_index.store(self.read, Ordering::Release);
            return 0;
        }
        avail as usize
    }

    /// Read up to `out.len()` frames. Returns how many; the caller decides about the rest.
    pub fn read(&mut self, out: &mut [Frame]) -> usize {
        let n = out.len().min(self.available());
        copy_out(&self.layout, self.read, &mut out[..n]);
        self.read = self.read.wrapping_add(n as u64);
        self.layout.header().read_index.store(self.read, Ordering::Release);
        n
    }

    /// Fill `out` completely: ring data first, silence for the rest (counted as underrun).
    pub fn read_or_silence(&mut self, out: &mut [Frame]) -> usize {
        let n = self.read(out);
        if n < out.len() {
            out[n..].fill([0.0; 2]);
            self.layout
                .header()
                .underruns
                .fetch_add((out.len() - n) as u64, Ordering::Relaxed);
        }
        n
    }
}

// Both handles own raw pointers into memory the other side shares; each is used by one thread.
unsafe impl Send for Producer {}
unsafe impl Send for Consumer {}

/// Copy `frames` into the ring starting at absolute frame index `at` (wrapping).
fn copy_in(layout: &Layout, at: u64, frames: &[Frame]) {
    let cap = layout.capacity;
    let start = (at % cap) as usize;
    let first = frames.len().min(cap as usize - start);
    // SAFETY: `start + first <= cap` and `frames.len() - first <= start`; the data area holds
    // `cap` frames of 2 f32 each; Frame is 2 contiguous f32.
    unsafe {
        let data = layout.data();
        core::ptr::copy_nonoverlapping(frames.as_ptr() as *const f32, data.add(start * 2), first * 2);
        core::ptr::copy_nonoverlapping(frames[first..].as_ptr() as *const f32, data, (frames.len() - first) * 2);
    }
}

/// Copy `out.len()` frames out of the ring starting at absolute frame index `at` (wrapping).
fn copy_out(layout: &Layout, at: u64, out: &mut [Frame]) {
    let cap = layout.capacity;
    let start = (at % cap) as usize;
    let first = out.len().min(cap as usize - start);
    // SAFETY: as in `copy_in`.
    unsafe {
        let data = layout.data();
        core::ptr::copy_nonoverlapping(data.add(start * 2), out.as_mut_ptr() as *mut f32, first * 2);
        core::ptr::copy_nonoverlapping(data, out[first..].as_mut_ptr() as *mut f32, (out.len() - first) * 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;
    use std::vec::Vec;

    /// A ring in a 64-byte-aligned heap buffer.
    struct Mem(Vec<Header>, usize);

    impl Mem {
        fn new(capacity: u32) -> Self {
            let bytes = ring_bytes(capacity);
            let headers = bytes.div_ceil(HEADER_BYTES);
            let mut v = Vec::with_capacity(headers);
            // SAFETY: Header is plain atomics and padding; all-zero is a valid value.
            v.extend((0..headers).map(|_| unsafe { core::mem::zeroed::<Header>() }));
            Mem(v, bytes)
        }
        fn ptr(&mut self) -> *mut u8 {
            self.0.as_mut_ptr() as *mut u8
        }
        fn header(&mut self) -> &Header {
            &self.0[0]
        }
    }

    fn ring(capacity: u32) -> (Mem, Producer, Consumer) {
        let mut m = Mem::new(capacity);
        let (p, len) = (m.ptr(), m.1);
        let layout = unsafe { init(p, len, capacity) }.unwrap();
        let attached = unsafe { attach(p, len) }.unwrap();
        assert_eq!(layout, attached);
        (m, Producer::new(layout), Consumer::new(attached))
    }

    fn frames(r: core::ops::Range<u32>) -> Vec<Frame> {
        r.map(|i| [i as f32, -(i as f32)]).collect()
    }

    #[test]
    fn round_trip_with_wraparound() {
        let (_m, mut p, mut c) = ring(8);
        let mut out = vec![[0.0; 2]; 5];
        for k in 0..10 {
            let input = frames(k * 5..k * 5 + 5);
            assert_eq!(p.write(&input), 5);
            assert_eq!(c.read(&mut out), 5);
            assert_eq!(out, input, "pass {k}");
        }
    }

    #[test]
    fn full_drops_and_counts_empty_pads_and_counts() {
        let (mut m, mut p, mut c) = ring(4);
        assert_eq!(p.write(&frames(0..6)), 4);
        let mut out = vec![[9.0; 2]; 6];
        assert_eq!(c.read_or_silence(&mut out), 4);
        assert_eq!(&out[..4], &frames(0..4)[..]);
        assert_eq!(&out[4..], &[[0.0; 2]; 2]);
        let h = m.header();
        assert_eq!(h.overruns.load(Ordering::Relaxed), 2);
        assert_eq!(h.underruns.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn attach_rejects_bad_headers() {
        let mut m = Mem::new(16);
        let (p, len) = (m.ptr(), m.1);
        assert_eq!(unsafe { attach(p, len) }, Err(LayoutError::BadMagic(0)));
        unsafe { init(p, len, 16) }.unwrap();
        let h = m.header();
        h.capacity_frames.store(17, Ordering::Relaxed);
        assert_eq!(
            unsafe { attach(p, len) },
            Err(LayoutError::LengthMismatch {
                expected: ring_bytes(17),
                actual: len
            })
        );
        h.capacity_frames.store(MAX_CAPACITY_FRAMES + 1, Ordering::Relaxed);
        assert_eq!(
            unsafe { attach(p, len) },
            Err(LayoutError::BadCapacity(MAX_CAPACITY_FRAMES + 1))
        );
        h.capacity_frames.store(16, Ordering::Relaxed);
        h.channels.store(6, Ordering::Relaxed);
        assert_eq!(unsafe { attach(p, len) }, Err(LayoutError::Unsupported));
        h.channels.store(2, Ordering::Relaxed);
        h.version.store(99, Ordering::Relaxed);
        assert_eq!(unsafe { attach(p, len) }, Err(LayoutError::BadVersion(99)));
        assert_eq!(
            unsafe { attach(core::ptr::null_mut(), len) },
            Err(LayoutError::BadBuffer)
        );
        assert_eq!(unsafe { attach(p.wrapping_add(4), len) }, Err(LayoutError::BadBuffer));
        assert_eq!(unsafe { init(p, len, 0) }, Err(LayoutError::BadCapacity(0)));
    }

    #[test]
    fn capacity_is_snapshotted_at_attach() {
        let (mut m, mut p, mut c) = ring(8);
        // Someone shrinks the header's capacity after attach: the handles must ignore it.
        m.header().capacity_frames.store(2, Ordering::Relaxed);
        assert_eq!(p.write(&frames(0..8)), 8);
        let mut out = vec![[0.0; 2]; 8];
        assert_eq!(c.read(&mut out), 8);
        assert_eq!(out, frames(0..8));
    }

    #[test]
    fn hostile_indices_never_go_out_of_bounds() {
        let (mut m, mut p, mut c) = ring(8);
        let mut out = vec![[7.0; 2]; 64];
        for &bogus in &[u64::MAX, u64::MAX / 2, 9, 1 << 40, 3] {
            // Producer index far ahead / behind: the consumer resyncs and reads nothing.
            m.header().write_index.store(bogus, Ordering::Relaxed);
            let n = c.read(&mut out);
            assert!(n <= 8, "read {n} frames from an 8-frame ring");
            // Consumer index nonsense: the producer treats the ring as full or bounded.
            m.header().read_index.store(bogus.wrapping_add(1000), Ordering::Relaxed);
            assert!(p.free() <= 8);
            assert!(p.write(&frames(0..64)) <= 8);
        }
    }

    #[test]
    fn producer_and_consumer_on_two_threads() {
        let (_m, mut p, mut c) = ring(256);
        const TOTAL: u32 = 200_000;
        let writer = std::thread::spawn(move || {
            let mut next = 0u32;
            while next < TOTAL {
                let chunk = frames(next..(next + 37).min(TOTAL));
                next += p.write(&chunk) as u32;
            }
        });
        let mut expect = 0u32;
        let mut out = vec![[0.0; 2]; 53];
        while expect < TOTAL {
            let n = c.read(&mut out);
            for f in &out[..n] {
                assert_eq!(*f, [expect as f32, -(expect as f32)], "frame {expect}");
                expect += 1;
            }
        }
        writer.join().unwrap();
    }
}
