//! Cross-Program Invocation (CPI) error types

use {
    crate::invoke_context::InvokeContext,
    solana_loader_v3_interface::instruction as bpf_loader_upgradeable,
    solana_pubkey::{Pubkey, PubkeyError},
    solana_sdk_ids::{bpf_loader, bpf_loader_deprecated, native_loader},
    solana_svm_log_collector::ic_msg,
    solana_transaction_context::{MAX_ACCOUNTS_PER_INSTRUCTION, MAX_INSTRUCTION_DATA_LEN},
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

/// Maximum number of account info structs that can be used in a single CPI invocation
pub const MAX_CPI_ACCOUNT_INFOS: usize = 128;

/// Check that an account info pointer field points to the expected address
pub fn check_account_info_pointer(
    invoke_context: &InvokeContext,
    vm_addr: u64,
    expected_vm_addr: u64,
    field: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if vm_addr != expected_vm_addr {
        ic_msg!(
            invoke_context,
            "Invalid account info pointer `{}': {:#x} != {:#x}",
            field,
            vm_addr,
            expected_vm_addr
        );
        return Err(Box::new(CpiError::InvalidPointer));
    }
    Ok(())
}

/// Check that an instruction's account and data lengths are within limits
pub fn check_instruction_size(
    num_accounts: usize,
    data_len: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    if num_accounts > MAX_ACCOUNTS_PER_INSTRUCTION {
        return Err(Box::new(CpiError::MaxInstructionAccountsExceeded {
            num_accounts: num_accounts as u64,
            max_accounts: MAX_ACCOUNTS_PER_INSTRUCTION as u64,
        }));
    }
    if data_len > MAX_INSTRUCTION_DATA_LEN {
        return Err(Box::new(CpiError::MaxInstructionDataLenExceeded {
            data_len: data_len as u64,
            max_data_len: MAX_INSTRUCTION_DATA_LEN as u64,
        }));
    }
    Ok(())
}

/// Check that the number of account infos is within the CPI limit
pub fn check_account_infos(
    num_account_infos: usize,
    invoke_context: &mut InvokeContext,
) -> Result<(), Box<dyn std::error::Error>> {
    let max_cpi_account_infos = if invoke_context
        .get_feature_set()
        .increase_tx_account_lock_limit
    {
        MAX_CPI_ACCOUNT_INFOS
    } else {
        64
    };
    let num_account_infos = num_account_infos as u64;
    let max_account_infos = max_cpi_account_infos as u64;
    if num_account_infos > max_account_infos {
        return Err(Box::new(CpiError::MaxInstructionAccountInfosExceeded {
            num_account_infos,
            max_account_infos,
        }));
    }
    Ok(())
}

/// Check whether a program is authorized for CPI
pub fn check_authorized_program(
    program_id: &Pubkey,
    instruction_data: &[u8],
    invoke_context: &InvokeContext,
) -> Result<(), Box<dyn std::error::Error>> {
    if native_loader::check_id(program_id)
        || bpf_loader::check_id(program_id)
        || bpf_loader_deprecated::check_id(program_id)
        || (solana_sdk_ids::bpf_loader_upgradeable::check_id(program_id)
            && !(bpf_loader_upgradeable::is_upgrade_instruction(instruction_data)
                || bpf_loader_upgradeable::is_set_authority_instruction(instruction_data)
                || (invoke_context
                    .get_feature_set()
                    .enable_bpf_loader_set_authority_checked_ix
                    && bpf_loader_upgradeable::is_set_authority_checked_instruction(
                        instruction_data,
                    ))
                || (invoke_context
                    .get_feature_set()
                    .enable_extend_program_checked
                    && bpf_loader_upgradeable::is_extend_program_checked_instruction(
                        instruction_data,
                    ))
                || bpf_loader_upgradeable::is_close_instruction(instruction_data)))
        || invoke_context.is_precompile(program_id)
    {
        return Err(Box::new(CpiError::ProgramNotSupported(*program_id)));
    }
    Ok(())
}
