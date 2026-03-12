//! Solana SVM test harness for instruction execution.
//!
//! This crate provides an API for Agave's program runtime in order to
//! execute program instructions directly against the VM.

pub mod file;
pub mod keyed_account;
pub mod logger;
pub mod program_cache;
pub mod sysvar_cache;

pub use solana_svm_test_harness_fixture as fixture;

#[cfg(all(feature = "fuzz", feature = "dev-context-only-utils"))]
pub mod fuzz;

#[cfg(feature = "dev-context-only-utils")]
mod harness;

#[cfg(feature = "dev-context-only-utils")]
pub use harness::{execute_instr, execute_instr_with_callback};
