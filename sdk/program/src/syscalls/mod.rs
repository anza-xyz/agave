//! Declarations of Solana program syscalls.
//!
//! This module is mostly empty when not compiling for BPF targets.

#[cfg(target_os = "solana")]
mod definitions;

#[cfg(target_os = "solana")]
pub use definitions::*;

/// Maximum CPI instruction data size. 1232 bytes was chosen so that CPI
/// instructions are limited similarly to transaction instructions
/// and use the same limit as `program_utils::limited_deserialize`.
pub const MAX_CPI_INSTRUCTION_DATA_LEN: u64 = 1232;

/// Maximum CPI instruction accounts. 255 was chosen to ensure that instruction
/// accounts are always within the maximum instruction account limit for SBF
/// program instructions.
pub const MAX_CPI_INSTRUCTION_ACCOUNTS: u8 = u8::MAX;

/// Maximum number of account info structs that can be used in a single CPI
/// invocation. A limit on account info structs is effectively the same as
/// limiting the number of unique accounts. 128 was chosen to match the max
/// number of locked accounts per transaction (MAX_TX_ACCOUNT_LOCKS).
pub const MAX_CPI_ACCOUNT_INFOS: usize = 128;
