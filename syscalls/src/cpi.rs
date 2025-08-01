use {
    super::*,
    crate::{translate_inner, translate_slice_inner, translate_type_inner},
    solana_instruction::Instruction,
    solana_loader_v3_interface::instruction as bpf_loader_upgradeable,
    solana_measure::measure::Measure,
    solana_program_runtime::{
        invoke_context::SerializedAccountMetadata, serialization::create_memory_region_of_account,
    },
    solana_sbpf::ebpf,
    solana_stable_layout::stable_instruction::StableInstruction,
    solana_transaction_context::BorrowedAccount,
    std::mem,
};

const MAX_CPI_INSTRUCTION_DATA_LEN: u64 = 10 * 1024;
const MAX_CPI_INSTRUCTION_ACCOUNTS: u8 = u8::MAX;
const MAX_CPI_ACCOUNT_INFOS: usize = 128;

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
        return Err(SyscallError::InvalidPointer.into());
    }
    Ok(())
}

// This version is missing lifetime 'a of the return type in the parameter &MemoryMapping.
fn translate_type_mut<'a, T>(
    memory_mapping: &MemoryMapping,
    vm_addr: u64,
    check_aligned: bool,
) -> Result<&'a mut T, Error> {
    translate_type_inner!(memory_mapping, AccessType::Store, vm_addr, T, check_aligned)
}
// This version is missing the lifetime 'a of the return type in the parameter &MemoryMapping.
fn translate_slice_mut<'a, T>(
    memory_mapping: &MemoryMapping,
    vm_addr: u64,
    len: u64,
    check_aligned: bool,
) -> Result<&'a mut [T], Error> {
    translate_slice_inner!(
        memory_mapping,
        AccessType::Store,
        vm_addr,
        len,
        T,
        check_aligned,
    )
}

/// Host side representation of AccountInfo or SolAccountInfo passed to the CPI syscall.
///
/// At the start of a CPI, this can be different from the data stored in the
/// corresponding BorrowedAccount, and needs to be synched.
struct CallerAccount<'a> {
    lamports: &'a mut u64,
    owner: &'a mut Pubkey,
    // The original data length of the account at the start of the current
    // instruction. We use this to determine wether an account was shrunk or
    // grown before or after CPI, and to derive the vm address of the realloc
    // region.
    original_data_len: usize,
    // This points to the data section for this account, as serialized and
    // mapped inside the vm (see serialize_parameters() in
    // BpfExecutor::execute).
    //
    // This is only set when direct mapping is off (see the relevant comment in
    // CallerAccount::from_account_info).
    serialized_data: &'a mut [u8],
    // Given the corresponding input AccountInfo::data, vm_data_addr points to
    // the pointer field and ref_to_len_in_vm points to the length field.
    vm_data_addr: u64,
    ref_to_len_in_vm: &'a mut u64,
}

impl<'a> CallerAccount<'a> {
    // Create a CallerAccount given an AccountInfo.
    fn from_account_info(
        invoke_context: &InvokeContext,
        memory_mapping: &MemoryMapping<'_>,
        check_aligned: bool,
        _vm_addr: u64,
        account_info: &AccountInfo,
        account_metadata: &SerializedAccountMetadata,
    ) -> Result<CallerAccount<'a>, Error> {
        let direct_mapping = invoke_context
            .get_feature_set()
            .bpf_account_data_direct_mapping;

        if direct_mapping {
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
            if direct_mapping {
                if account_info.lamports.as_ptr() as u64 >= ebpf::MM_INPUT_START {
                    return Err(SyscallError::InvalidPointer.into());
                }

                check_account_info_pointer(
                    invoke_context,
                    *ptr,
                    account_metadata.vm_lamports_addr,
                    "lamports",
                )?;
            }
            translate_type_mut::<u64>(memory_mapping, *ptr, check_aligned)?
        };

        let owner = translate_type_mut::<Pubkey>(
            memory_mapping,
            account_info.owner as *const _ as u64,
            check_aligned,
        )?;

