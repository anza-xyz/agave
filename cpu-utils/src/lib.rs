#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]
// Activate some of the Rust 2024 lints to make the future migration easier.
#![warn(if_let_rescope)]
#![warn(keyword_idents_2024)]
#![warn(missing_unsafe_on_extern)]
#![warn(rust_2024_guarded_string_incompatible_syntax)]
#![warn(rust_2024_incompatible_pat)]
#![warn(tail_expr_drop_order)]
#![warn(unsafe_attr_outside_unsafe)]
#![warn(unsafe_op_in_unsafe_fn)]

//! CPU affinity utilities for Linux systems.
//!
//! This crate provides safe Rust bindings for setting CPU affinity and querying
//! CPU topology information. Useful for performance-critical applications that need
//! precise control over thread placement.
//!
//! # Platform Support
//!
//! Linux only. All functions return [`CpuAffinityError::NotSupported`] on other platforms.
//!
//! # Examples
//!
//! ```no_run
//! use agave_cpu_utils::*;
//!
//! # fn main() -> Result<(), CpuAffinityError> {
//! // Pin thread to CPU 0
//! set_cpu_affinity([0])?;
//!
//! // Use isolated CPUs for low-latency work
//! let isolated = isolated_cpus()?;
//! if let Some(&cpu) = isolated.first() {
//!     set_cpu_affinity([cpu])?;
//! }
//!
//! // Pin to physical cores only (avoid hyperthreading)
//! set_affinity_physical_cores_only([0, 1])?;
//! # Ok(())
//! # }
//! ```
//!

mod affinity;
mod error;
mod topology;

pub use {
    affinity::{cpu_affinity, cpu_count, isolated_cpus, max_cpu_id, set_cpu_affinity},
    error::CpuAffinityError,
    topology::{core_to_cpus_mapping, physical_core_count, set_affinity_physical_cores_only},
};
