//! Cross-Program Invocation (CPI) error types

use {
    crate::{
        invoke_context::{InvokeContext, SerializedAccountMetadata},
        memory::{translate_slice, translate_type, translate_type_mut_for_cpi, translate_vm_slice},
        serialization::{create_memory_region_of_account, modify_memory_region_of_account},
    },
    solana_account_info::AccountInfo,
    solana_instruction::{error::InstructionError, AccountMeta, Instruction},
    solana_loader_v3_interface::instruction as bpf_loader_upgradeable,
    solana_program_entrypoint::MAX_PERMITTED_DATA_INCREASE,
    solana_pubkey::{Pubkey, PubkeyError, MAX_SEEDS},
    solana_sbpf::{ebpf, memory_region::MemoryMapping},
    solana_sdk_ids::{bpf_loader, bpf_loader_deprecated, native_loader},
    solana_stable_layout::stable_instruction::StableInstruction,
    solana_svm_log_collector::ic_msg,
    solana_svm_measure::measure::Measure,
    solana_svm_timings::ExecuteTimings,
    solana_transaction_context::{
        vm_slice::VmSlice, BorrowedInstructionAccount, IndexOfAccount,
        MAX_ACCOUNTS_PER_INSTRUCTION, MAX_INSTRUCTION_DATA_LEN,
    },
    std::mem,
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

type Error = Box<dyn std::error::Error>;

const SUCCESS: u64 = 0;
/// Maximum signers
const MAX_SIGNERS: usize = 16;
///SIMD-0339 based calculation of AccountInfo translation byte size. Fixed size of **80 bytes** for each AccountInfo broken down as:
/// - 32 bytes for account address
/// - 32 bytes for owner address
/// - 8 bytes for lamport balance
/// - 8 bytes for data length
const ACCOUNT_INFO_BYTE_SIZE: usize = 80;

/// Rust representation of C's SolInstruction
#[derive(Debug)]
#[repr(C)]
struct SolInstruction {
    pub program_id_addr: u64,
    pub accounts_addr: u64,
    pub accounts_len: u64,
    pub data_addr: u64,
    pub data_len: u64,
}

/// Rust representation of C's SolAccountMeta
#[derive(Debug)]
#[repr(C)]
struct SolAccountMeta {
    pub pubkey_addr: u64,
    pub is_writable: bool,
    pub is_signer: bool,
}

/// Rust representation of C's SolAccountInfo
#[derive(Debug)]
#[repr(C)]
struct SolAccountInfo {
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
struct SolSignerSeedC {
    pub addr: u64,
    pub len: u64,
}

/// Rust representation of C's SolSignerSeeds
#[derive(Debug)]
#[repr(C)]
struct SolSignerSeedsC {
    pub addr: u64,
    pub len: u64,
}

/// Maximum number of account info structs that can be used in a single CPI invocation
const MAX_CPI_ACCOUNT_INFOS: usize = 128;
/// Maximum number of account info structs that can be used in a single CPI invocation with SIMD-0339 active
const MAX_CPI_ACCOUNT_INFOS_SIMD_0339: usize = 255;

/// Check that an account info pointer field points to the expected address
fn check_account_info_pointer(
    invoke_context: &InvokeContext,
    vm_addr: u64,
    expected_vm_addr: u64,
    field: &str,
) -> Result<(), Error> {
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
fn check_instruction_size(num_accounts: usize, data_len: usize) -> Result<(), Error> {
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
fn check_account_infos(
    num_account_infos: usize,
    invoke_context: &mut InvokeContext,
) -> Result<(), Error> {
    let max_cpi_account_infos = if invoke_context
        .get_feature_set()
        .increase_cpi_account_info_limit
    {
        MAX_CPI_ACCOUNT_INFOS_SIMD_0339
    } else if invoke_context
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
fn check_authorized_program(
    program_id: &Pubkey,
    instruction_data: &[u8],
    invoke_context: &InvokeContext,
) -> Result<(), Error> {
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

/// Host side representation of AccountInfo or SolAccountInfo passed to the CPI syscall.
///
/// At the start of a CPI, this can be different from the data stored in the
/// corresponding BorrowedAccount, and needs to be synched.
pub struct CallerAccount<'a> {
    pub lamports: &'a mut u64,
    pub owner: &'a mut Pubkey,
    // The original data length of the account at the start of the current
    // instruction. We use this to determine wether an account was shrunk or
    // grown before or after CPI, and to derive the vm address of the realloc
    // region.
    pub original_data_len: usize,
    // This points to the data section for this account, as serialized and
    // mapped inside the vm (see serialize_parameters() in
    // BpfExecutor::execute).
    //
    // This is only set when account_data_direct_mapping is off.
    pub serialized_data: &'a mut [u8],
    // Given the corresponding input AccountInfo::data, vm_data_addr points to
    // the pointer field and ref_to_len_in_vm points to the length field.
    pub vm_data_addr: u64,
    pub ref_to_len_in_vm: &'a mut u64,
}

impl<'a> CallerAccount<'a> {
    pub fn get_serialized_data(
        memory_mapping: &solana_sbpf::memory_region::MemoryMapping<'_>,
        vm_addr: u64,
        len: u64,
        stricter_abi_and_runtime_constraints: bool,
        account_data_direct_mapping: bool,
    ) -> Result<&'a mut [u8], Error> {
        use crate::memory::translate_slice_mut_for_cpi;

        if stricter_abi_and_runtime_constraints && account_data_direct_mapping {
            Ok(&mut [])
        } else if stricter_abi_and_runtime_constraints {
            // Workaround the memory permissions (as these are from the PoV of being inside the VM)
            let serialization_ptr = translate_slice_mut_for_cpi::<u8>(
                memory_mapping,
                solana_sbpf::ebpf::MM_INPUT_START,
                1,
                false, // Don't care since it is byte aligned
            )?
            .as_mut_ptr();
            unsafe {
                Ok(std::slice::from_raw_parts_mut(
                    serialization_ptr
                        .add(vm_addr.saturating_sub(solana_sbpf::ebpf::MM_INPUT_START) as usize),
                    len as usize,
                ))
            }
        } else {
            translate_slice_mut_for_cpi::<u8>(
                memory_mapping,
                vm_addr,
                len,
                false, // Don't care since it is byte aligned
            )
        }
    }

    // Create a CallerAccount given an AccountInfo.
    pub fn from_account_info(
        invoke_context: &InvokeContext,
        memory_mapping: &solana_sbpf::memory_region::MemoryMapping<'_>,
        check_aligned: bool,
        _vm_addr: u64,
        account_info: &solana_account_info::AccountInfo,
        account_metadata: &crate::invoke_context::SerializedAccountMetadata,
    ) -> Result<CallerAccount<'a>, Error> {
        use crate::memory::{translate_type, translate_type_mut_for_cpi};

        let stricter_abi_and_runtime_constraints = invoke_context
            .get_feature_set()
            .stricter_abi_and_runtime_constraints;
        let account_data_direct_mapping =
            invoke_context.get_feature_set().account_data_direct_mapping;

        if stricter_abi_and_runtime_constraints {
            check_account_info_pointer(
                invoke_context,
                account_info.key as *const _ as u64,
                account_metadata.vm_key_addr,
                "key",
            )?;
            check_account_info_pointer(
                invoke_context,
                account_info.owner as *const _ as u64,
                account_metadata.vm_owner_addr,
                "owner",
            )?;
        }

        // account_info points to host memory. The addresses used internally are
        // in vm space so they need to be translated.
        let lamports = {
            // Double translate lamports out of RefCell
            let ptr = translate_type::<u64>(
                memory_mapping,
                account_info.lamports.as_ptr() as u64,
                check_aligned,
            )?;
            if stricter_abi_and_runtime_constraints {
                if account_info.lamports.as_ptr() as u64 >= solana_sbpf::ebpf::MM_INPUT_START {
                    return Err(Box::new(CpiError::InvalidPointer));
                }

                check_account_info_pointer(
                    invoke_context,
                    *ptr,
                    account_metadata.vm_lamports_addr,
                    "lamports",
                )?;
            }
            translate_type_mut_for_cpi::<u64>(memory_mapping, *ptr, check_aligned)?
        };

        let owner = translate_type_mut_for_cpi::<Pubkey>(
            memory_mapping,
            account_info.owner as *const _ as u64,
            check_aligned,
        )?;

        let (serialized_data, vm_data_addr, ref_to_len_in_vm) = {
            if stricter_abi_and_runtime_constraints
                && account_info.data.as_ptr() as u64 >= solana_sbpf::ebpf::MM_INPUT_START
            {
                return Err(Box::new(CpiError::InvalidPointer));
            }

            // Double translate data out of RefCell
            let data = *translate_type::<&[u8]>(
                memory_mapping,
                account_info.data.as_ptr() as *const _ as u64,
                check_aligned,
            )?;
            if stricter_abi_and_runtime_constraints {
                check_account_info_pointer(
                    invoke_context,
                    data.as_ptr() as u64,
                    account_metadata.vm_data_addr,
                    "data",
                )?;
            }

            invoke_context.consume_checked(
                (data.len() as u64)
                    .checked_div(invoke_context.get_execution_cost().cpi_bytes_per_unit)
                    .unwrap_or(u64::MAX),
            )?;

            let vm_len_addr = (account_info.data.as_ptr() as *const u64 as u64)
                .saturating_add(std::mem::size_of::<u64>() as u64);
            if stricter_abi_and_runtime_constraints {
                // In the same vein as the other check_account_info_pointer() checks, we don't lock
                // this pointer to a specific address but we don't want it to be inside accounts, or
                // callees might be able to write to the pointed memory.
                if vm_len_addr >= solana_sbpf::ebpf::MM_INPUT_START {
                    return Err(Box::new(CpiError::InvalidPointer));
                }
            }
            let vm_data_addr = data.as_ptr() as u64;
            let serialized_data = CallerAccount::get_serialized_data(
                memory_mapping,
                vm_data_addr,
                data.len() as u64,
                stricter_abi_and_runtime_constraints,
                account_data_direct_mapping,
            )?;
            let ref_to_len_in_vm =
                translate_type_mut_for_cpi::<u64>(memory_mapping, vm_len_addr, false)?;
            (serialized_data, vm_data_addr, ref_to_len_in_vm)
        };

        Ok(CallerAccount {
            lamports,
            owner,
            original_data_len: account_metadata.original_data_len,
            serialized_data,
            vm_data_addr,
            ref_to_len_in_vm,
        })
    }

    // Create a CallerAccount given a SolAccountInfo.
    fn from_sol_account_info(
        invoke_context: &InvokeContext,
        memory_mapping: &solana_sbpf::memory_region::MemoryMapping<'_>,
        check_aligned: bool,
        vm_addr: u64,
        account_info: &SolAccountInfo,
        account_metadata: &crate::invoke_context::SerializedAccountMetadata,
    ) -> Result<CallerAccount<'a>, Error> {
        use crate::memory::translate_type_mut_for_cpi;

        let stricter_abi_and_runtime_constraints = invoke_context
            .get_feature_set()
            .stricter_abi_and_runtime_constraints;
        let account_data_direct_mapping =
            invoke_context.get_feature_set().account_data_direct_mapping;

        if stricter_abi_and_runtime_constraints {
            check_account_info_pointer(
                invoke_context,
                account_info.key_addr,
                account_metadata.vm_key_addr,
                "key",
            )?;

            check_account_info_pointer(
                invoke_context,
                account_info.owner_addr,
                account_metadata.vm_owner_addr,
                "owner",
            )?;

            check_account_info_pointer(
                invoke_context,
                account_info.lamports_addr,
                account_metadata.vm_lamports_addr,
                "lamports",
            )?;

            check_account_info_pointer(
                invoke_context,
                account_info.data_addr,
                account_metadata.vm_data_addr,
                "data",
            )?;
        }

        // account_info points to host memory. The addresses used internally are
        // in vm space so they need to be translated.
        let lamports = translate_type_mut_for_cpi::<u64>(
            memory_mapping,
            account_info.lamports_addr,
            check_aligned,
        )?;
        let owner = translate_type_mut_for_cpi::<Pubkey>(
            memory_mapping,
            account_info.owner_addr,
            check_aligned,
        )?;

        invoke_context.consume_checked(
            account_info
                .data_len
                .checked_div(invoke_context.get_execution_cost().cpi_bytes_per_unit)
                .unwrap_or(u64::MAX),
        )?;

        let serialized_data = CallerAccount::get_serialized_data(
            memory_mapping,
            account_info.data_addr,
            account_info.data_len,
            stricter_abi_and_runtime_constraints,
            account_data_direct_mapping,
        )?;

        // we already have the host addr we want: &mut account_info.data_len.
        // The account info might be read only in the vm though, so we translate
        // to ensure we can write. This is tested by programs/sbf/rust/ro_modify
        // which puts SolAccountInfo in rodata.
        let vm_len_addr = vm_addr
            .saturating_add(&account_info.data_len as *const u64 as u64)
            .saturating_sub(account_info as *const _ as *const u64 as u64);
        let ref_to_len_in_vm =
            translate_type_mut_for_cpi::<u64>(memory_mapping, vm_len_addr, false)?;

        Ok(CallerAccount {
            lamports,
            owner,
            original_data_len: account_metadata.original_data_len,
            serialized_data,
            vm_data_addr: account_info.data_addr,
            ref_to_len_in_vm,
        })
    }
}

/// Implemented by language specific data structure translators
pub trait SyscallInvokeSigned {
    fn translate_instruction(
        addr: u64,
        memory_mapping: &MemoryMapping,
        invoke_context: &mut InvokeContext,
        check_aligned: bool,
    ) -> Result<Instruction, Error>;
    fn translate_accounts<'a>(
        account_infos_addr: u64,
        account_infos_len: u64,
        memory_mapping: &MemoryMapping<'_>,
        invoke_context: &mut InvokeContext,
        check_aligned: bool,
    ) -> Result<Vec<TranslatedAccount<'a>>, Error>;
    fn translate_signers(
        program_id: &Pubkey,
        signers_seeds_addr: u64,
        signers_seeds_len: u64,
        memory_mapping: &MemoryMapping,
        check_aligned: bool,
    ) -> Result<Vec<Pubkey>, Error>;
}

pub fn translate_instruction_rust(
    addr: u64,
    memory_mapping: &MemoryMapping,
    invoke_context: &mut InvokeContext,
    check_aligned: bool,
) -> Result<Instruction, Error> {
    let ix = translate_type::<StableInstruction>(memory_mapping, addr, check_aligned)?;
    let account_metas = translate_slice::<AccountMeta>(
        memory_mapping,
        ix.accounts.as_vaddr(),
        ix.accounts.len(),
        check_aligned,
    )?;
    let data = translate_slice::<u8>(
        memory_mapping,
        ix.data.as_vaddr(),
        ix.data.len(),
        check_aligned,
    )?;

    check_instruction_size(account_metas.len(), data.len())?;

    let mut total_cu_translation_cost: u64 = (data.len() as u64)
        .checked_div(invoke_context.get_execution_cost().cpi_bytes_per_unit)
        .unwrap_or(u64::MAX);

    if invoke_context
        .get_feature_set()
        .increase_cpi_account_info_limit
    {
        // Each account meta is 34 bytes (32 for pubkey, 1 for is_signer, 1 for is_writable)
        let account_meta_translation_cost =
            (account_metas.len().saturating_mul(size_of::<AccountMeta>()) as u64)
                .checked_div(invoke_context.get_execution_cost().cpi_bytes_per_unit)
                .unwrap_or(u64::MAX);

        total_cu_translation_cost =
            total_cu_translation_cost.saturating_add(account_meta_translation_cost);
    }

    consume_compute_meter(invoke_context, total_cu_translation_cost)?;

    let mut accounts = Vec::with_capacity(account_metas.len());
    #[allow(clippy::needless_range_loop)]
    for account_index in 0..account_metas.len() {
        #[allow(clippy::indexing_slicing)]
        let account_meta = &account_metas[account_index];
        if unsafe {
            std::ptr::read_volatile(&account_meta.is_signer as *const _ as *const u8) > 1
                || std::ptr::read_volatile(&account_meta.is_writable as *const _ as *const u8) > 1
        } {
            return Err(Box::new(InstructionError::InvalidArgument));
        }
        accounts.push(account_meta.clone());
    }

    Ok(Instruction {
        accounts,
        data: data.to_vec(),
        program_id: ix.program_id,
    })
}

pub fn translate_accounts_rust<'a>(
    account_infos_addr: u64,
    account_infos_len: u64,
    memory_mapping: &MemoryMapping<'_>,
    invoke_context: &mut InvokeContext,
    check_aligned: bool,
) -> Result<Vec<TranslatedAccount<'a>>, Error> {
    let (account_infos, account_info_keys) = translate_account_infos(
        account_infos_addr,
        account_infos_len,
        |account_info: &AccountInfo| account_info.key as *const _ as u64,
        memory_mapping,
        invoke_context,
        check_aligned,
    )?;

    translate_accounts_common(
        &account_info_keys,
        account_infos,
        account_infos_addr,
        invoke_context,
        memory_mapping,
        check_aligned,
        CallerAccount::from_account_info,
    )
}

