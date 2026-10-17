//! The Windows side: Core Audio on a COM thread of its own ([`com`]), including per-app
//! outputs ([`policy`]). The plain-data half of
//! it (scans, diffs, level bookkeeping) is [`crate::watch`], which builds and tests anywhere.

pub mod com;
pub mod policy;
