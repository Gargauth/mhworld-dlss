//! Pure, host-testable foundations of mhw-dlss.
//!
//! See `docs/ARCHITECTURE.md`, section `mhw-core`. Nothing here touches
//! Windows: every module builds and is tested on any host.

#![warn(missing_docs)]

pub mod bus;
pub mod config;
pub mod frame;
pub mod log;
pub mod math;
pub mod sig;