pub fn translate_signers_rust(
    program_id: &Pubkey,
    signers_seeds_addr: u64,
    signers_seeds_len: u64,
    memory_mapping: &MemoryMapping,
    check_aligned: bool,
) -> Result<Vec<Pubkey>, Error> {
    let mut signers = Vec::new();
    if signers_seeds_len > 0 {
        let signers_seeds = translate_slice::<VmSlice<VmSlice<u8>>>(
            memory_mapping,
            signers_seeds_addr,
            signers_seeds_len,
            check_aligned,
        )?;
        if signers_seeds.len() > MAX_SIGNERS {
            return Err(Box::new(CpiError::TooManySigners));
        }
        for signer_seeds in signers_seeds.iter() {
            let untranslated_seeds = translate_slice::<VmSlice<u8>>(
                memory_mapping,
                signer_seeds.ptr(),
                signer_seeds.len(),
                check_aligned,
            )?;
            if untranslated_seeds.len() > MAX_SEEDS {
                return Err(Box::new(InstructionError::MaxSeedLengthExceeded));
            }
            let seeds = untranslated_seeds
                .iter()
                .map(|untranslated_seed| {
                    translate_vm_slice(untranslated_seed, memory_mapping, check_aligned)
                })
                .collect::<Result<Vec<_>, Error>>()?;
            let signer =
                Pubkey::create_program_address(&seeds, program_id).map_err(CpiError::BadSeeds)?;
            signers.push(signer);
        }
        Ok(signers)
    } else {
        Ok(vec![])
    }
}

