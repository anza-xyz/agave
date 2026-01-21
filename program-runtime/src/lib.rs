#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]
#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]
#![deny(clippy::arithmetic_side_effects)]
#![deny(clippy::indexing_slicing)]

pub use solana_sbpf;
pub mod cpi;
pub mod deploy;
pub mod execution_budget;
pub mod invoke_context;
pub mod loaded_programs;
pub mod mem_pool;
pub mod memory;
pub mod serialization;
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
