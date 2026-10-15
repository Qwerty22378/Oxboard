//! Real-time building blocks. Everything here is allocation-free and lock-free once
//! constructed, and platform-independent apart from the Windows thread spawner in
//! `mix_thread`, so it is tested with plain `cargo test`.

pub mod mix_thread;
pub mod ring;
pub mod stats;