pub fn translate_instruction_c(
    addr: u64,
    memory_mapping: &MemoryMapping,
    invoke_context: &mut InvokeContext,
    check_aligned: bool,
) -> Result<Instruction, Error> {
    let ix_c = translate_type::<SolInstruction>(memory_mapping, addr, check_aligned)?;

    let program_id = translate_type::<Pubkey>(memory_mapping, ix_c.program_id_addr, check_aligned)?;
    let account_metas = translate_slice::<SolAccountMeta>(
        memory_mapping,
        ix_c.accounts_addr,
        ix_c.accounts_len,
        check_aligned,
    )?;
    let data = translate_slice::<u8>(memory_mapping, ix_c.data_addr, ix_c.data_len, check_aligned)?;

    check_instruction_size(ix_c.accounts_len as usize, data.len())?;

    let mut total_cu_translation_cost: u64 = (data.len() as u64)
        .checked_div(invoke_context.get_execution_cost().cpi_bytes_per_unit)
        .unwrap_or(u64::MAX);

    if invoke_context
        .get_feature_set()
        .increase_cpi_account_info_limit
    {
        // Each account meta is 34 bytes (32 for pubkey, 1 for is_signer, 1 for is_writable)
        let account_meta_translation_cost = (ix_c
            .accounts_len
            .saturating_mul(size_of::<AccountMeta>() as u64))
        .checked_div(invoke_context.get_execution_cost().cpi_bytes_per_unit)
        .unwrap_or(u64::MAX);

        total_cu_translation_cost =
            total_cu_translation_cost.saturating_add(account_meta_translation_cost);
    }

    consume_compute_meter(invoke_context, total_cu_translation_cost)?;

    let mut accounts = Vec::with_capacity(ix_c.accounts_len as usize);
    #[allow(clippy::needless_range_loop)]
    for account_index in 0..ix_c.accounts_len as usize {
        #[allow(clippy::indexing_slicing)]
        let account_meta = &account_metas[account_index];
        if unsafe {
            std::ptr::read_volatile(&account_meta.is_signer as *const _ as *const u8) > 1
                || std::ptr::read_volatile(&account_meta.is_writable as *const _ as *const u8) > 1
        } {
            return Err(Box::new(InstructionError::InvalidArgument));
        }
        let pubkey =
            translate_type::<Pubkey>(memory_mapping, account_meta.pubkey_addr, check_aligned)?;
        accounts.push(AccountMeta {
            pubkey: *pubkey,
            is_signer: account_meta.is_signer,
            is_writable: account_meta.is_writable,
        });
    }

    Ok(Instruction {
        accounts,
        data: data.to_vec(),
        program_id: *program_id,
    })
}

pub fn translate_accounts_c<'a>(
    account_infos_addr: u64,
    account_infos_len: u64,
    memory_mapping: &MemoryMapping<'_>,
    invoke_context: &mut InvokeContext,
    check_aligned: bool,
) -> Result<Vec<TranslatedAccount<'a>>, Error> {
    let (account_infos, account_info_keys) = translate_account_infos(
        account_infos_addr,
        account_infos_len,
        |account_info: &SolAccountInfo| account_info.key_addr,
        memory_mapping,
        invoke_context,
        check_aligned,
    )?;

    translate_accounts_common(
        &account_info_keys,
        account_infos,
        account_infos_addr,
        invoke_context,
        memory_mapping,
        check_aligned,
        CallerAccount::from_sol_account_info,
    )
}

pub fn translate_signers_c(
    program_id: &Pubkey,
    signers_seeds_addr: u64,
    signers_seeds_len: u64,
    memory_mapping: &MemoryMapping,
    check_aligned: bool,
) -> Result<Vec<Pubkey>, Error> {
    if signers_seeds_len > 0 {
        let signers_seeds = translate_slice::<SolSignerSeedsC>(
            memory_mapping,
            signers_seeds_addr,
            signers_seeds_len,
            check_aligned,
        )?;
        if signers_seeds.len() > MAX_SIGNERS {
            return Err(Box::new(CpiError::TooManySigners));
        }
        Ok(signers_seeds
            .iter()
            .map(|signer_seeds| {
                let seeds = translate_slice::<SolSignerSeedC>(
                    memory_mapping,
                    signer_seeds.addr,
                    signer_seeds.len,
                    check_aligned,
                )?;
                if seeds.len() > MAX_SEEDS {
                    return Err(Box::new(InstructionError::MaxSeedLengthExceeded) as Error);
                }
                let seeds_bytes = seeds
                    .iter()
                    .map(|seed| {
                        translate_slice::<u8>(memory_mapping, seed.addr, seed.len, check_aligned)
                    })
                    .collect::<Result<Vec<_>, Error>>()?;
                Pubkey::create_program_address(&seeds_bytes, program_id)
                    .map_err(|err| Box::new(CpiError::BadSeeds(err)) as Error)
            })
            .collect::<Result<Vec<_>, Error>>()?)
    } else {
        Ok(vec![])
    }
}

/// Call process instruction, common to both Rust and C
pub fn cpi_common<S: SyscallInvokeSigned>(
    invoke_context: &mut InvokeContext,
    instruction_addr: u64,
    account_infos_addr: u64,
    account_infos_len: u64,
    signers_seeds_addr: u64,
    signers_seeds_len: u64,
    memory_mapping: &mut MemoryMapping,
) -> Result<u64, Error> {
    // CPI entry.
    //
    // Translate the inputs to the syscall and synchronize the caller's account
    // changes so the callee can see them.
    consume_compute_meter(
        invoke_context,
        invoke_context.get_execution_cost().invoke_units,
    )?;
    if let Some(execute_time) = invoke_context.execute_time.as_mut() {
        execute_time.stop();
        invoke_context.timings.execute_us += execute_time.as_us();
    }
    let stricter_abi_and_runtime_constraints = invoke_context
        .get_feature_set()
        .stricter_abi_and_runtime_constraints;
    let account_data_direct_mapping = invoke_context.get_feature_set().account_data_direct_mapping;
    let check_aligned = invoke_context.get_check_aligned();

    let instruction = S::translate_instruction(
        instruction_addr,
        memory_mapping,
        invoke_context,
        check_aligned,
    )?;
    let transaction_context = &invoke_context.transaction_context;
    let instruction_context = transaction_context.get_current_instruction_context()?;
    let caller_program_id = instruction_context.get_program_key()?;
    let signers = S::translate_signers(
        caller_program_id,
        signers_seeds_addr,
        signers_seeds_len,
        memory_mapping,
        check_aligned,
    )?;
    check_authorized_program(&instruction.program_id, &instruction.data, invoke_context)?;
    invoke_context.prepare_next_instruction(instruction, &signers)?;

    let mut accounts = S::translate_accounts(
        account_infos_addr,
        account_infos_len,
        memory_mapping,
        invoke_context,
        check_aligned,
    )?;

    if stricter_abi_and_runtime_constraints {
        // before initiating CPI, the caller may have modified the
        // account (caller_account). We need to update the corresponding
        // BorrowedAccount (callee_account) so the callee can see the
        // changes.
        let transaction_context = &invoke_context.transaction_context;
        let instruction_context = transaction_context.get_current_instruction_context()?;
        for translated_account in accounts.iter_mut() {
            let callee_account = instruction_context
                .try_borrow_instruction_account(translated_account.index_in_caller)?;
            let update_caller = update_callee_account(
                memory_mapping,
                check_aligned,
                &translated_account.caller_account,
                callee_account,
                stricter_abi_and_runtime_constraints,
                account_data_direct_mapping,
            )?;
            translated_account.update_caller_account_region =
                translated_account.update_caller_account_info || update_caller;
        }
    }

    // Process the callee instruction
    let mut compute_units_consumed = 0;
    invoke_context
        .process_instruction(&mut compute_units_consumed, &mut ExecuteTimings::default())?;

    // re-bind to please the borrow checker
    let transaction_context = &invoke_context.transaction_context;
    let instruction_context = transaction_context.get_current_instruction_context()?;

    // CPI exit.
    //
    // Synchronize the callee's account changes so the caller can see them.
    for translated_account in accounts.iter_mut() {
        let mut callee_account = instruction_context
            .try_borrow_instruction_account(translated_account.index_in_caller)?;
        if translated_account.update_caller_account_info {
            update_caller_account(
                invoke_context,
                memory_mapping,
                check_aligned,
                &mut translated_account.caller_account,
                &mut callee_account,
                stricter_abi_and_runtime_constraints,
                account_data_direct_mapping,
            )?;
        }
    }

    if stricter_abi_and_runtime_constraints {
        for translated_account in accounts.iter() {
            let mut callee_account = instruction_context
                .try_borrow_instruction_account(translated_account.index_in_caller)?;
            if translated_account.update_caller_account_region {
                update_caller_account_region(
                    memory_mapping,
                    check_aligned,
                    &translated_account.caller_account,
                    &mut callee_account,
                    account_data_direct_mapping,
                )?;
            }
        }
    }

    invoke_context.execute_time = Some(Measure::start("execute"));
    Ok(SUCCESS)
}

/// Account data and metadata that has been translated from caller space.
pub struct TranslatedAccount<'a> {
    pub index_in_caller: IndexOfAccount,
    pub caller_account: CallerAccount<'a>,
    pub update_caller_account_region: bool,
    pub update_caller_account_info: bool,
}

