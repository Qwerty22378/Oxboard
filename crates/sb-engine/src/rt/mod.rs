//! Real-time building blocks. Everything here is allocation-free, lock-free and
//! platform-independent once constructed, so it is tested with plain `cargo test`.

pub mod ring;
pub mod stats;
