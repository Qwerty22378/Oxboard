//! SPSC frame rings between device threads, plus the fill policy on each end.
//!
//! The writer never blocks: if the ring is full, new frames are dropped and counted.
//! The reader keeps the ring near a target fill:
//! - **Priming:** after start or an underrun, output silence until the ring holds
//!   `target` frames, so one late capture packet doesn't cause a run of tiny underruns.
//! - **Trimming:** if the fill climbs past `max`, skip frames back down to `target`. A safety
//!   net only: the drift resamplers (`clock::drift`) keep the fill near the target.
//!
//! **Stamps (E6):** the device thread on each ring records its running frame count and the
//! time ([`RingWriter::stamp`] / [`RingReader::stamp`]). The mix side uses them to estimate
//! the fill *including frames the device has produced or consumed since its last packet*
//! ([`RingReader::estimated_fill`] / [`RingWriter::estimated_fill`]). That removes the
//! sawtooth of bursty device periods, which otherwise aliases into a slow wobble of the
//! drift controller.

use crate::Frame;
use crate::rt::stats::StreamStats;
use rtrb::{Consumer, Producer, RingBuffer};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Acquire, Ordering::Relaxed, Ordering::Release};

/// Nanoseconds on a process-wide monotonic clock, shared by device threads and the mix.
pub fn now_ns() -> u64 {
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    EPOCH.get_or_init(std::time::Instant::now).elapsed().as_nanos() as u64
}

/// A (frame count, time) pair published by one thread and read by another, without locks
/// (a seqlock: the writer bumps `seq` to odd, writes, bumps it to even).
#[derive(Debug, Default)]
pub struct Stamp {
    seq: AtomicU64,
    frames: AtomicU64,
    ns: AtomicU64,
}

impl Stamp {
    pub fn record(&self, frames: u64, ns: u64) {
        let s = self.seq.load(Relaxed);
        self.seq.store(s.wrapping_add(1), Relaxed);
        std::sync::atomic::fence(Release);
        self.frames.store(frames, Relaxed);
        self.ns.store(ns, Relaxed);
        self.seq.store(s.wrapping_add(2), Release);
    }

    /// Withdraw the stamp (the count isn't advancing at the nominal rate right now).
    pub fn clear(&self) {
        self.seq.store(0, Release);
    }

    /// The last consistent pair, or `None` if never recorded (or mid-write after retries).
    pub fn load(&self) -> Option<(u64, u64)> {
        for _ in 0..4 {
            let s1 = self.seq.load(Acquire);
            if s1 == 0 {
                return None;
            }
            let (f, t) = (self.frames.load(Relaxed), self.ns.load(Relaxed));
            std::sync::atomic::fence(Acquire);
            if s1 % 2 == 0 && self.seq.load(Relaxed) == s1 {
                return Some((f, t));
            }
        }
        None
    }
}

/// Both stamps of one ring.
#[derive(Debug, Default)]
pub struct Stamps {
    /// Frames written so far, by the producer's device thread.
    pub written: Stamp,
    /// Frames read so far, by the consumer's device thread.
    pub read: Stamp,
}

/// Frames a device moves in `ns` at the nominal rate.
fn frames_in(ns: i64) -> i64 {
    ns.max(0) * crate::SAMPLE_RATE as i64 / 1_000_000_000
}

/// Fill levels for a ring between two bursty sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingSizing {
    /// Start from (and trim back to) this fill.
    pub target: usize,
    /// Trim when the fill passes this (clock drift until E6 corrects it smoothly).
    pub max: usize,
    /// Ring size: room for a long stall on the reading side.
    pub capacity: usize,
}

impl RingSizing {
    /// The fill swings by the producer's burst plus the consumer's burst, so that is the
    /// lowest target that can't underrun from scheduling alone. Bursts are in frames: a
    /// WASAPI device moves one period per wake-up; the mix moves the master period rounded up
    /// to whole blocks (or one block on the timer clock).
    pub fn for_bursts(producer: usize, consumer: usize) -> Self {
        let target = producer + consumer;
        let max = target + crate::ms_to_frames(30.0);
        Self {
            target,
            max,
            capacity: (max + crate::ms_to_frames(200.0)).next_power_of_two(),
        }
    }

    /// A ring of this size, with a reader using these levels. Both ends know the target, so
    /// whichever end the mix holds can steer toward it.
    pub fn ring(self) -> (RingWriter, RingReader) {
        let (mut w, c) = ring(self.capacity);
        w.target = self.target;
        let r = RingReader::new(c, self.target, self.max).with_stamps(w.stamps.clone());
        (w, r)
    }
}

/// Create a ring holding up to `capacity` frames.
/// The writer has target 0 (no drift correction); [`RingSizing::ring`] sets one.
pub fn ring(capacity: usize) -> (RingWriter, Consumer<Frame>) {
    let (producer, consumer) = RingBuffer::new(capacity);
    (
        RingWriter {
            producer,
            target: 0,
            total: 0,
            stamps: Arc::default(),
        },
        consumer,
    )
}

