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
const MAX_CPI_ACCOUNT_INFOS: usize = 128;

/// Check that an account info pointer field points to the expected address
fn check_account_info_pointer(
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
    ) -> Result<&'a mut [u8], Box<dyn std::error::Error>> {
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
    ) -> Result<CallerAccount<'a>, Box<dyn std::error::Error>> {
        use crate::memory::{translate_type, translate_type_mut_for_cpi};

        let stricter_abi_and_runtime_constraints = invoke_context
            .get_feature_set()
            .stricter_abi_and_runtime_constraints;

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
            let ref_to_len_in_vm =
                translate_type_mut_for_cpi::<u64>(memory_mapping, vm_len_addr, false)?;
            let vm_data_addr = data.as_ptr() as u64;
            let serialized_data = CallerAccount::get_serialized_data(
                memory_mapping,
                vm_data_addr,
                data.len() as u64,
                stricter_abi_and_runtime_constraints,
                invoke_context.account_data_direct_mapping,
            )?;
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
    pub fn from_sol_account_info(
        invoke_context: &InvokeContext,
        memory_mapping: &solana_sbpf::memory_region::MemoryMapping<'_>,
        check_aligned: bool,
        vm_addr: u64,
        account_info: &SolAccountInfo,
        account_metadata: &crate::invoke_context::SerializedAccountMetadata,
    ) -> Result<CallerAccount<'a>, Box<dyn std::error::Error>> {
        use crate::memory::translate_type_mut_for_cpi;

        let stricter_abi_and_runtime_constraints = invoke_context
            .get_feature_set()
            .stricter_abi_and_runtime_constraints;

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
            invoke_context.account_data_direct_mapping,
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
