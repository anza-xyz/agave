//! Solana SVM test harness for instruction execution.
//!
//! This crate provides an API for Agave's program runtime in order to
//! execute program instructions directly against the VM.

pub mod file;
mod harness;
pub mod keyed_account;
pub mod logger;
pub mod program_cache;
pub mod sysvar_cache;

pub use {
    harness::{execute_instr, execute_instr_with_callback},
    solana_svm_test_harness_fixture as fixture,
};

#[cfg(feature = "fuzz")]
pub mod fuzz;
