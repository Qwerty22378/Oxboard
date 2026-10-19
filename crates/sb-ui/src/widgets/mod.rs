//! The overlay's controls (`UI Design` → Live controls). Each is a plain egui widget: it draws
//! from the value it's given and reports what the user did. The only state it keeps is its
//! own (a drag in progress, a peak being held) in egui's memory, never the controller's.
//!
//! - [`fader`]: the taper, relative drag (Shift: fine), wheel ±1 dB, double-click 0 dB, and an
//!   optimistic value while dragging.
//! - [`meter`]: stereo peak + RMS on the fader's scale, peak hold and fall, clip light.
//! - [`pad`]: a clip's button: click plays, right-click for settings; a ring spins while it plays.
//! - [`cell`]: a matrix cell: click toggles, drag sets the gain, double-click to type dB.

pub mod cell;
pub mod fader;
pub mod meter;
pub mod pad;

use egui::Color32;

pub const ACCENT: Color32 = Color32::from_rgb(90, 170, 255);
pub const TRACK: Color32 = Color32::from_gray(48);
pub const DIM: Color32 = Color32::from_gray(140);
pub const OFF: Color32 = Color32::from_gray(34);

/// How long a released fader keeps showing its own value, waiting for the controller's echo.
pub const ECHO_GRACE: f64 = 0.4;
