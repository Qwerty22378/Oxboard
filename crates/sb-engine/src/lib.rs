//! Stateless real-time audio engine. See the Engine Notes in the plan for the design.
//!
//! Current state (milestone E6): the controller-facing [`engine::Engine`] compiles each
//! `GraphSpec` into a render plan and swaps it into the running mix thread glitch-free (state
//! carried over by key, edges fading in and out). Params are live and smoothed; meters read
//! peak and RMS. Device threads move audio between WASAPI and SPSC rings; the mix thread runs
//! in fixed blocks, woken by the master device's event or a timer, and every device ring is
//! drift-corrected (sinc resampler + PI controller on timestamped fill estimates).

pub mod clock;
pub mod engine;
pub mod graph;
pub mod rt;

#[cfg(test)]
mod sim;
#[cfg(test)]
pub(crate) mod testing;

#[cfg(windows)]
pub mod backend;

/// Internal sample rate. Devices at other rates are converted at the edge.
pub const SAMPLE_RATE: u32 = 48_000;

/// One interleaved stereo frame, the unit carried by device rings.
pub type Frame = [f32; 2];

/// Frames → milliseconds at [`SAMPLE_RATE`].
pub fn frames_to_ms(frames: usize) -> f64 {
    frames as f64 * 1000.0 / SAMPLE_RATE as f64
}

/// Milliseconds → frames at [`SAMPLE_RATE`].
pub fn ms_to_frames(ms: f64) -> usize {
    (ms * SAMPLE_RATE as f64 / 1000.0).round() as usize
}