        let (serialized_data, vm_data_addr, ref_to_len_in_vm) = {
            if direct_mapping && account_info.data.as_ptr() as u64 >= ebpf::MM_INPUT_START {
                return Err(SyscallError::InvalidPointer.into());
            }

            // Double translate data out of RefCell
            let data = *translate_type::<&[u8]>(
                memory_mapping,
                account_info.data.as_ptr() as *const _ as u64,
                check_aligned,
            )?;
            if direct_mapping {
                check_account_info_pointer(
                    invoke_context,
                    data.as_ptr() as u64,
                    account_metadata.vm_data_addr,
                    "data",
                )?;
            }

            consume_compute_meter(
                invoke_context,
                (data.len() as u64)
                    .checked_div(invoke_context.get_execution_cost().cpi_bytes_per_unit)
                    .unwrap_or(u64::MAX),
            )?;

            let vm_len_addr = (account_info.data.as_ptr() as *const u64 as u64)
                .saturating_add(size_of::<u64>() as u64);
            if direct_mapping {
                // In the same vein as the other check_account_info_pointer() checks, we don't lock
                // this pointer to a specific address but we don't want it to be inside accounts, or
                // callees might be able to write to the pointed memory.
                if vm_len_addr >= ebpf::MM_INPUT_START {
                    return Err(SyscallError::InvalidPointer.into());
                }
            }
            let ref_to_len_in_vm = translate_type_mut::<u64>(memory_mapping, vm_len_addr, false)?;
            let vm_data_addr = data.as_ptr() as u64;

            let serialized_data = if direct_mapping {
                // when direct mapping is enabled, the permissions on the
                // realloc region can change during CPI so we must delay
                // translating until when we know whether we're going to mutate
                // the realloc region or not. Consider this case:
                //
                // [caller can't write to an account] <- we are here
                // [callee grows and assigns account to the caller]
                // [caller can now write to the account]
                //
                // If we always translated the realloc area here, we'd get a
                // memory access violation since we can't write to the account
                // _yet_, but we will be able to once the caller returns.
                &mut []
            } else {
                translate_slice_mut::<u8>(
                    memory_mapping,
                    vm_data_addr,
                    data.len() as u64,
                    check_aligned,
                )?
            };
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
        memory_mapping: &MemoryMapping<'_>,
        check_aligned: bool,
        vm_addr: u64,
        account_info: &SolAccountInfo,
        account_metadata: &SerializedAccountMetadata,
    ) -> Result<CallerAccount<'a>, Error> {
        let direct_mapping = invoke_context
            .get_feature_set()
            .bpf_account_data_direct_mapping;

        if direct_mapping {
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
        let lamports =
            translate_type_mut::<u64>(memory_mapping, account_info.lamports_addr, check_aligned)?;
        let owner =
            translate_type_mut::<Pubkey>(memory_mapping, account_info.owner_addr, check_aligned)?;

        consume_compute_meter(
            invoke_context,
            account_info
                .data_len
                .checked_div(invoke_context.get_execution_cost().cpi_bytes_per_unit)
                .unwrap_or(u64::MAX),
        )?;

        let serialized_data = if direct_mapping {
            // See comment in CallerAccount::from_account_info()
            &mut []
        } else {
            translate_slice_mut::<u8>(
                memory_mapping,
                account_info.data_addr,
                account_info.data_len,
                check_aligned,
            )?
        };

        // we already have the host addr we want: &mut account_info.data_len.
        // The account info might be read only in the vm though, so we translate
        // to ensure we can write. This is tested by programs/sbf/rust/ro_modify
        // which puts SolAccountInfo in rodata.
        let vm_len_addr = vm_addr
            .saturating_add(&account_info.data_len as *const u64 as u64)
            .saturating_sub(account_info as *const _ as *const u64 as u64);
        let ref_to_len_in_vm = translate_type_mut::<u64>(memory_mapping, vm_len_addr, false)?;

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

struct TranslatedAccount<'a> {
    index_in_caller: IndexOfAccount,
    caller_account: CallerAccount<'a>,
    update_caller_account_region: bool,
    update_caller_account_info: bool,
}

/// Implemented by language specific data structure translators
trait SyscallInvokeSigned {
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

declare_builtin_function!(
    /// Cross-program invocation called from Rust
    SyscallInvokeSignedRust,
    fn rust(
        invoke_context: &mut InvokeContext,
        instruction_addr: u64,
        account_infos_addr: u64,
        account_infos_len: u64,
        signers_seeds_addr: u64,
        signers_seeds_len: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        cpi_common::<Self>(
            invoke_context,
            instruction_addr,
            account_infos_addr,
            account_infos_len,
            signers_seeds_addr,
            signers_seeds_len,
            memory_mapping,
        )
    }
);

impl SyscallInvokeSigned for SyscallInvokeSignedRust {
    fn translate_instruction(
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
        )?
        .to_vec();

        check_instruction_size(account_metas.len(), data.len(), invoke_context)?;

        if invoke_context.get_feature_set().loosen_cpi_size_restriction {
            consume_compute_meter(
                invoke_context,
                (data.len() as u64)
                    .checked_div(invoke_context.get_execution_cost().cpi_bytes_per_unit)
                    .unwrap_or(u64::MAX),
            )?;
        }

