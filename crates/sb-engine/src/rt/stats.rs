//! Per-stream counters, written by the real-time thread and read by anyone.
//! Relaxed atomics: each value is independent and only used for reporting.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};

#[derive(Debug, Default)]
pub struct StreamStats {
    /// Frames moved between the device and the ring.
    pub frames: AtomicU64,
    /// Capture: frames lost because the ring was full. Render: frames of silence
    /// inserted because the ring ran dry.
    pub xrun_frames: AtomicU64,
    /// Number of separate xrun events (a burst counts once).
    pub xruns: AtomicU64,
    /// Render: frames skipped to pull the ring back to its target fill (clock drift, no
    /// resampler yet).
    pub trimmed_frames: AtomicU64,
    /// Capture: packets the device flagged as discontinuous (its own glitches).
    pub discontinuities: AtomicU64,
    /// Render: ring fill seen at the last device callback, in frames.
    pub ring_fill: AtomicU32,
    /// Largest single device request (render) or packet (capture), in frames. A value
    /// well above the period means the thread woke late.
    pub max_chunk: AtomicU32,
    /// Whether the thread got its MMCSS "Pro Audio" registration.
    pub mmcss: AtomicBool,
}

/// Plain copy of [`StreamStats`] for printing and diffing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StatsSnapshot {
    pub frames: u64,
    pub xrun_frames: u64,
    pub xruns: u64,
    pub trimmed_frames: u64,
    pub discontinuities: u64,
    pub ring_fill: u32,
    pub max_chunk: u32,
    pub mmcss: bool,
}

impl StreamStats {
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            frames: self.frames.load(Relaxed),
            xrun_frames: self.xrun_frames.load(Relaxed),
            xruns: self.xruns.load(Relaxed),
            trimmed_frames: self.trimmed_frames.load(Relaxed),
            discontinuities: self.discontinuities.load(Relaxed),
            ring_fill: self.ring_fill.load(Relaxed),
            max_chunk: self.max_chunk.load(Relaxed),
            mmcss: self.mmcss.load(Relaxed),
        }
    }

    /// Record a device chunk size (only the RT thread writes, so load+store is enough).
    pub(crate) fn chunk(&self, n: usize) {
        if n as u32 > self.max_chunk.load(Relaxed) {
            self.max_chunk.store(n as u32, Relaxed);
        }
    }

    pub(crate) fn add(counter: &AtomicU64, n: usize) {
        if n > 0 {
            counter.fetch_add(n as u64, Relaxed);
        }
    }
}