/// Producer side: a capture thread, or the mix feeding a render thread.
pub struct RingWriter {
    producer: Producer<Frame>,
    target: usize,
    /// Frames written so far.
    total: u64,
    stamps: Arc<Stamps>,
}

impl RingWriter {
    /// Fill to steer toward when the mix is this end; 0 = same clock, no correction.
    pub fn target(&self) -> usize {
        self.target
    }

    pub fn fill(&self) -> usize {
        self.producer.buffer().capacity() - self.producer.slots()
    }

    /// Device thread: publish how much has been written, as of `ns`.
    pub fn stamp(&self, ns: u64) {
        self.stamps.written.record(self.total, ns);
    }

    /// Mix side (feeding a render thread): fill at `now`, counting frames the render device
    /// has consumed since its last stamp. `None` until the render side is reading (its
    /// priming phase says nothing about drift).
    pub fn estimated_fill(&self, now: u64) -> Option<usize> {
        let (read, at) = self.stamps.read.load()?;
        let consumed = read as i64 + frames_in(now as i64 - at as i64);
        Some((self.total as i64 - consumed).max(0) as usize)
    }

    /// Write `frames`, or silence of that length when `frames` is `None`. Returns how many
    /// frames didn't fit and were dropped.
    pub fn write(&mut self, frames: Option<&[Frame]>, len: usize, stats: &StreamStats) -> usize {
        stats.chunk(len);
        let fit = len.min(self.producer.slots());
        if fit > 0 {
            // `fit` <= slots, so this can't fail.
            if let Ok(chunk) = self.producer.write_chunk_uninit(fit) {
                match frames {
                    Some(f) => chunk.fill_from_iter(f[..fit].iter().copied()),
                    None => chunk.fill_from_iter(std::iter::repeat([0.0; 2])),
                };
            }
        }
        let dropped = len - fit;
        self.total += fit as u64;
        StreamStats::add(&stats.frames, fit);
        if dropped > 0 {
            StreamStats::add(&stats.xrun_frames, dropped);
            StreamStats::add(&stats.xruns, 1);
        }
        dropped
    }
}

/// Consumer side: used by a render thread.
pub struct RingReader {
    consumer: Consumer<Frame>,
    target: usize,
    max: usize,
    primed: bool,
    /// Frames taken out so far (read or trimmed).
    total: u64,
    stamps: Arc<Stamps>,
}

impl RingReader {
    /// `target`: fill to start from and trim back to. `max`: fill above which to trim.
    pub fn new(consumer: Consumer<Frame>, target: usize, max: usize) -> Self {
        assert!(
            target <= max && max <= consumer.buffer().capacity(),
            "need target <= max <= capacity"
        );
        Self {
            consumer,
            target,
            max,
            primed: false,
            total: 0,
            stamps: Arc::default(),
        }
    }

    /// Share stamps with the writer of the same ring.
    pub fn with_stamps(mut self, stamps: Arc<Stamps>) -> Self {
        self.stamps = stamps;
        self
    }

    pub fn fill(&self) -> usize {
        self.consumer.slots()
    }

    pub fn target(&self) -> usize {
        self.target
    }

    /// Past priming: reading real data.
    pub fn is_primed(&self) -> bool {
        self.primed
    }

    /// Device thread: publish how much has been read, as of `ns`. While priming the device
    /// consumes nothing from the ring, so extrapolating at the nominal rate would be wrong:
    /// the stamp is withdrawn and the mix falls back to the plain fill.
    pub fn stamp(&self, ns: u64) {
        if self.primed {
            self.stamps.read.record(self.total, ns);
        } else {
            self.stamps.read.clear();
        }
    }

    /// Mix side (fed by a capture thread): fill at `now`, counting frames the capture device
    /// has produced since its last stamp. `None` until the capture side has stamped, or while
    /// this side is still priming.
    pub fn estimated_fill(&self, now: u64) -> Option<usize> {
        if !self.primed {
            return None;
        }
        let (written, at) = self.stamps.written.load()?;
        let produced = written as i64 + frames_in(now as i64 - at as i64);
        Some((produced - self.total as i64).max(0) as usize)
    }

    /// Fill `out` completely: ring data first, silence for whatever is missing.
    pub fn read(&mut self, out: &mut [Frame], stats: &StreamStats) {
        stats.chunk(out.len());
        let mut fill = self.consumer.slots();
        stats.ring_fill.store(fill as u32, Relaxed);

        if !self.primed {
            if fill < self.target {
                out.fill([0.0; 2]);
                return;
            }
            self.primed = true;
        }

        if fill > self.max {
            let skip = fill - self.target;
            self.skip(skip);
            StreamStats::add(&stats.trimmed_frames, skip);
            fill = self.target;
        }

        let n = fill.min(out.len());
        if let Ok(chunk) = self.consumer.read_chunk(n) {
            let (a, b) = chunk.as_slices();
            out[..a.len()].copy_from_slice(a);
            out[a.len()..n].copy_from_slice(b);
            chunk.commit_all();
        }
        self.total += n as u64;
        StreamStats::add(&stats.frames, n);

        if n < out.len() {
            out[n..].fill([0.0; 2]);
            StreamStats::add(&stats.xrun_frames, out.len() - n);
            StreamStats::add(&stats.xruns, 1);
            self.primed = false;
        }
    }

