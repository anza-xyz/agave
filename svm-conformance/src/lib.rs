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
#[cfg(feature = "ffi")]
pub mod txn;

/// No-op; required by Firedancer's conformance tooling.
#[cfg(feature = "ffi")]
#[unsafe(no_mangle)]
pub extern "C" fn sol_compat_init(_log_level: i32) {}

/// No-op; required by Firedancer's conformance tooling.
#[cfg(feature = "ffi")]
#[unsafe(no_mangle)]
pub extern "C" fn sol_compat_fini() {}