        let mut accounts = Vec::with_capacity(account_metas.len());
        #[allow(clippy::needless_range_loop)]
        for account_index in 0..account_metas.len() {
            #[allow(clippy::indexing_slicing)]
            let account_meta = &account_metas[account_index];
            if unsafe {
                std::ptr::read_volatile(&account_meta.is_signer as *const _ as *const u8) > 1
                    || std::ptr::read_volatile(&account_meta.is_writable as *const _ as *const u8)
                        > 1
            } {
                return Err(Box::new(InstructionError::InvalidArgument));
            }
            accounts.push(account_meta.clone());
        }

        Ok(Instruction {
            accounts,
            data,
            program_id: ix.program_id,
        })
    }

    fn translate_accounts<'a>(
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

        translate_and_update_accounts(
            &account_info_keys,
            account_infos,
            account_infos_addr,
            invoke_context,
            memory_mapping,
            check_aligned,
            CallerAccount::from_account_info,
        )
    }

    fn translate_signers(
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
                return Err(Box::new(SyscallError::TooManySigners));
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
                        untranslated_seed.translate(memory_mapping, check_aligned)
                    })
                    .collect::<Result<Vec<_>, Error>>()?;
                let signer = Pubkey::create_program_address(&seeds, program_id)
                    .map_err(SyscallError::BadSeeds)?;
                signers.push(signer);
            }
            Ok(signers)
        } else {
            Ok(vec![])
        }
    }
}

/// Rust representation of C's SolInstruction
#[derive(Debug)]
#[repr(C)]
struct SolInstruction {
    program_id_addr: u64,
    accounts_addr: u64,
    accounts_len: u64,
    data_addr: u64,
    data_len: u64,
}

/// Rust representation of C's SolAccountMeta
#[derive(Debug)]
#[repr(C)]
struct SolAccountMeta {
    pubkey_addr: u64,
    is_writable: bool,
    is_signer: bool,
}

/// Rust representation of C's SolAccountInfo
#[derive(Debug)]
#[repr(C)]
struct SolAccountInfo {
    key_addr: u64,
    lamports_addr: u64,
    data_len: u64,
    data_addr: u64,
    owner_addr: u64,
    rent_epoch: u64,
    is_signer: bool,
    is_writable: bool,
    executable: bool,
}

/// Rust representation of C's SolSignerSeed
#[derive(Debug)]
#[repr(C)]
struct SolSignerSeedC {
    addr: u64,
    len: u64,
}

/// Rust representation of C's SolSignerSeeds
#[derive(Debug)]
#[repr(C)]
struct SolSignerSeedsC {
    addr: u64,
    len: u64,
}

declare_builtin_function!(
    /// Cross-program invocation called from C
    SyscallInvokeSignedC,
    fn rust(
        invoke_context: &mut InvokeContext,
        instruction_addr: u64,
        account_infos_addr: u64,
        account_infos_len: u64,
        signers_seeds_addr: u64,
        signers_seeds_len: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        cpi_common::<Self>(
            invoke_context,
            instruction_addr,
            account_infos_addr,
            account_infos_len,
            signers_seeds_addr,
            signers_seeds_len,
            memory_mapping,
        )
    }
);

impl SyscallInvokeSigned for SyscallInvokeSignedC {
    fn translate_instruction(
        addr: u64,
        memory_mapping: &MemoryMapping,
        invoke_context: &mut InvokeContext,
        check_aligned: bool,
    ) -> Result<Instruction, Error> {
        let ix_c = translate_type::<SolInstruction>(memory_mapping, addr, check_aligned)?;

        let program_id =
            translate_type::<Pubkey>(memory_mapping, ix_c.program_id_addr, check_aligned)?;
        let account_metas = translate_slice::<SolAccountMeta>(
            memory_mapping,
            ix_c.accounts_addr,
            ix_c.accounts_len,
            check_aligned,
        )?;
        let data =
            translate_slice::<u8>(memory_mapping, ix_c.data_addr, ix_c.data_len, check_aligned)?
                .to_vec();

        check_instruction_size(ix_c.accounts_len as usize, data.len(), invoke_context)?;

        if invoke_context.get_feature_set().loosen_cpi_size_restriction {
            consume_compute_meter(
                invoke_context,
                (data.len() as u64)
                    .checked_div(invoke_context.get_execution_cost().cpi_bytes_per_unit)
                    .unwrap_or(u64::MAX),
            )?;
        }

        let mut accounts = Vec::with_capacity(ix_c.accounts_len as usize);
        #[allow(clippy::needless_range_loop)]
        for account_index in 0..ix_c.accounts_len as usize {
            #[allow(clippy::indexing_slicing)]
            let account_meta = &account_metas[account_index];
            if unsafe {
                std::ptr::read_volatile(&account_meta.is_signer as *const _ as *const u8) > 1
                    || std::ptr::read_volatile(&account_meta.is_writable as *const _ as *const u8)
                        > 1
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
            data,
            program_id: *program_id,
        })
    }

