//! `sb-ui`: the overlay. A pure view of the controller's state, shown over games on a hotkey;
//! it owns no authoritative state and never touches audio (see `UI Design`).
//!
//! - [`link`]: the controller connection, on its own thread, reconnecting by itself.
//! - [`log`]: `--log FILE`, since the overlay has no console.
//! - [`taper`]: fader position ↔ dB ↔ gain; [`coalesce`]: fader drags thinned to ~60 Hz.
//! - [`simple`]: Interface 1's rules ("clips → mic + me", outputs that carry it along).
//! - [`store`]: the view kept in sync, commands in flight, toasts, meters.
//! - `shell` (Windows): our own layered window drawn on the CPU (the U1 spike's decision),
//!   shown on the game's monitor and handing focus back on hide.
//! - `widgets` (Windows): fader, meter, pad, matrix cell.
//! - `views` (Windows): what's drawn, from the store, producing commands.
//! - `app` (Windows): ties them together.

pub mod coalesce;
pub mod link;
pub mod log;
pub mod simple;
pub mod store;
pub mod taper;

#[cfg(windows)]
pub mod app;
#[cfg(windows)]
pub mod shell;
#[cfg(windows)]
pub mod views;
#[cfg(windows)]
pub mod widgets;
