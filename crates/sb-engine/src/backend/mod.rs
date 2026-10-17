//! Device I/O: WASAPI devices (Windows), and the driver's virtual endpoints over shared rings
//! (`driver` feature; platform-independent apart from the real driver link).

#[cfg(feature = "driver")]
pub mod driver;
#[cfg(windows)]
pub mod wasapi;
