//! The window the overlay lives in: our own layered Win32 popup, drawn on the CPU (the U1
//! decision). Everything above it (views, store) only sees [`Content`], so the shell can be
//! swapped without touching them.
//!
//! - Created once at startup, hidden. Hidden, its thread blocks in `GetMessageW`: no frames,
//!   no timers, no CPU.
//! - Shown on the monitor of the window in front (the game's), at that monitor's DPI, after
//!   its first frame is drawn (no empty flash). It takes the foreground so the cursor is free.
//! - Hidden by the hotkey, Esc (via [`Content::ui`]) or a click outside (focus lost); the
//!   foreground goes back to the window that had it.

pub mod dialog;
pub mod raster;
mod win32;

pub use win32::{Options, Shell, Waker};

/// What the shell shows.
pub trait Content {
    /// The panel's size in logical pixels.
    fn size(&self) -> (f32, f32);
    /// Something arrived (a [`Waker::wake`]): take it in. Runs shown or hidden. Returns
    /// whether anything on screen changed (a frame is drawn only then).
    fn update(&mut self) -> bool;
    /// Draw a frame. Returns `true` to hide.
    fn ui(&mut self, ui: &mut egui::Ui) -> bool;
    /// The overlay was shown or hidden.
    fn visibility(&mut self, shown: bool);
}
