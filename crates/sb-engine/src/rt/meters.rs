//! Meters: peak and RMS per channel, computed on the mix thread, read from anywhere.
//!
//! - **Peak** is the maximum |sample| since the last read. The mix thread merges with an
//!   atomic max on the float's bits (valid because non-negative floats order like their bits);
//!   [`MeterTable::read`] takes it and resets it to 0, so no peak between reads is lost.
//! - **RMS** is a running mean square with a ~300 ms time constant (VU-like), kept in the plan
//!   ([`RmsState`]) and published once per block.
//!
//! Values are linear (1.0 = full scale). Converting to dB and peak hold are UI concerns.

use crate::SAMPLE_RATE;
use crate::graph::spec::MeterId;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

/// RMS averaging time constant.
pub const RMS_TAU_SECONDS: f32 = 0.3;

#[derive(Debug, Default)]
struct Slot {
    peak: [AtomicU32; 2],
    rms: [AtomicU32; 2],
}

/// One read of a meter: linear peak (since the previous read) and RMS, left and right.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MeterReading {
    pub peak: [f32; 2],
    pub rms: [f32; 2],
}

/// Meter values, allocated once. Ids index it directly.
pub struct MeterTable {
    slots: Box<[Slot]>,
}

impl MeterTable {
    pub fn new(capacity: usize) -> Self {
        Self {
            slots: (0..capacity).map(|_| Slot::default()).collect(),
        }
    }

    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// Take the peak (resetting it) and the current RMS. `None` for an out-of-range id.
    pub fn read(&self, id: MeterId) -> Option<MeterReading> {
        let slot = self.slots.get(id.0 as usize)?;
        let load = |a: &AtomicU32| f32::from_bits(a.load(Relaxed));
        Some(MeterReading {
            peak: [0, 1].map(|c| f32::from_bits(slot.peak[c].swap(0, Relaxed))),
            rms: [0, 1].map(|c| load(&slot.rms[c])),
        })
    }

    /// Mix thread: measure one block of `channels` into meter `id`.
    pub(crate) fn measure(&self, id: MeterId, channels: [&[f32]; 2], state: &mut RmsState) {
        let Some(slot) = self.slots.get(id.0 as usize) else {
            return;
        };
        for (c, samples) in channels.iter().enumerate() {
            let mut peak = 0.0f32;
            let mut ms = state.mean_square[c];
            for &x in *samples {
                peak = peak.max(x.abs()); // max() ignores NaN
                ms += state.coeff * (x * x - ms);
            }
            // A NaN or infinite sample would poison the average forever; restart it instead.
            if !ms.is_finite() {
                ms = 0.0;
            }
            state.mean_square[c] = ms;
            slot.peak[c].fetch_max(peak.to_bits(), Relaxed);
            slot.rms[c].store(ms.sqrt().to_bits(), Relaxed);
        }
    }
}

/// Per-meter running mean square, owned by the plan.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RmsState {
    mean_square: [f32; 2],
    coeff: f32,
}

impl Default for RmsState {
    fn default() -> Self {
        let coeff = 1.0 - (-1.0 / (RMS_TAU_SECONDS * SAMPLE_RATE as f32)).exp();
        Self {
            mean_square: [0.0; 2],
            coeff,
        }
    }
}

/// Linear amplitude → dBFS (−∞ for silence). For display.
pub fn to_db(linear: f32) -> f32 {
    20.0 * linear.log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(n: usize, amp: f32) -> Vec<f32> {
        (0..n).map(|i| (i as f32 * 0.05).sin() * amp).collect()
    }

    #[test]
    fn peak_is_max_since_last_read() {
        let t = MeterTable::new(2);
        let mut st = RmsState::default();
        t.measure(MeterId(1), [&[0.1, -0.8, 0.3], &[0.2, 0.2, 0.2]], &mut st);
        t.measure(MeterId(1), [&[0.5; 3], &[0.1; 3]], &mut st);
        let r = t.read(MeterId(1)).unwrap();
        assert_eq!(r.peak, [0.8, 0.2]);
        assert_eq!(t.read(MeterId(1)).unwrap().peak, [0.0, 0.0], "read resets the peak");
        assert!(t.read(MeterId(2)).is_none());
    }

    #[test]
    fn rms_of_a_sine_settles_at_amp_over_sqrt2() {
        let t = MeterTable::new(1);
        let mut st = RmsState::default();
        let s = sine(48_000 * 3, 0.5); // 3 s ≫ 300 ms time constant
        for block in s.chunks(256) {
            t.measure(MeterId(0), [block, block], &mut st);
        }
        let r = t.read(MeterId(0)).unwrap();
        let want = 0.5 / 2f32.sqrt();
        assert!((r.rms[0] - want).abs() < 0.01, "rms {} vs {want}", r.rms[0]);
        assert!((r.peak[0] - 0.5).abs() < 0.01);
    }

    #[test]
    fn rms_decays_after_signal_stops() {
        let t = MeterTable::new(1);
        let mut st = RmsState::default();
        let loud = sine(48_000, 1.0);
        t.measure(MeterId(0), [&loud, &loud], &mut st);
        let before = t.read(MeterId(0)).unwrap().rms[0];
        let silence = vec![0.0; 48_000];
        t.measure(MeterId(0), [&silence, &silence], &mut st);
        let after = t.read(MeterId(0)).unwrap().rms[0];
        // Mean square decays by e^(-t/τ), so RMS by e^(-t/2τ): ≈ 0.19 after 1 s at τ = 0.3 s.
        let want = before * (-1.0 / (2.0 * RMS_TAU_SECONDS)).exp();
        assert!((after - want).abs() < 0.01, "{before} → {after}, want {want}");
    }

    #[test]
    fn nan_input_does_not_poison_the_meter() {
        let t = MeterTable::new(1);
        let mut st = RmsState::default();
        t.measure(MeterId(0), [&[f32::NAN, 0.5], &[0.0, 0.0]], &mut st);
        t.measure(MeterId(0), [&[0.25; 8], &[0.0; 8]], &mut st);
        let r = t.read(MeterId(0)).unwrap();
        assert!(r.rms[0].is_finite() && r.peak[0].is_finite());
    }
}
