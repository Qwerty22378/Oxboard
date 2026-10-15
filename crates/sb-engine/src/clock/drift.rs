//! Drift correction between clock domains.
//!
//! Every device ticks at its own slightly different rate, so a ring between a device and the
//! mix slowly fills up or drains. A [`Drift`] sits on the mix side of each device ring: it
//! resamples by a ratio within ±[`MAX_PPM`] of 1, chosen by a PI controller that steers the
//! ring's (low-passed) fill toward its target.
//!
//! - **Input rings** (capture → mix): read a varying number of frames, produce exactly one
//!   block. More fill than wanted → read slightly more per block.
//! - **Output rings** (mix → render): take one block, produce a varying number of frames.
//!   More fill than wanted → produce slightly fewer per block.
//!
//! Either way, the resampler's step (input frames per output frame) is `1 + ppm·1e-6`.
//! The resampler is a 64-tap Kaiser-windowed sinc with a 512-phase table, linearly
//! interpolated between phases; every phase is normalized to unity DC gain.

use crate::{Frame, SAMPLE_RATE};
use std::sync::{Arc, OnceLock};

/// Filter length. Latency is about half of it (~0.7 ms).
pub const TAPS: usize = 64;
const HALF: usize = TAPS / 2;
const PHASES: usize = 512;
/// Cutoff in cycles per sample (0.5 = Nyquist): 21.6 kHz at 48 kHz.
const CUTOFF: f64 = 0.45;
const KAISER_BETA: f64 = 8.0;

/// Largest correction. Real hardware drifts well under 200 ppm; emulated devices in a VM can
/// exceed 1000. 2000 ppm is a pitch change of 3.5 cents at most.
pub const MAX_PPM: f64 = 2000.0;
/// Proportional gain: ppm per frame of (filtered) fill error.
const KP: f64 = 4.0;
/// Integral gain: ppm per frame of error per second.
const KI: f64 = 0.6;
/// Time constant of the fill low-pass. The fill is measured with timestamps (see
/// `rt::ring`), so this only has to smooth thread-wakeup jitter. Tuned in simulation over
/// ±1500 ppm drifts and 0.5–1 ms jitter: settles in ~70 s, ratio ripple ~15–30 ppm
/// (0.05 cents), fill never below the burst margin.
const FILL_TAU_S: f64 = 2.0;

/// Polyphase table: `PHASES + 1` rows of `TAPS` coefficients, shared by every resampler.
fn kernel() -> Arc<[f32]> {
    static TABLE: OnceLock<Arc<[f32]>> = OnceLock::new();
    TABLE
        .get_or_init(|| {
            let bessel_i0 = |x: f64| {
                // Power series; converges fast for the arguments used here.
                let (mut sum, mut term) = (1.0, 1.0);
                for k in 1..50 {
                    term *= (x / 2.0) * (x / 2.0) / (k * k) as f64;
                    sum += term;
                }
                sum
            };
            let norm = bessel_i0(KAISER_BETA);
            let mut t = Vec::with_capacity((PHASES + 1) * TAPS);
            for p in 0..=PHASES {
                let phase = p as f64 / PHASES as f64;
                let row: Vec<f64> = (0..TAPS)
                    .map(|i| {
                        let x = i as f64 - (HALF - 1) as f64 - phase; // distance from the output point
                        let arg = 2.0 * CUTOFF * x;
                        let sinc = if arg.abs() < 1e-12 {
                            1.0
                        } else {
                            (std::f64::consts::PI * arg).sin() / (std::f64::consts::PI * arg)
                        };
                        let w = (x / HALF as f64).clamp(-1.0, 1.0);
                        sinc * bessel_i0(KAISER_BETA * (1.0 - w * w).sqrt()) / norm
                    })
                    .collect();
                let sum: f64 = row.iter().sum();
                t.extend(row.iter().map(|c| (c / sum) as f32));
            }
            t.into()
        })
        .clone()
}

/// Streaming fractional-delay resampler, stereo planar. Allocation-free after `new`.
pub struct Resampler {
    kernel: Arc<[f32]>,
    buf: [Vec<f32>; 2],
    /// Position of the next output, in input frames, as an index into `buf`.
    pos: f64,
}

