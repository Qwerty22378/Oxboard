//! `sb-engine-host`: the soundboard engine in its own process, so an engine crash never takes
//! the controller (or the user's whole setup) with it. The controller spawns it, connects over
//! a named pipe, and replays the graph and params if it ever has to restart it.
//!
//! - [`host`]: the serving loop, independent of transport and audio backend (tested on Linux
//!   over a socket pair).
//! - [`silent`]: a backend with no devices whose mix runs on a timer, for tests (the
//!   controller's too) and anywhere without audio.
//!
//! The controller's end of the protocol is `sb_shared::engine_client`; the Windows named pipe
//! is `sb_pipe`.

pub mod host;
pub mod silent;
