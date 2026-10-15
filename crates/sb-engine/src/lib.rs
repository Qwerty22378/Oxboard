//! Stateless real-time audio engine. See the Engine Notes in the plan for the design.
//!
//! Current state (milestone E2): device threads move audio between WASAPI and SPSC rings;
//! one mix thread runs a hand-written render plan in fixed blocks, woken by the master clock
//! (the master output device's event, or a timer). No graph compiler, params or drift
//! correction yet.

pub mod clock;
pub mod graph;
pub mod rt;

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
