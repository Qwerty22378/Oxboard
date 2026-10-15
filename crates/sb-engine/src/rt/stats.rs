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
    /// Mix side of a device ring: the drift correction in ppm (f32 bits).
    pub drift_ppm: AtomicU32,
    /// Whether the thread got its MMCSS "Pro Audio" registration.
    pub mmcss: AtomicBool,
}

/// Plain copy of [`StreamStats`] for printing and diffing.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct StatsSnapshot {
    pub frames: u64,
    pub xrun_frames: u64,
    pub xruns: u64,
    pub trimmed_frames: u64,
    pub discontinuities: u64,
    pub ring_fill: u32,
    pub max_chunk: u32,
    pub drift_ppm: f32,
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
            drift_ppm: f32::from_bits(self.drift_ppm.load(Relaxed)),
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

/// Mix-thread counters.
#[derive(Debug, Default)]
pub struct MixStats {
    pub blocks: AtomicU64,
    /// Total time spent rendering blocks, in ns. `busy_ns / (blocks × block duration)` is
    /// the mix thread's load.
    pub busy_ns: AtomicU64,
    /// Slowest single block, in ns.
    pub max_block_ns: AtomicU64,
    /// Timer clock only: blocks skipped because the thread fell too far behind to catch up.
    pub late_blocks: AtomicU64,
    /// Plans adopted (graph updates, including settle plans).
    pub swaps: AtomicU64,
    /// The master device failed and the timer clock took over.
    pub master_fallback: AtomicBool,
    /// Whether the thread got its MMCSS "Pro Audio" registration.
    pub mmcss: AtomicBool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MixSnapshot {
    pub blocks: u64,
    pub busy_ns: u64,
    pub max_block_ns: u64,
    pub late_blocks: u64,
    pub swaps: u64,
    pub master_fallback: bool,
    pub mmcss: bool,
}

impl MixStats {
    pub fn snapshot(&self) -> MixSnapshot {
        MixSnapshot {
            blocks: self.blocks.load(Relaxed),
            busy_ns: self.busy_ns.load(Relaxed),
            max_block_ns: self.max_block_ns.load(Relaxed),
            late_blocks: self.late_blocks.load(Relaxed),
            swaps: self.swaps.load(Relaxed),
            master_fallback: self.master_fallback.load(Relaxed),
            mmcss: self.mmcss.load(Relaxed),
        }
    }

    /// Record one rendered block (only the mix thread writes).
    pub(crate) fn block_done(&self, ns: u64) {
        self.blocks.fetch_add(1, Relaxed);
        self.busy_ns.fetch_add(ns, Relaxed);
        if ns > self.max_block_ns.load(Relaxed) {
            self.max_block_ns.store(ns, Relaxed);
        }
    }
}