    fn translate_accounts<'a>(
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

        translate_and_update_accounts(
            &account_info_keys,
            account_infos,
            account_infos_addr,
            invoke_context,
            memory_mapping,
            check_aligned,
            CallerAccount::from_sol_account_info,
        )
    }

    fn translate_signers(
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
                return Err(Box::new(SyscallError::TooManySigners));
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
                            translate_slice::<u8>(
                                memory_mapping,
                                seed.addr,
                                seed.len,
                                check_aligned,
                            )
                        })
                        .collect::<Result<Vec<_>, Error>>()?;
                    Pubkey::create_program_address(&seeds_bytes, program_id)
                        .map_err(|err| Box::new(SyscallError::BadSeeds(err)) as Error)
                })
                .collect::<Result<Vec<_>, Error>>()?)
        } else {
            Ok(vec![])
        }
    }
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
    let direct_mapping = invoke_context
        .get_feature_set()
        .bpf_account_data_direct_mapping;

    // In the same vein as the other check_account_info_pointer() checks, we don't lock
    // this pointer to a specific address but we don't want it to be inside accounts, or
    // callees might be able to write to the pointed memory.
    if direct_mapping
        && account_infos_addr
            .saturating_add(account_infos_len.saturating_mul(std::mem::size_of::<T>() as u64))
            >= ebpf::MM_INPUT_START
    {
        return Err(SyscallError::InvalidPointer.into());
    }

    let account_infos = translate_slice::<T>(
        memory_mapping,
        account_infos_addr,
        account_infos_len,
        check_aligned,
    )?;
    check_account_infos(account_infos.len(), invoke_context)?;
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

// Finish translating accounts, build CallerAccount values and update callee
// accounts in preparation of executing the callee.
fn translate_and_update_accounts<'a, T, F>(
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
    let next_instruction_accounts = transaction_context
        .get_next_instruction_context()?
        .instruction_accounts();
    let instruction_context = transaction_context.get_current_instruction_context()?;
    let mut accounts = Vec::with_capacity(next_instruction_accounts.len());

    // unwrapping here is fine: we're in a syscall and the method below fails
    // only outside syscalls
    let accounts_metadata = &invoke_context
        .get_syscall_context()
        .unwrap()
        .accounts_metadata;

    let direct_mapping = invoke_context
        .get_feature_set()
        .bpf_account_data_direct_mapping;

    for (instruction_account_index, instruction_account) in
        next_instruction_accounts.iter().enumerate()
    {
        if instruction_account_index as IndexOfAccount != instruction_account.index_in_callee {
            continue; // Skip duplicate account
        }

        let index_in_caller = instruction_context
            .get_index_of_account_in_instruction(instruction_account.index_in_transaction)?;
        let callee_account = instruction_context
            .try_borrow_instruction_account(transaction_context, index_in_caller)?;
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
                return Err(Box::new(SyscallError::InvalidLength));
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

            // before initiating CPI, the caller may have modified the
            // account (caller_account). We need to update the corresponding
            // BorrowedAccount (callee_account) so the callee can see the
            // changes.
            let update_caller = update_callee_account(
                &caller_account,
                callee_account,
                direct_mapping,
                check_aligned,
            )?;

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

fn check_instruction_size(
    num_accounts: usize,
    data_len: usize,
    invoke_context: &mut InvokeContext,
) -> Result<(), Error> {
    if invoke_context.get_feature_set().loosen_cpi_size_restriction {
        let data_len = data_len as u64;
        let max_data_len = MAX_CPI_INSTRUCTION_DATA_LEN;
        if data_len > max_data_len {
            return Err(Box::new(SyscallError::MaxInstructionDataLenExceeded {
                data_len,
                max_data_len,
            }));
        }

        let num_accounts = num_accounts as u64;
        let max_accounts = MAX_CPI_INSTRUCTION_ACCOUNTS as u64;
        if num_accounts > max_accounts {
            return Err(Box::new(SyscallError::MaxInstructionAccountsExceeded {
                num_accounts,
                max_accounts,
            }));
        }
    } else {
        let max_size = invoke_context.get_compute_budget().max_cpi_instruction_size;
        let size = num_accounts
            .saturating_mul(size_of::<AccountMeta>())
            .saturating_add(data_len);
        if size > max_size {
            return Err(Box::new(SyscallError::InstructionTooLarge(size, max_size)));
        }
    }
    Ok(())
}

fn check_account_infos(
    num_account_infos: usize,
    invoke_context: &mut InvokeContext,
) -> Result<(), Error> {
    if invoke_context.get_feature_set().loosen_cpi_size_restriction {
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
            return Err(Box::new(SyscallError::MaxInstructionAccountInfosExceeded {
                num_account_infos,
                max_account_infos,
            }));
        }
    } else {
        let adjusted_len = num_account_infos.saturating_mul(size_of::<Pubkey>());

        if adjusted_len > invoke_context.get_compute_budget().max_cpi_instruction_size {
            // Cap the number of account_infos a caller can pass to approximate
            // maximum that accounts that could be passed in an instruction
            return Err(Box::new(SyscallError::TooManyAccounts));
        };
    }
    Ok(())
}

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
        return Err(Box::new(SyscallError::ProgramNotSupported(*program_id)));
    }
    Ok(())
}

