#![cfg(feature = "agave-unstable-api")]
//! Data shared between program runtime and built-in programs as well as SBF programs.
#![deny(clippy::indexing_slicing)]
#![cfg_attr(docsrs, feature(doc_auto_cfg))]

pub mod instruction;
pub mod instruction_accounts;
pub mod transaction_accounts;
mod vm_addresses;
pub mod vm_slice;

pub mod transaction;

pub const MAX_ACCOUNTS_PER_TRANSACTION: usize = 256;
// This is one less than MAX_ACCOUNTS_PER_TRANSACTION because
// one index is used as NON_DUP_MARKER in ABI v0 and v1.
pub const MAX_ACCOUNTS_PER_INSTRUCTION: usize = 255;
pub const MAX_INSTRUCTION_DATA_LEN: usize = 10 * 1024;
pub const MAX_ACCOUNT_DATA_LEN: u64 = 10 * 1024 * 1024;
// Note: With stricter_abi_and_runtime_constraints programs can grow accounts
// faster than they intend to, because the AccessViolationHandler might grow
// an account up to MAX_ACCOUNT_DATA_GROWTH_PER_INSTRUCTION at once.
pub const MAX_ACCOUNT_DATA_GROWTH_PER_TRANSACTION: i64 = MAX_ACCOUNT_DATA_LEN as i64 * 2;
pub const MAX_ACCOUNT_DATA_GROWTH_PER_INSTRUCTION: usize = 10 * 1_024;
// Maximum cross-program invocation and instructions per transaction
pub const MAX_INSTRUCTION_TRACE_LENGTH: usize = 64;

#[cfg(test)]
static_assertions::const_assert_eq!(
    MAX_ACCOUNTS_PER_INSTRUCTION,
    solana_program_entrypoint::NON_DUP_MARKER as usize,
);
#[cfg(test)]
static_assertions::const_assert_eq!(
    MAX_ACCOUNT_DATA_LEN,
    solana_system_interface::MAX_PERMITTED_DATA_LENGTH,
);
#[cfg(test)]
static_assertions::const_assert_eq!(
    MAX_ACCOUNT_DATA_GROWTH_PER_TRANSACTION,
    solana_system_interface::MAX_PERMITTED_ACCOUNTS_DATA_ALLOCATIONS_PER_TRANSACTION,
);
#[cfg(test)]
static_assertions::const_assert_eq!(
    MAX_ACCOUNT_DATA_GROWTH_PER_INSTRUCTION,
    solana_account_info::MAX_PERMITTED_DATA_INCREASE,
);

/// Index of an account inside of the transaction or an instruction.
pub type IndexOfAccount = u16;