    fn skip(&mut self, n: usize) {
        if let Ok(chunk) = self.consumer.read_chunk(n) {
            chunk.commit_all();
            self.total += n as u64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(range: std::ops::Range<usize>) -> Vec<Frame> {
        range.map(|i| [i as f32, -(i as f32)]).collect()
    }

    fn setup(capacity: usize, target: usize, max: usize) -> (RingWriter, RingReader, StreamStats, StreamStats) {
        let (w, c) = ring(capacity);
        (
            w,
            RingReader::new(c, target, max),
            StreamStats::default(),
            StreamStats::default(),
        )
    }

    #[test]
    fn passes_frames_through_in_order() {
        let (mut w, mut r, ws, rs) = setup(64, 4, 32);
        let input = frames(0..8);
        assert_eq!(w.write(Some(&input), 8, &ws), 0);
        let mut out = [[9.0; 2]; 8];
        r.read(&mut out, &rs);
        assert_eq!(out.to_vec(), input);
        let s = rs.snapshot();
        assert_eq!((s.frames, s.xruns, s.ring_fill), (8, 0, 8));
        assert_eq!(ws.snapshot().frames, 8);
    }

    #[test]
    fn writer_drops_and_counts_when_full() {
        let (mut w, _r, ws, _) = setup(4, 0, 4);
        assert_eq!(w.write(Some(&frames(0..6)), 6, &ws), 2);
        let s = ws.snapshot();
        assert_eq!((s.frames, s.xrun_frames, s.xruns), (4, 2, 1));
    }

    #[test]
    fn writer_writes_silence() {
        let (mut w, mut r, ws, rs) = setup(16, 0, 16);
        w.write(None, 3, &ws);
        let mut out = [[9.0; 2]; 3];
        r.read(&mut out, &rs);
        assert_eq!(out, [[0.0; 2]; 3]);
    }

    #[test]
    fn primes_before_playing() {
        let (mut w, mut r, ws, rs) = setup(64, 10, 32);
        w.write(Some(&frames(0..6)), 6, &ws);
        let mut out = [[9.0; 2]; 4];
        r.read(&mut out, &rs);
        assert_eq!(out, [[0.0; 2]; 4], "silence while below target");
        assert_eq!(r.fill(), 6, "nothing consumed while priming");
        assert_eq!(rs.snapshot().xruns, 0, "priming is not an underrun");

        w.write(Some(&frames(6..10)), 4, &ws);
        r.read(&mut out, &rs);
        assert_eq!(out.to_vec(), frames(0..4));
    }

    #[test]
    fn underrun_pads_with_silence_then_reprimes() {
        let (mut w, mut r, ws, rs) = setup(64, 4, 32);
        w.write(Some(&frames(0..6)), 6, &ws);
        let mut out = [[9.0; 2]; 8];
        r.read(&mut out, &rs);
        assert_eq!(out[..6].to_vec(), frames(0..6));
        assert_eq!(out[6..], [[0.0; 2]; 2]);
        let s = rs.snapshot();
        assert_eq!((s.xrun_frames, s.xruns), (2, 1));

        // Below target again: back to priming, not another underrun.
        w.write(Some(&frames(6..8)), 2, &ws);
        r.read(&mut out, &rs);
        assert_eq!(out, [[0.0; 2]; 8]);
        assert_eq!(rs.snapshot().xruns, 1);
    }

    #[test]
    fn trims_back_to_target_when_over_max() {
        let (mut w, mut r, ws, rs) = setup(64, 4, 10);
        w.write(Some(&frames(0..12)), 12, &ws);
        let mut out = [[9.0; 2]; 2];
        r.read(&mut out, &rs);
        // 12 > max 10: skip 8 oldest frames, leaving 4 (target), then read 2.
        assert_eq!(out.to_vec(), frames(8..10));
        assert_eq!(rs.snapshot().trimmed_frames, 8);
        assert_eq!(r.fill(), 2);
    }

    #[test]
    fn sizing_covers_both_bursts() {
        let s = RingSizing::for_bursts(480, 512);
        assert_eq!(s.target, 480 + 512);
        assert!(s.max > s.target && s.capacity >= s.max && s.capacity.is_power_of_two());
    }

    #[test]
    fn handles_wraparound() {
        let (mut w, mut r, ws, rs) = setup(8, 0, 8);
        let mut out = [[0.0; 2]; 5];
        w.write(Some(&frames(0..5)), 5, &ws);
        r.read(&mut out, &rs);
        w.write(Some(&frames(5..10)), 5, &ws); // wraps past the end of the buffer
        r.read(&mut out, &rs);
        assert_eq!(out.to_vec(), frames(5..10));
    }
}