/// Call process instruction, common to both Rust and C
fn cpi_common<S: SyscallInvokeSigned>(
    invoke_context: &mut InvokeContext,
    instruction_addr: u64,
    account_infos_addr: u64,
    account_infos_len: u64,
    signers_seeds_addr: u64,
    signers_seeds_len: u64,
    memory_mapping: &mut MemoryMapping,
) -> Result<u64, Error> {
    let check_aligned = invoke_context.get_check_aligned();

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

    let instruction = S::translate_instruction(
        instruction_addr,
        memory_mapping,
        invoke_context,
        check_aligned,
    )?;
    let transaction_context = &invoke_context.transaction_context;
    let instruction_context = transaction_context.get_current_instruction_context()?;
    let caller_program_id = instruction_context.get_last_program_key(transaction_context)?;
    let signers = S::translate_signers(
        caller_program_id,
        signers_seeds_addr,
        signers_seeds_len,
        memory_mapping,
        check_aligned,
    )?;
    check_authorized_program(&instruction.program_id, &instruction.data, invoke_context)?;
    invoke_context.prepare_next_instruction(&instruction, &signers)?;

    let mut accounts = S::translate_accounts(
        account_infos_addr,
        account_infos_len,
        memory_mapping,
        invoke_context,
        check_aligned,
    )?;

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
    let direct_mapping = invoke_context
        .get_feature_set()
        .bpf_account_data_direct_mapping;

    for translate_account in accounts.iter_mut() {
        let mut callee_account = instruction_context.try_borrow_instruction_account(
            transaction_context,
            translate_account.index_in_caller,
        )?;
        if translate_account.update_caller_account_info {
            update_caller_account(
                invoke_context,
                memory_mapping,
                check_aligned,
                &mut translate_account.caller_account,
                &mut callee_account,
                direct_mapping,
            )?;
        }
    }

    if direct_mapping {
        for translate_account in accounts.iter() {
            let mut callee_account = instruction_context.try_borrow_instruction_account(
                transaction_context,
                translate_account.index_in_caller,
            )?;
            if translate_account.update_caller_account_region {
                update_caller_account_region(
                    memory_mapping,
                    check_aligned,
                    &translate_account.caller_account,
                    &mut callee_account,
                )?;
            }
        }
    }

    invoke_context.execute_time = Some(Measure::start("execute"));
    Ok(SUCCESS)
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
// is only set for direct mapping when the pointer may have changed.
fn update_callee_account(
    caller_account: &CallerAccount,
    mut callee_account: BorrowedAccount<'_>,
    direct_mapping: bool,
    check_aligned: bool,
) -> Result<bool, Error> {
    let mut must_update_caller = false;

    if callee_account.get_lamports() != *caller_account.lamports {
        callee_account.set_lamports(*caller_account.lamports)?;
    }

    if direct_mapping {
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
            callee_account.set_data_length(post_len)?;
            // pointer to data may have changed, so caller must be updated
            must_update_caller = true;
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
    callee_account: &mut BorrowedAccount<'_>,
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
        let new_region = create_memory_region_of_account(callee_account, region.vm_addr)?;
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
// Safety: Once `direct_mapping` is enabled all fields of [CallerAccount] used
// in this function should never point inside the address space reserved for
// accounts (regardless of the current size of an account).
fn update_caller_account(
    invoke_context: &InvokeContext,
    memory_mapping: &MemoryMapping<'_>,
    check_aligned: bool,
    caller_account: &mut CallerAccount<'_>,
    callee_account: &mut BorrowedAccount<'_>,
    direct_mapping: bool,
) -> Result<(), Error> {
    *caller_account.lamports = callee_account.get_lamports();
    *caller_account.owner = *callee_account.get_owner();

    let prev_len = *caller_account.ref_to_len_in_vm as usize;
    let post_len = callee_account.get_data().len();
    let is_caller_loader_deprecated = !check_aligned;
    let address_space_reserved_for_account = if direct_mapping && is_caller_loader_deprecated {
        caller_account.original_data_len
    } else {
        caller_account
            .original_data_len
            .saturating_add(MAX_PERMITTED_DATA_INCREASE)
    };

    if post_len > address_space_reserved_for_account && (direct_mapping || prev_len != post_len) {
        let max_increase =
            address_space_reserved_for_account.saturating_sub(caller_account.original_data_len);
        ic_msg!(
            invoke_context,
            "Account data size realloc limited to {max_increase} in inner instructions",
        );
        return Err(Box::new(InstructionError::InvalidRealloc));
    }

    if prev_len != post_len {
        // when direct mapping is enabled we don't cache the serialized data in
        // caller_account.serialized_data. See CallerAccount::from_account_info.
        if !direct_mapping {
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
            caller_account.serialized_data = translate_slice_mut::<u8>(
                memory_mapping,
                caller_account.vm_data_addr,
                post_len as u64,
                false, // Don't care since it is byte aligned
            )?;
        }
        // this is the len field in the AccountInfo::data slice
        *caller_account.ref_to_len_in_vm = post_len as u64;

        // this is the len field in the serialized parameters
        let serialized_len_ptr = translate_type_mut::<u64>(
            memory_mapping,
            caller_account
                .vm_data_addr
                .saturating_sub(std::mem::size_of::<u64>() as u64),
            check_aligned,
        )?;
        *serialized_len_ptr = post_len as u64;
    }

    if !direct_mapping {
        // Propagate changes in the callee up to the caller.
        let to_slice = &mut caller_account.serialized_data;
        let from_slice = callee_account
            .get_data()
            .get(0..post_len)
            .ok_or(SyscallError::InvalidLength)?;
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
        assert_matches::assert_matches,
        solana_account::{Account, AccountSharedData, ReadableAccount},
        solana_clock::Epoch,
        solana_instruction::Instruction,
        solana_program_runtime::{
            invoke_context::{BpfAllocator, SerializedAccountMetadata, SyscallContext},
            with_mock_invoke_context_with_feature_set,
        },
        solana_sbpf::{
            ebpf::MM_INPUT_START, memory_region::MemoryRegion, program::SBPFVersion, vm::Config,
        },
        solana_sdk_ids::system_program,
        solana_transaction_context::{InstructionAccount, TransactionAccount},
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
         $program_accounts:expr,
         $instruction_accounts:expr) => {
            let instruction_data = $instruction_data;
            let instruction_accounts = $instruction_accounts
                .iter()
                .enumerate()
                .map(|(index_in_callee, index_in_transaction)| {
                    InstructionAccount::new(
                        *index_in_transaction as IndexOfAccount,
                        index_in_callee as IndexOfAccount,
                        false,
                        $transaction_accounts[*index_in_transaction as usize].2,
                    )
                })
                .collect::<Vec<_>>();
            let transaction_accounts = $transaction_accounts
                .into_iter()
                .map(|a| (a.0, a.1))
                .collect::<Vec<TransactionAccount>>();
            let mut feature_set = SVMFeatureSet::all_enabled();
            feature_set.bpf_account_data_direct_mapping = false;
            let feature_set = &feature_set;
            with_mock_invoke_context_with_feature_set!(
                $invoke_context,
                $transaction_context,
                feature_set,
                transaction_accounts
            );
            $invoke_context
                .transaction_context
                .get_next_instruction_context_mut()
                .unwrap()
                .configure($program_accounts, instruction_accounts, instruction_data);
            $invoke_context.push().unwrap();
        };
    }

    macro_rules! borrow_instruction_account {
        ($invoke_context:expr, $index:expr) => {{
            let instruction_context = $invoke_context
                .transaction_context
                .get_current_instruction_context()
                .unwrap();
            instruction_context
                .try_borrow_instruction_account($invoke_context.transaction_context, $index)
                .unwrap()
        }};
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
            vec![0],
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

        let ins = SyscallInvokeSignedRust::translate_instruction(
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
            vec![0],
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

        let signers = SyscallInvokeSignedRust::translate_signers(
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
    fn test_caller_account_from_account_info() {
        let transaction_accounts =
            transaction_with_one_writable_instruction_account(b"foo".to_vec());
        let account = transaction_accounts[1].1.clone();
        mock_invoke_context!(
            invoke_context,
            transaction_context,
            b"instruction data",
            transaction_accounts,
            vec![0],
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
    fn test_update_caller_account_lamports_owner(direct_mapping: bool) {
        let transaction_accounts = transaction_with_one_writable_instruction_account(vec![]);
        let account = transaction_accounts[1].1.clone();
        mock_invoke_context!(
            invoke_context,
            transaction_context,
            b"instruction data",
            transaction_accounts,
            vec![0],
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

        let mut callee_account = borrow_instruction_account!(invoke_context, 0);

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
            direct_mapping,
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
            vec![0],
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

        let mut callee_account = borrow_instruction_account!(invoke_context, 0);

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
        )
        .unwrap();
        let data_len = callee_account.get_data().len();
        assert_eq!(data_len, 0);
    }

    #[test_matrix([false, true])]
    fn test_update_callee_account_lamports_owner(direct_mapping: bool) {
        let transaction_accounts = transaction_with_one_writable_instruction_account(vec![]);
        let account = transaction_accounts[1].1.clone();

        mock_invoke_context!(
            invoke_context,
            transaction_context,
            b"instruction data",
            transaction_accounts,
            vec![0],
            &[1]
        );

        let mut mock_caller_account =
            MockCallerAccount::new(1234, *account.owner(), account.data(), false);

        let caller_account = mock_caller_account.caller_account();

        let callee_account = borrow_instruction_account!(invoke_context, 0);

        *caller_account.lamports = 42;
        *caller_account.owner = Pubkey::new_unique();

        update_callee_account(&caller_account, callee_account, direct_mapping, true).unwrap();

        let callee_account = borrow_instruction_account!(invoke_context, 0);
        assert_eq!(callee_account.get_lamports(), 42);
        assert_eq!(caller_account.owner, callee_account.get_owner());
    }

    #[test_matrix([false, true])]
    fn test_update_callee_account_data_writable(direct_mapping: bool) {
        let transaction_accounts =
            transaction_with_one_writable_instruction_account(b"foobar".to_vec());
        let account = transaction_accounts[1].1.clone();

        mock_invoke_context!(
            invoke_context,
            transaction_context,
            b"instruction data",
            transaction_accounts,
            vec![0],
            &[1]
        );

        let mut mock_caller_account =
            MockCallerAccount::new(1234, *account.owner(), account.data(), false);

        let mut caller_account = mock_caller_account.caller_account();
        let callee_account = borrow_instruction_account!(invoke_context, 0);

        // direct mapping does not copy data in update_callee_account()
        caller_account.serialized_data[0] = b'b';
        update_callee_account(&caller_account, callee_account, false, true).unwrap();
        let callee_account = borrow_instruction_account!(invoke_context, 0);
        assert_eq!(callee_account.get_data(), b"boobar");

        // growing resize
        let mut data = b"foobarbaz".to_vec();
        *caller_account.ref_to_len_in_vm = data.len() as u64;
        caller_account.serialized_data = &mut data;
        assert_eq!(
            update_callee_account(&caller_account, callee_account, direct_mapping, true).unwrap(),
            direct_mapping,
        );

        // truncating resize
        let mut data = b"baz".to_vec();
        *caller_account.ref_to_len_in_vm = data.len() as u64;
        caller_account.serialized_data = &mut data;
        let callee_account = borrow_instruction_account!(invoke_context, 0);
        assert_eq!(
            update_callee_account(&caller_account, callee_account, direct_mapping, true).unwrap(),
            direct_mapping,
        );

        // close the account
        let mut data = Vec::new();
        caller_account.serialized_data = &mut data;
        *caller_account.ref_to_len_in_vm = 0;
        let mut owner = system_program::id();
        caller_account.owner = &mut owner;
        let callee_account = borrow_instruction_account!(invoke_context, 0);
        update_callee_account(&caller_account, callee_account, direct_mapping, true).unwrap();
        let callee_account = borrow_instruction_account!(invoke_context, 0);
        assert_eq!(callee_account.get_data(), b"");

        // growing beyond address_space_reserved_for_account
        *caller_account.ref_to_len_in_vm = (7 + MAX_PERMITTED_DATA_INCREASE) as u64;
        let result = update_callee_account(&caller_account, callee_account, direct_mapping, true);
        if direct_mapping {
            assert_matches!(
                result,
                Err(error) if error.downcast_ref::<InstructionError>().unwrap() == &InstructionError::InvalidRealloc
            );
        } else {
            result.unwrap();
        }
    }

    #[test_matrix([false, true])]
    fn test_update_callee_account_data_readonly(direct_mapping: bool) {
        let transaction_accounts =
            transaction_with_one_readonly_instruction_account(b"foobar".to_vec());
        let account = transaction_accounts[1].1.clone();

        mock_invoke_context!(
            invoke_context,
            transaction_context,
            b"instruction data",
            transaction_accounts,
            vec![0],
            &[1]
        );

        let mut mock_caller_account =
            MockCallerAccount::new(1234, *account.owner(), account.data(), false);
        let mut caller_account = mock_caller_account.caller_account();
        let callee_account = borrow_instruction_account!(invoke_context, 0);

        // direct mapping does not copy data in update_callee_account()
        caller_account.serialized_data[0] = b'b';
        assert_matches!(
            update_callee_account(
                &caller_account,
                callee_account,
                false,
                true,
            ),
            Err(error) if error.downcast_ref::<InstructionError>().unwrap() == &InstructionError::ExternalAccountDataModified
        );

        // growing resize
        let mut data = b"foobarbaz".to_vec();
        *caller_account.ref_to_len_in_vm = data.len() as u64;
        caller_account.serialized_data = &mut data;
        let callee_account = borrow_instruction_account!(invoke_context, 0);
        assert_matches!(
            update_callee_account(
                &caller_account,
                callee_account,
                direct_mapping,
                true,
            ),
            Err(error) if error.downcast_ref::<InstructionError>().unwrap() == &InstructionError::AccountDataSizeChanged
        );

        // truncating resize
        let mut data = b"baz".to_vec();
        *caller_account.ref_to_len_in_vm = data.len() as u64;
        caller_account.serialized_data = &mut data;
        let callee_account = borrow_instruction_account!(invoke_context, 0);
        assert_matches!(
            update_callee_account(
                &caller_account,
                callee_account,
                direct_mapping,
                true,
            ),
            Err(error) if error.downcast_ref::<InstructionError>().unwrap() == &InstructionError::AccountDataSizeChanged
        );
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
            vec![0],
            &[1, 1]
        );

        invoke_context
            .set_syscall_context(SyscallContext {
                allocator: BpfAllocator::new(solana_program_entrypoint::HEAP_LENGTH as u64),
                accounts_metadata: vec![account_metadata],
                trace_log: Vec::new(),
            })
            .unwrap();

        invoke_context
            .transaction_context
            .get_next_instruction_context_mut()
            .unwrap()
            .configure(
                vec![0],
                vec![
                    InstructionAccount::new(1, 0, false, true),
                    InstructionAccount::new(1, 0, false, true),
                ],
                &[],
            );
        let accounts = SyscallInvokeSignedRust::translate_accounts(
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

    type TestTransactionAccount = (Pubkey, AccountSharedData, bool);
    struct MockCallerAccount {
        lamports: u64,
        owner: Pubkey,
        vm_addr: u64,
        data: Vec<u8>,
        len: u64,
        regions: Vec<MemoryRegion>,
        direct_mapping: bool,
    }

    impl MockCallerAccount {
        fn new(
            lamports: u64,
            owner: Pubkey,
            data: &[u8],
            direct_mapping: bool,
        ) -> MockCallerAccount {
            let vm_addr = MM_INPUT_START;
            let mut region_addr = vm_addr;
            let region_len = mem::size_of::<u64>()
                + if direct_mapping {
                    0
                } else {
                    data.len() + MAX_PERMITTED_DATA_INCREASE
                };
            let mut d = vec![0; region_len];
            let mut regions = vec![];

            // always write the [len] part even when direct mapping
            unsafe { ptr::write_unaligned::<u64>(d.as_mut_ptr().cast(), data.len() as u64) };

            // write the account data when not direct mapping
            if !direct_mapping {
                d[mem::size_of::<u64>()..][..data.len()].copy_from_slice(data);
            }

            // create a region for [len][data+realloc if !direct_mapping]
            regions.push(MemoryRegion::new_writable(&mut d[..region_len], vm_addr));
            region_addr += region_len as u64;

            if direct_mapping {
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
                direct_mapping,
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

        fn caller_account(&mut self) -> CallerAccount {
            let data = if self.direct_mapping {
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

    struct MockAccountInfo<'a> {
        key: Pubkey,
        is_signer: bool,
        is_writable: bool,
        lamports: u64,
        data: &'a [u8],
        owner: Pubkey,
        executable: bool,
        rent_epoch: Epoch,
    }

    impl MockAccountInfo<'_> {
        fn new(key: Pubkey, account: &AccountSharedData) -> MockAccountInfo {
            MockAccountInfo {
                key,
                is_signer: false,
                is_writable: false,
                lamports: account.lamports(),
                data: account.data(),
                owner: *account.owner(),
                executable: account.executable(),
                rent_epoch: account.rent_epoch(),
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
                rent_epoch: self.rent_epoch,
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

    fn is_zeroed(data: &[u8]) -> bool {
        data.iter().all(|b| *b == 0)
    }
}
