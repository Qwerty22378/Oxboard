//! From graph to something the audio thread can run: the `GraphSpec` (from `sb-shared`),
//! the compiler, and the render plan it produces.

pub mod compile;
pub mod plan;
pub mod spec;
