//! Conformance harnesses for the Solana SVM.
//!
//! Houses the `sol_compat_*` FFI entrypoints that Firedancer's conformance
//! tooling links against to exercise Agave's execution layer.

#[cfg(feature = "ffi")]
pub mod elf_loader;
#[cfg(feature = "ffi")]
pub mod serialization;
