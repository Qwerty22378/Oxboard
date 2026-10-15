//! Parameters: a fixed-size table of atomic `f32` targets, and the per-op smoothers that
//! follow them on the audio thread.
//!
//! The controller writes targets ([`ParamTable::set`]) from any thread. The mix thread reads
//! each target once per block and ramps to it linearly over [`RAMP_FRAMES`], so a fader move
//! never clicks and never touches the plan.

use crate::graph::spec::ParamId;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

/// Ramp length for every param change: 10 ms at 48 kHz.
pub const RAMP_FRAMES: u32 = 480;

/// Fade length when an edge appears or disappears: 5 ms at 48 kHz.
pub const EDGE_FADE_FRAMES: u32 = 240;

/// Param targets, allocated once. Ids index it directly, so the controller hands out dense ids.
pub struct ParamTable {
    slots: Box<[AtomicU32]>,
}

impl ParamTable {
    /// `capacity` slots, all 0.0.
    pub fn new(capacity: usize) -> Self {
        Self {
            slots: (0..capacity).map(|_| AtomicU32::new(0.0f32.to_bits())).collect(),
        }
    }

    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// Set a target. Returns `false` (and does nothing) for an out-of-range id or a
    /// non-finite value, so a controller bug can't put NaN into the mix.
    pub fn set(&self, id: ParamId, value: f32) -> bool {
        match self.slots.get(id.0 as usize) {
            Some(slot) if value.is_finite() => {
                slot.store(value.to_bits(), Relaxed);
                true
            }
            _ => false,
        }
    }

    /// Current target; 0.0 for an out-of-range id.
    pub fn get(&self, id: ParamId) -> f32 {
        self.slots
            .get(id.0 as usize)
            .map_or(0.0, |s| f32::from_bits(s.load(Relaxed)))
    }
}

/// Linear ramp from the current value to the latest target. Owned by one op; lives in the plan.
/// Also used for edge fades, with a shorter ramp.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Smoother {
    current: f32,
    target: f32,
    step: f32,
    remaining: u32,
    ramp: u32,
}

impl Smoother {
    /// Start settled at `value`: a new plan continues from where the param is, without a fade.
    pub fn new(value: f32) -> Self {
        Self::with_ramp(value, RAMP_FRAMES)
    }

    /// Settled at `value`, ramping over `ramp` frames on each retarget.
    pub fn with_ramp(value: f32, ramp: u32) -> Self {
        Self {
            current: value,
            target: value,
            step: 0.0,
            remaining: 0,
            ramp: ramp.max(1),
        }
    }

    /// Point at a new target (once per block). Retargeting mid-ramp continues from the
    /// current value, so there is never a jump.
    pub fn set_target(&mut self, target: f32) {
        if target != self.target {
            self.target = target;
            self.remaining = self.ramp;
            self.step = (target - self.current) / self.ramp as f32;
        }
    }

    /// Still ramping? When not, callers can use [`Self::value`] for the whole block.
    pub fn is_ramping(&self) -> bool {
        self.remaining > 0
    }

    pub fn value(&self) -> f32 {
        self.current
    }

    /// Advance one sample and return the gain for it.
    #[inline]
    pub fn tick(&mut self) -> f32 {
        if self.remaining > 0 {
            self.remaining -= 1;
            // Land exactly on the target, whatever float rounding did on the way.
            self.current = if self.remaining == 0 {
                self.target
            } else {
                self.current + self.step
            };
        }
        self.current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_set_get_and_rejects() {
        let t = ParamTable::new(4);
        assert!(t.set(ParamId(2), 0.5));
        assert_eq!(t.get(ParamId(2)), 0.5);
        assert!(!t.set(ParamId(4), 1.0), "out of range");
        assert!(!t.set(ParamId(1), f32::NAN), "non-finite");
        assert_eq!(t.get(ParamId(1)), 0.0);
        assert_eq!(t.get(ParamId(99)), 0.0);
    }

    #[test]
    fn settled_smoother_is_constant() {
        let mut s = Smoother::new(0.7);
        s.set_target(0.7);
        assert!(!s.is_ramping());
        assert!((0..1000).all(|_| s.tick() == 0.7));
    }

    #[test]
    fn ramps_linearly_and_lands_exactly() {
        let mut s = Smoother::new(0.0);
        s.set_target(1.0);
        let v: Vec<f32> = (0..RAMP_FRAMES).map(|_| s.tick()).collect();
        assert!((v[0] - 1.0 / RAMP_FRAMES as f32).abs() < 1e-6);
        assert!(v.windows(2).all(|w| w[1] > w[0]), "monotonic");
        assert_eq!(*v.last().unwrap(), 1.0);
        assert!(!s.is_ramping());
        assert_eq!(s.tick(), 1.0);
    }

    #[test]
    fn retarget_mid_ramp_has_no_jump() {
        let mut s = Smoother::new(0.0);
        s.set_target(1.0);
        let mut last = 0.0;
        for _ in 0..RAMP_FRAMES / 2 {
            last = s.tick();
        }
        s.set_target(0.0);
        let next = s.tick();
        let max_step = 1.0 / RAMP_FRAMES as f32 + 1e-6;
        assert!((next - last).abs() <= max_step, "jumped from {last} to {next}");
        assert!(next < last, "now heading down");
    }
}
