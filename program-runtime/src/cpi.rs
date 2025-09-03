//! Cross-Program Invocation (CPI) error types

use {
    solana_pubkey::{Pubkey, PubkeyError},
    thiserror::Error,
};

/// CPI-specific error types
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CpiError {
    #[error("Invalid pointer")]
    InvalidPointer,
    #[error("Too many signers")]
    TooManySigners,
    #[error("Could not create program address with signer seeds: {0}")]
    BadSeeds(PubkeyError),
    #[error("InvalidLength")]
    InvalidLength,
    #[error("Invoked an instruction with too many accounts ({num_accounts} > {max_accounts})")]
    MaxInstructionAccountsExceeded {
        num_accounts: u64,
        max_accounts: u64,
    },
    #[error("Invoked an instruction with data that is too large ({data_len} > {max_data_len})")]
    MaxInstructionDataLenExceeded { data_len: u64, max_data_len: u64 },
    #[error(
        "Invoked an instruction with too many account info's ({num_account_infos} > \
         {max_account_infos})"
    )]
    MaxInstructionAccountInfosExceeded {
        num_account_infos: u64,
        max_account_infos: u64,
    },
    #[error("Program {0} not supported by inner instructions")]
    ProgramNotSupported(Pubkey),
}

/// Rust representation of C's SolInstruction
#[derive(Debug)]
#[repr(C)]
pub struct SolInstruction {
    pub program_id_addr: u64,
    pub accounts_addr: u64,
    pub accounts_len: u64,
    pub data_addr: u64,
    pub data_len: u64,
}

/// Rust representation of C's SolAccountMeta
#[derive(Debug)]
#[repr(C)]
pub struct SolAccountMeta {
    pub pubkey_addr: u64,
    pub is_writable: bool,
    pub is_signer: bool,
}

/// Rust representation of C's SolAccountInfo
#[derive(Debug)]
#[repr(C)]
pub struct SolAccountInfo {
    pub key_addr: u64,
    pub lamports_addr: u64,
    pub data_len: u64,
    pub data_addr: u64,
    pub owner_addr: u64,
    pub rent_epoch: u64,
    pub is_signer: bool,
    pub is_writable: bool,
    pub executable: bool,
}

/// Rust representation of C's SolSignerSeed
#[derive(Debug)]
#[repr(C)]
pub struct SolSignerSeedC {
    pub addr: u64,
    pub len: u64,
}

/// Rust representation of C's SolSignerSeeds
#[derive(Debug)]
#[repr(C)]
pub struct SolSignerSeedsC {
    pub addr: u64,
    pub len: u64,
}
