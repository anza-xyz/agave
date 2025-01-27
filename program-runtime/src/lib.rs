#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]
#![deny(clippy::arithmetic_side_effects)]
#![deny(clippy::indexing_slicing)]

#[cfg(feature = "metrics")]
#[macro_use]
extern crate solana_metrics;

pub use solana_sbpf;
pub mod invoke_context;
pub mod loaded_programs;
pub mod mem_pool;
pub mod stable_log;
pub mod sysvar_cache;
// re-exports for macros
pub mod __private {
    pub use {
        solana_account::ReadableAccount, solana_hash::Hash,
        solana_instruction::error::InstructionError, solana_rent::Rent,
        solana_transaction_context::TransactionContext,
    };
}