impl Resampler {
    /// `max_in`: the most input frames ever pushed at once.
    pub fn new(max_in: usize) -> Self {
        let cap = TAPS + max_in + 8;
        let history = || {
            let mut v = Vec::with_capacity(cap);
            v.resize(HALF - 1, 0.0);
            v
        };
        Self {
            kernel: kernel(),
            buf: [history(), history()],
            pos: (HALF - 1) as f64,
        }
    }

    fn len(&self) -> usize {
        self.buf[0].len()
    }

    /// Input frames to push before `n_out` outputs can be produced at `step`.
    pub fn needed(&self, n_out: usize, step: f64) -> usize {
        if n_out == 0 {
            return 0;
        }
        let last = self.pos + (n_out - 1) as f64 * step;
        (last.floor() as usize + HALF + 1).saturating_sub(self.len())
    }

    /// Append input. Anything beyond the preallocated capacity is dropped (never reallocates).
    pub fn push_frames(&mut self, frames: &[Frame]) {
        let room = self.buf[0].capacity() - self.len();
        for f in &frames[..frames.len().min(room)] {
            self.buf[0].push(f[0]);
            self.buf[1].push(f[1]);
        }
    }

    pub fn push_planar(&mut self, l: &[f32], r: &[f32]) {
        let n = l.len().min(r.len()).min(self.buf[0].capacity() - self.len());
        self.buf[0].extend_from_slice(&l[..n]);
        self.buf[1].extend_from_slice(&r[..n]);
    }

    /// Produce up to `max` outputs, as far as the buffered input allows. Calls `emit(i, l, r)`
    /// for each; returns how many.
    pub fn produce(&mut self, max: usize, step: f64, mut emit: impl FnMut(usize, f32, f32)) -> usize {
        let mut n = 0;
        while n < max && self.pos.floor() as usize + HALF < self.len() {
            let c0 = self.pos.floor() as usize;
            let phase = (self.pos - c0 as f64) * PHASES as f64;
            let p0 = (phase as usize).min(PHASES - 1);
            let t = (phase - p0 as f64) as f32;
            let (row0, row1) = (
                &self.kernel[p0 * TAPS..][..TAPS],
                &self.kernel[(p0 + 1) * TAPS..][..TAPS],
            );
            let start = c0 + 1 - HALF;
            let (l, r) = (&self.buf[0][start..start + TAPS], &self.buf[1][start..start + TAPS]);
            let (mut al, mut ar) = (0.0f32, 0.0f32);
            for i in 0..TAPS {
                let k = row0[i] + t * (row1[i] - row0[i]);
                al += k * l[i];
                ar += k * r[i];
            }
            emit(n, al, ar);
            self.pos += step;
            n += 1;
        }
        // Drop input no future output needs.
        let drop = (self.pos.floor() as usize + 1).saturating_sub(HALF).min(self.len());
        if drop > 0 {
            for ch in &mut self.buf {
                ch.copy_within(drop.., 0);
                ch.truncate(ch.len() - drop);
            }
            self.pos -= drop as f64;
        }
        n
    }
}

/// PI controller on a ring's fill, in ppm.
#[derive(Debug, Clone, Copy)]
pub struct Pi {
    filtered: Option<f64>,
    integral: f64,
    ppm: f64,
}

impl Default for Pi {
    fn default() -> Self {
        Self {
            filtered: None,
            integral: 0.0,
            ppm: 0.0,
        }
    }
}

impl Pi {
    /// Feed one fill measurement (frames), `dt` seconds after the previous one. Returns the
    /// correction in ppm: positive = the ring is too full, so move frames out of it faster.
    pub fn update(&mut self, fill: f64, target: f64, dt: f64) -> f64 {
        let f = match self.filtered {
            None => fill,
            Some(prev) => prev + (fill - prev) * (dt / FILL_TAU_S).min(1.0),
        };
        self.filtered = Some(f);
        let error = f - target;
        // Anti-windup: stop integrating while the output is pinned at the limit in the same
        // direction, so a startup transient doesn't leave the integral to unwind for minutes.
        let integral = (self.integral + KI * error * dt).clamp(-MAX_PPM, MAX_PPM);
        let unclamped = KP * error + integral;
        if unclamped.abs() <= MAX_PPM || unclamped.signum() != error.signum() {
            self.integral = integral;
        }
        self.ppm = (KP * error + self.integral).clamp(-MAX_PPM, MAX_PPM);
        self.ppm
    }