fn translate_account_infos<'a, T, F>(
    account_infos_addr: u64,
    account_infos_len: u64,
    key_addr: F,
    memory_mapping: &'a MemoryMapping,
    invoke_context: &mut InvokeContext,
    check_aligned: bool,
) -> Result<(&'a [T], Vec<&'a Pubkey>), Error>
where
    F: Fn(&T) -> u64,
{
    let stricter_abi_and_runtime_constraints = invoke_context
        .get_feature_set()
        .stricter_abi_and_runtime_constraints;

    // In the same vein as the other check_account_info_pointer() checks, we don't lock
    // this pointer to a specific address but we don't want it to be inside accounts, or
    // callees might be able to write to the pointed memory.
    if stricter_abi_and_runtime_constraints
        && account_infos_addr
            .saturating_add(account_infos_len.saturating_mul(std::mem::size_of::<T>() as u64))
            >= ebpf::MM_INPUT_START
    {
        return Err(CpiError::InvalidPointer.into());
    }

    let account_infos = translate_slice::<T>(
        memory_mapping,
        account_infos_addr,
        account_infos_len,
        check_aligned,
    )?;
    check_account_infos(account_infos.len(), invoke_context)?;

    if invoke_context
        .get_feature_set()
        .increase_cpi_account_info_limit
    {
        let account_infos_bytes = account_infos.len().saturating_mul(ACCOUNT_INFO_BYTE_SIZE);

        consume_compute_meter(
            invoke_context,
            (account_infos_bytes as u64)
                .checked_div(invoke_context.get_execution_cost().cpi_bytes_per_unit)
                .unwrap_or(u64::MAX),
        )?;
    }

    let mut account_info_keys = Vec::with_capacity(account_infos_len as usize);
    #[allow(clippy::needless_range_loop)]
    for account_index in 0..account_infos_len as usize {
        #[allow(clippy::indexing_slicing)]
        let account_info = &account_infos[account_index];
        account_info_keys.push(translate_type::<Pubkey>(
            memory_mapping,
            key_addr(account_info),
            check_aligned,
        )?);
    }
    Ok((account_infos, account_info_keys))
}

// Finish translating accounts and build TranslatedAccount from CallerAccount.
fn translate_accounts_common<'a, T, F>(
    account_info_keys: &[&Pubkey],
    account_infos: &[T],
    account_infos_addr: u64,
    invoke_context: &mut InvokeContext,
    memory_mapping: &MemoryMapping<'_>,
    check_aligned: bool,
    do_translate: F,
) -> Result<Vec<TranslatedAccount<'a>>, Error>
where
    F: Fn(
        &InvokeContext,
        &MemoryMapping<'_>,
        bool,
        u64,
        &T,
        &SerializedAccountMetadata,
    ) -> Result<CallerAccount<'a>, Error>,
{
    let transaction_context = &invoke_context.transaction_context;
    let next_instruction_context = transaction_context.get_next_instruction_context()?;
    let next_instruction_accounts = next_instruction_context.instruction_accounts();
    let instruction_context = transaction_context.get_current_instruction_context()?;
    let mut accounts = Vec::with_capacity(next_instruction_accounts.len());

    // unwrapping here is fine: we're in a syscall and the method below fails
    // only outside syscalls
    let accounts_metadata = &invoke_context
        .get_syscall_context()
        .unwrap()
        .accounts_metadata;

    let stricter_abi_and_runtime_constraints = invoke_context
        .get_feature_set()
        .stricter_abi_and_runtime_constraints;
    let account_data_direct_mapping = invoke_context.get_feature_set().account_data_direct_mapping;

    for (instruction_account_index, instruction_account) in
        next_instruction_accounts.iter().enumerate()
    {
        if next_instruction_context
            .is_instruction_account_duplicate(instruction_account_index as IndexOfAccount)?
            .is_some()
        {
            continue; // Skip duplicate account
        }

        let index_in_caller = instruction_context
            .get_index_of_account_in_instruction(instruction_account.index_in_transaction)?;
        let callee_account = instruction_context.try_borrow_instruction_account(index_in_caller)?;
        let account_key = invoke_context
            .transaction_context
            .get_key_of_account_at_index(instruction_account.index_in_transaction)?;

        #[allow(deprecated)]
        if callee_account.is_executable() {
            // Use the known account
            consume_compute_meter(
                invoke_context,
                (callee_account.get_data().len() as u64)
                    .checked_div(invoke_context.get_execution_cost().cpi_bytes_per_unit)
                    .unwrap_or(u64::MAX),
            )?;
        } else if let Some(caller_account_index) =
            account_info_keys.iter().position(|key| *key == account_key)
        {
            let serialized_metadata =
                accounts_metadata
                    .get(index_in_caller as usize)
                    .ok_or_else(|| {
                        ic_msg!(
                            invoke_context,
                            "Internal error: index mismatch for account {}",
                            account_key
                        );
                        Box::new(InstructionError::MissingAccount)
                    })?;

            // build the CallerAccount corresponding to this account.
            if caller_account_index >= account_infos.len() {
                return Err(Box::new(CpiError::InvalidLength));
            }
            #[allow(clippy::indexing_slicing)]
            let caller_account =
                do_translate(
                    invoke_context,
                    memory_mapping,
                    check_aligned,
                    account_infos_addr.saturating_add(
                        caller_account_index.saturating_mul(mem::size_of::<T>()) as u64,
                    ),
                    &account_infos[caller_account_index],
                    serialized_metadata,
                )?;

            let update_caller = if stricter_abi_and_runtime_constraints {
                true
            } else {
                // before initiating CPI, the caller may have modified the
                // account (caller_account). We need to update the corresponding
                // BorrowedAccount (callee_account) so the callee can see the
                // changes.
                update_callee_account(
                    memory_mapping,
                    check_aligned,
                    &caller_account,
                    callee_account,
                    stricter_abi_and_runtime_constraints,
                    account_data_direct_mapping,
                )?
            };

            accounts.push(TranslatedAccount {
                index_in_caller,
                caller_account,
                update_caller_account_region: instruction_account.is_writable() || update_caller,
                update_caller_account_info: instruction_account.is_writable(),
            });
        } else {
            ic_msg!(
                invoke_context,
                "Instruction references an unknown account {}",
                account_key
            );
            return Err(Box::new(InstructionError::MissingAccount));
        }
    }

    Ok(accounts)
}

fn consume_compute_meter(invoke_context: &InvokeContext, amount: u64) -> Result<(), Error> {
    invoke_context.consume_checked(amount)?;
    Ok(())
}

// Update the given account before executing CPI.
//
// caller_account and callee_account describe the same account. At CPI entry
// caller_account might include changes the caller has made to the account
// before executing CPI.
//
// This method updates callee_account so the CPI callee can see the caller's
// changes.
//
// When true is returned, the caller account must be updated after CPI. This
// is only set for stricter_abi_and_runtime_constraints when the pointer may have changed.
fn update_callee_account(
    memory_mapping: &MemoryMapping,
    check_aligned: bool,
    caller_account: &CallerAccount,
    mut callee_account: BorrowedInstructionAccount<'_, '_>,
    stricter_abi_and_runtime_constraints: bool,
    account_data_direct_mapping: bool,
) -> Result<bool, Error> {
    let mut must_update_caller = false;

    if callee_account.get_lamports() != *caller_account.lamports {
        callee_account.set_lamports(*caller_account.lamports)?;
    }

    if stricter_abi_and_runtime_constraints {
        let prev_len = callee_account.get_data().len();
        let post_len = *caller_account.ref_to_len_in_vm as usize;
        if prev_len != post_len {
            let is_caller_loader_deprecated = !check_aligned;
            let address_space_reserved_for_account = if is_caller_loader_deprecated {
                caller_account.original_data_len
            } else {
                caller_account
                    .original_data_len
                    .saturating_add(MAX_PERMITTED_DATA_INCREASE)
            };
            if post_len > address_space_reserved_for_account {
                return Err(InstructionError::InvalidRealloc.into());
            }
            if !account_data_direct_mapping && post_len < prev_len {
                // If the account has been shrunk, we're going to zero the unused memory
                // *that was previously used*.
                let serialized_data = CallerAccount::get_serialized_data(
                    memory_mapping,
                    caller_account.vm_data_addr,
                    prev_len as u64,
                    stricter_abi_and_runtime_constraints,
                    account_data_direct_mapping,
                )?;
                serialized_data
                    .get_mut(post_len..)
                    .ok_or_else(|| Box::new(InstructionError::AccountDataTooSmall))?
                    .fill(0);
            }
            callee_account.set_data_length(post_len)?;
            // pointer to data may have changed, so caller must be updated
            must_update_caller = true;
        }
        if !account_data_direct_mapping && callee_account.can_data_be_changed().is_ok() {
            callee_account.set_data_from_slice(caller_account.serialized_data)?;
        }
    } else {
        // The redundant check helps to avoid the expensive data comparison if we can
        match callee_account.can_data_be_resized(caller_account.serialized_data.len()) {
            Ok(()) => callee_account.set_data_from_slice(caller_account.serialized_data)?,
            Err(err) if callee_account.get_data() != caller_account.serialized_data => {
                return Err(Box::new(err));
            }
            _ => {}
        }
    }

    // Change the owner at the end so that we are allowed to change the lamports and data before
    if callee_account.get_owner() != caller_account.owner {
        callee_account.set_owner(caller_account.owner.as_ref())?;
        // caller gave ownership and thus write access away, so caller must be updated
        must_update_caller = true;
    }

    Ok(must_update_caller)
}

