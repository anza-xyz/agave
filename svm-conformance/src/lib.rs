//! Conformance harnesses for the Solana SVM.
//!
//! Houses the `sol_compat_*` FFI entrypoints that Firedancer's conformance
//! tooling links against to exercise Agave's execution layer.

#[cfg(feature = "ffi")]
pub mod elf_loader;
#[cfg(feature = "ffi")]
pub mod instr;
#[cfg(feature = "ffi")]
pub mod serialization;
#[cfg(feature = "ffi")]
pub mod syscall;
#[cfg(test)]
mod test_utils;
#[cfg(feature = "ffi")]
pub mod txn;