    pub fn ppm(&self) -> f64 {
        self.ppm
    }

    pub fn filtered_fill(&self) -> Option<f64> {
        self.filtered
    }
}

/// Resampler + controller for one device ring. Lives with the ring in the plan, so it carries
/// over plan swaps.
pub struct Drift {
    rs: Resampler,
    pi: Pi,
    target: usize,
    block: usize,
}

impl Drift {
    /// `target`: the ring fill to steer toward. `block`: the mix block size.
    pub fn new(target: usize, block: usize) -> Self {
        // Worst case per call: one block at the largest step, plus the filter's lookahead.
        let max_in = block + block * MAX_PPM as usize / 1_000_000 + TAPS + 2;
        Self {
            rs: Resampler::new(max_in),
            pi: Pi::default(),
            target,
            block,
        }
    }

    fn step(&self) -> f64 {
        1.0 + self.pi.ppm() * 1e-6
    }

    pub fn ppm(&self) -> f64 {
        self.pi.ppm()
    }

    pub fn target(&self) -> usize {
        self.target
    }

    /// Input side: how many frames to read from the ring for the next block.
    pub fn input_needed(&self) -> usize {
        self.rs.needed(self.block, self.step())
    }

    /// Most frames [`Self::input_needed`] or [`Self::push_block`] can ask for or produce.
    pub fn max_frames(block: usize) -> usize {
        block + block * MAX_PPM as usize / 1_000_000 + TAPS + 4
    }

    /// Input side: resample what was read into exactly one block.
    pub fn pull(&mut self, read: &[Frame], l: &mut [f32], r: &mut [f32]) {
        self.rs.push_frames(read);
        let step = self.step();
        let n = self.rs.produce(l.len(), step, |i, a, b| {
            l[i] = a;
            r[i] = b;
        });
        // Only short if the input was (it can't be, but never leave stale samples).
        l[n..].fill(0.0);
        r[n..].fill(0.0);
    }

    /// Output side: resample one block into `out`; returns frames produced.
    pub fn push_block(&mut self, l: &[f32], r: &[f32], out: &mut [Frame]) -> usize {
        self.rs.push_planar(l, r);
        let step = self.step();
        self.rs.produce(out.len(), step, |i, a, b| out[i] = [a, b])
    }

