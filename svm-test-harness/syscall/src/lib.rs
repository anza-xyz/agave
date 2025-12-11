//! Solana SVM test harness for syscall execution.
//!
//! This crate provides an API for Agave's program runtime in order to
//! execute individual syscalls directly against the VM.

mod harness;
mod vm_utils;

pub use {harness::execute_vm_syscall, solana_svm_test_harness_fixture as fixture};

#[cfg(feature = "fuzz")]
pub mod fuzz;
