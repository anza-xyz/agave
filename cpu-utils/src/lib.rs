#![cfg(all(feature = "agave-unstable-api", target_os = "linux"))]

//! CPU affinity utilities for Linux systems.
//!
//! This crate provides safe Rust bindings for setting CPU affinity and querying
//! the current task affinity mask. Useful for performance-critical applications
//! that need precise control over thread placement.
//!
//! # Examples
//!
//! ```no_run
//! use agave_cpu_utils::*;
//!
//! # fn main() -> std::io::Result<()> {
//! let allowed = cpu_affinity(None)?;
//! if let Some(&cpu) = allowed.first() {
//!     set_cpu_affinity(None, [cpu])?;
//! }
//! # Ok(())
//! # }
//! ```
//!

mod affinity;

pub use affinity::{CpuId, cpu_affinity, set_cpu_affinity};
