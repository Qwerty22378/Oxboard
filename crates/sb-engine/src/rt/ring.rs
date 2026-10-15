//! SPSC frame rings between device threads, plus the fill policy on each end.
//!
//! The writer never blocks: if the ring is full, new frames are dropped and counted.
//! The reader keeps the ring near a target fill:
//! - **Priming:** after start or an underrun, output silence until the ring holds
//!   `target` frames, so one late capture packet doesn't cause a run of tiny underruns.
//! - **Trimming:** if the fill climbs past `max` (the capture clock runs faster than the
//!   render clock), skip frames back down to `target`. This bounds latency until the drift
//!   resamplers (E6) replace it with a smooth correction.

use crate::Frame;
use crate::rt::stats::StreamStats;
use rtrb::{Consumer, Producer, RingBuffer};
use std::sync::atomic::Ordering::Relaxed;

/// Create a ring holding up to `capacity` frames.
pub fn ring(capacity: usize) -> (RingWriter, Consumer<Frame>) {
    let (producer, consumer) = RingBuffer::new(capacity);
    (RingWriter { producer }, consumer)
}

/// Producer side: used by a capture thread.
pub struct RingWriter {
    producer: Producer<Frame>,
}

impl RingWriter {
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
        }
    }

    pub fn fill(&self) -> usize {
        self.consumer.slots()
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