    /// Once per block, with the ring's fill as the mix side sees it.
    pub fn measure(&mut self, fill: usize) {
        let dt = self.block as f64 / SAMPLE_RATE as f64;
        self.pi.update(fill as f64, self.target as f64, dt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(n: usize, freq: f64, offset: f64) -> Vec<f32> {
        (0..n)
            .map(|i| ((i as f64 + offset) * freq / 48_000.0 * std::f64::consts::TAU).sin() as f32)
            .collect()
    }

    fn run(rs: &mut Resampler, input: &[f32], step: f64, out_len: usize) -> Vec<f32> {
        let mut out = Vec::new();
        let mut fed = 0;
        while out.len() < out_len {
            let need = rs.needed(64, step).min(input.len() - fed);
            let chunk: Vec<Frame> = input[fed..fed + need].iter().map(|&x| [x, x]).collect();
            rs.push_frames(&chunk);
            fed += need;
            let mut block = vec![0.0; 64];
            let n = rs.produce(64, step, |i, a, _| block[i] = a);
            out.extend_from_slice(&block[..n]);
            if need == 0 && n == 0 {
                break;
            }
        }
        out
    }

    #[test]
    fn dc_passes_exactly_at_any_ratio() {
        for step in [1.0, 1.0 + 1e-3, 1.0 - 2e-3] {
            let mut rs = Resampler::new(256);
            let out = run(&mut rs, &[0.5; 20_000], step, 4096);
            assert!(
                out[100..].iter().all(|&v| (v - 0.5).abs() < 1e-5),
                "step {step}: {:?}",
                &out[100..110]
            );
        }
    }

    #[test]
    fn sine_comes_out_clean_at_unity_with_a_fixed_delay() {
        let mut rs = Resampler::new(256);
        let input = sine(20_000, 1000.0, 0.0);
        let out = run(&mut rs, &input, 1.0, 8192);
        // The kernel is centred on the output point: output i is input i (no index shift),
        // though the filter needs HALF frames of lookahead before it can produce it.
        let want = sine(8192, 1000.0, 0.0);
        let err = out[200..]
            .iter()
            .zip(&want[200..])
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f32::max);
        assert!(err < 1e-3, "max error {err}");
    }

    #[test]
    fn resampled_sine_has_the_expected_frequency_and_low_distortion() {
        // step > 1: consume faster, so the output pitch rises by the same factor.
        let step = 1.0 + 1500e-6;
        let mut rs = Resampler::new(512);
        let input = sine(100_000, 5000.0, 0.0);
        let out = run(&mut rs, &input, step, 32_768);
        // Compare against the ideal: input position i·step.
        let mut worst = 0.0f32;
        for (i, &v) in out.iter().enumerate().skip(500) {
            let x = i as f64 * step;
            let ideal = (x * 5000.0 / 48_000.0 * std::f64::consts::TAU).sin() as f32;
            worst = worst.max((v - ideal).abs());
        }
        // −66 dB worst case at 5 kHz; far better at typical frequencies.
        assert!(worst < 5e-4, "worst error {worst}");
    }

    #[test]
    fn needed_matches_what_produce_consumes() {
        let mut rs = Resampler::new(512);
        let step = 1.0 - 700e-6;
        for _ in 0..1000 {
            let need = rs.needed(256, step);
            rs.push_frames(&vec![[0.1, 0.1]; need]);
            assert_eq!(rs.produce(256, step, |_, _, _| {}), 256, "exactly one block per call");
        }
    }

    #[test]
    fn pi_settles_on_a_constant_drift() {
        // Ring between a producer 800 ppm fast and a consumer corrected by the controller.
        let mut pi = Pi::default();
        let (dt, target) = (256.0 / 48_000.0, 1000.0);
        let mut fill = target;
        for _ in 0..(300.0 / dt) as usize {
            let ppm = pi.update(fill, target, dt);
            fill += 256.0 * (800e-6 - ppm * 1e-6); // in: 256·(1+800ppm), out: 256·(1+ppm)
        }
        assert!((pi.ppm() - 800.0).abs() < 5.0, "ppm {}", pi.ppm());
        assert!((fill - target).abs() < 5.0, "fill {fill}");
    }

    #[test]
    fn pi_recovers_after_saturating() {
        // The ring sits empty for 10 s (device stalled): the output pins at the limit. Then
        // the loop closes with no real drift; it must come back near 0 and stay there.
        let mut pi = Pi::default();
        let (dt, target) = (256.0 / 48_000.0, 1000.0);
        for _ in 0..(10.0 / dt) as usize {
            pi.update(0.0, target, dt);
        }
        assert_eq!(pi.ppm(), -MAX_PPM);
        let mut fill = 0.0;
        let mut settled_at = None;
        for k in 0..(300.0 / dt) as usize {
            let ppm = pi.update(fill, target, dt);
            fill += 256.0 * (-ppm * 1e-6) + 0.0; // no true drift: only the correction moves it
            fill = fill.max(0.0);
            if ppm.abs() < 20.0 {
                settled_at.get_or_insert(k as f64 * dt);
            } else {
                settled_at = None;
            }
        }
        let t = settled_at.expect("never settled");
        assert!(t < 150.0, "took {t:.0} s to settle");
    }

    #[test]
    fn pi_output_is_clamped() {
        let mut pi = Pi::default();
        for _ in 0..10_000 {
            pi.update(1e6, 0.0, 0.01);
        }
        assert_eq!(pi.ppm(), MAX_PPM);
    }
}
