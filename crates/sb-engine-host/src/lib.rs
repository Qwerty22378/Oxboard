//! `sb-engine-host`: the soundboard engine in its own process, so an engine crash never takes
//! the controller (or the user's whole setup) with it. The controller spawns it, connects over
//! a named pipe, and replays the graph and params if it ever has to restart it.
//!
//! - [`host`]: the serving loop, independent of transport and audio backend (tested on Linux
//!   over a socket pair).
//! - [`client`]: the controller's side of the protocol.
//! - `pipe` (Windows): the named pipe, owner-only and local-only.

pub mod client;
pub mod host;
#[cfg(windows)]
pub mod pipe;
