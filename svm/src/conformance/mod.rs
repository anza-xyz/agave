//! Solana SVM conformance.

#[cfg(feature = "conformance")]
pub mod account_state;
pub mod callback;
#[cfg(feature = "conformance")]
pub mod elf_loader;
#[cfg(feature = "conformance")]
pub mod fd_hash;
#[cfg(feature = "conformance")]
pub mod feature_set;
pub mod instr;
pub mod programs;
#[cfg(feature = "conformance")]
pub mod serialization;
mod setup;