fn update_caller_account_region(
    memory_mapping: &mut MemoryMapping,
    check_aligned: bool,
    caller_account: &CallerAccount,
    callee_account: &mut BorrowedInstructionAccount<'_, '_>,
    account_data_direct_mapping: bool,
) -> Result<(), Error> {
    let is_caller_loader_deprecated = !check_aligned;
    let address_space_reserved_for_account = if is_caller_loader_deprecated {
        caller_account.original_data_len
    } else {
        caller_account
            .original_data_len
            .saturating_add(MAX_PERMITTED_DATA_INCREASE)
    };

    if address_space_reserved_for_account > 0 {
        // We can trust vm_data_addr to point to the correct region because we
        // enforce that in CallerAccount::from_(sol_)account_info.
        let (region_index, region) = memory_mapping
            .find_region(caller_account.vm_data_addr)
            .ok_or_else(|| Box::new(InstructionError::MissingAccount))?;
        // vm_data_addr must always point to the beginning of the region
        debug_assert_eq!(region.vm_addr, caller_account.vm_data_addr);
        let mut new_region;
        if !account_data_direct_mapping {
            new_region = region.clone();
            modify_memory_region_of_account(callee_account, &mut new_region);
        } else {
            new_region = create_memory_region_of_account(callee_account, region.vm_addr)?;
        }
        memory_mapping.replace_region(region_index, new_region)?;
    }

    Ok(())
}

// Update the given account after executing CPI.
//
// caller_account and callee_account describe to the same account. At CPI exit
// callee_account might include changes the callee has made to the account
// after executing.
//
// This method updates caller_account so the CPI caller can see the callee's
// changes.
//
// Safety: Once `stricter_abi_and_runtime_constraints` is enabled all fields of [CallerAccount] used
// in this function should never point inside the address space reserved for
// accounts (regardless of the current size of an account).
fn update_caller_account(
    invoke_context: &InvokeContext,
    memory_mapping: &MemoryMapping<'_>,
    check_aligned: bool,
    caller_account: &mut CallerAccount<'_>,
    callee_account: &mut BorrowedInstructionAccount<'_, '_>,
    stricter_abi_and_runtime_constraints: bool,
    account_data_direct_mapping: bool,
) -> Result<(), Error> {
    *caller_account.lamports = callee_account.get_lamports();
    *caller_account.owner = *callee_account.get_owner();

    let prev_len = *caller_account.ref_to_len_in_vm as usize;
    let post_len = callee_account.get_data().len();
    let is_caller_loader_deprecated = !check_aligned;
    let address_space_reserved_for_account =
        if stricter_abi_and_runtime_constraints && is_caller_loader_deprecated {
            caller_account.original_data_len
        } else {
            caller_account
                .original_data_len
                .saturating_add(MAX_PERMITTED_DATA_INCREASE)
        };

    if post_len > address_space_reserved_for_account
        && (stricter_abi_and_runtime_constraints || prev_len != post_len)
    {
        let max_increase =
            address_space_reserved_for_account.saturating_sub(caller_account.original_data_len);
        ic_msg!(
            invoke_context,
            "Account data size realloc limited to {max_increase} in inner instructions",
        );
        return Err(Box::new(InstructionError::InvalidRealloc));
    }

    if prev_len != post_len {
        // when stricter_abi_and_runtime_constraints is enabled we don't cache the serialized data in
        // caller_account.serialized_data. See CallerAccount::from_account_info.
        if !(stricter_abi_and_runtime_constraints && account_data_direct_mapping) {
            // If the account has been shrunk, we're going to zero the unused memory
            // *that was previously used*.
            if post_len < prev_len {
                caller_account
                    .serialized_data
                    .get_mut(post_len..)
                    .ok_or_else(|| Box::new(InstructionError::AccountDataTooSmall))?
                    .fill(0);
            }
            // Set the length of caller_account.serialized_data to post_len.
            caller_account.serialized_data = CallerAccount::get_serialized_data(
                memory_mapping,
                caller_account.vm_data_addr,
                post_len as u64,
                stricter_abi_and_runtime_constraints,
                account_data_direct_mapping,
            )?;
        }
        // this is the len field in the AccountInfo::data slice
        *caller_account.ref_to_len_in_vm = post_len as u64;

        // this is the len field in the serialized parameters
        let serialized_len_ptr = translate_type_mut_for_cpi::<u64>(
            memory_mapping,
            caller_account
                .vm_data_addr
                .saturating_sub(std::mem::size_of::<u64>() as u64),
            check_aligned,
        )?;
        *serialized_len_ptr = post_len as u64;
    }

    if !(stricter_abi_and_runtime_constraints && account_data_direct_mapping) {
        // Propagate changes in the callee up to the caller.
        let to_slice = &mut caller_account.serialized_data;
        let from_slice = callee_account
            .get_data()
            .get(0..post_len)
            .ok_or(CpiError::InvalidLength)?;
        if to_slice.len() != from_slice.len() {
            return Err(Box::new(InstructionError::AccountDataTooSmall));
        }
        to_slice.copy_from_slice(from_slice);
    }

    Ok(())
}

#[allow(clippy::indexing_slicing)]
#[allow(clippy::arithmetic_side_effects)]
#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            invoke_context::{BpfAllocator, SerializedAccountMetadata, SyscallContext},
            memory::translate_type,
            with_mock_invoke_context_with_feature_set,
        },
        assert_matches::assert_matches,
        solana_account::{Account, AccountSharedData, ReadableAccount},
        solana_account_info::AccountInfo,
        solana_sbpf::{
            ebpf::MM_INPUT_START, memory_region::MemoryRegion, program::SBPFVersion, vm::Config,
        },
        solana_sdk_ids::{bpf_loader, system_program},
        solana_svm_feature_set::SVMFeatureSet,
        solana_transaction_context::{
            transaction_accounts::KeyedAccountSharedData, IndexOfAccount, InstructionAccount,
        },
        std::{
            cell::{Cell, RefCell},
            mem, ptr,
            rc::Rc,
            slice,
        },
        test_case::test_matrix,
    };

    macro_rules! mock_invoke_context {
        ($invoke_context:ident,
         $transaction_context:ident,
         $instruction_data:expr,
         $transaction_accounts:expr,
         $program_account:expr,
         $instruction_accounts:expr) => {
            let instruction_data = $instruction_data;
            let instruction_accounts = $instruction_accounts
                .iter()
                .map(|index_in_transaction| {
                    InstructionAccount::new(
                        *index_in_transaction as IndexOfAccount,
                        false,
                        $transaction_accounts[*index_in_transaction as usize].2,
                    )
                })
                .collect::<Vec<_>>();
            let transaction_accounts = $transaction_accounts
                .into_iter()
                .map(|a| (a.0, a.1))
                .collect::<Vec<KeyedAccountSharedData>>();
            let mut feature_set = SVMFeatureSet::all_enabled();
            feature_set.stricter_abi_and_runtime_constraints = false;
            let feature_set = &feature_set;
            with_mock_invoke_context_with_feature_set!(
                $invoke_context,
                $transaction_context,
                feature_set,
                transaction_accounts
            );
            $invoke_context
                .transaction_context
                .configure_next_instruction_for_tests(
                    $program_account,
                    instruction_accounts,
                    instruction_data.to_vec(),
                )
                .unwrap();
            $invoke_context.push().unwrap();
        };
    }

    macro_rules! borrow_instruction_account {
        ($borrowed_account:ident, $invoke_context:expr, $index:expr) => {
            let instruction_context = $invoke_context
                .transaction_context
                .get_current_instruction_context()
                .unwrap();
            let $borrowed_account = instruction_context
                .try_borrow_instruction_account($index)
                .unwrap();
        };
    }

    fn is_zeroed(data: &[u8]) -> bool {
        data.iter().all(|b| *b == 0)
    }

    struct MockCallerAccount {
        lamports: u64,
        owner: Pubkey,
        vm_addr: u64,
        data: Vec<u8>,
        len: u64,
        regions: Vec<MemoryRegion>,
        stricter_abi_and_runtime_constraints: bool,
    }

    impl MockCallerAccount {
        fn new(
            lamports: u64,
            owner: Pubkey,
            data: &[u8],
            stricter_abi_and_runtime_constraints: bool,
        ) -> MockCallerAccount {
            let vm_addr = MM_INPUT_START;
            let mut region_addr = vm_addr;
            let region_len = mem::size_of::<u64>()
                + if stricter_abi_and_runtime_constraints {
                    0
                } else {
                    data.len() + MAX_PERMITTED_DATA_INCREASE
                };
            let mut d = vec![0; region_len];
            let mut regions = vec![];

            // always write the [len] part even when stricter_abi_and_runtime_constraints
            unsafe { ptr::write_unaligned::<u64>(d.as_mut_ptr().cast(), data.len() as u64) };

            // write the account data when not stricter_abi_and_runtime_constraints
            if !stricter_abi_and_runtime_constraints {
                d[mem::size_of::<u64>()..][..data.len()].copy_from_slice(data);
            }

            // create a region for [len][data+realloc if !stricter_abi_and_runtime_constraints]
            regions.push(MemoryRegion::new_writable(&mut d[..region_len], vm_addr));
            region_addr += region_len as u64;

            if stricter_abi_and_runtime_constraints {
                // create a region for the directly mapped data
                regions.push(MemoryRegion::new_readonly(data, region_addr));
                region_addr += data.len() as u64;

                // create a region for the realloc padding
                regions.push(MemoryRegion::new_writable(
                    &mut d[mem::size_of::<u64>()..],
                    region_addr,
                ));
            } else {
                // caller_account.serialized_data must have the actual data length
                d.truncate(mem::size_of::<u64>() + data.len());
            }

            MockCallerAccount {
                lamports,
                owner,
                vm_addr,
                data: d,
                len: data.len() as u64,
                regions,
                stricter_abi_and_runtime_constraints,
            }
        }

        fn data_slice<'a>(&self) -> &'a [u8] {
            // lifetime crimes
            unsafe {
                slice::from_raw_parts(
                    self.data[mem::size_of::<u64>()..].as_ptr(),
                    self.data.capacity() - mem::size_of::<u64>(),
                )
            }
        }

        fn caller_account(&mut self) -> CallerAccount<'_> {
            let data = if self.stricter_abi_and_runtime_constraints {
                &mut []
            } else {
                &mut self.data[mem::size_of::<u64>()..]
            };
            CallerAccount {
                lamports: &mut self.lamports,
                owner: &mut self.owner,
                original_data_len: self.len as usize,
                serialized_data: data,
                vm_data_addr: self.vm_addr + mem::size_of::<u64>() as u64,
                ref_to_len_in_vm: &mut self.len,
            }
        }
    }

    struct MockAccountInfo<'a> {
        key: Pubkey,
        is_signer: bool,
        is_writable: bool,
        lamports: u64,
        data: &'a [u8],
        owner: Pubkey,
        executable: bool,
        _unused: u64,
    }

    impl MockAccountInfo<'_> {
        fn new(key: Pubkey, account: &AccountSharedData) -> MockAccountInfo<'_> {
            MockAccountInfo {
                key,
                is_signer: false,
                is_writable: false,
                lamports: account.lamports(),
                data: account.data(),
                owner: *account.owner(),
                executable: account.executable(),
                _unused: account.rent_epoch(),
            }
        }

        fn into_region(self, vm_addr: u64) -> (Vec<u8>, MemoryRegion, SerializedAccountMetadata) {
            let size = mem::size_of::<AccountInfo>()
                + mem::size_of::<Pubkey>() * 2
                + mem::size_of::<RcBox<RefCell<&mut u64>>>()
                + mem::size_of::<u64>()
                + mem::size_of::<RcBox<RefCell<&mut [u8]>>>()
                + self.data.len();
            let mut data = vec![0; size];

            let vm_addr = vm_addr as usize;
            let key_addr = vm_addr + mem::size_of::<AccountInfo>();
            let lamports_cell_addr = key_addr + mem::size_of::<Pubkey>();
            let lamports_addr = lamports_cell_addr + mem::size_of::<RcBox<RefCell<&mut u64>>>();
            let owner_addr = lamports_addr + mem::size_of::<u64>();
            let data_cell_addr = owner_addr + mem::size_of::<Pubkey>();
            let data_addr = data_cell_addr + mem::size_of::<RcBox<RefCell<&mut [u8]>>>();

            #[allow(deprecated)]
            #[allow(clippy::used_underscore_binding)]
            let info = AccountInfo {
                key: unsafe { (key_addr as *const Pubkey).as_ref() }.unwrap(),
                is_signer: self.is_signer,
                is_writable: self.is_writable,
                lamports: unsafe {
                    Rc::from_raw((lamports_cell_addr + RcBox::<&mut u64>::VALUE_OFFSET) as *const _)
                },
                data: unsafe {
                    Rc::from_raw((data_cell_addr + RcBox::<&mut [u8]>::VALUE_OFFSET) as *const _)
                },
                owner: unsafe { (owner_addr as *const Pubkey).as_ref() }.unwrap(),
                executable: self.executable,
                _unused: self._unused,
            };

            unsafe {
                ptr::write_unaligned(data.as_mut_ptr().cast(), info);
                ptr::write_unaligned(
                    (data.as_mut_ptr() as usize + key_addr - vm_addr) as *mut _,
                    self.key,
                );
                ptr::write_unaligned(
                    (data.as_mut_ptr() as usize + lamports_cell_addr - vm_addr) as *mut _,
                    RcBox::new(RefCell::new((lamports_addr as *mut u64).as_mut().unwrap())),
                );
                ptr::write_unaligned(
                    (data.as_mut_ptr() as usize + lamports_addr - vm_addr) as *mut _,
                    self.lamports,
                );
                ptr::write_unaligned(
                    (data.as_mut_ptr() as usize + owner_addr - vm_addr) as *mut _,
                    self.owner,
                );
                ptr::write_unaligned(
                    (data.as_mut_ptr() as usize + data_cell_addr - vm_addr) as *mut _,
                    RcBox::new(RefCell::new(slice::from_raw_parts_mut(
                        data_addr as *mut u8,
                        self.data.len(),
                    ))),
                );
                data[data_addr - vm_addr..].copy_from_slice(self.data);
            }

            let region = MemoryRegion::new_writable(data.as_mut_slice(), vm_addr as u64);
            (
                data,
                region,
                SerializedAccountMetadata {
                    original_data_len: self.data.len(),
                    vm_key_addr: key_addr as u64,
                    vm_lamports_addr: lamports_addr as u64,
                    vm_owner_addr: owner_addr as u64,
                    vm_data_addr: data_addr as u64,
                },
            )
        }
    }

    struct MockInstruction {
        program_id: Pubkey,
        accounts: Vec<AccountMeta>,
        data: Vec<u8>,
    }

    impl MockInstruction {
        fn into_region(self, vm_addr: u64) -> (Vec<u8>, MemoryRegion) {
            let accounts_len = mem::size_of::<AccountMeta>() * self.accounts.len();

            let size = mem::size_of::<StableInstruction>() + accounts_len + self.data.len();

            let mut data = vec![0; size];

            let vm_addr = vm_addr as usize;
            let accounts_addr = vm_addr + mem::size_of::<StableInstruction>();
            let data_addr = accounts_addr + accounts_len;

            let ins = Instruction {
                program_id: self.program_id,
                accounts: unsafe {
                    Vec::from_raw_parts(
                        accounts_addr as *mut _,
                        self.accounts.len(),
                        self.accounts.len(),
                    )
                },
                data: unsafe {
                    Vec::from_raw_parts(data_addr as *mut _, self.data.len(), self.data.len())
                },
            };
            let ins = StableInstruction::from(ins);

            unsafe {
                ptr::write_unaligned(data.as_mut_ptr().cast(), ins);
                data[accounts_addr - vm_addr..][..accounts_len].copy_from_slice(
                    slice::from_raw_parts(self.accounts.as_ptr().cast(), accounts_len),
                );
                data[data_addr - vm_addr..].copy_from_slice(&self.data);
            }

            let region = MemoryRegion::new_writable(data.as_mut_slice(), vm_addr as u64);
            (data, region)
        }
    }

    #[repr(C)]
    struct RcBox<T> {
        strong: Cell<usize>,
        weak: Cell<usize>,
        value: T,
    }

    impl<T> RcBox<T> {
        const VALUE_OFFSET: usize = mem::size_of::<Cell<usize>>() * 2;
        fn new(value: T) -> RcBox<T> {
            RcBox {
                strong: Cell::new(0),
                weak: Cell::new(0),
                value,
            }
        }
    }

    type TestTransactionAccount = (Pubkey, AccountSharedData, bool);

    fn transaction_with_one_writable_instruction_account(
        data: Vec<u8>,
    ) -> Vec<TestTransactionAccount> {
        let program_id = Pubkey::new_unique();
        let account = AccountSharedData::from(Account {
            lamports: 1,
            data,
            owner: program_id,
            executable: false,
            rent_epoch: 100,
        });
        vec![
            (
                program_id,
                AccountSharedData::from(Account {
                    lamports: 0,
                    data: vec![],
                    owner: bpf_loader::id(),
                    executable: true,
                    rent_epoch: 0,
                }),
                false,
            ),
            (Pubkey::new_unique(), account, true),
        ]
    }

    fn transaction_with_one_readonly_instruction_account(
        data: Vec<u8>,
    ) -> Vec<TestTransactionAccount> {
        let program_id = Pubkey::new_unique();
        let account_owner = Pubkey::new_unique();
        let account = AccountSharedData::from(Account {
            lamports: 1,
            data,
            owner: account_owner,
            executable: false,
            rent_epoch: 100,
        });
        vec![
            (
                program_id,
                AccountSharedData::from(Account {
                    lamports: 0,
                    data: vec![],
                    owner: bpf_loader::id(),
                    executable: true,
                    rent_epoch: 0,
                }),
                false,
            ),
            (Pubkey::new_unique(), account, true),
        ]
    }

    fn mock_signers(signers: &[&[u8]], vm_addr: u64) -> (Vec<u8>, MemoryRegion) {
        let vm_addr = vm_addr as usize;

        // calculate size
        let fat_ptr_size_of_slice = mem::size_of::<&[()]>(); // pointer size + length size
        let singers_length = signers.len();
        let sum_signers_data_length: usize = signers.iter().map(|s| s.len()).sum();

        // init data vec
        let total_size = fat_ptr_size_of_slice
            + singers_length * fat_ptr_size_of_slice
            + sum_signers_data_length;
        let mut data = vec![0; total_size];

        // data is composed by 3 parts
        // A.
        // [ singers address, singers length, ...,
        // B.                                      |
        //                                         signer1 address, signer1 length, signer2 address ...,
        //                                         ^ p1 --->
        // C.                                                                                           |
        //                                                                                              signer1 data, signer2 data, ... ]
        //                                                                                              ^ p2 --->

        // A.
        data[..fat_ptr_size_of_slice / 2]
            .clone_from_slice(&(fat_ptr_size_of_slice + vm_addr).to_le_bytes());
        data[fat_ptr_size_of_slice / 2..fat_ptr_size_of_slice]
            .clone_from_slice(&(singers_length).to_le_bytes());

        // B. + C.
        let (mut p1, mut p2) = (
            fat_ptr_size_of_slice,
            fat_ptr_size_of_slice + singers_length * fat_ptr_size_of_slice,
        );
        for signer in signers.iter() {
            let signer_length = signer.len();

            // B.
            data[p1..p1 + fat_ptr_size_of_slice / 2]
                .clone_from_slice(&(p2 + vm_addr).to_le_bytes());
            data[p1 + fat_ptr_size_of_slice / 2..p1 + fat_ptr_size_of_slice]
                .clone_from_slice(&(signer_length).to_le_bytes());
            p1 += fat_ptr_size_of_slice;

            // C.
            data[p2..p2 + signer_length].clone_from_slice(signer);
            p2 += signer_length;
        }

        let region = MemoryRegion::new_writable(data.as_mut_slice(), vm_addr as u64);
        (data, region)
    }

    #[test]
    fn test_translate_instruction() {
        let transaction_accounts =
            transaction_with_one_writable_instruction_account(b"foo".to_vec());
        mock_invoke_context!(
            invoke_context,
            transaction_context,
            b"instruction data",
            transaction_accounts,
            0,
            &[1]
        );

        let program_id = Pubkey::new_unique();
        let accounts = vec![AccountMeta {
            pubkey: Pubkey::new_unique(),
            is_signer: true,
            is_writable: false,
        }];
        let data = b"ins data".to_vec();
        let vm_addr = MM_INPUT_START;
        let (_mem, region) = MockInstruction {
            program_id,
            accounts: accounts.clone(),
            data: data.clone(),
        }
        .into_region(vm_addr);

        let config = Config {
            aligned_memory_mapping: false,
            ..Config::default()
        };
        let memory_mapping = MemoryMapping::new(vec![region], &config, SBPFVersion::V3).unwrap();

        let ins = translate_instruction_rust(
            vm_addr,
            &memory_mapping,
            &mut invoke_context,
            true, // check_aligned
        )
        .unwrap();
        assert_eq!(ins.program_id, program_id);
        assert_eq!(ins.accounts, accounts);
        assert_eq!(ins.data, data);
    }

    #[test]
    fn test_translate_signers() {
        let transaction_accounts =
            transaction_with_one_writable_instruction_account(b"foo".to_vec());
        mock_invoke_context!(
            invoke_context,
            transaction_context,
            b"instruction data",
            transaction_accounts,
            0,
            &[1]
        );

        let program_id = Pubkey::new_unique();
        let (derived_key, bump_seed) = Pubkey::find_program_address(&[b"foo"], &program_id);

        let vm_addr = MM_INPUT_START;
        let (_mem, region) = mock_signers(&[b"foo", &[bump_seed]], vm_addr);

        let config = Config {
            aligned_memory_mapping: false,
            ..Config::default()
        };
        let memory_mapping = MemoryMapping::new(vec![region], &config, SBPFVersion::V3).unwrap();

        let signers = translate_signers_rust(
            &program_id,
            vm_addr,
            1,
            &memory_mapping,
            true, // check_aligned
        )
        .unwrap();
        assert_eq!(signers[0], derived_key);
    }

    #[test]
    fn test_translate_accounts_rust() {
        let transaction_accounts =
            transaction_with_one_writable_instruction_account(b"foobar".to_vec());
        let account = transaction_accounts[1].1.clone();
        let key = transaction_accounts[1].0;
        let original_data_len = account.data().len();

        let vm_addr = MM_INPUT_START;
        let (_mem, region, account_metadata) =
            MockAccountInfo::new(key, &account).into_region(vm_addr);

        let config = Config {
            aligned_memory_mapping: false,
            ..Config::default()
        };
        let memory_mapping = MemoryMapping::new(vec![region], &config, SBPFVersion::V3).unwrap();

        mock_invoke_context!(
            invoke_context,
            transaction_context,
            b"instruction data",
            transaction_accounts,
            0,
            &[1, 1]
        );

        invoke_context
            .set_syscall_context(SyscallContext {
                allocator: BpfAllocator::new(solana_program_entrypoint::HEAP_LENGTH as u64),
                accounts_metadata: vec![account_metadata],
            })
            .unwrap();

        invoke_context
            .transaction_context
            .configure_next_instruction_for_tests(
                0,
                vec![
                    InstructionAccount::new(1, false, true),
                    InstructionAccount::new(1, false, true),
                ],
                vec![],
            )
            .unwrap();
        let accounts = translate_accounts_rust(
            vm_addr,
            1,
            &memory_mapping,
            &mut invoke_context,
            true, // check_aligned
        )
        .unwrap();
        assert_eq!(accounts.len(), 1);
        let caller_account = &accounts[0].caller_account;
        assert_eq!(caller_account.serialized_data, account.data());
        assert_eq!(caller_account.original_data_len, original_data_len);
    }

    #[test]
    fn test_caller_account_from_account_info() {
        let transaction_accounts =
            transaction_with_one_writable_instruction_account(b"foo".to_vec());
        let account = transaction_accounts[1].1.clone();
        mock_invoke_context!(
            invoke_context,
            transaction_context,
            b"instruction data",
            transaction_accounts,
            0,
            &[1]
        );

        let key = Pubkey::new_unique();
        let vm_addr = MM_INPUT_START;
        let (_mem, region, account_metadata) =
            MockAccountInfo::new(key, &account).into_region(vm_addr);

        let config = Config {
            aligned_memory_mapping: false,
            ..Config::default()
        };
        let memory_mapping = MemoryMapping::new(vec![region], &config, SBPFVersion::V3).unwrap();

        let account_info = translate_type::<AccountInfo>(&memory_mapping, vm_addr, false).unwrap();

        let caller_account = CallerAccount::from_account_info(
            &invoke_context,
            &memory_mapping,
            true, // check_aligned
            vm_addr,
            account_info,
            &account_metadata,
        )
        .unwrap();
        assert_eq!(*caller_account.lamports, account.lamports());
        assert_eq!(caller_account.owner, account.owner());
        assert_eq!(caller_account.original_data_len, account.data().len());
        assert_eq!(
            *caller_account.ref_to_len_in_vm as usize,
            account.data().len()
        );
        assert_eq!(caller_account.serialized_data, account.data());
    }

    #[test_matrix([false, true])]
    fn test_update_caller_account_lamports_owner(stricter_abi_and_runtime_constraints: bool) {
        let transaction_accounts = transaction_with_one_writable_instruction_account(vec![]);
        let account = transaction_accounts[1].1.clone();
        mock_invoke_context!(
            invoke_context,
            transaction_context,
            b"instruction data",
            transaction_accounts,
            0,
            &[1]
        );

        let mut mock_caller_account =
            MockCallerAccount::new(1234, *account.owner(), account.data(), false);

        let config = Config {
            aligned_memory_mapping: false,
            ..Config::default()
        };
        let memory_mapping = MemoryMapping::new(
            mock_caller_account.regions.split_off(0),
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        let mut caller_account = mock_caller_account.caller_account();
        let instruction_context = invoke_context
            .transaction_context
            .get_current_instruction_context()
            .unwrap();
        let mut callee_account = instruction_context
            .try_borrow_instruction_account(0)
            .unwrap();
        callee_account.set_lamports(42).unwrap();
        callee_account
            .set_owner(Pubkey::new_unique().as_ref())
            .unwrap();

        update_caller_account(
            &invoke_context,
            &memory_mapping,
            true, // check_aligned
            &mut caller_account,
            &mut callee_account,
            stricter_abi_and_runtime_constraints,
            stricter_abi_and_runtime_constraints,
        )
        .unwrap();

        assert_eq!(*caller_account.lamports, 42);
        assert_eq!(caller_account.owner, callee_account.get_owner());
    }

    #[test]
    fn test_update_caller_account_data() {
        let transaction_accounts =
            transaction_with_one_writable_instruction_account(b"foobar".to_vec());
        let account = transaction_accounts[1].1.clone();
        let original_data_len = account.data().len();

        mock_invoke_context!(
            invoke_context,
            transaction_context,
            b"instruction data",
            transaction_accounts,
            0,
            &[1]
        );

        let mut mock_caller_account =
            MockCallerAccount::new(account.lamports(), *account.owner(), account.data(), false);

        let config = Config {
            aligned_memory_mapping: false,
            ..Config::default()
        };
        let memory_mapping = MemoryMapping::new(
            mock_caller_account.regions.clone(),
            &config,
            SBPFVersion::V3,
        )
        .unwrap();

        let data_slice = mock_caller_account.data_slice();
        let len_ptr = unsafe {
            data_slice
                .as_ptr()
                .offset(-(mem::size_of::<u64>() as isize))
        };
        let serialized_len = || unsafe { *len_ptr.cast::<u64>() as usize };
        let mut caller_account = mock_caller_account.caller_account();
        let instruction_context = invoke_context
            .transaction_context
            .get_current_instruction_context()
            .unwrap();
        let mut callee_account = instruction_context
            .try_borrow_instruction_account(0)
            .unwrap();

        for (new_value, expected_realloc_size) in [
            (b"foo".to_vec(), MAX_PERMITTED_DATA_INCREASE + 3),
            (b"foobaz".to_vec(), MAX_PERMITTED_DATA_INCREASE),
            (b"foobazbad".to_vec(), MAX_PERMITTED_DATA_INCREASE - 3),
        ] {
            assert_eq!(caller_account.serialized_data, callee_account.get_data());
            callee_account.set_data_from_slice(&new_value).unwrap();

            update_caller_account(
                &invoke_context,
                &memory_mapping,
                true, // check_aligned
                &mut caller_account,
                &mut callee_account,
                false,
                false,
            )
            .unwrap();

            let data_len = callee_account.get_data().len();
            assert_eq!(data_len, *caller_account.ref_to_len_in_vm as usize);
            assert_eq!(data_len, serialized_len());
            assert_eq!(data_len, caller_account.serialized_data.len());
            assert_eq!(
                callee_account.get_data(),
                &caller_account.serialized_data[..data_len]
            );
            assert_eq!(data_slice[data_len..].len(), expected_realloc_size);
            assert!(is_zeroed(&data_slice[data_len..]));
        }

        callee_account
            .set_data_length(original_data_len + MAX_PERMITTED_DATA_INCREASE)
            .unwrap();
        update_caller_account(
            &invoke_context,
            &memory_mapping,
            true, // check_aligned
            &mut caller_account,
            &mut callee_account,
            false,
            false,
        )
        .unwrap();
        let data_len = callee_account.get_data().len();
        assert_eq!(data_slice[data_len..].len(), 0);
        assert!(is_zeroed(&data_slice[data_len..]));

        callee_account
            .set_data_length(original_data_len + MAX_PERMITTED_DATA_INCREASE + 1)
            .unwrap();
        assert_matches!(
            update_caller_account(
                &invoke_context,
                &memory_mapping,
                true, // check_aligned
                &mut caller_account,
                &mut callee_account,
                false,
                false,
            ),
            Err(error) if error.downcast_ref::<InstructionError>().unwrap() == &InstructionError::InvalidRealloc
        );

        // close the account
        callee_account.set_data_length(0).unwrap();
        callee_account
            .set_owner(system_program::id().as_ref())
            .unwrap();
        update_caller_account(
            &invoke_context,
            &memory_mapping,
            true, // check_aligned
            &mut caller_account,
            &mut callee_account,
            false,
            false,
        )
        .unwrap();
        let data_len = callee_account.get_data().len();
        assert_eq!(data_len, 0);
    }

    #[test_matrix([false, true])]
    fn test_update_callee_account_lamports_owner(stricter_abi_and_runtime_constraints: bool) {
        let transaction_accounts = transaction_with_one_writable_instruction_account(vec![]);
        let account = transaction_accounts[1].1.clone();

        mock_invoke_context!(
            invoke_context,
            transaction_context,
            b"instruction data",
            transaction_accounts,
            0,
            &[1]
        );

        let mut mock_caller_account =
            MockCallerAccount::new(1234, *account.owner(), account.data(), false);
        let config = Config {
            aligned_memory_mapping: false,
            ..Config::default()
        };
        let memory_mapping = MemoryMapping::new(
            mock_caller_account.regions.clone(),
            &config,
            SBPFVersion::V3,
        )
        .unwrap();
        let caller_account = mock_caller_account.caller_account();

        borrow_instruction_account!(callee_account, invoke_context, 0);

        *caller_account.lamports = 42;
        *caller_account.owner = Pubkey::new_unique();

        update_callee_account(
            &memory_mapping,
            true, // check_aligned
            &caller_account,
            callee_account,
            stricter_abi_and_runtime_constraints,
            true, // account_data_direct_mapping
        )
        .unwrap();

        borrow_instruction_account!(callee_account, invoke_context, 0);
        assert_eq!(callee_account.get_lamports(), 42);
        assert_eq!(caller_account.owner, callee_account.get_owner());
    }

    #[test_matrix([false, true])]
    fn test_update_callee_account_data_writable(stricter_abi_and_runtime_constraints: bool) {
        let transaction_accounts =
            transaction_with_one_writable_instruction_account(b"foobar".to_vec());
        let account = transaction_accounts[1].1.clone();

        mock_invoke_context!(
            invoke_context,
            transaction_context,
            b"instruction data",
            transaction_accounts,
            0,
            &[1]
        );

        let mut mock_caller_account =
            MockCallerAccount::new(1234, *account.owner(), account.data(), false);
        let config = Config {
            aligned_memory_mapping: false,
            ..Config::default()
        };
        let memory_mapping = MemoryMapping::new(
            mock_caller_account.regions.clone(),
            &config,
            SBPFVersion::V3,
        )
        .unwrap();
        let mut caller_account = mock_caller_account.caller_account();
        borrow_instruction_account!(callee_account, invoke_context, 0);

        // stricter_abi_and_runtime_constraints does not copy data in update_callee_account()
        caller_account.serialized_data[0] = b'b';
        update_callee_account(
            &memory_mapping,
            true, // check_aligned
            &caller_account,
            callee_account,
            false, // stricter_abi_and_runtime_constraints
            false, // account_data_direct_mapping
        )
        .unwrap();
        borrow_instruction_account!(callee_account, invoke_context, 0);
        assert_eq!(callee_account.get_data(), b"boobar");

        // growing resize
        let mut data = b"foobarbaz".to_vec();
        *caller_account.ref_to_len_in_vm = data.len() as u64;
        caller_account.serialized_data = &mut data;
        assert_eq!(
            update_callee_account(
                &memory_mapping,
                true, // check_aligned
                &caller_account,
                callee_account,
                stricter_abi_and_runtime_constraints,
                true, // account_data_direct_mapping
            )
            .unwrap(),
            stricter_abi_and_runtime_constraints,
        );

        // truncating resize
        let mut data = b"baz".to_vec();
        *caller_account.ref_to_len_in_vm = data.len() as u64;
        caller_account.serialized_data = &mut data;
        borrow_instruction_account!(callee_account, invoke_context, 0);
        assert_eq!(
            update_callee_account(
                &memory_mapping,
                true, // check_aligned
                &caller_account,
                callee_account,
                stricter_abi_and_runtime_constraints,
                true, // account_data_direct_mapping
            )
            .unwrap(),
            stricter_abi_and_runtime_constraints,
        );

        // close the account
        let mut data = Vec::new();
        caller_account.serialized_data = &mut data;
        *caller_account.ref_to_len_in_vm = 0;
        let mut owner = system_program::id();
        caller_account.owner = &mut owner;
        borrow_instruction_account!(callee_account, invoke_context, 0);
        update_callee_account(
            &memory_mapping,
            true, // check_aligned
            &caller_account,
            callee_account,
            stricter_abi_and_runtime_constraints,
            true, // account_data_direct_mapping
        )
        .unwrap();
        borrow_instruction_account!(callee_account, invoke_context, 0);
        assert_eq!(callee_account.get_data(), b"");

        // growing beyond address_space_reserved_for_account
        *caller_account.ref_to_len_in_vm = (7 + MAX_PERMITTED_DATA_INCREASE) as u64;
        let result = update_callee_account(
            &memory_mapping,
            true, // check_aligned
            &caller_account,
            callee_account,
            stricter_abi_and_runtime_constraints,
            true, // account_data_direct_mapping
        );
        if stricter_abi_and_runtime_constraints {
            assert_matches!(
                result,
                Err(error) if error.downcast_ref::<InstructionError>().unwrap() == &InstructionError::InvalidRealloc
            );
        } else {
            result.unwrap();
        }
    }

    #[test_matrix([false, true])]
    fn test_update_callee_account_data_readonly(stricter_abi_and_runtime_constraints: bool) {
        let transaction_accounts =
            transaction_with_one_readonly_instruction_account(b"foobar".to_vec());
        let account = transaction_accounts[1].1.clone();

        mock_invoke_context!(
            invoke_context,
            transaction_context,
            b"instruction data",
            transaction_accounts,
            0,
            &[1]
        );

        let mut mock_caller_account =
            MockCallerAccount::new(1234, *account.owner(), account.data(), false);
        let config = Config {
            aligned_memory_mapping: false,
            ..Config::default()
        };
        let memory_mapping = MemoryMapping::new(
            mock_caller_account.regions.clone(),
            &config,
            SBPFVersion::V3,
        )
        .unwrap();
        let mut caller_account = mock_caller_account.caller_account();
        borrow_instruction_account!(callee_account, invoke_context, 0);

        // stricter_abi_and_runtime_constraints does not copy data in update_callee_account()
        caller_account.serialized_data[0] = b'b';
        assert_matches!(
            update_callee_account(
                &memory_mapping,
                true, // check_aligned
                &caller_account,
                callee_account,
                false, // stricter_abi_and_runtime_constraints
                false, // account_data_direct_mapping
            ),
            Err(error) if error.downcast_ref::<InstructionError>().unwrap() == &InstructionError::ExternalAccountDataModified
        );

        // growing resize
        let mut data = b"foobarbaz".to_vec();
        *caller_account.ref_to_len_in_vm = data.len() as u64;
        caller_account.serialized_data = &mut data;
        borrow_instruction_account!(callee_account, invoke_context, 0);
        assert_matches!(
            update_callee_account(
                &memory_mapping,
                true, // check_aligned
                &caller_account,
                callee_account,
                stricter_abi_and_runtime_constraints,
                true, // account_data_direct_mapping
            ),
            Err(error) if error.downcast_ref::<InstructionError>().unwrap() == &InstructionError::AccountDataSizeChanged
        );

        // truncating resize
        let mut data = b"baz".to_vec();
        *caller_account.ref_to_len_in_vm = data.len() as u64;
        caller_account.serialized_data = &mut data;
        borrow_instruction_account!(callee_account, invoke_context, 0);
        assert_matches!(
            update_callee_account(
                &memory_mapping,
                true, // check_aligned
                &caller_account,
                callee_account,
                stricter_abi_and_runtime_constraints,
                true, // account_data_direct_mapping
            ),
            Err(error) if error.downcast_ref::<InstructionError>().unwrap() == &InstructionError::AccountDataSizeChanged
        );
    }
}
